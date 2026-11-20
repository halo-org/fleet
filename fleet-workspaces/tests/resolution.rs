//! Tests for live workspace references and bound views.

#![cfg(test)]

use core::{convert::Infallible, future::ready};
use fleet_core::{
    Agent, AgentId, ClientStore as _, Configured, Error as _, JsonObject, Role, RoleParameter,
    RoleParameterTarget,
};
use fleet_kit::{
    AgentParent, Extension as _, RoleStore as _, Workspace, WorkspaceAccess as _, WorkspaceCommand,
    WorkspaceContext, WorkspaceName, WorkspaceOutput, WorkspaceResolution, WorkspaceResolvers,
    WorkspaceSource, WorkspaceWrite, Workspaces, WorkspacesErrorKind,
};
use fleet_store::Store;
use fleet_workspaces::AgentWorkspaces;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Providers(Arc<Mutex<Vec<(WorkspaceContext, String)>>>);

impl WorkspaceResolvers for Providers {
    type Error = Infallible;

    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send {
        ready(Ok((name.as_ref() != "host:default").then(|| Workspace {
            name: name.clone(),
            extensions: JsonObject::new(),
        })))
    }

    fn resolve(
        &self,
        _: AgentId,
        workspace: &Configured<WorkspaceName>,
    ) -> impl Future<Output = Result<WorkspaceResolution, Self::Error>> + Send {
        let directory = workspace
            .config
            .get("directory")
            .and_then(Value::as_str)
            .map(Into::into);
        let source = workspace
            .config
            .get("agent")
            .and_then(Value::as_str)
            .map_or_else(
                || {
                    WorkspaceSource::Environment(
                        Configured {
                            name: "host:default".into(),
                            config: JsonObject::new(),
                        },
                        42.into(),
                    )
                },
                |id| WorkspaceSource::Agent(id.parse::<u128>().unwrap().into()),
            );
        ready(Ok(WorkspaceResolution { source, directory }))
    }
}

impl Workspaces for Providers {
    type Error = Infallible;

    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send {
        ready(Ok(Some(Workspace {
            name: name.clone(),
            extensions: JsonObject::new(),
        })))
    }
    fn open(&self, _: &WorkspaceContext) -> impl Future<Output = Result<(), Self::Error>> + Send {
        ready(Ok(()))
    }
    fn release(
        &self,
        _: &WorkspaceContext,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        ready(Ok(()))
    }
    fn read(
        &self,
        context: &WorkspaceContext,
        path: &str,
        _: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        self.0.lock().unwrap().push((context.clone(), path.into()));
        ready(Ok(vec![]))
    }
    fn write(
        &self,
        context: &WorkspaceContext,
        file: &WorkspaceWrite,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.0
            .lock()
            .unwrap()
            .push((context.clone(), file.path.clone()));
        ready(Ok(()))
    }
    fn exec(
        &self,
        context: &WorkspaceContext,
        _: bool,
        command: &WorkspaceCommand,
    ) -> impl Future<Output = Result<WorkspaceOutput, Self::Error>> + Send {
        self.0
            .lock()
            .unwrap()
            .push((context.clone(), command.cwd.clone().unwrap_or_default()));
        ready(Ok(WorkspaceOutput {
            exit: fleet_kit::WorkspaceExit::Code(0),
            stdout: fleet_kit::CapturedOutput::default(),
            stderr: fleet_kit::CapturedOutput::default(),
        }))
    }
}

fn object(value: Value) -> JsonObject {
    let Value::Object(object) = value else {
        panic!("expected an object")
    };
    object
}

async fn agent(
    store: &Store,
    id: u128,
    parent: Option<u128>,
    workspace: Value,
    parameters: Value,
) -> Role {
    let role = Role {
        name: id.to_string().into(),
        parameters: BTreeMap::from([(
            "config".into(),
            RoleParameter {
                target: RoleParameterTarget::Extensions(vec![
                    "fleet.workspace".into(),
                    "config".into(),
                ]),
                extensions: JsonObject::new(),
            },
        )]),
        model: Configured {
            name: "model".into(),
            config: JsonObject::new(),
        },
        tools: BTreeMap::new(),
        system_prompt: String::new(),
        compaction: None,
        extensions: if workspace.is_null() {
            JsonObject::new()
        } else {
            object(json!({ "fleet.workspace": workspace }))
        },
    };
    store
        .put_role(&format!("role-{id}"), &role, None)
        .await
        .unwrap();
    let mut extensions = JsonObject::new();
    if let Some(parent) = parent {
        AgentParent {
            agent: parent.into(),
            call: "create".into(),
        }
        .insert(&mut extensions)
        .unwrap();
    }
    store
        .create_agent(
            &format!("agent-{id}"),
            &Agent {
                id: id.into(),
                role: Configured {
                    name: role.name.clone(),
                    config: object(parameters),
                },
                extensions,
            },
            None,
        )
        .await
        .unwrap();
    role
}

