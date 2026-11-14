use crate::{Fleet, FleetError};
use fleet_core::{
    Agent, AgentId, ClientStore as _, Configured, ContentBlock, JsonObject, Message, MessageId,
    MessageMode, RoleName, ToolCallId,
};
use fleet_kit::{
    AgentParent, Blobs as _, Extension as _, Queries as _, QueryUpdate, Subagents, ToolCallRecord,
};
use sha2::{Digest as _, Sha256};

/// Agents created by a plugin provider's tool call.
///
/// The IDs and idempotency key are derived from the provider, agent and call. Images must be
/// readable by the calling agent. A child created after its call was interrupted is interrupted.
pub struct PluginSubagents {
    fleet: Fleet,
    parent: AgentParent,
    scope: String,
}

impl PluginSubagents {
    /// Creates agent creation for a provider's tool call.
    pub fn new(fleet: Fleet, provider: &str, agent: AgentId, call: ToolCallId) -> Self {
        let scope = format!("subagent:{provider}:{agent}:{}", call.as_ref());
        Self {
            fleet,
            parent: AgentParent { agent, call },
            scope,
        }
    }

    fn id(&self, kind: &str) -> u128 {
        let digest = Sha256::digest(format!("{}:{kind}", self.scope));
        let (id, _) = digest.split_first_chunk().expect("digest has 16 bytes");
        u128::from_be_bytes(*id)
    }
}

impl Subagents for PluginSubagents {
    type Error = FleetError;

    async fn create(
        &self,
        role: &Configured<RoleName>,
        extensions: &JsonObject,
        content: &[ContentBlock],
    ) -> Result<(AgentId, MessageId), FleetError> {
        let mut extensions = extensions.clone();
        self.parent
            .clone()
            .insert(&mut extensions)
            .expect("parent is JSON");
        let agent = Agent {
            id: self.id("agent").into(),
            role: role.clone(),
            extensions,
        };
        let message = Message {
            id: self.id("message").into(),
            mode: MessageMode::Queue,
            content: content.to_vec(),
        };
        let created = (agent.id, message.id);
        let parent = self.parent.clone();
        let key = self.scope.clone();
        self.fleet
            .mutate(async move |fleet| {
                let store = fleet.blob_store();
                let blobs = store.agent_blobs(parent.agent);
                for block in &message.content {
                    if let ContentBlock::Image(id) = block {
                        blobs.get(*id).await?;
                    }
                }
                store.create_agent(&key, &agent, Some(&message)).await?;
                let pending = store
                    .tool_calls(parent.agent, None)
                    .await?
                    .value
                    .iter()
                    .any(|record| {
                        matches!(record, ToolCallRecord::Pending { id, .. } if *id == parent.call)
                    });
                let interrupting = match store.activity(parent.agent, None).await?.value {
                    QueryUpdate::Changed(activity) => activity.interrupt_pending,
                    QueryUpdate::Unchanged => false,
                };
                if !pending || interrupting {
                    store
                        .interrupt_agent(&format!("{key}:interrupt"), agent.id, None)
                        .await?;
                }
                Ok(())
            })
            .await?;
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{
        ModelOutput, ModelProvenance, ModelResponse, ModelUsage, RuntimeStore as _, ToolCall,
    };
    use fleet_kit::AgentsQuery;
    use fleet_store::Store;

    async fn fleet() -> Fleet {
        let fleet = Fleet::new(Store::memory().unwrap(), None);
        let store = fleet.blob_store();
        let agent = Agent {
            id: 1.into(),
            role: role(),
            extensions: JsonObject::new(),
        };
        let message = Message {
            id: 1.into(),
            mode: MessageMode::Steer,
            content: vec![ContentBlock::Text("Delegate".into())],
        };
        let response = ModelResponse {
            output: vec![ModelOutput::ToolCall(ToolCall {
                id: "a".into(),
                tool: "oneshot_agent".into(),
                is_async: false,
                input: JsonObject::new(),
            })],
            usage: ModelUsage {
                input: 1,
                output: 1,
                cache_read: 0,
                cache_write: 0,
            },
            provenance: ModelProvenance {
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            continuation_token: None,
        };
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store.consume_messages(1.into(), None, true).await.unwrap();
        store
            .append_model_response(1.into(), 0.into(), false, &response)
            .await
            .unwrap();
        fleet
    }

    fn role() -> Configured<RoleName> {
        Configured {
            name: "worker".into(),
            config: JsonObject::new(),
        }
    }

    async fn interrupt_pending(fleet: &Fleet, agent: AgentId) -> bool {
        match fleet.activity(agent, None).await.unwrap().value {
            QueryUpdate::Changed(activity) => activity.interrupt_pending,
            QueryUpdate::Unchanged => unreachable!(),
        }
    }

    #[test]
    fn children_are_created_once_per_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let subagents = PluginSubagents::new(fleet.clone(), "agents", 1.into(), "a".into());
            let mut extensions = JsonObject::new();
            extensions.insert("fleet.parent".into(), "forged".into());
            let content = [ContentBlock::Text("Review".into())];

            let created = subagents
                .create(&role(), &extensions, &content)
                .await
                .unwrap();
            let retried = subagents
                .create(&role(), &extensions, &content)
                .await
                .unwrap();
            let roots = fleet.agents(AgentsQuery::default()).await.unwrap();
            let children = fleet
                .agents(AgentsQuery {
                    parent: Some(1.into()),
                    ..AgentsQuery::default()
                })
                .await
                .unwrap();

            assert_eq!(created, retried);
            let roots: Vec<_> = roots.agents.iter().map(|agent| agent.id).collect();
            assert_eq!(roots, [AgentId::from(1)]);
            assert_eq!(children.agents.len(), 1);
            assert_eq!(children.agents[0].id, created.0);
            assert_eq!(
                children.agents[0].parent,
                Some(AgentParent {
                    agent: 1.into(),
                    call: "a".into(),
                })
            );
            assert!(!interrupt_pending(&fleet, created.0).await);
        });
    }

    #[test]
    fn children_follow_interrupts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let fleet = fleet().await;
            let content = [ContentBlock::Text("Review".into())];
            let (pending, _) = PluginSubagents::new(fleet.clone(), "agents", 1.into(), "a".into())
                .create(&role(), &JsonObject::new(), &content)
                .await
                .unwrap();
            let (resolved, _) = PluginSubagents::new(fleet.clone(), "agents", 1.into(), "b".into())
                .create(&role(), &JsonObject::new(), &content)
                .await
                .unwrap();
            assert!(!interrupt_pending(&fleet, pending).await);
            assert!(interrupt_pending(&fleet, resolved).await);

            fleet
                .interrupt_agent("interrupt".into(), 1.into(), None)
                .await
                .unwrap();

            assert!(interrupt_pending(&fleet, pending).await);
        });
    }
}
