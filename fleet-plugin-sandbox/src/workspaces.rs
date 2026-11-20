use crate::{
    Sandbox, WorkspacesError, WorkspacesErrorKind,
    bindings::exports::fleet::providers::workspaces::{
        CapturedOutput, Guest, Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit,
        WorkspaceOutput, WorkspaceWrite,
    },
    files::{self, Reply, Request},
};

const NAME: &str = "default";
const ROOT: &str = "/workspace";

impl Guest for Sandbox {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn get(name: String) -> Result<Option<Workspace>, WorkspacesError> {
        Ok((name == NAME).then(|| Workspace {
            name,
            extensions: serde_json::json!({
                "fleet.title": "Sandbox",
                "fleet.description": "Keeps files in memory and does not run commands.",
            })
            .to_string(),
        }))
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn open(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        workspace(&context).map(drop)
    }

    async fn release(context: WorkspaceContext) -> Result<(), WorkspacesError> {
        let request = Request::Release {
            workspace: workspace(&context)?,
        };
        match files::send(&request).await? {
            Reply::Released => Ok(()),
            reply => Err(unexpected(&reply)),
        }
    }

    async fn read(
        context: WorkspaceContext,
        path: String,
        limit: u64,
    ) -> Result<Vec<u8>, WorkspacesError> {
        let workspace = workspace(&context)?;
        let path = resolve(&path);
        let request = Request::Read {
            workspace,
            path: path.clone(),
            limit,
        };
        match files::send(&request).await? {
            Reply::Contents(contents) => Ok(contents),
            Reply::NotFound => Err(error(
                WorkspacesErrorKind::FileNotFound,
                format!("{path} does not exist"),
            )),
            Reply::TooLarge => Err(error(
                WorkspacesErrorKind::FileTooLarge,
                format!("{path} exceeds the read limit"),
            )),
            reply => Err(unexpected(&reply)),
        }
    }

    async fn write(context: WorkspaceContext, file: WorkspaceWrite) -> Result<(), WorkspacesError> {
        let request = Request::Write {
            workspace: workspace(&context)?,
            path: resolve(&file.path),
            content: file.content,
            expected: file.expected,
        };
        match files::send(&request).await? {
            Reply::Written => Ok(()),
            Reply::Changed => Err(error(
                WorkspacesErrorKind::FileChanged,
                "file does not match expected contents",
            )),
            reply => Err(unexpected(&reply)),
        }
    }

    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn exec(
        context: WorkspaceContext,
        _is_retry: bool,
        command: WorkspaceCommand,
    ) -> Result<WorkspaceOutput, WorkspacesError> {
        workspace(&context)?;
        Ok(simulate(&command))
    }
}

fn workspace(context: &WorkspaceContext) -> Result<String, WorkspacesError> {
    if context.workspace.name != NAME {
        return Err(error(
            WorkspacesErrorKind::NotFound,
            "workspace was not found",
        ));
    }
    Ok(format!("{:016x}{:016x}", context.id.high, context.id.low))
}

/// Returns the absolute path, resolving relative paths in the workspace directory.
fn resolve(path: &str) -> String {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{ROOT}/{path}")
    };
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    format!("/{}", segments.join("/"))
}

/// Reports a command without running it. `exit N` exits with that code.
fn simulate(command: &WorkspaceCommand) -> WorkspaceOutput {
    let script = command.args.last().unwrap_or(&command.program);
    let (exit, stdout) = script
        .strip_prefix("exit ")
        .and_then(|code| code.trim().parse().ok())
        .map_or_else(
            || {
                (
                    WorkspaceExit::Code(0),
                    format!("Simulated: {script}\n").into_bytes(),
                )
            },
            |code| (WorkspaceExit::Code(code), Vec::new()),
        );
    WorkspaceOutput {
        exit,
        stdout: captured(stdout, command.output_limit),
        stderr: captured(Vec::new(), command.output_limit),
    }
}

/// Keeps the first and last halves of the limit from longer output.
fn captured(mut head: Vec<u8>, limit: u64) -> CapturedOutput {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if head.len() <= limit {
        return CapturedOutput {
            head,
            omitted: 0,
            tail: Vec::new(),
        };
    }
    let tail = head.split_off(head.len().saturating_sub(limit.div_euclid(2)));
    let omitted = head.len().saturating_sub(limit.div_ceil(2));
    head.truncate(limit.div_ceil(2));
    CapturedOutput {
        head,
        omitted: u64::try_from(omitted).unwrap_or(u64::MAX),
        tail,
    }
}

fn error(kind: WorkspacesErrorKind, message: impl Into<String>) -> WorkspacesError {
    WorkspacesError {
        kind,
        message: message.into(),
    }
}

fn unexpected(reply: &Reply) -> WorkspacesError {
    error(
        WorkspacesErrorKind::Other,
        format!("unexpected files worker reply: {reply:?}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(script: &str) -> WorkspaceCommand {
        WorkspaceCommand {
            id: "1".into(),
            program: "bash".into(),
            args: vec!["-c".into(), script.into()],
            env: Vec::new(),
            cwd: None,
            stdin: Vec::new(),
            timeout_ms: 1000,
            output_limit: 1000,
        }
    }

    #[test]
    fn paths_resolve_in_the_workspace_directory() {
        assert_eq!(resolve("notes.txt"), "/workspace/notes.txt");
        assert_eq!(resolve("./src/../notes.txt"), "/workspace/notes.txt");
        assert_eq!(resolve("/etc//hosts"), "/etc/hosts");
        assert_eq!(resolve("../../.."), "/");
    }

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let mut long = command("abcdefghijklmnop");
        long.output_limit = 8;

        let output = simulate(&long).stdout;

        assert_eq!(output.head, b"Simu");
        assert_eq!(output.omitted, 20);
        assert_eq!(output.tail, b"nop\n");
    }

    #[test]
    fn commands_are_reported_without_running() {
        let ran = simulate(&command("ls -la"));
        let failed = simulate(&command("exit 3"));

        assert!(matches!(ran.exit, WorkspaceExit::Code(0)));
        assert_eq!(ran.stdout.head, b"Simulated: ls -la\n");
        assert!(matches!(failed.exit, WorkspaceExit::Code(3)));
        assert_eq!(failed.stdout.head, b"");
    }
}
