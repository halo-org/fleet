use crate::{WasmState, bindings::workers};
use bytes::BytesMut;
use core::{
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};
use futures_channel::oneshot;
use futures_util::{
    FutureExt as _,
    future::{BoxFuture, Shared},
};
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead as _, AsyncWrite as _, DuplexStream, ReadBuf},
    sync::OwnedSemaphorePermit,
};
use wasmtime::{
    StoreContextMut,
    component::{
        Access, Accessor, Destination, FutureReader, HasSelf, Resource, Source, StreamConsumer,
        StreamProducer, StreamReader, StreamResult,
    },
};

const BUFFER_SIZE: usize = 64 * 1024;
type Completion = Shared<BoxFuture<'static, Result<(), String>>>;

pub struct WasmWorkerConnection {
    input: Option<Pipe>,
    output: Option<Pipe>,
    state: Arc<ConnectionState>,
}

pub struct ConnectionState {
    sender: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    closed: Completion,
    _permit: OwnedSemaphorePermit,
}

struct Pipe {
    io: DuplexStream,
    state: Arc<ConnectionState>,
    closed: Completion,
    finished: Arc<AtomicBool>,
}

struct Input {
    pipe: Pipe,
    result: Option<oneshot::Sender<Result<(), String>>>,
}

struct Output {
    pipe: Pipe,
    result: Option<oneshot::Sender<Result<(), String>>>,
}

impl WasmWorkerConnection {
    pub(crate) fn pair(permit: OwnedSemaphorePermit) -> (Self, Self) {
        let (sender, closed) = oneshot::channel();
        let state = Arc::new(ConnectionState {
            sender: Mutex::new(Some(sender)),
            closed: async {
                closed
                    .await
                    .unwrap_or_else(|_| Err("worker connection was lost".into()))
            }
            .boxed()
            .shared(),
            _permit: permit,
        });
        let (client_output, server_input) = tokio::io::duplex(BUFFER_SIZE);
        let (server_output, client_input) = tokio::io::duplex(BUFFER_SIZE);
        let client_finished = Arc::new(AtomicBool::new(false));
        let server_finished = Arc::new(AtomicBool::new(false));
        let endpoint = |input, output, incoming, outgoing| Self {
            input: Some(Pipe::new(input, state.clone(), incoming)),
            output: Some(Pipe::new(output, state.clone(), outgoing)),
            state: state.clone(),
        };
        (
            endpoint(
                client_input,
                client_output,
                server_finished.clone(),
                client_finished.clone(),
            ),
            endpoint(
                server_input,
                server_output,
                client_finished,
                server_finished,
            ),
        )
    }

    pub const fn state(&self) -> &Arc<ConnectionState> {
        &self.state
    }
}

impl Drop for WasmWorkerConnection {
    fn drop(&mut self) {
        self.state.close(Ok(()));
    }
}

impl ConnectionState {
    pub fn closed(&self) -> Completion {
        self.closed.clone()
    }

    pub(crate) fn close(&self, result: Result<(), String>) {
        let sender = self
            .sender
            .lock()
            .expect("connection lock is not poisoned")
            .take();
        if let Some(sender) = sender {
            drop(sender.send(result));
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.sender
            .lock()
            .expect("connection lock is not poisoned")
            .is_some()
    }
}

impl Pipe {
    fn new(io: DuplexStream, state: Arc<ConnectionState>, finished: Arc<AtomicBool>) -> Self {
        Self {
            io,
            closed: state.closed.clone(),
            state,
            finished,
        }
    }

    fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        match self.closed.poll_unpin(cx) {
            Poll::Pending => Ok(()),
            Poll::Ready(result) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                result
                    .err()
                    .unwrap_or_else(|| "worker connection closed".into()),
            )),
        }
    }
}

impl workers::HostConnection for WasmState {}

