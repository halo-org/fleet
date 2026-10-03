use crate::{
    Probability, RandomError, Scheduler, TrySuspendConfig, WrapConfig, content,
    content::ContentConfig,
};
use core::fmt;
use fastrand::Rng;
use fleet_core::{
    AgentId, ContentBlock, JsonObject, JsonValue, Tool, ToolCall, ToolCallId, ToolContext,
    ToolName, ToolOutput, Tools, ToolsErrorKind,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

const MAX_OUTPUT_TEXT_LENGTH_LOG2: u32 = 6;

/// A simulated [`Tools`] provider.
pub struct ToolsSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: ToolsSimulatorConfig,
    state: Mutex<State>,
}

struct State {
    rng: Rng,
    tools: BTreeMap<ToolName, ToolState>,
    non_retry_calls: BTreeSet<(AgentId, ToolCallId)>,
}

struct ToolState {
    tool: Tool,
    is_safe_to_retry: bool,
}

/// An error returned by [`ToolsSimulator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolsSimulatorError {
    inner: ToolsSimulatorErrorInner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ToolsSimulatorErrorInner {
    Random(RandomError),
    Kind(ToolsErrorKind, &'static str),
}

struct ToolsSimulatorConfig {
    get: WrapConfig,
    execute: WrapConfig,
    interrupted_output: TrySuspendConfig,
    tool_count: usize,
    description: Probability,
    is_async: Probability,
    is_safe_to_retry: Probability,
    supports_structured_output: Probability,
    generate_structured_output: Probability,
    description_content: ContentConfig,
    output_content: ContentConfig,
}

impl<'sim> ToolsSimulator<'sim> {
    /// Creates a tool simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let config = ToolsSimulatorConfig::generate(&mut rng);
        let tools = generate_tools(&mut rng, &config);

        Self {
            scheduler,
            config,
            state: Mutex::new(State {
                rng,
                tools,
                non_retry_calls: BTreeSet::new(),
            }),
        }
    }

    /// Returns the names of the generated tools in sorted order.
    pub fn tool_names(&self) -> Vec<ToolName> {
        self.state
            .lock()
            .expect("never poisoned")
            .tools
            .keys()
            .cloned()
            .collect()
    }

    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "future side effects will be async"
    )]
    async fn execute_tool(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, ToolsSimulatorError> {
        let (is_safe_to_retry, supports_structured_output) = self
            .state
            .lock()
            .expect("never poisoned")
            .tools
            .get(&context.tool.name)
            .map(|state| (state.is_safe_to_retry, state.tool.output_schema.is_some()))
            .ok_or_else(|| ToolsSimulatorError::other("unknown tool"))?;

        if is_retry && !is_safe_to_retry {
            return Err(ToolsSimulatorError::unsafe_to_retry("unsafe to retry"));
        }

        if input.len() != 1 || !input.get("input").is_some_and(JsonValue::is_string) {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(
                    "Expected exactly one string field: input".into(),
                )],
                structured_content: None,
                is_error: true,
            });
        }

        // TODO: Add side effects using client store.

        let rng = &mut self.state.lock().expect("never poisoned").rng;

        Ok(ToolOutput {
            content: content::generate(rng, &self.config.output_content),
            structured_content: (supports_structured_output
                && self.config.generate_structured_output.sample(rng))
            .then(|| {
                JsonObject::from_iter([(
                    "output".into(),
                    content::generate_text(rng, &self.config.output_content).into(),
                )])
            }),
            is_error: false,
        })
    }
}

