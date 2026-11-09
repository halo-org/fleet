use crate::{
    WorkspaceCommand, WorkspaceExit, WorkspaceOutput, WorkspacesError, WorkspacesErrorKind,
    bindings,
};
use bindings::{fleet::providers::process, wasi::clocks::monotonic_clock};
use futures_util::future::{self, Either};
use wit_bindgen::{StreamReader, StreamResult};

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Capture {
    async fn read(&mut self, mut stream: StreamReader<u8>, limit: usize) {
        loop {
            let (status, bytes) = stream.read(Vec::with_capacity(16 * 1024)).await;
            let keep = bytes.len().min(limit.saturating_sub(self.bytes.len()));
            self.truncated |= keep < bytes.len();
            self.bytes.extend(bytes.into_iter().take(keep));
            if matches!(status, StreamResult::Dropped) {
                return;
            }
        }
    }
}

pub async fn run(
    cwd: String,
    command: WorkspaceCommand,
) -> Result<WorkspaceOutput, WorkspacesError> {
    let child = process::spawn(&process::Command {
        program: command.program,
        args: command.args,
        cwd: Some(cwd),
        inherit_env: true,
        env: command
            .env
            .into_iter()
            .map(|(name, value)| (name, Some(value)))
            .collect(),
    })
    .map_err(error)?;
    let output = child.stdout().map_err(error)?;
    let errors = child.stderr().map_err(error)?;
    let (mut writer, reader) = bindings::wit_stream::new();
    let written = child.write_stdin(reader).map_err(error)?;
    let mut stdout = Capture::default();
    let mut stderr = Capture::default();
    let limit = usize::try_from(command.output_limit).unwrap_or(usize::MAX);
    let exit = {
        let run = future::try_join4(
            async move {
                let (remaining, result) = future::join(
                    async move {
                        let remaining = writer.write_all(command.stdin).await;
                        drop(writer);
                        remaining
                    },
                    async { written.await },
                )
                .await;
                result?;
                if !remaining.is_empty() {
                    return Err("standard input closed early".into());
                }
                Ok(())
            },
            async {
                stdout.read(output.data, limit).await;
                output.result.await
            },
            async {
                stderr.read(errors.data, limit).await;
                errors.result.await
            },
            child.wait(),
        );
        let timeout = monotonic_clock::wait_for(command.timeout_ms.saturating_mul(1_000_000));
        match future::select(core::pin::pin!(run), core::pin::pin!(timeout)).await {
            Either::Left((result, _)) => {
                let ((), (), (), exit) = result.map_err(error)?;
                match exit {
                    process::ExitStatus::Code(code) => WorkspaceExit::Code(code),
                    process::ExitStatus::Terminated => WorkspaceExit::Terminated,
                }
            }
            Either::Right(_) => {
                child.kill().map_err(error)?;
                WorkspaceExit::TimedOut
            }
        }
    };
    Ok(WorkspaceOutput {
        exit,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn error(message: String) -> WorkspacesError {
    crate::error(WorkspacesErrorKind::Other, message)
}
