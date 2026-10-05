use crate::{
    Probability, RandomError, Scheduler, WrapConfig, content, content::ContentConfig, tools,
};
use core::{fmt, range::RangeInclusive};
use fastrand::Rng;
use fleet_core::{
    JsonObject, JsonValue, Model, ModelContext, ModelContinuation, ModelInput, ModelName,
    ModelOutput, ModelProvenance, ModelReasoning, ModelResponse, ModelResponseChunk, ModelTool,
    ModelUsage, Models, ModelsErrorKind, ToolCall, ToolCallId,
};
use futures_util::{Stream, StreamExt as _, stream};
use std::{collections::BTreeMap, sync::Mutex};

/// The minimum context limit of generated models in bytes.
pub const MIN_CONTEXT_LIMIT: usize = 8_192;

/// A simulated [`Models`] provider.
pub struct ModelsSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: ModelsSimulatorConfig,
    state: Mutex<State>,
}

struct State {
    rng: Rng,
    models: BTreeMap<ModelName, ModelState>,
    continuations: BTreeMap<String, ContinuationState>,
    next_continuation_id: u64,
}

struct ModelState {
    model: Model,
    context_limit: usize,
}

struct ContinuationState {
    provenance: ModelProvenance,
    history: Vec<ModelInput>,
}

/// An error returned by [`ModelsSimulator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelsSimulatorError {
    inner: ModelsSimulatorErrorInner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ModelsSimulatorErrorInner {
    Random(RandomError),
    Kind(ModelsErrorKind, &'static str),
}

struct ModelsSimulatorConfig {
    model_count: usize,
    context_limit: RangeInclusive<usize>,
    output_content: ContentConfig,
    get: WrapConfig,
    generate_suspend: Probability,
    generate_error: Probability,
    generate_incomplete: Probability,
    generate_chunk_split: Probability,
    generate_save_continuation: Probability,
    generate_expire_continuation: Probability,
    generate_text: Probability,
    generate_reasoning: Probability,
    generate_reasoning_content: Probability,
    generate_reasoning_redacted_content: Probability,
    generate_reasoning_attestation: Probability,
    generate_interleaved: Probability,
    generate_unknown_tool: Probability,
    generate_invalid_tool_input: Probability,
    invalid_tool_input: InvalidToolInput,
    generate_duplicate_tool_call_id: Probability,
    generate_commentary_limit: RangeInclusive<usize>,
    generate_tool_call_limit: RangeInclusive<usize>,
    generate_tool_rounds_limit: usize,
}

#[derive(Clone, Copy)]
enum InvalidToolInput {
    Missing,
    WrongType,
    ExtraField,
}

impl<'sim> ModelsSimulator<'sim> {
    /// Creates a model simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let config = ModelsSimulatorConfig::generate(&mut rng);
        let models = generate_models(&mut rng, &config);

        Self {
            scheduler,
            config,
            state: Mutex::new(State {
                rng,
                models,
                continuations: BTreeMap::new(),
                next_continuation_id: 0,
            }),
        }
    }

    /// Returns the names of the generated models in sorted order.
    pub fn model_names(&self) -> Vec<ModelName> {
        self.state
            .lock()
            .expect("never poisoned")
            .models
            .keys()
            .cloned()
            .collect()
    }
}

