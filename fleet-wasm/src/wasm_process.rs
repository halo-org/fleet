#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::wasm_process_group::ProcessGroup;
use crate::{WasmState, bindings::process};
use bytes::BytesMut;
use core::{
    cfg_select,
    pin::Pin,
    task::{Context, Poll},
};
use futures_channel::oneshot;
use futures_util::{
    FutureExt as _,
    future::{BoxFuture, Shared, poll_fn},
};
use std::{
    io,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncWrite as _, ReadBuf},
    process::{Child, ChildStdin, Command},
};
use wasmtime::{
    StoreContextMut,
    component::{
        Access, Accessor, Destination, FutureReader, HasSelf, Resource, Source, StreamConsumer,
        StreamProducer, StreamReader, StreamResult,
    },
};

// === Processes ===

pub struct WasmProcess {
    child: Arc<Mutex<Process>>,
    exit: Shared<BoxFuture<'static, Result<process::ExitStatus, String>>>,
}

enum Process {
    Child(Child),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Group(ProcessGroup),
}

impl Process {
    const fn child(&mut self) -> &mut Child {
        match self {
            Self::Child(child) => child,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Group(group) => group.child(),
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Child(child) => child.start_kill(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Group(group) => group.kill(),
        }
    }

    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        match self {
            Self::Child(child) => core::pin::pin!(child.wait()).poll(cx),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Group(group) => group.poll_wait(cx),
        }
    }
}

impl WasmProcess {
    fn spawn(command: process::Command) -> io::Result<Self> {
        Ok(Self::new(Process::Child(Self::command(command).spawn()?)))
    }

    fn spawn_group(command: process::Command) -> io::Result<Self> {
        cfg_select! {
            any(target_os = "linux", target_os = "macos") => {
                let group = ProcessGroup::spawn(Self::command(command))?;
                Ok(Self::new(Process::Group(group)))
            }
            _ => {
                drop(command);
                Err(io::Error::new(io::ErrorKind::Unsupported, "process groups are not supported on this platform"))
            }
        }
    }

    fn command(command: process::Command) -> Command {
        let mut builder = Command::new(command.program);
        builder
            .args(command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = command.cwd {
            builder.current_dir(cwd);
        }
        if !command.inherit_env {
            builder.env_clear();
        }
        for (name, value) in command.env {
            match value {
                Some(value) => builder.env(name, value),
                None => builder.env_remove(name),
            };
        }

        builder
    }

    fn new(child: Process) -> Self {
        let child = Arc::new(Mutex::new(child));
        let waiting_child = child.clone();
        let exit = poll_fn(move |cx| {
            waiting_child
                .lock()
                .expect("process lock is not poisoned")
                .poll_wait(cx)
        })
        .map(|result| {
            result
                .map(|status| {
                    status
                        .code()
                        .map_or(process::ExitStatus::Terminated, process::ExitStatus::Code)
                })
                .map_err(|error| error.to_string())
        })
        .boxed()
        .shared();

        Self { child, exit }
    }

    fn kill(&self) -> io::Result<()> {
        self.child
            .lock()
            .expect("process lock is not poisoned")
            .kill()
    }
}

impl Drop for WasmProcess {
    fn drop(&mut self) {
        drop(self.kill());
    }
}

// === Host ===

impl process::Host for WasmState {}

impl process::HostChild for WasmState {}

impl process::HostWithStore<WasmState> for HasSelf<WasmState> {
    fn spawn(
        mut store: Access<WasmState, Self>,
        command: process::Command,
    ) -> wasmtime::Result<Result<Resource<WasmProcess>, String>> {
        match WasmProcess::spawn(command) {
            Ok(process) => Ok(Ok(store.get().table().push(process)?)),
            Err(error) => Ok(Err(error.to_string())),
        }
    }

