//! WebAssembly tools that wait for and cancel an agent's tasks.

mod bindings;

pub(crate) use self::bindings::{
    exports::fleet::providers::tools::{Guest, ToolsError, ToolsErrorKind},
    fleet::common::types::{ContentBlock, Tool, ToolContext, ToolOutput},
    fleet::host::tool_calls::{
        self, TaskStatus, ToolCallsError, ToolCallsErrorKind, ToolCallsState,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

struct Tasks;

enum Error {
    Input(String),
    Interrupted,
    Tools(ToolsError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitInput {
    tasks: BTreeSet<String>,
    #[serde(default)]
    until: Until,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Until {
    #[default]
    All,
    Any,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    tasks: BTreeSet<String>,
}

impl Guest for Tasks {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Tool>, ToolsError> {
        Ok(match name.as_str() {
            "wait" => Some(metadata(
                "wait",
                "Wait for tasks",
                "Waits until the named tasks finish or a steering message arrives. Each task's \
                 result arrives as its own tool result.",
                &json!({
                    "tasks": tasks_schema(),
                    "until": {
                        "type": "string",
                        "enum": ["all", "any"],
                        "description": "Whether to wait for all tasks or any task to finish. \
                                        Defaults to all.",
                    },
                }),
            )),
            "cancel" => Some(metadata(
                "cancel",
                "Cancel tasks",
                "Interrupts the named tasks and waits until they stop.",
                &json!({
                    "tasks": tasks_schema(),
                }),
            )),
            _ => None,
        })
    }

    async fn execute(
        context: ToolContext,
        _is_retry: bool,
        input: String,
    ) -> Result<ToolOutput, ToolsError> {
        let result = match context.tool.name.as_str() {
            "wait" => wait(&input).await,
            "cancel" => cancel(&input).await,
            _ => return Ok(error_output("The tool is not available.")),
        };
        match result {
            Ok(text) => Ok(ToolOutput {
                content: vec![ContentBlock::Text(text)],
                structured_content: None,
                is_error: false,
            }),
            Err(Error::Input(message)) => Ok(error_output(message)),
            Err(Error::Interrupted) => Ok(interrupted_output(&context)),
            Err(Error::Tools(error)) => Err(error),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn interrupted_output(
        context: ToolContext,
        _input: String,
    ) -> Result<ToolOutput, ToolsError> {
        Ok(interrupted_output(&context))
    }
}

async fn wait(input: &str) -> Result<String, Error> {
    let input: WaitInput = parse(input)?;
    let tasks = names(input.tasks)?;
    let mut state = read_state(None, &tasks).await?;
    loop {
        if let Some(text) = wait_output(&state, input.until)? {
            return Ok(text);
        }
        state = read_state(Some(state.revision), &tasks).await?;
    }
}

/// Returns the wait's output once it is done.
fn wait_output(state: &ToolCallsState, until: Until) -> Result<Option<String>, Error> {
    check_tasks(state)?;
    let finished = tasks_with(state, |status| matches!(status, TaskStatus::Completed(_)));
    let running = tasks_with(state, |status| matches!(status, TaskStatus::Pending(_)));
    let steering = state.messages.steering;
    let done = match until {
        Until::All => running.is_empty(),
        Until::Any => !finished.is_empty(),
    };
    if !done && steering == 0 {
        return Ok(None);
    }

    let mut lines = Vec::new();
    if !finished.is_empty() {
        lines.push(format!("Finished: {}.", finished.join(", ")));
    }
    if !running.is_empty() {
        lines.push(format!("Still running: {}.", running.join(", ")));
    }
    match steering {
        0 => {}
        1 => lines.push("A new message arrived.".into()),
        count => lines.push(format!("{count} new messages arrived.")),
    }
    Ok(Some(lines.join("\n")))
}

async fn cancel(input: &str) -> Result<String, Error> {
    let input: CancelInput = parse(input)?;
    let tasks = names(input.tasks)?;
    let mut state = read_state(None, &tasks).await?;
    check_tasks(&state)?;
    let mut lines = Vec::new();
    let running = tasks_with(&state, |status| matches!(status, TaskStatus::Pending(_)));
    if !running.is_empty() {
        lines.push(format!("Stopped: {}.", running.join(", ")));
    }
    let finished = tasks_with(&state, |status| matches!(status, TaskStatus::Completed(_)));
    if !finished.is_empty() {
        lines.push(format!("Already finished: {}.", finished.join(", ")));
    }
    let calls = state
        .task_handles
        .iter()
        .filter_map(|(_, status)| match status {
            TaskStatus::Pending(id) | TaskStatus::Completed(id) => Some(id.clone()),
            TaskStatus::Unknown | TaskStatus::Duplicate => None,
        })
        .collect::<Vec<_>>();

    tool_calls::interrupt("cancel".into(), calls.clone())
        .await
        .map_err(|error| tool_calls_error("failed to cancel tasks", &error))?;
    while state
        .pending_tool_calls
        .iter()
        .any(|call| calls.contains(&call.id))
    {
        state = read_state(Some(state.revision), &[]).await?;
    }
    Ok(lines.join("\n"))
}

/// Returns the tasks whose status matches `filter`, in order.
fn tasks_with(state: &ToolCallsState, filter: impl Fn(&TaskStatus) -> bool) -> Vec<&str> {
    state
        .task_handles
        .iter()
        .filter(|(_, status)| filter(status))
        .map(|(task, _)| task.as_str())
        .collect()
}

/// Returns an error naming any unknown and duplicate tasks.
fn check_tasks(state: &ToolCallsState) -> Result<(), Error> {
    let unknown = tasks_with(state, |status| matches!(status, TaskStatus::Unknown));
    let duplicate = tasks_with(state, |status| matches!(status, TaskStatus::Duplicate));
    let mut lines = Vec::new();
    if !unknown.is_empty() {
        lines.push(format!(
            "No call in this conversation uses these task names: {}.",
            unknown.join(", ")
        ));
    }
    if !duplicate.is_empty() {
        lines.push(format!(
            "More than one call in this conversation uses these task names: {}.",
            duplicate.join(", ")
        ));
    }
    if lines.is_empty() {
        Ok(())
    } else {
        Err(Error::Input(lines.join("\n")))
    }
}

fn names(tasks: BTreeSet<String>) -> Result<Vec<String>, Error> {
    if tasks.is_empty() {
        return Err(Error::Input("Name at least one task.".into()));
    }
    Ok(Vec::from_iter(tasks))
}

async fn read_state(
    after: Option<tool_calls::Revision>,
    tasks: &[String],
) -> Result<ToolCallsState, Error> {
    tool_calls::state(after, tasks.to_vec())
        .await
        .map_err(|error| tool_calls_error("failed to read tasks", &error))
}

fn parse<T: for<'de> Deserialize<'de>>(input: &str) -> Result<T, Error> {
    serde_json::from_str(input).map_err(|error| Error::Input(error.to_string()))
}

fn tool_calls_error(context: &str, error: &ToolCallsError) -> Error {
    match error.kind {
        ToolCallsErrorKind::ScopeClosed => Error::Interrupted,
        ToolCallsErrorKind::Conflict | ToolCallsErrorKind::Other => Error::Tools(ToolsError {
            kind: ToolsErrorKind::Other,
            message: format!("{context}: {}", error.message),
        }),
    }
}

fn interrupted_output(context: &ToolContext) -> ToolOutput {
    error_output(match context.tool.name.as_str() {
        "cancel" => "The cancellation was interrupted. Some tasks may already be stopping.",
        _ => "The wait was interrupted.",
    })
}

fn error_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(text.into())],
        structured_content: None,
        is_error: true,
    }
}

fn tasks_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "string",
        },
        "minItems": 1,
        "description": "The names of the tasks.",
    })
}

