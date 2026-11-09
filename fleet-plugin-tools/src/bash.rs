use crate::{Config, Error, Tool, ToolContext, ToolOutput, WorkspaceContext, workspaces};
use serde::Deserialize;
use serde_json::json;

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
        "Runs a Bash command in the workspace and returns its output. Each call starts a new shell. Paths are relative to the workspace.",
        &json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The Bash command to run.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory relative to the workspace.",
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
    workspace: WorkspaceContext,
    is_retry: bool,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: Input = serde_json::from_str(input)?;
    if let Some(cwd) = &input.cwd {
        crate::files::validate_path(cwd)?;
    }
    let maximum = config.timeout_ms.unwrap_or(120_000);
    let timeout = input.timeout_ms.unwrap_or(maximum);
    if timeout == 0 || timeout > maximum {
        return Err(Error::Input(format!(
            "timeout_ms must be between 1 and {maximum}."
        )));
    }
    let command = workspaces::WorkspaceCommand {
        id: format!(
            "{:016x}{:016x}:{}",
            context.agent_id.high, context.agent_id.low, context.call_id,
        ),
        program: config.shell.as_deref().unwrap_or("bash").into(),
        args: vec!["-c".into(), input.command],
        env: Vec::new(),
        cwd: input.cwd,
        stdin: Vec::new(),
        timeout_ms: timeout,
        output_limit: config.output_limit.unwrap_or(65_536),
    };
    let result = workspaces::exec(workspace, is_retry, command).await?;
    Ok(render(&result))
}

fn render(result: &workspaces::WorkspaceOutput) -> ToolOutput {
    let mut text = String::from_utf8_lossy(&result.stdout).into_owned();
    if result.stdout_truncated {
        push_line(&mut text, "[stdout truncated]");
    }
    if !result.stderr.is_empty() || result.stderr_truncated {
        push_line(&mut text, "[stderr]");
        text.push_str(&String::from_utf8_lossy(&result.stderr));
    }
    if result.stderr_truncated {
        push_line(&mut text, "[stderr truncated]");
    }
    match result.exit {
        workspaces::WorkspaceExit::Code(0) => {}
        workspaces::WorkspaceExit::Code(code) => {
            push_line(&mut text, &format!("[exit status {code}]"));
        }
        workspaces::WorkspaceExit::Terminated => push_line(&mut text, "[terminated]"),
        workspaces::WorkspaceExit::TimedOut => push_line(&mut text, "[timed out]"),
    }
    if text.is_empty() {
        text.push_str("The command completed with no output.");
    }
    ToolOutput {
        is_error: !matches!(result.exit, workspaces::WorkspaceExit::Code(0)),
        ..crate::success_output(text)
    }
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
    fn render_preserves_output_and_reports_failure() {
        let result = workspaces::WorkspaceOutput {
            exit: workspaces::WorkspaceExit::Code(1),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            stdout_truncated: true,
            stderr_truncated: false,
        };

        let output = render(&result);
        let ContentBlock::Text(text) = output.content.first().unwrap();

        assert!(output.is_error);
        assert_eq!(
            text,
            "out\n[stdout truncated]\n[stderr]\nerr\n[exit status 1]\n"
        );
    }
}
