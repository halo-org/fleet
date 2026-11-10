//! Streams revision groups and transient model activity on separate subscriptions.
//!
//! Each state update carries its revision ID. Preview subscriptions have no resume ID. Preview text
//! appends to the selected block. A preview replacement clears that selection.

use crate::{ApiAgentUpdates, ApiPreview, ApiPreviewKind, ApiPreviewSelection, Fleet, FleetError};
use core::{fmt::Write as _, pin::pin, time::Duration};
use fleet_core::AgentId;
use fleet_kit::{AgentUpdates, Preview, PreviewContent, Revision};
use futures_util::{
    Stream, StreamExt as _,
    future::{self, Either},
    stream,
};
use hyper::body::{Bytes, Frame};
use serde::Serialize;

#[derive(Default)]
struct PreviewStream {
    preview: Option<Preview>,
    selected_block: Option<u32>,
}

/// Streams collection changes from a read or previously applied SSE revision.
pub fn agents(
    fleet: Fleet,
    after: fleet_kit::CollectionRevision,
) -> impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send {
    let updates = stream::try_unfold((fleet.clone(), after), async move |(fleet, after)| {
        let updates = fleet.agents_updates(after).await?;
        let revision = updates.revision;
        let mut buffer = String::new();
        writeln!(buffer, "id: {revision}").expect("can write to string");
        json_event(
            &mut buffer,
            "update",
            &crate::api_types::ApiAgentsUpdates::from(updates),
        );
        Ok(Some((Frame::data(Bytes::from(buffer)), (fleet, revision))))
    });
    keep_alive(fleet, updates)
}

/// Streams complete resource revision groups.
pub fn stream(
    fleet: Fleet,
    agent_id: AgentId,
    after: Revision,
) -> impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send {
    let updates = stream::try_unfold(
        (fleet.clone(), agent_id, after),
        async move |(fleet, agent_id, after)| {
            let updates = fleet.updates(agent_id, after).await?;
            let revision = updates.revision;
            Ok(Some((
                Frame::data(encode_updates(updates)),
                (fleet, agent_id, revision),
            )))
        },
    );
    keep_alive(fleet, updates)
}

/// Streams the current preview and incremental text.
pub fn previews(
    fleet: Fleet,
    agent_id: AgentId,
) -> impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send {
    let previews = fleet
        .previews(agent_id)
        .scan(PreviewStream::default(), |state, preview| {
            future::ready(Some(
                preview.map(|preview| Frame::data(state.encode(preview))),
            ))
        })
        .then(async |frame| {
            tokio::time::sleep(Duration::from_millis(30)).await;
            frame
        });
    keep_alive(fleet, previews)
}

fn keep_alive(
    fleet: Fleet,
    updates: impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send,
) -> impl Stream<Item = Result<Frame<Bytes>, FleetError>> + Send {
    stream::try_unfold(
        (fleet, Box::pin(updates)),
        async move |(fleet, mut updates)| {
            let result = {
                let shutdown = pin!(fleet.wait_for_shutdown());
                let next = pin!(tokio::time::timeout(
                    Duration::from_secs(15),
                    updates.next(),
                ));
                match future::select(shutdown, next).await {
                    Either::Left(((), _)) => return Ok(None),
                    Either::Right((result, _)) => result,
                }
            };
            let frame = match result {
                Ok(Some(frame)) => frame?,
                Ok(None) => return Ok(None),
                Err(_) => Frame::data(Bytes::from_static(b": keep-alive\n\n")),
            };
            Ok(Some((frame, (fleet, updates))))
        },
    )
}

fn encode_updates(updates: AgentUpdates) -> Bytes {
    let mut buffer = String::new();
    writeln!(buffer, "id: {}", updates.revision).expect("can write to string");
    json_event(&mut buffer, "update", &ApiAgentUpdates::from(updates));
    buffer.into()
}