    fn spawn_group(
        mut store: Access<WasmState, Self>,
        command: process::Command,
    ) -> wasmtime::Result<Result<Resource<WasmProcess>, String>> {
        match WasmProcess::spawn_group(command) {
            Ok(process) => Ok(Ok(store.get().table().push(process)?)),
            Err(error) => Ok(Err(error.to_string())),
        }
    }
}

impl process::HostChildWithStore<WasmState> for HasSelf<WasmState> {
    fn write_stdin(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmProcess>,
        mut data: StreamReader<u8>,
    ) -> wasmtime::Result<Result<FutureReader<Result<(), String>>, String>> {
        let stdin = store
            .get()
            .table()
            .get(&resource)?
            .child
            .lock()
            .expect("process lock is not poisoned")
            .child()
            .stdin
            .take();
        let Some(stdin) = stdin else {
            data.close(&mut store)?;
            return Ok(Err("standard input has already been taken or closed".into()));
        };
        let (error, result) = oneshot::channel();
        data.pipe(
            &mut store,
            ProcessInput {
                stdin,
                error: Some(error),
            },
        )?;
        let result = FutureReader::new(&mut store, async {
            wasmtime::error::Ok(result.await.map_or(Ok(()), Err))
        })?;
        Ok(Ok(result))
    }

    fn stdout(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmProcess>,
    ) -> wasmtime::Result<Result<process::Output, String>> {
        let stdout = store
            .get()
            .table()
            .get(&resource)?
            .child
            .lock()
            .expect("process lock is not poisoned")
            .child()
            .stdout
            .take();
        let Some(stdout) = stdout else {
            return Ok(Err("standard output has already been taken".into()));
        };
        output(store, stdout).map(Ok)
    }

    fn stderr(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmProcess>,
    ) -> wasmtime::Result<Result<process::Output, String>> {
        let stderr = store
            .get()
            .table()
            .get(&resource)?
            .child
            .lock()
            .expect("process lock is not poisoned")
            .child()
            .stderr
            .take();
        let Some(stderr) = stderr else {
            return Ok(Err("standard error has already been taken".into()));
        };
        output(store, stderr).map(Ok)
    }

    async fn wait(
        store: &Accessor<WasmState, Self>,
        resource: Resource<WasmProcess>,
    ) -> wasmtime::Result<Result<process::ExitStatus, String>> {
        let exit = store.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table().get(&resource)?.exit.clone())
        })?;
        Ok(exit.await)
    }

    fn kill(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmProcess>,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(store
            .get()
            .table()
            .get(&resource)?
            .kill()
            .map_err(|error| error.to_string()))
    }

    fn drop(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmProcess>,
    ) -> wasmtime::Result<()> {
        store.get().table().delete(resource)?;
        Ok(())
    }
}

// === Streams ===

struct ProcessInput {
    stdin: ChildStdin,
    error: Option<oneshot::Sender<String>>,
}

struct ProcessOutput<R> {
    reader: R,
    error: Option<oneshot::Sender<String>>,
}

fn output(
    mut store: Access<WasmState, HasSelf<WasmState>>,
    reader: impl AsyncRead + Unpin + Send + 'static,
) -> wasmtime::Result<process::Output> {
    let (error, result) = oneshot::channel();
    let data = StreamReader::new(
        &mut store,
        ProcessOutput {
            reader,
            error: Some(error),
        },
    )?;
    let result = FutureReader::new(&mut store, async {
        wasmtime::error::Ok(result.await.map_or(Ok(()), Err))
    })?;
    Ok(process::Output { data, result })
}

impl StreamConsumer<WasmState> for ProcessInput {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<WasmState>,
        source: Source<Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let this = self.get_mut();
        let mut source = source.as_direct(store);
        let bytes = source.remaining();
        if bytes.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let error = match Pin::new(&mut this.stdin).poll_write(cx, bytes) {
            Poll::Ready(Ok(0)) => io::Error::from(io::ErrorKind::WriteZero),
            Poll::Ready(Ok(count)) => {
                source.mark_read(count);
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            Poll::Ready(Err(error)) => error,
            Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => return Poll::Pending,
        };
        if let Some(sender) = this.error.take() {
            drop(sender.send(error.to_string()));
        }
        Poll::Ready(Ok(StreamResult::Dropped))
    }
}

