use crate::{
    WasmEnvironment, WasmError, WasmState,
    bindings::{worker::WorkerProviderPre, workers},
    wasm_worker_connection::{ConnectionState, WasmWorkerConnection},
};
use core::time::Duration;
use futures_channel::oneshot;
use futures_util::{
    FutureExt as _,
    future::{self, BoxFuture, Either, Shared},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use wasmtime::{
    StoreLimitsBuilder,
    component::{Access, Accessor, HasSelf, InstancePre, Resource},
};

#[cfg(test)]
mod tests;

const MAX_WORKERS: usize = 64;
const MAX_CONNECTIONS: usize = 64;
const MAX_INITIALIZATION: usize = 64 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
type Completion = Shared<BoxFuture<'static, ()>>;

#[derive(Clone)]
pub struct WasmWorkers(Arc<Workers>);

struct Workers {
    factory: Factory,
    registry: Mutex<Registry>,
    connections: Arc<Semaphore>,
}

#[derive(Clone)]
struct Factory {
    instance: WorkerProviderPre<WasmState>,
    environment: WasmEnvironment,
}

#[derive(Default)]
struct Registry {
    entries: BTreeMap<String, Entry>,
    shutting_down: bool,
}

struct Entry {
    initialization: Vec<u8>,
    listener: Arc<Listener>,
    _task: Task,
}

struct Task(JoinHandle<()>);

struct Running {
    workers: Weak<Workers>,
    key: String,
    listener: Arc<Listener>,
    result: Result<(), String>,
}

pub struct WasmWorkerListener(Arc<Listener>);

struct Listener {
    admission: Mutex<Admission>,
    available: Notify,
    closed: Completion,
    finished: Completion,
    done: Mutex<Option<oneshot::Sender<()>>>,
}

struct Admission {
    pending: VecDeque<WasmWorkerConnection>,
    connections: Vec<Weak<ConnectionState>>,
    close: Option<oneshot::Sender<()>>,
}

enum Connect {
    Connected(WasmWorkerConnection),
    Retiring(Completion),
}

impl WasmWorkers {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
    ) -> Result<Self, WasmError> {
        Ok(Self(Arc::new(Workers {
            factory: Factory {
                instance: WorkerProviderPre::new(instance).map_err(WasmError::runtime)?,
                environment,
            },
            registry: Mutex::new(Registry::default()),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        })))
    }

    async fn connect(
        &self,
        key: String,
        initialization: Vec<u8>,
    ) -> Result<WasmWorkerConnection, String> {
        if key.is_empty() || key.len() > 256 {
            return Err("worker keys must contain 1 to 256 bytes".into());
        }
        if initialization.len() > MAX_INITIALIZATION {
            return Err("worker initialization exceeds 64 KiB".into());
        }
        let runtime = Handle::try_current()
            .map_err(|error| format!("worker runtime is unavailable: {error}"))?;
        loop {
            let attempt = {
                let mut registry = self
                    .0
                    .registry
                    .lock()
                    .expect("worker registry lock is not poisoned");
                if registry.shutting_down {
                    return Err("worker provider is shutting down".into());
                }
                if let Some(entry) = registry.entries.get(&key) {
                    if entry.listener.is_closing() {
                        Connect::Retiring(entry.listener.finished.clone())
                    } else {
                        if entry.initialization != initialization {
                            return Err(
                                "worker initialization does not match the existing worker".into()
                            );
                        }
                        entry.listener.connect(&self.0.connections)?.map_or_else(
                            || Connect::Retiring(entry.listener.finished.clone()),
                            Connect::Connected,
                        )
                    }
                } else {
                    if registry.entries.len() >= MAX_WORKERS {
                        return Err("provider worker limit reached".into());
                    }
                    let listener = Arc::new(Listener::new());
                    let connection = listener
                        .connect(&self.0.connections)?
                        .expect("new listener is open");
                    let running = Running {
                        workers: Arc::downgrade(&self.0),
                        key: key.clone(),
                        listener: listener.clone(),
                        result: Err("worker stopped before completing".into()),
                    };
                    let factory = self.0.factory.clone();
                    let input = initialization.clone();
                    let task = runtime.spawn(async move {
                        let mut running = running;
                        running.result = factory.run(input, &running.listener).await;
                    });
                    registry.entries.insert(
                        key.clone(),
                        Entry {
                            initialization: initialization.clone(),
                            listener,
                            _task: Task(task),
                        },
                    );
                    Connect::Connected(connection)
                }
            };
            match attempt {
                Connect::Connected(connection) => return Ok(connection),
                Connect::Retiring(finished) => finished.await,
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        let listeners: Vec<_> = {
            let mut registry = self
                .0
                .registry
                .lock()
                .expect("worker registry lock is not poisoned");
            registry.shutting_down = true;
            registry
                .entries
                .values()
                .map(|entry| entry.listener.clone())
                .collect()
        };
        for listener in &listeners {
            listener.close();
        }
        future::join_all(listeners.iter().map(|listener| listener.finished.clone())).await;
    }
}

impl Factory {
    async fn run(&self, initialization: Vec<u8>, listener: &Arc<Listener>) -> Result<(), String> {
        let mut store =
            WasmState::new(self.environment.clone(), None).into_store(self.instance.engine());
        store
            .set_fuel(u64::MAX)
            .map_err(|error| format!("failed to configure worker fuel: {error:#}"))?;
        *store.data_mut().limits() = StoreLimitsBuilder::new()
            .memory_size(64 * 1024 * 1024)
            .build();
        store.limiter(|state| state.limits());
        store
            .fuel_async_yield_interval(Some(100_000))
            .map_err(|error| format!("failed to configure worker execution: {error:#}"))?;
        let run = async {
            let instance = self.instance.instantiate_async(&mut store).await?;
            let incoming = store
                .data_mut()
                .table()
                .push(WasmWorkerListener(listener.clone()))?;
            store
                .run_concurrent(async |accessor| {
                    instance
                        .fleet_providers_worker()
                        .call_run(accessor, initialization, incoming)
                        .await
                })
                .await?
        };
        let result = {
            let mut run = core::pin::pin!(run);
            match future::select(run.as_mut(), listener.closed.clone()).await {
                Either::Left((result, _)) => result
                    .map_err(|error| format!("worker execution failed: {error:#}"))
                    .and_then(core::convert::identity),
                Either::Right(((), _)) => tokio::time::timeout(SHUTDOWN_TIMEOUT, run)
                    .await
                    .map_err(|_| "worker did not stop within five seconds".to_owned())
                    .and_then(|result| {
                        result
                            .map_err(|error| format!("worker shutdown failed: {error:#}"))
                            .and_then(core::convert::identity)
                    }),
            }
        };
        listener.close();
        listener.close_connections(&result);
        result
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.listener.close();
        self.listener.close_connections(&self.result);
        if let Some(workers) = self.workers.upgrade() {
            let mut registry = workers
                .registry
                .lock()
                .expect("worker registry lock is not poisoned");
            if registry
                .entries
                .get(&self.key)
                .is_some_and(|entry| Arc::ptr_eq(&entry.listener, &self.listener))
            {
                registry.entries.remove(&self.key);
            }
        }
        let done = self
            .listener
            .done
            .lock()
            .expect("worker completion lock is not poisoned")
            .take();
        if let Some(done) = done {
            done.send(()).unwrap_or_default();
        }
    }
}

impl Listener {
    fn new() -> Self {
        let (close, closed) = oneshot::channel();
        let (done, finished) = oneshot::channel();
        Self {
            admission: Mutex::new(Admission {
                pending: VecDeque::new(),
                connections: Vec::new(),
                close: Some(close),
            }),
            available: Notify::new(),
            closed: async {
                closed.await.unwrap_or_default();
            }
            .boxed()
            .shared(),
            finished: async {
                finished.await.unwrap_or_default();
            }
            .boxed()
            .shared(),
            done: Mutex::new(Some(done)),
        }
    }

    fn connect(
        &self,
        connections: &Arc<Semaphore>,
    ) -> Result<Option<WasmWorkerConnection>, String> {
        let mut admission = self
            .admission
            .lock()
            .expect("worker listener lock is not poisoned");
        if admission.close.is_none() {
            return Ok(None);
        }
        admission
            .pending
            .retain(|connection| connection.state().is_open());
        let permit = connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| "provider worker connection limit reached")?;
        let (client, server) = WasmWorkerConnection::pair(permit);
        admission
            .connections
            .retain(|connection| connection.upgrade().is_some_and(|state| state.is_open()));
        admission.connections.push(Arc::downgrade(client.state()));
        admission.pending.push_back(server);
        self.available.notify_one();
        Ok(Some(client))
    }

    async fn accept(&self) -> Option<WasmWorkerConnection> {
        loop {
            let mut available = core::pin::pin!(self.available.notified());
            available.as_mut().enable();
            {
                let mut admission = self
                    .admission
                    .lock()
                    .expect("worker listener lock is not poisoned");
                admission.close.as_ref()?;
                while let Some(connection) = admission.pending.pop_front() {
                    if connection.state().is_open() {
                        return Some(connection);
                    }
                }
            }
            match future::select(available, self.closed.clone()).await {
                Either::Left(_) => {}
                Either::Right(_) => return None,
            }
        }
    }

    fn try_retire(&self) -> bool {
        let mut admission = self
            .admission
            .lock()
            .expect("worker listener lock is not poisoned");
        if admission
            .connections
            .iter()
            .any(|connection| connection.upgrade().is_some_and(|state| state.is_open()))
        {
            return false;
        }
        if let Some(close) = admission.close.take() {
            close.send(()).unwrap_or_default();
        }
        admission.pending.clear();
        true
    }

    fn is_closing(&self) -> bool {
        self.admission
            .lock()
            .expect("worker listener lock is not poisoned")
            .close
            .is_none()
    }

    fn close(&self) {
        let mut admission = self
            .admission
            .lock()
            .expect("worker listener lock is not poisoned");
        if let Some(close) = admission.close.take() {
            close.send(()).unwrap_or_default();
        }
    }

    fn close_connections(&self, result: &Result<(), String>) {
        let mut admission = self
            .admission
            .lock()
            .expect("worker listener lock is not poisoned");
        for connection in &admission.connections {
            if let Some(connection) = connection.upgrade() {
                connection.close(result.clone());
            }
        }
        admission.pending.clear();
    }
}

impl Drop for WasmWorkerListener {
    fn drop(&mut self) {
        self.0.close();
    }
}

// === Host ===

impl workers::Host for WasmState {}
impl workers::HostListener for WasmState {}

impl workers::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn connect(
        store: &Accessor<WasmState, Self>,
        key: String,
        initialization: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<WasmWorkerConnection>, String>> {
        let workers = store.with(|mut access| access.get().workers());
        let Some(workers) = workers else {
            return Ok(Err("workers are not available in this instance".into()));
        };
        match workers.connect(key, initialization).await {
            Ok(connection) => {
                Ok(Ok(store.with(|mut access| {
                    access.get().table().push(connection)
                })?))
            }
            Err(error) => Ok(Err(error)),
        }
    }
}

impl workers::HostListenerWithStore<WasmState> for HasSelf<WasmState> {
    async fn accept(
        store: &Accessor<WasmState, Self>,
        resource: Resource<WasmWorkerListener>,
    ) -> wasmtime::Result<Option<Resource<WasmWorkerConnection>>> {
        let listener = store.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table().get(&resource)?.0.clone())
        })?;
        Ok(listener
            .accept()
            .await
            .map(|connection| store.with(|mut access| access.get().table().push(connection)))
            .transpose()?)
    }

    fn try_retire(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmWorkerListener>,
    ) -> wasmtime::Result<bool> {
        Ok(store.get().table().get(&resource)?.0.try_retire())
    }

    async fn closed(
        store: &Accessor<WasmState, Self>,
        resource: Resource<WasmWorkerListener>,
    ) -> wasmtime::Result<()> {
        let closed = store.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table().get(&resource)?.0.closed.clone())
        })?;
        closed.await;
        Ok(())
    }

    fn drop(
        mut store: Access<WasmState, Self>,
        resource: Resource<WasmWorkerListener>,
    ) -> wasmtime::Result<()> {
        store.get().table().delete(resource)?;
        Ok(())
    }
}