impl PreviewStream {
    fn encode(&mut self, preview: Option<Preview>) -> Bytes {
        let mut buffer = String::new();
        if let (
            Some(Preview::Response {
                input_cursor: previous_cursor,
                output: previous,
            }),
            Some(Preview::Response {
                input_cursor,
                output: current,
            }),
        ) = (self.preview.as_ref(), preview.as_ref())
            && previous_cursor == input_cursor
            && previous.keys().all(|index| current.contains_key(index))
            && current.iter().all(|(index, content)| {
                appended_text(previous.get(index), content).is_some_and(|text| !text.contains('\r'))
            })
        {
            for (index, content) in current {
                let prior = previous.get(index);
                let text = appended_text(prior, content).expect("content was appended");
                if prior.is_some() && text.is_empty() {
                    continue;
                }
                if self.selected_block != Some(*index) || prior.is_none() {
                    let kind = prior.is_none().then(|| match content {
                        PreviewContent::Text(_) => ApiPreviewKind::Text,
                        PreviewContent::Reasoning(_) => ApiPreviewKind::Reasoning,
                        PreviewContent::ToolCall { id, tool, .. } => ApiPreviewKind::ToolCall {
                            id: id.as_ref(),
                            tool: tool.as_ref(),
                        },
                        PreviewContent::ProviderToolCall { call_id, name, .. } => {
                            ApiPreviewKind::ProviderToolCall {
                                call_id: call_id.as_ref(),
                                name: name.as_ref(),
                            }
                        }
                    });
                    json_event(
                        &mut buffer,
                        "preview_block",
                        &ApiPreviewSelection {
                            index: *index,
                            kind,
                        },
                    );
                    self.selected_block = Some(*index);
                }
                if !text.is_empty() {
                    buffer.push_str("event: preview_text\n");
                    for line in text.split('\n') {
                        buffer.push_str("data: ");
                        buffer.push_str(line);
                        buffer.push('\n');
                    }
                    buffer.push('\n');
                }
            }
        } else {
            self.selected_block = None;
            json_event(&mut buffer, "preview", &ApiPreview::from(preview.clone()));
        }
        self.preview = preview;
        buffer.into()
    }
}

fn appended_text<'a>(
    previous: Option<&PreviewContent>,
    current: &'a PreviewContent,
) -> Option<&'a str> {
    match (previous, current) {
        (
            None,
            PreviewContent::Text(text)
            | PreviewContent::Reasoning(text)
            | PreviewContent::ToolCall { input: text, .. }
            | PreviewContent::ProviderToolCall { content: text, .. },
        ) => Some(text),
        (Some(PreviewContent::Text(previous)), PreviewContent::Text(text))
        | (Some(PreviewContent::Reasoning(previous)), PreviewContent::Reasoning(text)) => {
            text.strip_prefix(previous)
        }
        (
            Some(PreviewContent::ToolCall {
                id: previous_id,
                tool: previous_tool,
                input: previous,
            }),
            PreviewContent::ToolCall { id, tool, input },
        ) if id == previous_id && tool == previous_tool => input.strip_prefix(previous),
        (
            Some(PreviewContent::ProviderToolCall {
                call_id: previous_id,
                name: previous_name,
                content: previous,
            }),
            PreviewContent::ProviderToolCall {
                call_id,
                name,
                content,
            },
        ) if call_id == previous_id && name == previous_name => content.strip_prefix(previous),
        _ => None,
    }
}

