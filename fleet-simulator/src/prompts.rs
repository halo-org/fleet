use crate::{Probability, RandomError, Scheduler, WrapConfig, content, content::ContentConfig};
use core::{fmt, range::RangeInclusive};
use fastrand::Rng;
use fleet_core::{
    JsonObject, Prompt, PromptContext, PromptName, PromptOutput, PromptResponse, Prompts,
};
use std::collections::BTreeMap;

/// A simulated [`Prompts`] provider.
pub struct PromptsSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: PromptsSimulatorConfig,
    prompts: BTreeMap<PromptName, PromptState>,
}

struct PromptState {
    prompt: Prompt,
    response: PromptResponse,
}

/// An error returned by [`PromptsSimulator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptsSimulatorError {
    inner: PromptsSimulatorErrorInner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PromptsSimulatorErrorInner {
    Random(RandomError),
    UnknownPrompt,
}

struct PromptsSimulatorConfig {
    get: WrapConfig,
    render: WrapConfig,
    prompt_count: usize,
    output_count: RangeInclusive<usize>,
    assistant: Probability,
    output_content: ContentConfig,
}

impl<'sim> PromptsSimulator<'sim> {
    /// Creates a prompt simulator from the given seed.
    pub fn new(scheduler: &'sim Scheduler, seed: u64) -> Self {
        let mut rng = Rng::with_seed(seed);
        let config = PromptsSimulatorConfig::generate(&mut rng);
        let prompts = generate_prompts(&mut rng, &config);

        Self {
            scheduler,
            config,
            prompts,
        }
    }

    /// Returns the names of the generated prompts in sorted order.
    #[must_use]
    pub fn prompt_names(&self) -> Vec<PromptName> {
        self.prompts.keys().cloned().collect()
    }
}

impl Prompts for PromptsSimulator<'_> {
    type Error = PromptsSimulatorError;

    async fn get(&self, name: &PromptName) -> Result<Option<Prompt>, Self::Error> {
        self.scheduler
            .wrap(&self.config.get, PromptsSimulatorError::random, async {
                Ok(self.prompts.get(name).map(|state| state.prompt.clone()))
            })
            .await
    }

    async fn render(
        &self,
        context: &PromptContext,
        _input: &JsonObject,
    ) -> Result<PromptResponse, Self::Error> {
        self.scheduler
            .wrap(&self.config.render, PromptsSimulatorError::random, async {
                self.prompts
                    .get(&context.prompt.name)
                    .map(|state| state.response.clone())
                    .ok_or_else(PromptsSimulatorError::unknown_prompt)
            })
            .await
    }
}

impl PromptsSimulatorError {
    const fn unknown_prompt() -> Self {
        Self {
            inner: PromptsSimulatorErrorInner::UnknownPrompt,
        }
    }

    const fn random() -> Self {
        Self {
            inner: PromptsSimulatorErrorInner::Random(RandomError),
        }
    }
}

impl fmt::Display for PromptsSimulatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            PromptsSimulatorErrorInner::Random(_) => f.write_str("prompts simulator error"),
            PromptsSimulatorErrorInner::UnknownPrompt => {
                f.write_str("prompts simulator error: unknown prompt")
            }
        }
    }
}

impl core::error::Error for PromptsSimulatorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.inner {
            PromptsSimulatorErrorInner::Random(error) => Some(error),
            PromptsSimulatorErrorInner::UnknownPrompt => None,
        }
    }
}

impl PromptsSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            get: WrapConfig::generate(rng),
            render: WrapConfig::generate(rng),
            prompt_count: rng.usize(1..=5),
            output_count: (0..=rng.usize(0..=5)).into(),
            assistant: Probability::generate(rng),
            output_content: ContentConfig::generate(rng, 4),
        }
    }
}

