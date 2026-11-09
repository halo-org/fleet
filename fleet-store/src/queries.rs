use crate::{
    Store, StoreError, Topic,
    records::{self, AgentRecord, Change, Records},
};
use fleet_core::{Agent, AgentId, MessageId};
use fleet_kit::{
    AgentActivity, AgentUpdates, EventDirection, EventQuery, EventRecord, MessageRecord, Preview,
    PreviewContent, Queries, QueriesErrorKind, QueryResponse, QueryUpdate, Revision,
    ToolCallRecord, TurnEndEvent, TurnStatus,
};
use futures_util::{Stream, stream};
use redb::{ReadOnlyTable, ReadableDatabase as _};

type ReadRecords = Records<ReadOnlyTable<&'static [u8], &'static [u8]>>;

impl Queries for Store {
    type Error = StoreError;

    async fn agent(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<QueryUpdate<Agent>>, StoreError> {
        self.query(agent_id, since, move |_, agent| {
            Ok(if since.is_none_or(|since| since < 1.into()) {
                QueryUpdate::Changed(agent.agent.into())
            } else {
                QueryUpdate::Unchanged
            })
        })
        .await
    }

    async fn messages(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<Vec<MessageRecord>>, StoreError> {
        self.query(agent_id, since, move |records, agent| {
            let mut messages = if let Some(since) = since {
                changes_since(records, agent.agent.id, records::MESSAGE, since.into())?
                    .into_iter()
                    .map(|change| {
                        records
                            .get::<records::MessageRecord>(&records::key(
                                records::MESSAGE,
                                agent.agent.id,
                                &change.suffix,
                            ))?
                            .ok_or(StoreError::Conflict("message index is invalid"))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?
            } else {
                records
                    .prefix::<u128>(&records::key(records::QUEUE, agent.agent.id, &[]))?
                    .map(|id| {
                        records
                            .get::<records::MessageRecord>(&records::key(
                                records::MESSAGE,
                                agent.agent.id,
                                &id?.to_be_bytes(),
                            ))?
                            .ok_or(StoreError::Conflict("message index is invalid"))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?
            };
            messages.sort_by_key(|record| record.position);
            Ok(messages.into_iter().map(Into::into).collect())
        })
        .await
    }

    async fn activity(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<QueryUpdate<AgentActivity>>, StoreError> {
        self.query(agent_id, since, move |_, agent| {
            Ok(
                if since.is_none_or(|since| u128::from(since) < agent.activity_revision) {
                    QueryUpdate::Changed(activity(&agent))
                } else {
                    QueryUpdate::Unchanged
                },
            )
        })
        .await
    }

    async fn tool_calls(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
    ) -> Result<QueryResponse<Vec<ToolCallRecord>>, StoreError> {
        self.query(agent_id, since, move |records, agent| {
            let mut calls = if let Some(since) = since {
                changes_since(records, agent.agent.id, records::TOOL_CALL, since.into())?
                    .into_iter()
                    .map(|change| {
                        records
                            .get::<records::ToolCallRecord>(&records::key(
                                records::TOOL_CALL,
                                agent.agent.id,
                                &change.suffix,
                            ))?
                            .ok_or(StoreError::Conflict("tool call index is invalid"))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?
            } else {
                records
                    .prefix::<String>(&records::key(records::PENDING_TOOL, agent.agent.id, &[]))?
                    .map(|id| {
                        records
                            .get::<records::ToolCallRecord>(&records::key(
                                records::TOOL_CALL,
                                agent.agent.id,
                                id?.as_bytes(),
                            ))?
                            .ok_or(StoreError::Conflict("tool call index is invalid"))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?
            };
            calls.sort_by(|a, b| a.call.id.cmp(&b.call.id));
            Ok(calls.into_iter().map(Into::into).collect())
        })
        .await
    }

    fn previews(
        &self,
        agent_id: AgentId,
    ) -> impl Stream<Item = Result<Option<Preview>, StoreError>> + Send + use<> {
        stream::try_unfold(
            (self.clone(), None),
            move |(store, mut previous)| async move {
                loop {
                    let notified = store
                        .notifications()
                        .listen(Topic::Preview(agent_id.into()));
                    let preview = store
                        .worker()
                        .call(move |worker| {
                            let transaction = worker.database.begin_read()?;
                            Records(transaction.open_table(records::RECORDS)?)
                                .agent(agent_id.into())?;
                            Ok(worker.previews.get(&agent_id.into()).and_then(|state| {
                                state.output.as_ref().map(|output| Preview {
                                    input_cursor: state.input_cursor.into(),
                                    output: output
                                        .iter()
                                        .map(|(index, content)| (*index, content.clone().into()))
                                        .collect(),
                                })
                            }))
                        })
                        .await?;
                    if previous.as_ref() != Some(&preview) {
                        previous = Some(preview.clone());
                        return Ok(Some((preview, (store, previous))));
                    }
                    notified.await;
                }
            },
        )
    }

    async fn events(
        &self,
        agent_id: AgentId,
        query: EventQuery,
    ) -> Result<QueryResponse<Vec<EventRecord>>, StoreError> {
        self.query(agent_id, query.since, move |records, agent| {
            let (lower, upper) = match query.direction {
                EventDirection::Forward => (query.from, query.until),
                EventDirection::Backward => (query.until, query.from),
            };
            let prefix = records::key(records::EVENT, agent.agent.id, &[]);
            let start = if let Some(lower) = lower {
                let Some(next) = u128::from(lower).checked_add(1) else {
                    return Ok(Vec::new());
                };
                records::key(records::EVENT, agent.agent.id, &next.to_be_bytes())
            } else {
                prefix.clone()
            };
            let end = upper.map_or_else(
                || records::prefix_end(&prefix),
                |upper| {
                    records::key(
                        records::EVENT,
                        agent.agent.id,
                        &u128::from(upper).to_be_bytes(),
                    )
                },
            );
            if start >= end {
                return Ok(Vec::new());
            }
            let events = records
                .range::<records::EventRecord>(&start, &end)?
                .filter_map(|record| match record {
                    Ok(record)
                        if query
                            .since
                            .is_none_or(|since| record.revision > since.into()) =>
                    {
                        Some(Ok(EventRecord::Settled(record.event.into())))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                });
            match query.direction {
                EventDirection::Forward => events.take(query.limit.get()).collect(),
                EventDirection::Backward => events.rev().take(query.limit.get()).collect(),
            }
        })
        .await
    }

    async fn updates(
        &self,
        agent_id: AgentId,
        after: Revision,
    ) -> Result<AgentUpdates, StoreError> {
        loop {
            let notified = self.notifications().listen(Topic::Agent(agent_id.into()));
            let response = self
                .query(agent_id, Some(after), move |records, agent| {
                    let Some(next) = u128::from(after).checked_add(1) else {
                        return Ok(None);
                    };
                    let start = records::key(records::CHANGE, agent.agent.id, &next.to_be_bytes());
                    let end =
                        records::prefix_end(&records::key(records::CHANGE, agent.agent.id, &[]));
                    let mut changes = records.range::<Change>(&start, &end)?;
                    let Some(first) = changes.next().transpose()? else {
                        return Ok(None);
                    };
                    let revision = first.revision;
                    let mut updates = AgentUpdates {
                        revision: revision.into(),
                        ..AgentUpdates::default()
                    };
                    let mut messages = Vec::new();
                    for change in core::iter::once(Ok(first)).chain(changes) {
                        let change = change?;
                        if change.revision != revision {
                            break;
                        }
                        let key = records::key(change.kind, agent.agent.id, &change.suffix);
                        match change.kind {
                            records::AGENT => {
                                updates.agent = QueryUpdate::Changed(agent.agent.clone().into());
                            }
                            records::ACTIVITY => {
                                updates.activity = QueryUpdate::Changed(activity(&agent));
                            }
                            records::MESSAGE => messages.push(
                                records
                                    .get::<records::MessageRecord>(&key)?
                                    .expect("indexed message exists"),
                            ),
                            records::TOOL_CALL => updates.tool_calls.push(
                                records
                                    .get::<records::ToolCallRecord>(&key)?
                                    .expect("indexed tool call exists")
                                    .into(),
                            ),
                            records::EVENT => updates.events.push(EventRecord::Settled(
                                records
                                    .get::<records::EventRecord>(&key)?
                                    .expect("indexed event exists")
                                    .event
                                    .into(),
                            )),
                            _ => return Err(StoreError::UnsupportedFormat),
                        }
                    }
                    messages.sort_by_key(|record| record.position);
                    updates.messages = messages.into_iter().map(Into::into).collect();
                    Ok(Some(updates))
                })
                .await?;
            if let Some(updates) = response.value {
                return Ok(updates);
            }
            notified.await;
        }
    }

    async fn wait_for_turn_end(
        &self,
        agent_id: AgentId,
        message_id: MessageId,
    ) -> Result<TurnEndEvent, StoreError> {
        loop {
            let notified = self.notifications().listen(Topic::Agent(agent_id.into()));
            let result = self
                .query(agent_id, None, move |records, agent| {
                    let message = records
                        .get::<records::MessageRecord>(&records::key(
                            records::MESSAGE,
                            agent.agent.id,
                            &u128::from(message_id).to_be_bytes(),
                        ))?
                        .ok_or(StoreError::MessageNotFound)?;
                    let Some(cursor) = message.input_cursor else {
                        return Ok(None);
                    };
                    let start =
                        records::key(records::TURN_END, agent.agent.id, &cursor.to_be_bytes());
                    let end =
                        records::prefix_end(&records::key(records::TURN_END, agent.agent.id, &[]));
                    Ok(records
                        .range::<(u128, crate::data::TurnEnd)>(&start, &end)?
                        .next()
                        .transpose()?
                        .map(|(cursor, payload)| TurnEndEvent {
                            cursor: cursor.into(),
                            payload: payload.into(),
                        }))
                })
                .await?;
            if let Some(turn_end) = result.value {
                return Ok(turn_end);
            }
            notified.await;
        }
    }
}

impl Store {
    async fn query<T: Send + 'static>(
        &self,
        agent_id: AgentId,
        since: Option<Revision>,
        read: impl FnOnce(&ReadRecords, AgentRecord) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<QueryResponse<T>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let agent = records.agent(agent_id.into())?;
                if since.is_some_and(|since| u128::from(since) > agent.revision) {
                    return Err(StoreError::InvalidRevision);
                }
                let revision = agent.revision.into();
                Ok(QueryResponse {
                    revision,
                    value: read(&records, agent)?,
                })
            })
            .await
    }
}

fn changes_since(
    records: &ReadRecords,
    agent_id: u128,
    kind: u8,
    since: u128,
) -> Result<Vec<Change>, StoreError> {
    let Some(next) = since.checked_add(1) else {
        return Ok(Vec::new());
    };
    let start = records::key(
        records::RESOURCE_CHANGE,
        agent_id,
        &[&[kind], next.to_be_bytes().as_slice()].concat(),
    );
    let end = records::prefix_end(&records::key(records::RESOURCE_CHANGE, agent_id, &[kind]));
    records.range(&start, &end)?.collect()
}

const fn activity(agent: &AgentRecord) -> AgentActivity {
    AgentActivity {
        turn: if agent.turn_active {
            TurnStatus::Active
        } else {
            TurnStatus::Idle
        },
        interrupt_pending: agent.interrupt_intent,
    }
}

impl From<records::MessageRecord> for MessageRecord {
    fn from(record: records::MessageRecord) -> Self {
        match record.message {
            Some(message) => Self::Pending {
                position: record.position,
                message: message.into(),
            },
            None => Self::Consumed(record.id.into()),
        }
    }
}

impl From<records::ToolCallRecord> for ToolCallRecord {
    fn from(record: records::ToolCallRecord) -> Self {
        if record.resolved_at.is_some() {
            Self::Resolved(record.call.id.into())
        } else {
            Self::Pending {
                id: record.call.id.into(),
                tool: record.call.tool.into(),
                is_async: record.call.is_async,
            }
        }
    }
}

impl From<crate::preview::PreviewContent> for PreviewContent {
    fn from(content: crate::preview::PreviewContent) -> Self {
        match content {
            crate::preview::PreviewContent::Text(text) => Self::Text(text),
            crate::preview::PreviewContent::Reasoning(text) => Self::Reasoning(text),
            crate::preview::PreviewContent::ToolCall { id, tool, input } => {
                Self::ToolCall { id, tool, input }
            }
            crate::preview::PreviewContent::ProviderToolCall {
                call_id,
                name,
                content,
            } => Self::ProviderToolCall {
                call_id,
                name,
                content,
            },
        }
    }
}

impl fleet_core::Error<QueriesErrorKind> for StoreError {
    fn kind(&self) -> QueriesErrorKind {
        match self {
            Self::AgentNotFound => QueriesErrorKind::AgentNotFound,
            Self::MessageNotFound => QueriesErrorKind::MessageNotFound,
            Self::InvalidRevision => QueriesErrorKind::InvalidRevision,
            _ => QueriesErrorKind::Other,
        }
    }
}