fn json_event(buffer: &mut String, event: &str, value: &impl Serialize) {
    let json = serde_json::to_string(value).expect("response is serializable");
    writeln!(buffer, "event: {event}\ndata: {json}\n").expect("can write to string");
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_kit::{AgentActivity, EventRecord, MessageRecord, QueryUpdate};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn preview(text: &str) -> Preview {
        Preview::Response {
            input_cursor: 1.into(),
            output: BTreeMap::from([(0, PreviewContent::Text(text.into()))]),
        }
    }

    #[test]
    fn encode_groups_resource_changes_in_one_update() {
        let updates = AgentUpdates {
            revision: 2.into(),
            messages: vec![MessageRecord::Consumed(1.into())],
            activity: QueryUpdate::Changed(AgentActivity::default()),
            events: vec![EventRecord::Deleted(3.into())],
            ..AgentUpdates::default()
        };

        let bytes = encode_updates(updates);
        let text = str::from_utf8(&bytes).unwrap();
        let data = text.strip_prefix("id: 2\nevent: update\ndata: ").unwrap();
        let updates: Value = serde_json::from_str(data).unwrap();

        assert_eq!(
            updates,
            json!({
                "messages": [
                    {
                        "status": "consumed",
                        "id": "1",
                    },
                ],
                "activity": {
                    "turn": "idle",
                    "interrupt_pending": false,
                },
                "events": [
                    {
                        "status": "deleted",
                        "cursor": "3",
                    },
                ],
            }),
        );
    }

    #[test]
    fn encode_appends_preview_text() {
        let mut subscription = PreviewStream {
            preview: Some(preview("Hello")),
            selected_block: None,
        };
        let preview = Some(preview("Hello there\n\nworld\n"));

        let bytes = subscription.encode(preview);
        let text = str::from_utf8(&bytes).unwrap();

        assert_eq!(
            text,
            concat!(
                "event: preview_block\ndata: {\"index\":0}\n\n",
                "event: preview_text\ndata:  there\ndata: \ndata: world\ndata: \n\n",
            )
        );
    }

    #[test]
    fn encode_selects_new_and_existing_blocks() {
        let mut subscription = PreviewStream {
            preview: Some(preview("Hello")),
            selected_block: Some(0),
        };
        let mut output = BTreeMap::from([
            (0, PreviewContent::Text("Hello".into())),
            (1, PreviewContent::Reasoning("Thinking".into())),
        ]);

        let bytes = subscription.encode(Some(Preview::Response {
            input_cursor: 1.into(),
            output: output.clone(),
        }));
        let text = str::from_utf8(&bytes).unwrap();
        assert!(
            text.contains("event: preview_block\ndata: {\"index\":1,\"type\":\"reasoning\"}\n\n")
        );
        assert!(text.contains("event: preview_text\ndata: Thinking\n\n"));

        output.insert(0, PreviewContent::Text("Hello again".into()));
        let bytes = subscription.encode(Some(Preview::Response {
            input_cursor: 1.into(),
            output,
        }));
        let text = str::from_utf8(&bytes).unwrap();
        assert!(text.contains("event: preview_block\ndata: {\"index\":0}\n\n"));
        assert!(text.contains("event: preview_text\ndata:  again\n\n"));
    }

    #[test]
    fn encode_replaces_preview_on_reconnect_or_changed_content() {
        for previous in [None, Some(preview("Different")), Some(preview("Hello"))] {
            let mut subscription = PreviewStream {
                preview: previous,
                selected_block: Some(0),
            };
            let current = preview("Hello\rworld");

            let bytes = subscription.encode(Some(current.clone()));
            let text = str::from_utf8(&bytes).unwrap();
            let json = serde_json::to_string(&ApiPreview::from(current)).unwrap();

            assert_eq!(text, format!("event: preview\ndata: {json}\n\n"));
            assert_eq!(subscription.selected_block, None);
        }
    }

    #[test]
    fn encode_replaces_response_output_with_compaction_and_back() {
        let mut subscription = PreviewStream {
            preview: Some(preview("Hello")),
            selected_block: Some(0),
        };

        let bytes = subscription.encode(Some(Preview::Compaction {
            input_cursor: 1.into(),
        }));
        let text = str::from_utf8(&bytes).unwrap();
        assert_eq!(
            text,
            "event: preview\ndata: {\"status\":\"compacting\",\"input_cursor\":\"1\"}\n\n",
        );
        assert_eq!(subscription.selected_block, None);

        let current = preview("New response");
        let bytes = subscription.encode(Some(current.clone()));
        let text = str::from_utf8(&bytes).unwrap();
        let json = serde_json::to_string(&ApiPreview::from(current)).unwrap();
        assert_eq!(text, format!("event: preview\ndata: {json}\n\n"));
        assert_eq!(subscription.selected_block, None);
    }

    #[test]
    fn encode_clears_preview() {
        let mut subscription = PreviewStream {
            preview: Some(preview("Hello")),
            selected_block: Some(0),
        };

        let bytes = subscription.encode(None);
        let text = str::from_utf8(&bytes).unwrap();

        assert_eq!(text, "event: preview\ndata: {\"status\":\"idle\"}\n\n");
        assert_eq!(subscription.preview, None);
        assert_eq!(subscription.selected_block, None);
    }
}