impl Models for ModelsSimulator<'_> {
    type Error = ModelsSimulatorError;

    async fn get(&self, name: &ModelName) -> Result<Option<Model>, Self::Error> {
        self.scheduler
            .wrap(&self.config.get, ModelsSimulatorError::random, async {
                let model = self
                    .state
                    .lock()
                    .expect("never poisoned")
                    .models
                    .get(name)
                    .map(|state| state.model.clone());

                Ok(model)
            })
            .await
    }

    fn generate(
        &self,
        context: &ModelContext,
        input: &[ModelInput],
    ) -> impl Stream<Item = Result<ModelResponseChunk, Self::Error>> + Send {
        #[expect(clippy::significant_drop_tightening, reason = "false positive")]
        let items_future = async move {
            let mut state = self.state.lock().expect("never poisoned");

            let response = state.generate_response(self.scheduler, &self.config, context, input);

            match response {
                Ok(response) => {
                    let mut items =
                        chunk_response(&mut state.rng, response, self.config.generate_chunk_split)
                            .map(Ok)
                            .collect::<Vec<_>>();

                    if let Some(position) = self
                        .scheduler
                        .sample_fault_position(self.config.generate_error, 0..items.len())
                    {
                        items.truncate(position);
                        items.push(Err(ModelsSimulatorError::random()));
                    } else if let Some(position) = self
                        .scheduler
                        .sample_fault_position(self.config.generate_incomplete, 0..items.len())
                    {
                        items.truncate(position);
                    }

                    items
                }
                Err(error) => [Err(error)].into(),
            }
        };

        stream::once(items_future).flat_map(move |items| {
            self.scheduler
                .suspend_stream(self.config.generate_suspend, items)
        })
    }
}

impl State {
    fn generate_response(
        &mut self,
        scheduler: &Scheduler,
        config: &ModelsSimulatorConfig,
        context: &ModelContext,
        input: &[ModelInput],
    ) -> Result<ModelResponse, ModelsSimulatorError> {
        let provenance = ModelProvenance {
            provider_id: "fleet-simulator".into(),
            model_id: context.model.name.to_string(),
        };

        let context_limit = self
            .models
            .get(&context.model.name)
            .ok_or_else(|| ModelsSimulatorError::other("unknown model"))?
            .context_limit;

        let mut history = self.restore_history(
            scheduler,
            &provenance,
            context.continuation.as_ref(),
            config.generate_expire_continuation,
        )?;

        history.extend_from_slice(input);

        if context_size(context, &history) > context_limit {
            return Err(ModelsSimulatorError::context_too_long("context too long"));
        }

        let output = generate_output(scheduler, config, &mut self.rng, context, &history);
        let usage = usage(context, &history, &output);

        history.push(ModelInput::Assistant(provenance.clone(), output.clone()));

        let continuation_token =
            self.save_history(&provenance, history, config.generate_save_continuation);

        Ok(ModelResponse {
            output,
            usage,
            provenance,
            continuation_token,
        })
    }

    fn restore_history(
        &mut self,
        scheduler: &Scheduler,
        provenance: &ModelProvenance,
        continuation: Option<&ModelContinuation>,
        expire_continuation: Probability,
    ) -> Result<Vec<ModelInput>, ModelsSimulatorError> {
        let Some(continuation) = continuation else {
            return Ok(Vec::new());
        };

        if continuation.provenance != *provenance {
            return Err(ModelsSimulatorError::invalid_continuation(
                "unknown continuation provenance",
            ));
        }

        let continuation_state = if scheduler.sample_fault(expire_continuation) {
            self.continuations.remove(&continuation.token);
            None
        } else {
            self.continuations.get(&continuation.token)
        };

        let continuation_state = continuation_state.ok_or_else(|| {
            ModelsSimulatorError::invalid_continuation("unknown or expired continuation")
        })?;

        if continuation_state.provenance != *provenance {
            return Err(ModelsSimulatorError::invalid_continuation(
                "continuation token provenance mismatch",
            ));
        }

        Ok(continuation_state.history.clone())
    }

    fn save_history(
        &mut self,
        provenance: &ModelProvenance,
        history: Vec<ModelInput>,
        save_continuation: Probability,
    ) -> Option<String> {
        if !save_continuation.sample(&mut self.rng) {
            return None;
        }

        let token = self.next_continuation_id.to_string();

        self.next_continuation_id = self
            .next_continuation_id
            .checked_add(1)
            .expect("IDs never overflow");

        self.continuations.insert(
            token.clone(),
            ContinuationState {
                provenance: provenance.clone(),
                history,
            },
        );

        Some(token)
    }
}

impl ModelsSimulatorError {
    const fn invalid_continuation(message: &'static str) -> Self {
        Self {
            inner: ModelsSimulatorErrorInner::Kind(ModelsErrorKind::InvalidContinuation, message),
        }
    }

