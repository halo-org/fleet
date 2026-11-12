//! Moves agent titles from the `fleet` object to the `fleet.title` key.

use crate::{
    StoreError,
    records::{self, AgentRecord, Records, Request},
};
use fleet_core::{JsonObject, JsonValue};
use redb::{ReadableTable as _, Table};

pub fn migrate(
    records: &mut Records<Table<'_, &'static [u8], &'static [u8]>>,
) -> Result<(), StoreError> {
    let agents = records
        .prefix::<AgentRecord>(&[records::AGENT])?
        .collect::<Result<Vec<_>, _>>()?;
    for mut agent in agents {
        if flatten(&mut agent.agent.extensions) {
            records.save_agent(&agent)?;
        }
    }

    let start = [records::REQUEST];
    let end = records::prefix_end(&start);
    let mut requests = Vec::new();
    for entry in records.0.range(start.as_slice()..end.as_slice())? {
        let (key, value) = entry?;
        let mut request: Request = serde_json::from_slice(value.value())?;
        if let Request::CreateAgent(agent, _) = &mut request
            && flatten(&mut agent.extensions)
        {
            requests.push((key.value().to_vec(), request));
        }
    }
    for (key, request) in requests {
        records.put(&key, &request)?;
    }
    Ok(())
}

fn flatten(extensions: &mut JsonObject) -> bool {
    let Some(fleet) = extensions
        .get_mut("fleet")
        .and_then(JsonValue::as_object_mut)
    else {
        return false;
    };
    let Some(title) = fleet.remove("title") else {
        return false;
    };
    if fleet.is_empty() {
        extensions.remove("fleet");
    }
    extensions.insert("fleet.title".into(), title);
    true
}

#[cfg(test)]
mod tests {
    use crate::{Store, records};
    use fleet_core::{Agent, ClientStore as _, Configured, JsonObject, RuntimeStore as _};
    use redb::ReadableDatabase as _;
    use serde_json::json;

    #[test]
    fn open_moves_titles_from_format_4() {
        let path =
            std::env::temp_dir().join(format!("fleet-title-migration-{}.redb", std::process::id()));
        let legacy = Agent {
            id: 1.into(),
            role: Configured {
                name: "test".into(),
                config: JsonObject::new(),
            },
            extensions: serde_json::from_value(json!({
                "fleet": {
                    "title": "Title",
                    "color": "blue",
                },
            }))
            .unwrap(),
        };
        let mut agent = legacy.clone();
        agent.extensions = serde_json::from_value(json!({
            "fleet": {
                "color": "blue",
            },
            "fleet.title": "Title",
        }))
        .unwrap();
        let revisions = |store: Store| async move {
            store
                .worker()
                .call(|worker| {
                    let transaction = worker.database.begin_read()?;
                    let records = records::Records(transaction.open_table(records::RECORDS)?);
                    let agent = records.agent(1)?;
                    Ok((agent.revision, agent.metadata_revision))
                })
                .await
                .unwrap()
        };

        futures_executor::block_on(async {
            let before = {
                let store = Store::open(&path).unwrap();
                store.create_agent("create", &legacy, None).await.unwrap();
                store
                    .worker()
                    .call(|worker| {
                        let transaction = worker.database.begin_write()?;
                        records::Records(transaction.open_table(records::RECORDS)?)
                            .put(&[], &4_u32)?;
                        transaction.commit()?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                revisions(store).await
            };

            let store = Store::open(&path).unwrap();
            let stored = store.agent(agent.id).await.unwrap().unwrap();
            assert_eq!(stored, agent);
            assert_eq!(revisions(store.clone()).await, before);

            store.create_agent("create", &agent, None).await.unwrap();
        });

        std::fs::remove_file(path).unwrap();
    }
}