#[test]
fn references_load_each_roles_parameters_and_compose_views() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(
            &store,
            1,
            None,
            json!({ "name": "host:default" }),
            json!({ "config": { "cwd": "/repo" } }),
        )
        .await;
        let mut middle = agent(
            &store,
            2,
            Some(1),
            json!({ "name": "helpers:parent", "config": { "agent": "1", "directory": "old" } }),
            json!({}),
        )
        .await;
        agent(
            &store,
            3,
            Some(2),
            json!({ "name": "delegates:ancestor" }),
            json!({ "config": { "agent": "2", "directory": "tests" } }),
        )
        .await;
        let providers = Providers::default();
        let access = AgentWorkspaces::new(
            3.into(),
            store.agent_access(3.into()),
            store.clone(),
            providers.clone(),
        );
        let first = access.open().await.unwrap();
        assert_eq!(first.context.id, 1.into());
        assert_eq!(first.context.agent_id, 3.into());
        assert_eq!(
            first.context.workspace.config,
            object(json!({ "cwd": "/repo" }))
        );
        assert_eq!(first.directory.as_deref(), Some("old/tests"));

        let version = store.role(&middle.name).await.unwrap().unwrap().version;
        middle.extensions = object(
            json!({ "fleet.workspace": { "name": "renamed:parent", "config": { "agent": "1", "directory": "new" } } }),
        );
        store
            .put_role("change", &middle, Some(version))
            .await
            .unwrap();
        access.read(&first, "file", 100).await.unwrap();
        access
            .write(
                &first,
                &WorkspaceWrite {
                    path: "file".into(),
                    content: vec![],
                    expected: Some(vec![]),
                    create_parents: false,
                },
            )
            .await
            .unwrap();
        let second = access.open().await.unwrap();
        access.read(&second, "/absolute", 100).await.unwrap();
        let mut command = WorkspaceCommand {
            id: "command".into(),
            program: "pwd".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            stdin: vec![],
            timeout_ms: 1000,
            output_limit: 1000,
        };
        access.exec(&second, false, &command).await.unwrap();
        command.cwd = Some("../src".into());
        access.exec(&second, false, &command).await.unwrap();

        assert_eq!(second.directory.as_deref(), Some("new/tests"));
        let paths: Vec<_> = providers
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        assert_eq!(
            paths,
            [
                "old/tests/file",
                "old/tests/file",
                "/absolute",
                "new/tests",
                "new/tests/../src"
            ]
        );
    });
}

#[test]
fn cycles_fail_and_concrete_bindings_can_share_an_environment() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        let mut role = agent(
            &store,
            1,
            None,
            json!({ "name": "resolver:reference", "config": { "agent": "1" } }),
            json!({}),
        )
        .await;
        let access = AgentWorkspaces::new(
            1.into(),
            store.agent_access(1.into()),
            store.clone(),
            Providers::default(),
        );
        let error = access.open().await.unwrap_err();
        assert_eq!(error.kind(), WorkspacesErrorKind::InvalidConfiguration);
        let version = store.role(&role.name).await.unwrap().unwrap().version;
        role.extensions = object(json!({ "fleet.workspace": { "name": "resolver:shared" } }));
        store
            .put_role("repair", &role, Some(version))
            .await
            .unwrap();
        let binding = access.open().await.unwrap();
        assert_eq!(binding.context.id, 42.into());
        assert_eq!(binding.context.agent_id, 1.into());
    });
}

#[test]
fn agents_own_only_unshared_provider_environments() {
    futures_executor::block_on(async {
        let store = Store::memory().unwrap();
        agent(
            &store,
            1,
            None,
            json!({ "name": "host:default", "config": { "cwd": "/repo" } }),
            json!({}),
        )
        .await;
        agent(
            &store,
            2,
            Some(1),
            json!({ "name": "helpers:parent", "config": { "agent": "1" } }),
            json!({}),
        )
        .await;
        agent(
            &store,
            3,
            None,
            json!({ "name": "host:default", "id": "42" }),
            json!({}),
        )
        .await;
        agent(&store, 4, None, Value::Null, json!({})).await;
        let providers = Providers::default();
        let owned = async |id: u128| {
            AgentWorkspaces::new(
                id.into(),
                store.agent_access(id.into()),
                store.clone(),
                providers.clone(),
            )
            .owned_environment()
            .await
            .unwrap()
        };

        assert_eq!(
            owned(1).await,
            Some(WorkspaceContext {
                workspace: Configured {
                    name: "host:default".into(),
                    config: object(json!({ "cwd": "/repo" })),
                },
                agent_id: 1.into(),
                id: 1.into(),
            })
        );
        for id in [2, 3, 4] {
            assert_eq!(owned(id).await, None);
        }
        assert_eq!(*providers.0.lock().unwrap(), []);
    });
}