    const fn context_too_long(message: &'static str) -> Self {
        Self {
            inner: ModelsSimulatorErrorInner::Kind(ModelsErrorKind::ContextTooLong, message),
        }
    }

    const fn other(message: &'static str) -> Self {
        Self {
            inner: ModelsSimulatorErrorInner::Kind(ModelsErrorKind::Other, message),
        }
    }

    const fn random() -> Self {
        Self {
            inner: ModelsSimulatorErrorInner::Random(RandomError),
        }
    }
}

impl fmt::Display for ModelsSimulatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            ModelsSimulatorErrorInner::Random(_) => f.write_str("models simulator error"),
            ModelsSimulatorErrorInner::Kind(_, message) => {
                write!(f, "models simulator error: {message}")
            }
        }
    }
}

impl core::error::Error for ModelsSimulatorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.inner {
            ModelsSimulatorErrorInner::Random(error) => Some(error),
            ModelsSimulatorErrorInner::Kind(..) => None,
        }
    }
}

impl fleet_core::Error<ModelsErrorKind> for ModelsSimulatorError {
    fn kind(&self) -> ModelsErrorKind {
        match self.inner {
            ModelsSimulatorErrorInner::Random(_) => ModelsErrorKind::Other,
            ModelsSimulatorErrorInner::Kind(kind, _) => kind,
        }
    }
}

impl ModelsSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            model_count: rng.usize(1..=5),
            context_limit: if rng.usize(0..5) == 0 {
                (MIN_CONTEXT_LIMIT..=32_768).into()
            } else {
                (65_536..=262_144).into()
            },
            output_content: ContentConfig::generate(rng, 6),
            get: WrapConfig::generate(rng),
            generate_suspend: Probability::generate(rng),
            generate_error: Probability::generate_for_faults(rng),
            generate_incomplete: Probability::generate_for_faults(rng),
            generate_chunk_split: Probability::generate(rng),
            generate_save_continuation: Probability::generate(rng),
            generate_expire_continuation: Probability::generate_for_faults(rng),
            generate_text: Probability::generate(rng),
            generate_reasoning: Probability::generate(rng),
            generate_reasoning_content: Probability::generate(rng),
            generate_reasoning_redacted_content: Probability::generate(rng),
            generate_reasoning_attestation: Probability::generate(rng),
            generate_interleaved: Probability::generate(rng),
            generate_unknown_tool: Probability::generate_for_faults(rng),
            generate_invalid_tool_input: Probability::generate_for_faults(rng),
            invalid_tool_input: match rng.usize(0..3) {
                0 => InvalidToolInput::Missing,
                1 => InvalidToolInput::WrongType,
                _ => InvalidToolInput::ExtraField,
            },
            generate_duplicate_tool_call_id: Probability::generate_for_faults(rng),
            generate_commentary_limit: (0..=rng.usize(0..=5)).into(),
            generate_tool_call_limit: (0..=rng.usize(0..=5)).into(),
            generate_tool_rounds_limit: rng.usize(0..=25),
        }
    }
}

fn chunk_response(
    rng: &mut Rng,
    response: ModelResponse,
    split: Probability,
) -> impl Iterator<Item = ModelResponseChunk> {
    let mut chunks = Vec::new();

    for (index, output) in response.output.iter().enumerate() {
        let index = u32::try_from(index).expect("output count fits u32");

        match output {
            ModelOutput::Text(text) => {
                chunks.push(ModelResponseChunk::TextStart(index));
                chunks.extend(
                    split_text(rng, text, split)
                        .map(|text| ModelResponseChunk::TextDelta(index, text)),
                );
            }
            ModelOutput::Reasoning(reasoning) => {
                chunks.push(ModelResponseChunk::ReasoningStart(index));

                if let Some(text) = &reasoning.content {
                    chunks.extend(
                        split_text(rng, text, split)
                            .map(|text| ModelResponseChunk::ReasoningDelta(index, text)),
                    );
                }
            }
            ModelOutput::ToolCall(call) => {
                chunks.push(ModelResponseChunk::ToolCallStart(
                    index,
                    call.id.clone(),
                    call.tool.clone(),
                ));

                let input = JsonValue::Object(call.input.clone()).to_string();

                chunks.extend(
                    split_text(rng, &input, split)
                        .map(|text| ModelResponseChunk::ToolInputDelta(index, text)),
                );
            }
        }

        chunks.push(ModelResponseChunk::OutputEnd(index, output.clone()));
    }

    chunks.push(ModelResponseChunk::ResponseEnd(response));

    chunks.into_iter()
}