impl<R> StreamProducer<WasmState> for ProcessOutput<R>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    type Item = u8;
    type Buffer = BytesMut;

    fn poll_produce<'a>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, WasmState>,
        destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if destination.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let this = self.get_mut();
        let mut destination = destination.as_direct(store, 16 * 1024);
        let mut buffer = ReadBuf::new(destination.remaining());
        match Pin::new(&mut this.reader).poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Ok(())) => {
                let count = buffer.filled().len();
                destination.mark_written(count);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Err(error)) => {
                if let Some(sender) = this.error.take() {
                    drop(sender.send(error.to_string()));
                }
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use core::time::Duration;
    use futures_util::future::{join, join4};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn command(script: &str) -> process::Command {
        process::Command {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            inherit_env: true,
            env: Vec::new(),
        }
    }

    fn run(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime is available")
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), future)
                    .await
                    .expect("process completes");
            });
    }

    #[test]
    fn spawn_streams_input_and_output() {
        run(async {
            let process = WasmProcess::spawn(command("cat; printf error >&2; exit 7")).unwrap();
            let (mut stdin, mut stdout, mut stderr) = {
                let mut process = process.child.lock().unwrap();
                let child = process.child();
                (
                    child.stdin.take().unwrap(),
                    child.stdout.take().unwrap(),
                    child.stderr.take().unwrap(),
                )
            };
            let input = vec![42; 128 * 1024];
            let mut output = Vec::new();
            let mut error = Vec::new();

            let (written, read_output, read_error, (exit_a, exit_b)) = join4(
                async {
                    let result = stdin.write_all(&input).await;
                    drop(stdin);
                    result
                },
                stdout.read_to_end(&mut output),
                stderr.read_to_end(&mut error),
                join(process.exit.clone(), process.exit.clone()),
            )
            .await;
            written.unwrap();
            read_output.unwrap();
            read_error.unwrap();
            let exit_a = exit_a.unwrap();
            let exit_b = exit_b.unwrap();

            assert_eq!(output, input);
            assert_eq!(error, b"error");
            assert!(matches!(exit_a, process::ExitStatus::Code(7)));
            assert!(matches!(exit_b, process::ExitStatus::Code(7)));
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn dropping_a_process_group_terminates_descendants() {
        run(async {
            let process =
                WasmProcess::spawn_group(command("sleep 60 & printf ready; wait")).unwrap();
            let mut stdout = process.child.lock().unwrap().child().stdout.take().unwrap();
            let exit = process.exit.clone();
            let mut ready = [0; 5];
            stdout.read_exact(&mut ready).await.unwrap();

            drop(process);
            let exit = exit.await.unwrap();
            let mut remaining = Vec::new();
            stdout.read_to_end(&mut remaining).await.unwrap();

            assert!(matches!(exit, process::ExitStatus::Terminated));
            assert_eq!(remaining, b"");
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn waiting_for_a_process_group_cleans_up_after_its_leader_exits() {
        run(async {
            let process =
                WasmProcess::spawn_group(command("sleep 60 & printf ready; exit 7")).unwrap();
            let mut stdout = process.child.lock().unwrap().child().stdout.take().unwrap();
            let mut output = Vec::new();

            let (exit, read) = join(process.exit.clone(), stdout.read_to_end(&mut output)).await;
            let exit = exit.unwrap();
            read.unwrap();
            process.kill().unwrap();
            drop(process);

            assert!(matches!(exit, process::ExitStatus::Code(7)));
            assert_eq!(output, b"ready");
        });
    }

    #[test]
    fn drop_terminates_child() {
        run(async {
            let process = WasmProcess::spawn(command("read value")).unwrap();
            let stdin = process.child.lock().unwrap().child().stdin.take().unwrap();
            let mut exit = core::pin::pin!(process.exit.clone());

            assert!(exit.as_mut().now_or_never().is_none());

            drop(process);
            let exit = exit.await.unwrap();

            assert!(matches!(exit, process::ExitStatus::Terminated));
            drop(stdin);
        });
    }
}
