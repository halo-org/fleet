use fleet_core::{AgentId, Configured};
use fleet_kit::{
    Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceId, WorkspaceName, WorkspaceOutput,
    WorkspaceResolution, WorkspaceResolvers, WorkspaceWrite, Workspaces,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};
use tokio::sync::watch;

type Environment = (WorkspaceName, WorkspaceId);

/// Keeps workspace environments in use from being released, and their use waiting for releases.
#[derive(Clone)]
pub struct WorkspaceLocks(watch::Sender<BTreeMap<Environment, Lock>>);

/// Holds an environment for its release until dropped.
pub struct ReleaseLock {
    locks: WorkspaceLocks,
    environment: Environment,
}

/// Workspaces that use each environment they access until dropped.
pub struct LockingWorkspaces<W> {
    locks: WorkspaceLocks,
    used: Mutex<BTreeSet<Environment>>,
    workspaces: W,
}

#[derive(Default)]
struct Lock {
    uses: usize,
    releasing: bool,
}

impl WorkspaceLocks {
    /// Creates locks for environments that are not in use.
    pub fn new() -> Self {
        Self(watch::Sender::new(BTreeMap::new()))
    }

    /// Locks an environment for its release unless it is in use or already being released.
    pub fn try_release(&self, context: &WorkspaceContext) -> Option<ReleaseLock> {
        let environment = (context.workspace.name.clone(), context.id);
        let locked = self.0.send_if_modified(|locks| {
            let lock = locks.entry(environment.clone()).or_default();
            let available = lock.uses == 0 && !lock.releasing;
            lock.releasing |= available;
            available
        });
        locked.then(|| ReleaseLock {
            locks: self.clone(),
            environment,
        })
    }

    /// Wraps workspaces used by one tool call.
    pub fn wrap<W>(&self, workspaces: W) -> LockingWorkspaces<W> {
        LockingWorkspaces {
            locks: self.clone(),
            used: Mutex::new(BTreeSet::new()),
            workspaces,
        }
    }

    async fn use_environment(&self, environment: &Environment) {
        let mut changes = self.0.subscribe();
        while !self.0.send_if_modified(|locks| {
            let lock = locks.entry(environment.clone()).or_default();
            if !lock.releasing {
                lock.uses = lock.uses.checked_add(1).expect("uses do not overflow");
            }
            !lock.releasing
        }) {
            changes
                .wait_for(|locks| locks.get(environment).is_none_or(|lock| !lock.releasing))
                .await
                .expect("sender outlives receiver");
        }
    }

    fn unuse(&self, environment: &Environment) {
        self.update(environment, |lock| {
            lock.uses = lock.uses.checked_sub(1).expect("the environment is in use");
        });
    }

    fn update(&self, environment: &Environment, update: impl FnOnce(&mut Lock)) {
        self.0.send_modify(|locks| {
            let lock = locks
                .get_mut(environment)
                .expect("a held environment is locked");
            update(lock);
            if lock.uses == 0 && !lock.releasing {
                locks.remove(environment);
            }
        });
    }
}

impl Drop for ReleaseLock {
    fn drop(&mut self) {
        self.locks
            .update(&self.environment, |lock| lock.releasing = false);
    }
}

impl<W: Sync> LockingWorkspaces<W> {
    async fn use_environment(&self, context: &WorkspaceContext) {
        let environment = (context.workspace.name.clone(), context.id);
        if self.used().contains(&environment) {
            return;
        }
        self.locks.use_environment(&environment).await;
        if !self.used().insert(environment.clone()) {
            self.locks.unuse(&environment);
        }
    }

    fn used(&self) -> std::sync::MutexGuard<'_, BTreeSet<Environment>> {
        self.used
            .lock()
            .expect("used environments are not poisoned")
    }
}

impl<W> Drop for LockingWorkspaces<W> {
    fn drop(&mut self) {
        let used = self
            .used
            .get_mut()
            .expect("used environments are not poisoned");
        for environment in core::mem::take(used) {
            self.locks.unuse(&environment);
        }
    }
}

impl<W: Workspaces + Sync> Workspaces for LockingWorkspaces<W> {
    type Error = W::Error;

    async fn get(&self, name: &WorkspaceName) -> Result<Option<Workspace>, W::Error> {
        self.workspaces.get(name).await
    }

    async fn open(&self, context: &WorkspaceContext) -> Result<(), W::Error> {
        self.use_environment(context).await;
        self.workspaces.open(context).await
    }

    async fn release(&self, context: &WorkspaceContext) -> Result<(), W::Error> {
        self.use_environment(context).await;
        self.workspaces.release(context).await
    }

    async fn read(
        &self,
        context: &WorkspaceContext,
        path: &str,
        limit: u64,
    ) -> Result<Vec<u8>, W::Error> {
        self.use_environment(context).await;
        self.workspaces.read(context, path, limit).await
    }

    async fn write(
        &self,
        context: &WorkspaceContext,
        file: &WorkspaceWrite,
    ) -> Result<(), W::Error> {
        self.use_environment(context).await;
        self.workspaces.write(context, file).await
    }

    async fn exec(
        &self,
        context: &WorkspaceContext,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> Result<WorkspaceOutput, W::Error> {
        self.use_environment(context).await;
        self.workspaces.exec(context, is_retry, command).await
    }
}

impl<W: WorkspaceResolvers + Sync> WorkspaceResolvers for LockingWorkspaces<W> {
    type Error = W::Error;

    async fn get(&self, name: &WorkspaceName) -> Result<Option<Workspace>, W::Error> {
        self.workspaces.get(name).await
    }

    async fn resolve(
        &self,
        subject: AgentId,
        workspace: &Configured<WorkspaceName>,
    ) -> Result<WorkspaceResolution, W::Error> {
        self.workspaces.resolve(subject, workspace).await
    }
}