fn split_text(rng: &mut Rng, text: &str, split: Probability) -> impl Iterator<Item = String> {
    let mut chunks = Vec::<String>::new();

    for character in text.chars() {
        match chunks.last_mut() {
            Some(chunk) if !split.sample(rng) => chunk.push(character),
            _ => chunks.push(character.to_string()),
        }
    }

    chunks.into_iter()
}

fn context_size(context: &ModelContext, history: &[ModelInput]) -> usize {
    format!(
        "{:?} {:?} {history:?}",
        context.system_prompt, context.tools
    )
    .len()
}

fn usage(context: &ModelContext, history: &[ModelInput], output: &[ModelOutput]) -> ModelUsage {
    let input = context_size(context, history);
    let output = format!("{output:?}").len();

    ModelUsage {
        input: u32::try_from(input).unwrap_or(u32::MAX),
        output: u32::try_from(output).unwrap_or(u32::MAX),
        cache_read: 0,
        cache_write: 0,
    }
}

fn generate_models(
    rng: &mut Rng,
    config: &ModelsSimulatorConfig,
) -> BTreeMap<ModelName, ModelState> {
    (0..config.model_count)
        .map(|index| {
            let model = Model {
                name: format!("model-{index}").into(),
                config_schema: JsonObject::new(),
            };

            (
                model.name.clone(),
                ModelState {
                    model,
                    context_limit: rng.usize(config.context_limit),
                },
            )
        })
        .collect()
}

fn generate_output(
    scheduler: &Scheduler,
    config: &ModelsSimulatorConfig,
    rng: &mut Rng,
    context: &ModelContext,
    history: &[ModelInput],
) -> Vec<ModelOutput> {
    let tool_rounds_since_user = history
        .iter()
        .rev()
        .take_while(|input| !matches!(input, ModelInput::User(_)))
        .filter(|input| match input {
            ModelInput::Assistant(_, output) => output
                .iter()
                .any(|block| matches!(block, ModelOutput::ToolCall(_))),
            _ => false,
        })
        .count();

    let tools_allowed =
        !context.tools.is_empty() && tool_rounds_since_user < config.generate_tool_rounds_limit;

    let mut output = Vec::new();

    for _ in 0..rng.usize(config.generate_commentary_limit) {
        if config.generate_reasoning.sample(rng) {
            output.push(ModelOutput::Reasoning(generate_reasoning(rng, config)));
        }

        if config.generate_text.sample(rng) {
            output.push(ModelOutput::Text(content::generate_text(
                rng,
                &config.output_content,
            )));
        }
    }

    if tools_allowed {
        let mut calls = (0..rng.usize(config.generate_tool_call_limit))
            .map(|_| generate_tool_call(rng, config, &context.tools))
            .collect::<Vec<_>>();

        if !calls.is_empty() && scheduler.sample_fault(config.generate_duplicate_tool_call_id) {
            let index = rng.usize(0..calls.len());
            let previous_ids = history
                .iter()
                .flat_map(|input| match input {
                    ModelInput::Assistant(_, output) => output.as_slice(),
                    _ => &[],
                })
                .filter_map(|output| match output {
                    ModelOutput::ToolCall(call) => Some(&call.id),
                    _ => None,
                })
                .chain(
                    calls
                        .iter()
                        .enumerate()
                        .filter_map(|(other, call)| (other != index).then_some(&call.id)),
                )
                .collect::<Vec<_>>();

            if let Some(id) = rng.choice(previous_ids).cloned() {
                calls.get_mut(index).expect("index came from calls").id = id;
            }
        }

        output.extend(calls.into_iter().map(ModelOutput::ToolCall));
    }

    if config.generate_interleaved.sample(rng) {
        rng.shuffle(&mut output);
    }

    output
}

