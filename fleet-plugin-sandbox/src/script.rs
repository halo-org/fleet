use crate::{ContentBlock, ModelInput, ModelsErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One scripted model response.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Reasoning streamed before the response.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// The response text.
    #[serde(default)]
    pub text: Option<String>,
    /// Tool calls made after the text.
    #[serde(default)]
    pub tool_calls: Vec<Call>,
    /// Whether to describe the connection's credential.
    #[serde(default)]
    pub credential: bool,
    /// Whether to reject the current OAuth token before describing the credential.
    #[serde(default)]
    pub refresh: bool,
    /// An error returned instead of a response.
    #[serde(default)]
    pub error: Option<Failure>,
    /// The pause before each streamed chunk.
    #[serde(default)]
    pub delay_ms: u64,
}

/// A scripted tool call.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The tool alias.
    pub tool: String,
    /// The tool input.
    #[serde(default)]
    pub input: Map<String, Value>,
}

/// A scripted model error.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    /// The kind of error.
    #[serde(default)]
    pub kind: FailureKind,
    /// The error message.
    pub message: String,
}

/// The kind of a scripted model error.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidContinuation,
    InvalidCompaction,
    ContextTooLong,
    Rejected,
    #[default]
    Other,
}

/// The latest script and the responses since it, preserved through compaction.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    script: String,
    responses: usize,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Script {
    Steps { steps: Vec<Step> },
    Step(Step),
}

impl Step {
    fn text(text: String) -> Self {
        Self {
            text: Some(text),
            ..Self::default()
        }
    }
}

impl From<FailureKind> for ModelsErrorKind {
    fn from(kind: FailureKind) -> Self {
        match kind {
            FailureKind::InvalidContinuation => Self::InvalidContinuation,
            FailureKind::InvalidCompaction => Self::InvalidCompaction,
            FailureKind::ContextTooLong => Self::ContextTooLong,
            FailureKind::Rejected => Self::Rejected,
            FailureKind::Other => Self::Other,
        }
    }
}

/// Returns the step for the next response.
///
/// The latest user input selects the script, and each response since then advances it.
pub fn next(input: &[ModelInput]) -> Step {
    position(input).map_or_else(
        || Step::text("Done.".into()),
        |position| select(&position.script, position.responses),
    )
}

/// Returns the latest script and the responses since it.
pub fn position(input: &[ModelInput]) -> Option<Position> {
    let mut responses = 0_usize;
    for input in input.iter().rev() {
        match input {
            ModelInput::User(content) => {
                return Some(Position {
                    script: text(content),
                    responses,
                });
            }
            ModelInput::Assistant(_) => responses = responses.saturating_add(1),
            ModelInput::Compaction((provenance, compaction)) => {
                if provenance.provider_id == crate::PROVIDER
                    && let Some(state) = &compaction.redacted_content
                    && let Ok(position) = serde_json::from_str::<Position>(state)
                {
                    return Some(Position {
                        responses: position.responses.saturating_add(responses),
                        ..position
                    });
                }
            }
            ModelInput::ToolResult(_) => {}
        }
    }
    None
}

fn text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.as_str()),
            ContentBlock::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn select(text: &str, responses: usize) -> Step {
    let steps = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(script)) => match serde_json::from_value(Value::Object(script)) {
            Ok(Script::Steps { steps }) => steps,
            Ok(Script::Step(step)) => vec![step],
            Err(error) => vec![Step::text(format!("Invalid script: {error}"))],
        },
        _ => vec![Step::text(format!("You said: {text}"))],
    };
    steps
        .into_iter()
        .nth(responses)
        .unwrap_or_else(|| Step::text("Done.".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelCompaction, ModelOutput, ModelProvenance};

    fn user(text: &str) -> ModelInput {
        ModelInput::User(vec![ContentBlock::Text(text.into())])
    }

    fn provenance() -> ModelProvenance {
        ModelProvenance {
            provider_id: "sandbox".into(),
            model_id: "scripted".into(),
        }
    }

    fn assistant() -> ModelInput {
        ModelInput::Assistant((provenance(), vec![ModelOutput::Text("Hi".into())]))
    }

    #[test]
    fn plain_text_is_answered_once() {
        assert_eq!(next(&[user("Hello")]), Step::text("You said: Hello".into()));
        assert_eq!(next(&[]), Step::text("Done.".into()));
    }

    #[test]
    fn responses_advance_the_latest_script() {
        let script = r#"
        {
            "steps": [
                { "tool_calls": [{ "tool": "read", "input": { "path": "notes.txt" } }] },
                { "text": "Read it.", "delay_ms": 5 }
            ]
        }
        "#;
        let first = next(&[user("Earlier"), assistant(), user(script)]);
        let second = next(&[user(script), assistant()]);
        let third = next(&[user(script), assistant(), assistant()]);

        assert_eq!(first.tool_calls[0].tool, "read");
        assert_eq!(first.tool_calls[0].input["path"], "notes.txt");
        assert_eq!(second.text.as_deref(), Some("Read it."));
        assert_eq!(second.delay_ms, 5);
        assert_eq!(third, Step::text("Done.".into()));
    }

    #[test]
    fn compaction_preserves_the_script_position() {
        let script = r#"{ "steps": [{ "text": "One" }, { "text": "Two" }, { "text": "Three" }] }"#;
        let position = position(&[user(script), assistant()]).unwrap();
        let compaction = ModelCompaction {
            content: Some("Compacted 2 inputs.".into()),
            redacted_content: Some(serde_json::to_string(&position).unwrap()),
            attestation: None,
        };

        let resumed = next(&[
            ModelInput::Compaction((provenance(), compaction)),
            assistant(),
        ]);

        assert_eq!(resumed, Step::text("Three".into()));
    }

    #[test]
    fn single_steps_and_invalid_scripts_are_answered() {
        let error = next(&[user(r#"{ "error": { "message": "Overloaded" } }"#)]);
        let invalid = next(&[user(r#"{ "txt": "Hi" }"#)]);

        assert_eq!(
            error.error,
            Some(Failure {
                kind: FailureKind::Other,
                message: "Overloaded".into(),
            })
        );
        assert!(invalid.text.unwrap().starts_with("Invalid script: "));
    }
}