impl Tools for ToolsSimulator<'_> {
    type Error = ToolsSimulatorError;

    async fn get(&self, name: &ToolName) -> Result<Option<Tool>, Self::Error> {
        self.scheduler
            .wrap(&self.config.get, ToolsSimulatorError::random, async {
                let tool = self
                    .state
                    .lock()
                    .expect("never poisoned")
                    .tools
                    .get(name)
                    .map(|state| state.tool.clone());

                Ok(tool)
            })
            .await
    }

    async fn execute(
        &self,
        context: &ToolContext,
        is_retry: bool,
        input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        self.state.lock().expect("never poisoned").record_attempt(
            context.agent_id,
            &context.call_id,
            is_retry,
        );

        self.scheduler
            .wrap(&self.config.execute, ToolsSimulatorError::random, async {
                self.execute_tool(context, is_retry, input).await
            })
            .await
    }

    async fn interrupted_output(
        &self,
        context: &ToolContext,
        _input: &JsonObject,
    ) -> Result<ToolOutput, Self::Error> {
        self.scheduler
            .try_suspend(&self.config.interrupted_output, ToolsSimulatorError::random)
            .await?;

        Ok(ToolOutput {
            content: Vec::from([ContentBlock::Text(format!(
                "Tool call interrupted: {}",
                context.tool.name,
            ))]),
            structured_content: None,
            is_error: true,
        })
    }

    fn unknown_tool_output(&self, _agent_id: AgentId, call: &ToolCall) -> ToolOutput {
        ToolOutput {
            content: Vec::from([ContentBlock::Text(format!("Unknown tool: {}", call.tool))]),
            structured_content: None,
            is_error: true,
        }
    }
}

impl State {
    fn record_attempt(&mut self, agent_id: AgentId, call_id: &ToolCallId, is_retry: bool) {
        assert!(
            is_retry || self.non_retry_calls.insert((agent_id, call_id.clone())),
            "tool call must have at most one non-retry execution",
        );
    }
}

impl ToolsSimulatorError {
    const fn unsafe_to_retry(message: &'static str) -> Self {
        Self {
            inner: ToolsSimulatorErrorInner::Kind(ToolsErrorKind::UnsafeToRetry, message),
        }
    }

    const fn other(message: &'static str) -> Self {
        Self {
            inner: ToolsSimulatorErrorInner::Kind(ToolsErrorKind::Other, message),
        }
    }

    const fn random() -> Self {
        Self {
            inner: ToolsSimulatorErrorInner::Random(RandomError),
        }
    }
}

impl fmt::Display for ToolsSimulatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            ToolsSimulatorErrorInner::Random(_) => f.write_str("tools simulator error"),
            ToolsSimulatorErrorInner::Kind(_, message) => {
                write!(f, "tools simulator error: {message}")
            }
        }
    }
}

impl core::error::Error for ToolsSimulatorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.inner {
            ToolsSimulatorErrorInner::Random(error) => Some(error),
            ToolsSimulatorErrorInner::Kind(..) => None,
        }
    }
}

impl fleet_core::Error<ToolsErrorKind> for ToolsSimulatorError {
    fn kind(&self) -> ToolsErrorKind {
        match self.inner {
            ToolsSimulatorErrorInner::Random(_) => ToolsErrorKind::Other,
            ToolsSimulatorErrorInner::Kind(kind, _) => kind,
        }
    }
}

impl ToolsSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            get: WrapConfig::generate(rng),
            execute: WrapConfig::generate(rng),
            interrupted_output: TrySuspendConfig::generate(rng),
            tool_count: rng.usize(0..=5),
            description: Probability::generate(rng),
            is_async: Probability::generate(rng),
            is_safe_to_retry: Probability::generate(rng),
            supports_structured_output: Probability::generate(rng),
            generate_structured_output: Probability::generate(rng),
            description_content: ContentConfig::generate(rng, 6),
            output_content: ContentConfig::generate(rng, MAX_OUTPUT_TEXT_LENGTH_LOG2),
        }
    }
}

fn generate_tools(rng: &mut Rng, config: &ToolsSimulatorConfig) -> BTreeMap<ToolName, ToolState> {
    (0..config.tool_count)
        .map(|index| {
            let tool = Tool {
                name: format!("tool-{index}").into(),
                description: config
                    .description
                    .sample(rng)
                    .then(|| content::generate_text(rng, &config.description_content)),
                is_async: config.is_async.sample(rng),
                config_schema: JsonObject::new(),
                input_schema: input_schema(),
                output_schema: config
                    .supports_structured_output
                    .sample(rng)
                    .then(|| string_object_schema("output")),
            };

            (
                tool.name.clone(),
                ToolState {
                    tool,
                    is_safe_to_retry: config.is_safe_to_retry.sample(rng),
                },
            )
        })
        .collect()
}