fn generate_reasoning(rng: &mut Rng, config: &ModelsSimulatorConfig) -> ModelReasoning {
    ModelReasoning {
        content: config
            .generate_reasoning_content
            .sample(rng)
            .then(|| content::generate_text(rng, &config.output_content)),
        redacted_content: config
            .generate_reasoning_redacted_content
            .sample(rng)
            .then(|| content::generate_text(rng, &config.output_content)),
        attestation: config
            .generate_reasoning_attestation
            .sample(rng)
            .then(|| content::generate_text(rng, &config.output_content)),
    }
}

fn generate_tool_call(
    rng: &mut Rng,
    config: &ModelsSimulatorConfig,
    tools: &[ModelTool],
) -> ToolCall {
    let tool = rng.choice(tools).expect("tools is not empty");
    let id = ToolCallId::from(format!("{:032x}", rng.u128(..)));

    let name = if config.generate_unknown_tool.sample(rng) {
        let mut name = "unknown-tool".to_owned();

        while tools.iter().any(|tool| tool.name.as_ref() == name) {
            name.push('_');
        }

        name.into()
    } else {
        tool.name.clone()
    };

    ToolCall {
        id,
        tool: name,
        is_async: tool.is_async,
        input: generate_tool_input(rng, config),
    }
}

fn generate_tool_input(rng: &mut Rng, config: &ModelsSimulatorConfig) -> JsonObject {
    let mut input = tools::input(content::generate_text(rng, &config.output_content));

    if config.generate_invalid_tool_input.sample(rng) {
        match config.invalid_tool_input {
            InvalidToolInput::Missing => input.clear(),
            InvalidToolInput::WrongType => {
                input.insert("input".into(), 42.into());
            }
            InvalidToolInput::ExtraField => {
                input.insert("extra".into(), true.into());
            }
        }
    }

    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{Configured, ContentBlock};
    use futures_util::FutureExt as _;
    use std::collections::BTreeSet;

    fn simulator(scheduler: &Scheduler) -> ModelsSimulator<'_> {
        let mut simulator = ModelsSimulator::new(scheduler, 0);
        let config = &mut simulator.config;
        config.generate_suspend = Probability::NEVER;
        config.generate_error = Probability::NEVER;
        config.generate_incomplete = Probability::NEVER;
        config.generate_save_continuation = Probability::NEVER;
        config.generate_expire_continuation = Probability::NEVER;
        config.generate_unknown_tool = Probability::NEVER;
        config.generate_invalid_tool_input = Probability::NEVER;
        config.generate_duplicate_tool_call_id = Probability::NEVER;
        config.generate_commentary_limit = (0..=0).into();
        config.generate_tool_call_limit = (0..=0).into();
        config.generate_tool_rounds_limit = 3;
        simulator
    }

    fn context() -> ModelContext {
        ModelContext {
            model: Configured {
                name: "model-0".into(),
                config: JsonObject::new(),
            },
            system_prompt: String::new(),
            tools: vec![ModelTool {
                name: "unknown-tool".into(),
                description: None,
                is_async: true,
                input_schema: tools::input_schema(),
            }],
            continuation: None,
        }
    }

    fn chunks(
        simulator: &ModelsSimulator<'_>,
        input: &[ModelInput],
    ) -> Vec<Result<ModelResponseChunk, ModelsSimulatorError>> {
        simulator
            .generate(&context(), input)
            .collect()
            .now_or_never()
            .unwrap()
    }

    fn completed(chunks: &[Result<ModelResponseChunk, ModelsSimulatorError>]) -> &ModelResponse {
        let Some(Ok(ModelResponseChunk::ResponseEnd(response))) = chunks.last() else {
            panic!("expected a completed response");
        };
        response
    }

    fn calls(response: &ModelResponse) -> Vec<&ToolCall> {
        response
            .output
            .iter()
            .filter_map(|output| match output {
                ModelOutput::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_and_reasoning_only_responses_complete() {
        for reasoning in [Probability::NEVER, Probability::ALWAYS] {
            let scheduler = Scheduler::new(0);
            let mut simulator = simulator(&scheduler);
            simulator.config.generate_commentary_limit = (1..=1).into();
            simulator.config.generate_text = Probability::NEVER;
            simulator.config.generate_reasoning = reasoning;
            simulator.config.generate_reasoning_content = Probability::ALWAYS;
            simulator.config.generate_reasoning_redacted_content = Probability::NEVER;
            simulator.config.generate_reasoning_attestation = Probability::ALWAYS;

            let chunks = chunks(&simulator, &[]);
            let response = completed(&chunks);

            if reasoning == Probability::NEVER {
                assert!(response.output.is_empty());
                assert_eq!(chunks.len(), 1);
            } else {
                let [ModelOutput::Reasoning(reasoning)] = response.output.as_slice() else {
                    panic!("expected only reasoning");
                };
                assert!(reasoning.content.is_some());
                assert!(reasoning.redacted_content.is_none());
                assert!(reasoning.attestation.is_some());
            }
        }
    }

    #[test]
    fn interleaving_covers_between_and_after_calls() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler);
        simulator.config.generate_commentary_limit = (2..=2).into();
        simulator.config.generate_tool_call_limit = (2..=2).into();
        simulator.config.generate_text = Probability::ALWAYS;
        simulator.config.generate_reasoning = Probability::ALWAYS;
        simulator.config.generate_interleaved = Probability::NEVER;

        let ordered = chunks(&simulator, &[]);
        assert!(matches!(
            completed(&ordered).output.last(),
            Some(ModelOutput::ToolCall(_))
        ));

        simulator.config.generate_interleaved = Probability::ALWAYS;
        let mut text_between = false;
        let mut reasoning_between = false;
        let mut text_after = false;
        let mut reasoning_after = false;

        for _ in 0..128 {
            let chunks = chunks(&simulator, &[]);
            let response = completed(&chunks);
            let kinds = response
                .output
                .iter()
                .map(|output| match output {
                    ModelOutput::Text(_) => 'T',
                    ModelOutput::Reasoning(_) => 'R',
                    ModelOutput::ToolCall(_) => 'C',
                })
                .collect::<String>();

            text_between |= kinds.contains("CTC");
            reasoning_between |= kinds.contains("CRC");
            text_after |= kinds.ends_with('T');
            reasoning_after |= kinds.ends_with('R');

            for chunk in chunks.iter().flatten() {
                if let ModelResponseChunk::OutputEnd(index, output) = chunk {
                    assert_eq!(response.output.get(*index as usize), Some(output));
                }
            }
        }

        assert!(text_between && reasoning_between && text_after && reasoning_after);
    }

    #[test]
    fn tool_behaviour_remains_enabled_during_recovery() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler);
        simulator.config.generate_tool_call_limit = (1..=1).into();

        let valid = chunks(&simulator, &[]);
        let call = calls(completed(&valid))[0];
        assert_eq!(call.tool, context().tools[0].name);
        assert!(call.is_async);
        assert_eq!(call.input.len(), 1);
        assert!(call.input["input"].is_string());

        simulator.config.generate_unknown_tool = Probability::ALWAYS;
        simulator.config.generate_invalid_tool_input = Probability::ALWAYS;
        scheduler.begin_recovery();

        for kind in [
            InvalidToolInput::Missing,
            InvalidToolInput::WrongType,
            InvalidToolInput::ExtraField,
        ] {
            simulator.config.invalid_tool_input = kind;

            let invalid = chunks(&simulator, &[]);
            let call = calls(completed(&invalid))[0];

            assert_ne!(call.tool, context().tools[0].name);
            assert!(
                call.input.len() != 1 || !call.input.get("input").is_some_and(JsonValue::is_string)
            );
        }
    }

    #[test]
    fn duplicate_ids_cover_current_and_previous_responses_and_stop_in_recovery() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler);
        simulator.config.generate_tool_call_limit = (5..=5).into();
        simulator.config.generate_duplicate_tool_call_id = Probability::ALWAYS;

        let first = chunks(&simulator, &[]);
        let response = completed(&first);
        let first_calls = calls(response);

        let first_ids = first_calls
            .iter()
            .map(|call| &call.id)
            .collect::<BTreeSet<_>>();

        assert_eq!(first_calls.len(), 5);
        assert_eq!(first_ids.len(), 4);

        let history = [ModelInput::Assistant(
            response.provenance.clone(),
            response.output.clone(),
        )];
        simulator.config.generate_tool_call_limit = (1..=1).into();
        let repeated = chunks(&simulator, &history);

        assert!(first_ids.contains(&calls(completed(&repeated))[0].id));

        scheduler.begin_recovery();
        simulator.config.generate_tool_call_limit = (5..=5).into();

        let recovered = chunks(&simulator, &history);
        let recovered_calls = calls(completed(&recovered));

        assert_eq!(
            recovered_calls
                .iter()
                .map(|call| &call.id)
                .collect::<BTreeSet<_>>()
                .len(),
            5
        );
        assert!(
            recovered_calls
                .iter()
                .all(|call| !first_ids.contains(&call.id))
        );

        simulator.config.generate_tool_rounds_limit = 1;
        assert!(calls(completed(&chunks(&simulator, &history))).is_empty());
    }

    #[test]
    fn incomplete_and_error_streams_recover() {
        for error in [false, true] {
            let scheduler = Scheduler::new(0);
            let mut simulator = simulator(&scheduler);
            simulator.config.generate_commentary_limit = (2..=2).into();
            simulator.config.generate_text = Probability::ALWAYS;
            simulator.config.generate_incomplete = Probability::ALWAYS;
            simulator.config.generate_error = if error {
                Probability::ALWAYS
            } else {
                Probability::NEVER
            };

            let incomplete = chunks(&simulator, &[]);

            assert!(
                !incomplete
                    .iter()
                    .any(|chunk| matches!(chunk, Ok(ModelResponseChunk::ResponseEnd(_))))
            );
            assert_eq!(incomplete.iter().any(Result::is_err), error);
            if error {
                assert!(incomplete.last().unwrap().is_err());
            }

            scheduler.begin_recovery();

            let recovered = chunks(&simulator, &[]);
            assert!(recovered.iter().all(Result::is_ok));
            completed(&recovered);
        }
    }

    #[test]
    fn context_limits_include_small_and_large_regimes() {
        let mut small = false;
        let mut large = false;

        for seed in 0..128 {
            let mut rng = Rng::with_seed(seed);
            let config = ModelsSimulatorConfig::generate(&mut rng);
            let models = generate_models(&mut rng, &config);

            assert_eq!(models.len(), config.model_count);
            for model in models.values() {
                small |= model.context_limit <= 32_768;
                large |= model.context_limit >= 65_536;
            }
        }

        assert!(small && large);

        let scheduler = Scheduler::new(0);
        let simulator = simulator(&scheduler);
        simulator
            .state
            .lock()
            .unwrap()
            .models
            .get_mut(&"model-0".into())
            .unwrap()
            .context_limit = MIN_CONTEXT_LIMIT;
        let input = [ModelInput::User(vec![ContentBlock::Text(
            "x".repeat(MIN_CONTEXT_LIMIT),
        )])];

        let too_long = chunks(&simulator, &input);

        assert!(matches!(
            too_long.as_slice(),
            [Err(ModelsSimulatorError {
                inner: ModelsSimulatorErrorInner::Kind(ModelsErrorKind::ContextTooLong, _),
            })]
        ));

        completed(&chunks(&simulator, &[]));
    }
}