impl workers::HostConnectionWithStore<WasmState> for HasSelf<WasmState> {
    fn write(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmWorkerConnection>,
        mut data: StreamReader<u8>,
    ) -> wasmtime::Result<Result<FutureReader<Result<(), String>>, String>> {
        let Some(pipe) = store.get().table().get_mut(&resource)?.output.take() else {
            data.close(&mut store)?;
            return Ok(Err("worker connection output has already been taken".into()));
        };
        let (sender, result) = oneshot::channel();
        data.pipe(
            &mut store,
            Input {
                pipe,
                result: Some(sender),
            },
        )?;
        Ok(Ok(FutureReader::new(&mut store, async {
            wasmtime::error::Ok(
                result
                    .await
                    .unwrap_or_else(|_| Err("worker connection write was interrupted".into())),
            )
        })?))
    }

    fn read(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmWorkerConnection>,
    ) -> wasmtime::Result<Result<workers::Output, String>> {
        let Some(pipe) = store.get().table().get_mut(&resource)?.input.take() else {
            return Ok(Err("worker connection input has already been taken".into()));
        };
        let (sender, result) = oneshot::channel();
        let data = StreamReader::new(
            &mut store,
            Output {
                pipe,
                result: Some(sender),
            },
        )?;
        let result = FutureReader::new(&mut store, async {
            wasmtime::error::Ok(
                result
                    .await
                    .unwrap_or_else(|_| Err("worker connection read was interrupted".into())),
            )
        })?;
        Ok(Ok(workers::Output { data, result }))
    }

    async fn closed(
        store: &Accessor<WasmState, Self>,
        resource: Resource<WasmWorkerConnection>,
    ) -> wasmtime::Result<Result<(), String>> {
        let closed = store.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table().get(&resource)?.state.closed())
        })?;
        Ok(closed.await)
    }

    fn drop(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmWorkerConnection>,
    ) -> wasmtime::Result<()> {
        store.get().table().delete(resource)?;
        Ok(())
    }
}

impl StreamConsumer<WasmState> for Input {
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
        let error = match this.pipe.check(cx) {
            Err(error) => error,
            Ok(()) if bytes.is_empty() => return Poll::Ready(Ok(StreamResult::Completed)),
            Ok(()) => match Pin::new(&mut this.pipe.io).poll_write(cx, bytes) {
                Poll::Ready(Ok(0)) => io::Error::from(io::ErrorKind::WriteZero),
                Poll::Ready(Ok(count)) => {
                    source.mark_read(count);
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
                Poll::Ready(Err(error)) => error,
                Poll::Pending if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Poll::Pending => return Poll::Pending,
            },
        };
        if let Some(sender) = this.result.take() {
            drop(sender.send(Err(error.to_string())));
        }
        Poll::Ready(Ok(StreamResult::Dropped))
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(sender) = self.result.take() {
            let result = if self.pipe.state.is_open() {
                self.pipe.finished.store(true, Ordering::Release);
                Ok(())
            } else {
                Err("worker connection closed".into())
            };
            drop(sender.send(result));
        }
    }
}

impl StreamProducer<WasmState> for Output {
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
        let result = match Pin::new(&mut this.pipe.io).poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) if !buffer.filled().is_empty() => {
                let count = buffer.filled().len();
                destination.mark_written(count);
                return Poll::Ready(Ok(StreamResult::Completed));
            }
            Poll::Ready(Ok(())) if this.pipe.finished.load(Ordering::Acquire) => Ok(()),
            Poll::Ready(Ok(())) => this.pipe.check(cx).and_then(|()| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "worker connection input was interrupted",
                ))
            }),
            Poll::Ready(Err(error)) => Err(error),
            Poll::Pending => match this.pipe.check(cx) {
                Err(error) => Err(error),
                Ok(()) if finish => return Poll::Ready(Ok(StreamResult::Cancelled)),
                Ok(()) => return Poll::Pending,
            },
        };
        if let Some(sender) = this.result.take() {
            drop(sender.send(result.map_err(|error| error.to_string())));
        }
        Poll::Ready(Ok(StreamResult::Dropped))
    }
}
