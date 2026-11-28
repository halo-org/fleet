use crate::{Config, Error, Tool, ToolContext, ToolOutput, WorkspaceBinding, types};
use serde::Deserialize;
use serde_json::json;

const MERGE_STDERR: &str = "exec 2>&1; exec \"$0\" \"$@\"";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
}

pub fn metadata() -> Tool {
    crate::metadata(
        "bash",
        "Bash",
        "Runs a Bash command in the workspace and returns its output. Each call starts a new shell.",
        &json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The Bash command to run.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory. Relative paths start in the workspace directory.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout in milliseconds, up to the configured limit.",
                },
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
    )
}

pub async fn execute(
    context: &ToolContext,
    config: &Config,
    workspace: WorkspaceBinding,
    is_retry: bool,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: Input = serde_json::from_str(input)?;
    if let Some(cwd) = &input.cwd {
        crate::files::validate_path(cwd)?;
    }
    let maximum = config.timeout_ms.unwrap_or(3_600_000);
    let timeout = input.timeout_ms.unwrap_or(maximum);
    if timeout == 0 || timeout > maximum {
        return Err(Error::Input(format!(
            "timeout_ms must be between 1 and {maximum}."
        )));
    }
    let shell = config.shell.as_deref().unwrap_or("bash");
    let command = types::WorkspaceCommand {
        id: format!(
            "{:016x}{:016x}:{}",
            context.agent_id.high, context.agent_id.low, context.call_id,
        ),
        program: shell.into(),
        args: vec![
            "-c".into(),
            MERGE_STDERR.into(),
            shell.into(),
            "-c".into(),
            input.command,
        ],
        env: Vec::new(),
        cwd: input.cwd,
        stdin: Vec::new(),
        timeout_ms: timeout,
        output_limit: config.output_limit.unwrap_or(65_536),
    };
    let result = crate::workspace::exec(workspace, is_retry, command).await?;
    Ok(render(&result))
}

fn render(result: &types::WorkspaceOutput) -> ToolOutput {
    let mut text = String::new();
    push_output(&mut text, &result.stdout);
    push_output(&mut text, &result.stderr);
    match result.exit {
        types::WorkspaceExit::Code(0) => {}
        types::WorkspaceExit::Code(code) => {
            push_line(&mut text, &format!("[exit status {code}]"));
        }
        types::WorkspaceExit::Terminated => push_line(&mut text, "[terminated]"),
        types::WorkspaceExit::TimedOut => push_line(&mut text, "[timed out]"),
    }
    if text.is_empty() {
        text.push_str("The command completed with no output.");
    }
    ToolOutput {
        is_error: !matches!(result.exit, types::WorkspaceExit::Code(0)),
        ..crate::success_output(text)
    }
}

fn push_output(text: &mut String, output: &types::CapturedOutput) {
    if output.omitted == 0 {
        text.push_str(&String::from_utf8_lossy(&output.head));
        return;
    }
    let partial_end = output.head.utf8_chunks().last().map_or(0, |chunk| {
        let invalid = chunk.invalid();
        if str::from_utf8(invalid).is_err_and(|error| error.error_len().is_none()) {
            invalid.len()
        } else {
            0
        }
    });
    let partial_start = output
        .tail
        .iter()
        .take(3)
        .take_while(|byte| matches!(byte, 0x80..=0xBF))
        .count();
    let (head, _) = output
        .head
        .split_at(output.head.len().saturating_sub(partial_end));
    let (_, tail) = output.tail.split_at(partial_start);
    let omitted = output.omitted.saturating_add(
        u64::try_from(partial_end.saturating_add(partial_start)).unwrap_or(u64::MAX),
    );
    text.push_str(&String::from_utf8_lossy(head));
    push_line(text, &format!("[Omitted {omitted} bytes of tool output]"));
    text.push_str(&String::from_utf8_lossy(tail));
}

fn push_line(text: &mut String, line: &str) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn render_marks_omitted_output_and_reports_failure() {
        let result = types::WorkspaceOutput {
            exit: types::WorkspaceExit::Code(1),
            stdout: types::CapturedOutput {
                head: "start é".as_bytes()[..7].to_vec(),
                omitted: 1024,
                tail: "é end\n".as_bytes()[1..].to_vec(),
            },
            stderr: types::CapturedOutput {
                head: b"err".to_vec(),
                omitted: 0,
                tail: Vec::new(),
            },
        };

        let output = render(&result);
        let ContentBlock::Text(text) = output.content.first().unwrap() else {
            panic!("expected text output");
        };

        assert!(output.is_error);
        assert_eq!(
            text,
            "start \n[Omitted 1026 bytes of tool output]\n end\nerr\n[exit status 1]\n"
        );
    }
}
