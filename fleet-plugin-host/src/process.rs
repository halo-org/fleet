use crate::{
    CapturedOutput, WorkspaceCommand, WorkspaceExit, WorkspaceOutput, WorkspacesError,
    WorkspacesErrorKind, bindings,
};
use bindings::{fleet::providers::process, wasi::clocks::monotonic_clock};
use futures_util::future::{self, Either};
use std::collections::VecDeque;
use wit_bindgen::{StreamReader, StreamResult};

#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    omitted: u64,
    tail: VecDeque<u8>,
}

impl Capture {
    async fn read(&mut self, mut stream: StreamReader<u8>, limit: usize) {
        loop {
            let (status, bytes) = stream.read(Vec::with_capacity(16 * 1024)).await;
            self.push(&bytes, limit);
            if matches!(status, StreamResult::Dropped) {
                return;
            }
        }
    }

    fn push(&mut self, bytes: &[u8], limit: usize) {
        let head = limit
            .div_ceil(2)
            .saturating_sub(self.head.len())
            .min(bytes.len());
        let (head, tail) = bytes.split_at(head);
        self.head.extend_from_slice(head);
        self.tail.extend(tail);
        let excess = self.tail.len().saturating_sub(limit.div_euclid(2));
        self.tail.drain(..excess);
        self.omitted = self
            .omitted
            .saturating_add(u64::try_from(excess).unwrap_or(u64::MAX));
    }

    fn finish(self) -> CapturedOutput {
        let mut head = self.head;
        let mut tail = Vec::from(self.tail);
        if self.omitted == 0 {
            head.append(&mut tail);
        }
        CapturedOutput {
            head,
            omitted: self.omitted,
            tail,
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
        stdout: stdout.finish(),
        stderr: stderr.finish(),
    })
}

fn error(message: String) -> WorkspacesError {
    crate::error(WorkspacesErrorKind::Other, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_head_and_tail() {
        let mut capture = Capture::default();
        for chunk in [b"abc".as_slice(), b"defg", b"hij"] {
            capture.push(chunk, 5);
        }

        let output = capture.finish();

        assert_eq!(output.head, b"abc");
        assert_eq!(output.omitted, 5);
        assert_eq!(output.tail, b"ij");
    }

    #[test]
    fn capture_keeps_short_output_in_head() {
        let mut capture = Capture::default();
        capture.push(b"abcd", 5);

        let output = capture.finish();

        assert_eq!(output.head, b"abcd");
        assert_eq!(output.omitted, 0);
        assert_eq!(output.tail, b"");
    }
}
