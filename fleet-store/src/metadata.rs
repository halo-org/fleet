//! Application metadata edits, independent of runtime execution.

use crate::{
    StoreError, agents,
    records::{AgentRecord, Records},
};
use fleet_core::JsonObject;
use redb::Table;

// Keep the persisted projection independent of the optional kit interfaces.
// Core-only writers must maintain it when reopening a database used by kit.
pub fn agent_title(extensions: &JsonObject) -> Option<&str> {
    extensions.get("fleet.title")?.as_str()
}

pub fn set_title(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
    id: u128,
    expected: u128,
    title: Option<&str>,
) -> Result<(bool, bool), StoreError> {
    let mut agent: AgentRecord = records.agent(id)?;
    if agent.metadata_revision != expected {
        return Err(StoreError::PreconditionFailed);
    }
    if agent_title(&agent.agent.extensions) == title {
        return Ok((false, false));
    }
    let mut metadata = agent.agent.clone();
    match title {
        Some(title) => {
            metadata
                .extensions
                .insert("fleet.title".into(), title.into());
        }
        None => {
            metadata.extensions.remove("fleet.title");
        }
    }
    let changed = records.update_metadata(&mut agent, metadata)?;
    let summary_changed = agents::sync_agent(records, &agent, agents::now_millis())?;
    Ok((changed, summary_changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, Topic, records::Request};
    use fleet_core::{Agent, ClientStore as _, Configured};
    use futures_util::FutureExt as _;
    use serde_json::json;

    #[test]
    fn agent_title_reads_string_titles() {
        let cases = [
            (json!(null), None),
            (json!(1), None),
            (json!("Title"), Some("Title")),
        ];

        for (value, expected) in cases {
            let extensions = JsonObject::from_iter([("fleet.title".into(), value)]);

            let title = agent_title(&extensions);
            assert_eq!(title, expected);

            #[cfg(feature = "queries")]
            assert_eq!(title, fleet_kit::agent_title(&extensions));
        }
    }

    #[test]
    fn set_title_does_not_notify_execution() {
        let store = Store::memory().unwrap();
        let agent = Agent {
            id: 1.into(),
            role: Configured {
                name: "test".into(),
                config: JsonObject::new(),
            },
            extensions: JsonObject::new(),
        };
        let request = Request::SetTitle {
            id: 1,
            expected: 1,
            title: Some("Title".into()),
        };

        futures_executor::block_on(async {
            store.create_agent("create", &agent, None).await.unwrap();
            store
                .interrupt_agent("interrupt", agent.id, None)
                .await
                .unwrap();
            let wake = store.notifications().listen(Topic::Wakes);
            let preview = store.notifications().listen(Topic::Preview(1));

            store.request("rename".into(), request).await.unwrap();
            assert_eq!(wake.now_or_never(), None);
            assert_eq!(preview.now_or_never(), None);
        });
    }
}