/// Wraps text in the input object expected by simulated tools.
pub fn input(text: String) -> JsonObject {
    JsonObject::from_iter([("input".into(), text.into())])
}

/// Returns the input schema shared by simulated tools.
pub fn input_schema() -> JsonObject {
    string_object_schema("input")
}

fn string_object_schema(field: &str) -> JsonObject {
    JsonObject::from_iter([
        ("type".into(), "object".into()),
        (
            "properties".into(),
            JsonObject::from_iter([(
                field.into(),
                JsonObject::from_iter([("type".into(), "string".into())]).into(),
            )])
            .into(),
        ),
        ("required".into(), vec![JsonValue::from(field)].into()),
        ("additionalProperties".into(), false.into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MIN_CONTEXT_WINDOW;
    use fleet_core::Configured;
    use futures_util::FutureExt as _;

    fn simulator(scheduler: &Scheduler) -> ToolsSimulator<'_> {
        let mut simulator = ToolsSimulator::new(scheduler, 0);
        simulator.config.execute = WrapConfig {
            suspend_before: Probability::NEVER,
            suspend_after: Probability::NEVER,
            error: Probability::NEVER,
            error_after: Probability::NEVER,
        };
        simulator.config.tool_count = 1;
        simulator.config.is_safe_to_retry = Probability::NEVER;
        simulator.config.supports_structured_output = Probability::ALWAYS;
        simulator.state.get_mut().unwrap().tools =
            generate_tools(&mut Rng::with_seed(0), &simulator.config);
        simulator
    }

    fn context(id: usize) -> ToolContext {
        ToolContext {
            tool: Configured {
                name: "tool-0".into(),
                config: JsonObject::new(),
            },
            agent_id: 0.into(),
            call_id: id.to_string().into(),
        }
    }

    #[test]
    fn initial_execution_can_arrive_before_or_after_retries() {
        for attempts in [
            [false, true, true],
            [true, false, true],
            [true, true, false],
        ] {
            for is_safe_to_retry in [false, true] {
                let scheduler = Scheduler::new(0);
                let mut simulator = simulator(&scheduler);
                simulator
                    .state
                    .get_mut()
                    .unwrap()
                    .tools
                    .get_mut(&"tool-0".into())
                    .unwrap()
                    .is_safe_to_retry = is_safe_to_retry;

                for is_retry in attempts {
                    let result = simulator
                        .execute(&context(0), is_retry, &input("valid".into()))
                        .now_or_never()
                        .unwrap();

                    if is_retry && !is_safe_to_retry {
                        assert!(matches!(
                            result,
                            Err(ToolsSimulatorError {
                                inner: ToolsSimulatorErrorInner::Kind(
                                    ToolsErrorKind::UnsafeToRetry,
                                    _
                                ),
                            })
                        ));
                    } else {
                        assert!(!result.unwrap().is_error);
                    }
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "tool call must have at most one non-retry execution")]
    fn repeated_non_retry_execution_is_rejected() {
        let scheduler = Scheduler::new(0);
        let simulator = simulator(&scheduler);

        for is_retry in [false, true, false] {
            let _result = simulator
                .execute(&context(0), is_retry, &input("valid".into()))
                .now_or_never()
                .unwrap();
        }
    }

    #[test]
    fn invalid_input_is_a_tool_error_output() {
        let scheduler = Scheduler::new(0);
        let simulator = simulator(&scheduler);
        let schema = input_schema();

        assert_eq!(schema["properties"]["input"]["type"], "string");
        assert_eq!(schema["required"], JsonValue::from(vec!["input"]));
        assert_eq!(schema["additionalProperties"], false);

        let invalid = [
            JsonObject::new(),
            JsonObject::from_iter([("input".into(), 42.into())]),
            JsonObject::from_iter([
                ("input".into(), "text".into()),
                ("extra".into(), true.into()),
            ]),
        ];

        for (index, input) in invalid.iter().enumerate() {
            let output = simulator
                .execute(&context(index), false, input)
                .now_or_never()
                .unwrap()
                .unwrap();

            assert!(output.is_error);
            assert!(!output.content.is_empty());
            assert!(output.structured_content.is_none());
        }

        let valid = simulator
            .execute(&context(3), false, &input(String::new()))
            .now_or_never()
            .unwrap()
            .unwrap();

        assert!(!valid.is_error);
    }

    #[test]
    fn outputs_cover_empty_varied_and_structured_content() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler);
        simulator.config.output_content.blocks = (0..=0).into();
        simulator.config.generate_structured_output = Probability::NEVER;
        let input = input("valid".into());

        let empty = simulator
            .execute(&context(0), false, &input)
            .now_or_never()
            .unwrap()
            .unwrap();

        assert!(empty.content.is_empty());
        assert!(empty.structured_content.is_none());
        assert!(!empty.is_error);

        simulator.config.output_content.blocks = (1..=1).into();
        simulator.config.output_content.text_length_log2 =
            (MAX_OUTPUT_TEXT_LENGTH_LOG2..=MAX_OUTPUT_TEXT_LENGTH_LOG2).into();
        simulator.config.generate_structured_output = Probability::ALWAYS;
        let mut lengths = BTreeSet::new();

        for id in 1..=8 {
            let output = simulator
                .execute(&context(id), false, &input)
                .now_or_never()
                .unwrap()
                .unwrap();
            let [ContentBlock::Text(text)] = output.content.as_slice() else {
                panic!("expected one text block");
            };

            lengths.insert(text.len());
            assert!(text.len() <= (1 << MAX_OUTPUT_TEXT_LENGTH_LOG2) * "👩‍💻".len());
            assert!(format!("{output:?}").len() < MIN_CONTEXT_WINDOW);
            assert!(!output.is_error);
            assert!(output.structured_content.unwrap()["output"].is_string());
        }

        assert!(lengths.len() > 1);

        simulator
            .state
            .get_mut()
            .unwrap()
            .tools
            .get_mut(&"tool-0".into())
            .unwrap()
            .tool
            .output_schema = None;

        let unstructured = simulator
            .execute(&context(9), false, &input)
            .now_or_never()
            .unwrap()
            .unwrap();

        assert!(unstructured.structured_content.is_none());

        let retry = simulator
            .execute(&context(9), true, &input)
            .now_or_never()
            .unwrap();

        assert!(matches!(
            retry,
            Err(ToolsSimulatorError {
                inner: ToolsSimulatorErrorInner::Kind(ToolsErrorKind::UnsafeToRetry, _),
            })
        ));
    }

    #[test]
    fn largest_tool_output_fits_smallest_context() {
        let text = "👩‍💻".repeat(1 << MAX_OUTPUT_TEXT_LENGTH_LOG2);
        let output = ToolOutput {
            content: vec![ContentBlock::Text(text.clone()); 5],
            structured_content: Some(JsonObject::from_iter([("output".into(), text.into())])),
            is_error: false,
        };

        assert!(format!("{output:?}").len() < MIN_CONTEXT_WINDOW);
    }

    #[test]
    fn tool_definitions_follow_config() {
        for enabled in [Probability::NEVER, Probability::ALWAYS] {
            let mut rng = Rng::with_seed(0);
            let mut config = ToolsSimulatorConfig::generate(&mut rng);
            config.tool_count = 3;
            config.description = enabled;
            config.is_async = enabled;
            config.is_safe_to_retry = enabled;
            config.supports_structured_output = enabled;

            let tools = generate_tools(&mut rng, &config);

            assert_eq!(tools.len(), 3);
            for state in tools.values() {
                assert_eq!(
                    state.tool.description.is_some(),
                    enabled == Probability::ALWAYS
                );
                assert_eq!(state.tool.is_async, enabled == Probability::ALWAYS);
                assert_eq!(state.is_safe_to_retry, enabled == Probability::ALWAYS);
                assert_eq!(
                    state.tool.output_schema.is_some(),
                    enabled == Probability::ALWAYS
                );
            }

            config.tool_count = 0;
            assert!(generate_tools(&mut rng, &config).is_empty());
        }
    }
}
