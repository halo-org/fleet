use crate::{
    WasmEnvironment, WasmError, WasmState, WasmWorkers,
    bindings::{
        types,
        workspace_resolvers::{
            WorkspaceResolverProviderPre, exports::fleet::providers::workspace_resolvers,
        },
    },
};
use fleet_core::{AgentId, Configured};
use fleet_kit::{Workspace, WorkspaceName, WorkspaceResolution, WorkspaceSource};
use wasmtime::{Store, component::InstancePre};

/// A WebAssembly workspace resolver.
#[derive(Clone)]
pub struct WasmWorkspaceResolvers {
    instance: WorkspaceResolverProviderPre<WasmState>,
    environment: WasmEnvironment,
    workers: Option<WasmWorkers>,
}

impl WasmWorkspaceResolvers {
    pub(crate) fn new(
        instance: InstancePre<WasmState>,
        environment: WasmEnvironment,
        workers: Option<WasmWorkers>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            instance: WorkspaceResolverProviderPre::new(instance).map_err(WasmError::runtime)?,
            environment,
            workers,
        })
    }

    fn store(&self) -> Store<WasmState> {
        WasmState::new(self.environment.clone(), self.workers.clone())
            .into_store(self.instance.engine())
    }

    pub(crate) async fn get(&self, name: &str) -> Result<Option<Workspace>, WasmError> {
        let mut store = self.store();
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        let workspace = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspace_resolvers()
                    .call_get(accessor, name.into())
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)?;
        workspace
            .map(|workspace| {
                Ok(Workspace {
                    name: workspace.name.into(),
                    extensions: serde_json::from_str(&workspace.extensions)?,
                })
            })
            .transpose()
    }

    pub(crate) async fn resolve(
        &self,
        name: &str,
        subject: AgentId,
        workspace: &Configured<WorkspaceName>,
    ) -> Result<WorkspaceResolution, WasmError> {
        let mut store = self.store();
        let instance = self
            .instance
            .instantiate_async(&mut store)
            .await
            .map_err(WasmError::runtime)?;
        store.data_mut().set_agents(
            self.environment
                .agents
                .as_ref()
                .map(|agents| agents.for_agent(subject)),
        );
        let resolution = store
            .run_concurrent(async |accessor| {
                instance
                    .fleet_providers_workspace_resolvers()
                    .call_resolve(
                        accessor,
                        subject.into(),
                        types::Configured {
                            name: name.into(),
                            config: crate::convert::json(&workspace.config),
                        },
                    )
                    .await
            })
            .await
            .flatten()
            .map_err(WasmError::runtime)?
            .map_err(WasmError::from_workspace)?;
        Ok(WorkspaceResolution {
            source: match resolution.source {
                workspace_resolvers::WorkspaceSource::Agent(agent) => {
                    WorkspaceSource::Agent(agent.into())
                }
                workspace_resolvers::WorkspaceSource::Environment((workspace, id)) => {
                    WorkspaceSource::Environment(
                        Configured {
                            name: workspace.name.into(),
                            config: serde_json::from_str(&workspace.config)?,
                        },
                        ((u128::from(id.high) << 64) | u128::from(id.low)).into(),
                    )
                }
            },
            directory: resolution.directory,
        })
    }
}
