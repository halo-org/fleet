use crate::{
    Store, StoreError, Topic,
    records::{self, Records},
    release_records::{self, DELETED, ENDED, RELEASE, Release, SETTLED},
};
use fleet_core::{AgentId, Configured, RoleName};
use redb::ReadableDatabase as _;

/// A pending release of an agent's workspace.
///
/// A release is scheduled when an agent's scope closes, a root agent settles or an agent is
/// deleted. Restoring, snoozing or messaging a settled agent cancels its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRelease {
    agent: AgentId,
    kind: u8,
    since: u64,
    generation: u128,
    role: Option<Configured<RoleName>>,
}

/// Whether a pending release can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceReleaseState {
    /// The agent is idle.
    Ready,
    /// The agent has a turn, interruption, input or tool call in progress.
    Busy,
    /// The release was completed, cancelled or replaced by a later one.
    Cancelled,
}

impl WorkspaceRelease {
    /// Returns the agent whose workspace is released.
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.agent
    }

    /// Returns the role a deleted agent had, or `None` if the agent was not deleted.
    #[must_use]
    pub const fn deleted_role(&self) -> Option<&Configured<RoleName>> {
        self.role.as_ref()
    }

    fn key(&self) -> Vec<u8> {
        release_records::key(self.kind, self.since, self.agent.into())
    }

    fn pending<T: redb::ReadableTable<&'static [u8], &'static [u8]>>(
        &self,
        records: &Records<T>,
    ) -> Result<bool, StoreError> {
        Ok(records
            .get::<Release>(&self.key())?
            .is_some_and(|Release(_, _, generation, _)| generation == self.generation))
    }
}

impl Store {
    /// Lists the workspace releases due at `now` and the next time a release becomes due.
    ///
    /// Releases of agents whose scope closed or that were deleted are due immediately. Settled root
    /// agents are due once settled for `settled_for` milliseconds, or never without it.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn workspace_releases(
        &self,
        now: u64,
        settled_for: Option<u64>,
    ) -> Result<(Vec<WorkspaceRelease>, Option<u64>), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let mut due = Vec::new();
                for kind in [ENDED, DELETED] {
                    for entry in records.prefix::<Release>(&[RELEASE, kind])? {
                        let Release(since, agent, generation, role) = entry?;
                        due.push(WorkspaceRelease {
                            agent: agent.into(),
                            kind,
                            since,
                            generation,
                            role: role.map(Into::into),
                        });
                    }
                }
                let mut next = None;
                if let Some(settled_for) = settled_for {
                    for entry in records.prefix::<(u64, u128, u128)>(&[RELEASE, SETTLED])? {
                        let (since, agent, generation) = entry?;
                        let deadline = since.saturating_add(settled_for);
                        if deadline > now {
                            next = Some(deadline);
                            break;
                        }
                        due.push(WorkspaceRelease {
                            agent: agent.into(),
                            kind: SETTLED,
                            since,
                            generation,
                            role: None,
                        });
                    }
                }
                Ok((due, next))
            })
            .await
    }

    /// Reads whether a pending release can start.
    ///
    /// A deleted agent is idle. The host must keep the agent's environment from being used from
    /// before this check until the release ends.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn workspace_release_state(
        &self,
        release: &WorkspaceRelease,
    ) -> Result<WorkspaceReleaseState, StoreError> {
        let release = release.clone();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                if !release.pending(&records)? {
                    return Ok(WorkspaceReleaseState::Cancelled);
                }
                Ok(
                    if release.role.is_some() || records.agent(release.agent.into())?.idle() {
                        WorkspaceReleaseState::Ready
                    } else {
                        WorkspaceReleaseState::Busy
                    },
                )
            })
            .await
    }

    /// Removes a completed release unless it was cancelled or replaced.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the removal.
    pub async fn complete_workspace_release(
        &self,
        release: &WorkspaceRelease,
    ) -> Result<(), StoreError> {
        let release = release.clone();
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    if release.pending(&records)? {
                        records.remove(&release.key())?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    /// Registers immediately for newly scheduled releases. Register before listing them.
    pub fn workspace_releases_changed(&self) -> impl Future<Output = ()> + Send + use<> {
        self.listeners().listen(Topic::WorkspaceReleases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{Agent, ClientStore as _, Configured, JsonObject};
    use fleet_kit::{AgentCollection as _, AgentsQuery};

    async fn set_inbox(store: &Store, inbox: u8, now: u64) {
        let version = store.agents(AgentsQuery::default()).await.unwrap().agents[0]
            .version
            .0;
        store
            .worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                crate::agents::set_inbox(
                    &mut Records(transaction.open_table(records::RECORDS)?),
                    1,
                    version,
                    inbox,
                    None,
                    now,
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn store(path: &std::path::Path) -> Store {
        let store = Store::open(path).unwrap();
        let agent = Agent {
            id: 1.into(),
            role: Configured {
                name: "assistant".into(),
                config: JsonObject::new(),
            },
            extensions: JsonObject::new(),
        };
        store.create_agent("create", &agent, None).await.unwrap();
        store
    }

    #[test]
    fn a_release_replaced_within_the_same_millisecond_survives_completion() {
        let path =
            std::env::temp_dir().join(format!("fleet-release-replace-{}.redb", std::process::id()));
        futures_executor::block_on(async {
            let store = store(&path).await;
            set_inbox(&store, crate::agents::SETTLED, 1).await;
            let (first, _) = store.workspace_releases(1, Some(0)).await.unwrap();
            set_inbox(&store, crate::agents::PENDING, 1).await;
            set_inbox(&store, crate::agents::SETTLED, 1).await;

            assert_eq!(
                store.workspace_release_state(&first[0]).await.unwrap(),
                WorkspaceReleaseState::Cancelled
            );
            store.complete_workspace_release(&first[0]).await.unwrap();
            let (second, _) = store.workspace_releases(1, Some(0)).await.unwrap();
            assert_eq!(second.len(), 1);
            assert_ne!(second, first);
        });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn agents_settled_before_releases_were_indexed_are_scheduled_once() {
        let path =
            std::env::temp_dir().join(format!("fleet-release-index-{}.redb", std::process::id()));
        futures_executor::block_on(async {
            {
                let store = store(&path).await;
                set_inbox(&store, crate::agents::SETTLED, 1).await;
                store
                    .worker()
                    .call(|worker| {
                        let transaction = worker.database.begin_write()?;
                        {
                            let mut records = Records(transaction.open_table(records::RECORDS)?);
                            records.remove(&release_records::key(SETTLED, 1, 1))?;
                            records.remove(release_records::INDEXED)?;
                        }
                        transaction.commit()?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let store = Store::open(&path).unwrap();
            let (due, _) = store.workspace_releases(1, Some(0)).await.unwrap();
            assert_eq!(
                due.iter().map(WorkspaceRelease::agent).collect::<Vec<_>>(),
                [AgentId::from(1)]
            );
            store.complete_workspace_release(&due[0]).await.unwrap();
            drop(store);

            let store = Store::open(&path).unwrap();
            assert_eq!(
                store.workspace_releases(1, Some(0)).await.unwrap(),
                (vec![], None)
            );
        });
        std::fs::remove_file(path).unwrap();
    }
}