fn metadata(name: &str, title: &str, description: &str, properties: &Value) -> Tool {
    Tool {
        name: name.into(),
        description: Some(description.into()),
        is_async: false,
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": ["tasks"],
            "additionalProperties": false,
        })
        .to_string(),
        output_schema: None,
        extensions: json!({
            "fleet.title": title,
        })
        .to_string(),
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Tasks with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    use crate::bindings::fleet::host::tool_calls::MessageCounts;

    fn state(statuses: &[(&str, TaskStatus)], messages: MessageCounts) -> ToolCallsState {
        ToolCallsState {
            revision: tool_calls::Revision { high: 0, low: 1 },
            pending_tool_calls: vec![],
            task_handles: statuses
                .iter()
                .map(|(task, status)| ((*task).into(), status.clone()))
                .collect(),
            messages,
        }
    }

    fn output(state: &ToolCallsState, until: Until) -> Option<String> {
        match wait_output(state, until) {
            Ok(output) => output,
            Err(Error::Input(message)) => Some(format!("Error: {message}")),
            Err(Error::Interrupted) => panic!("interrupted"),
            Err(Error::Tools(error)) => panic!("{}", error.message),
        }
    }

    #[test]
    fn waits_end_when_tasks_finish_or_steering_arrives() {
        let partial = state(
            &[
                ("build", TaskStatus::Completed("1".into())),
                ("tests", TaskStatus::Pending("2".into())),
            ],
            MessageCounts {
                steering: 0,
                queued: 1,
            },
        );
        assert_eq!(output(&partial, Until::All), None);
        assert_eq!(
            output(&partial, Until::Any).unwrap(),
            "Finished: build.\nStill running: tests."
        );

        let interrupted = state(
            &[("tests", TaskStatus::Pending("2".into()))],
            MessageCounts {
                steering: 2,
                queued: 0,
            },
        );
        assert_eq!(
            output(&interrupted, Until::All).unwrap(),
            "Still running: tests.\n2 new messages arrived."
        );
    }

    #[test]
    fn waits_reject_unknown_and_duplicate_tasks() {
        let state = state(
            &[
                ("build", TaskStatus::Unknown),
                ("tests", TaskStatus::Duplicate),
                ("lint", TaskStatus::Completed("1".into())),
            ],
            MessageCounts {
                steering: 0,
                queued: 0,
            },
        );

        assert_eq!(
            output(&state, Until::All).unwrap(),
            "Error: No call in this conversation uses these task names: build.\nMore than one \
             call in this conversation uses these task names: tests."
        );
    }
}