fn generate_prompts(
    rng: &mut Rng,
    config: &PromptsSimulatorConfig,
) -> BTreeMap<PromptName, PromptState> {
    (0..config.prompt_count)
        .map(|index| {
            let prompt = Prompt {
                name: format!("prompt-{index}").into(),
                config_schema: JsonObject::new(),
                input_schema: JsonObject::new(),
            };

            let output = (0..rng.usize(config.output_count))
                .map(|_| {
                    let content = content::generate(rng, &config.output_content);

                    if config.assistant.sample(rng) {
                        PromptOutput::Assistant(content)
                    } else {
                        PromptOutput::User(content)
                    }
                })
                .collect();

            (
                prompt.name.clone(),
                PromptState {
                    prompt,
                    response: PromptResponse { output },
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use fleet_core::Configured;
    use futures_util::FutureExt as _;

    fn simulator(scheduler: &Scheduler, seed: u64) -> PromptsSimulator<'_> {
        let mut simulator = PromptsSimulator::new(scheduler, seed);

        for config in [&mut simulator.config.get, &mut simulator.config.render] {
            *config = WrapConfig {
                suspend_before: Probability::NEVER,
                suspend_after: Probability::NEVER,
                error: Probability::NEVER,
                error_after: Probability::NEVER,
            };
        }

        simulator
    }

    fn context(name: PromptName) -> PromptContext {
        PromptContext {
            prompt: Configured {
                name,
                config: JsonObject::new(),
            },
            agent_id: 0.into(),
        }
    }

    #[test]
    fn generated_prompts_are_reproducible() {
        let scheduler = Scheduler::new(0);

        for seed in 0..32 {
            let first = simulator(&scheduler, seed);
            let second = simulator(&scheduler, seed);
            let names = first.prompt_names();

            assert!(!names.is_empty());
            assert!(names.is_sorted());
            assert_eq!(names, second.prompt_names());

            for name in names {
                let prompt = first.get(&name).now_or_never().unwrap().unwrap().unwrap();
                assert_eq!(prompt.name, name);
                assert_eq!(second.get(&name).now_or_never().unwrap(), Ok(Some(prompt)));

                let context = context(name);
                let input = JsonObject::new();
                let response = first.render(&context, &input).now_or_never().unwrap();

                assert!(response.is_ok());
                assert_eq!(
                    second.render(&context, &input).now_or_never().unwrap(),
                    response
                );
                assert_eq!(
                    first.render(&context, &input).now_or_never().unwrap(),
                    response
                );
            }
        }
    }

    #[test]
    fn unknown_prompts_are_rejected() {
        let scheduler = Scheduler::new(0);
        let simulator = simulator(&scheduler, 0);
        let context = context("unknown".into());

        assert_eq!(
            simulator.get(&context.prompt.name).now_or_never().unwrap(),
            Ok(None)
        );
        assert_eq!(
            simulator
                .render(&context, &JsonObject::new())
                .now_or_never()
                .unwrap(),
            Err(PromptsSimulatorError::unknown_prompt()),
        );
    }

    #[test]
    fn outputs_cover_empty_user_and_assistant_content() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler, 0);

        for assistant in [Probability::NEVER, Probability::ALWAYS] {
            for (outputs, blocks) in [(0, 0), (3, 0), (3, 2)] {
                simulator.config.prompt_count = 1;
                simulator.config.output_count = (outputs..=outputs).into();
                simulator.config.assistant = assistant;
                simulator.config.output_content.blocks = (blocks..=blocks).into();
                simulator.prompts = generate_prompts(&mut Rng::with_seed(0), &simulator.config);

                let response = simulator
                    .render(&context("prompt-0".into()), &JsonObject::new())
                    .now_or_never()
                    .unwrap()
                    .unwrap();

                assert_eq!(response.output.len(), outputs);
                for output in response.output {
                    let content = match output {
                        PromptOutput::User(content) => {
                            assert_eq!(assistant, Probability::NEVER);
                            content
                        }
                        PromptOutput::Assistant(content) => {
                            assert_eq!(assistant, Probability::ALWAYS);
                            content
                        }
                    };

                    assert_eq!(content.len(), blocks);
                }
            }
        }
    }

    #[test]
    fn lookup_and_render_errors_stop_during_recovery() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler, 0);
        simulator.config.get.error = Probability::ALWAYS;
        simulator.config.render.error = Probability::ALWAYS;
        let context = context("prompt-0".into());
        let input = JsonObject::new();

        assert_eq!(
            simulator.get(&context.prompt.name).now_or_never().unwrap(),
            Err(PromptsSimulatorError::random()),
        );
        assert_eq!(
            simulator.render(&context, &input).now_or_never().unwrap(),
            Err(PromptsSimulatorError::random()),
        );

        scheduler.begin_recovery();

        assert!(
            simulator
                .get(&context.prompt.name)
                .now_or_never()
                .unwrap()
                .unwrap()
                .is_some()
        );
        assert!(
            simulator
                .render(&context, &input)
                .now_or_never()
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn rendering_suspends_before_and_after_lookup() {
        let scheduler = Scheduler::new(0);
        let mut simulator = simulator(&scheduler, 0);
        simulator.config.render.suspend_before = Probability::ALWAYS;
        simulator.config.render.suspend_after = Probability::ALWAYS;
        let context = context("prompt-0".into());
        let input = JsonObject::new();
        let mut future = pin!(simulator.render(&context, &input));
        let mut cx = Context::from_waker(Waker::noop());

        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert_eq!(future.as_mut().poll(&mut cx), Poll::Pending);
        assert!(scheduler.release());
        assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
        assert!(!scheduler.release());
    }
}
