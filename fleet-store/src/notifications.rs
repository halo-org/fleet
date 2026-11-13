use crate::{
    Store, StoreError, Topic,
    notification_records::{DEVICE, NOTIFICATION, NotificationRecord},
    records::{self, AgentRecord, Records},
};
use fleet_core::{AgentId, JsonObject};
use redb::ReadableDatabase as _;

/// A claim on an agent's notification.
///
/// The claim lasts until its lease expires or the notification is completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationClaim {
    agent: AgentId,
    cursor: u128,
    claim: u64,
}

impl NotificationClaim {
    /// Returns the agent that finished its turn.
    #[must_use]
    pub const fn agent(&self) -> AgentId {
        self.agent
    }
}

impl Store {
    /// Claims a notification whose lease has expired, renewing its lease until `now + lease`.
    ///
    /// Notifications are created when a turn completes with no queued messages. A notification is
    /// dropped instead of claimed once its agent records another event or queues a message.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the claim.
    pub async fn claim_notification(
        &self,
        now: u64,
        lease: u64,
    ) -> Result<Option<NotificationClaim>, StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let claim = {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let pending = records
                        .prefix::<NotificationRecord>(&[NOTIFICATION])?
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut claim = None;
                    for record in pending {
                        if record.lease_until > now {
                            continue;
                        }
                        let key = records::key(NOTIFICATION, record.agent, &[]);
                        let current = records
                            .get::<AgentRecord>(&records::key(records::AGENT, record.agent, &[]))?
                            .is_some_and(|agent| {
                                agent.latest_event == Some(record.cursor)
                                    && agent.pending_messages == 0
                            });
                        if !current {
                            records.remove(&key)?;
                            continue;
                        }
                        let record = NotificationRecord {
                            lease_until: now.saturating_add(lease),
                            claim: record
                                .claim
                                .checked_add(1)
                                .expect("notification claims do not overflow"),
                            ..record
                        };
                        records.put(&key, &record)?;
                        claim = Some(NotificationClaim {
                            agent: record.agent.into(),
                            cursor: record.cursor,
                            claim: record.claim,
                        });
                        break;
                    }
                    claim
                };
                transaction.commit()?;
                Ok(claim)
            })
            .await
    }

    /// Deletes a delivered notification unless it was claimed again or replaced.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the deletion.
    pub async fn complete_notification(&self, claim: NotificationClaim) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                let completed = {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let key = records::key(NOTIFICATION, claim.agent.into(), &[]);
                    let completed =
                        records
                            .get::<NotificationRecord>(&key)?
                            .is_some_and(|record| {
                                record.cursor == claim.cursor && record.claim == claim.claim
                            });
                    if completed {
                        records.remove(&key)?;
                    }
                    completed
                };
                transaction.commit()?;
                if completed {
                    worker.listeners.notify(Topic::Notifications);
                }
                Ok(())
            })
            .await
    }

    /// Reads when the next notification can be claimed, in Unix milliseconds.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn next_notification(&self) -> Result<Option<u64>, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                let records = Records(transaction.open_table(records::RECORDS)?);
                let leases = records
                    .prefix::<NotificationRecord>(&[NOTIFICATION])?
                    .map(|record| record.map(|record| record.lease_until))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(leases.into_iter().min())
            })
            .await
    }

    /// Registers immediately for notification changes. Register before reading
    /// `next_notification`.
    pub fn notification_changed(&self) -> impl Future<Output = ()> + Send + use<> {
        self.listeners().listen(Topic::Notifications)
    }

    /// Creates or replaces a device that receives notifications.
    ///
    /// The store does not interpret device data.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the device.
    pub async fn put_device(&self, id: u128, device: JsonObject) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                Records(transaction.open_table(records::RECORDS)?)
                    .put(&records::key(DEVICE, id, &[]), &(id, device))?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    /// Deletes a device if it exists.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the deletion.
    pub async fn delete_device(&self, id: u128) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                Records(transaction.open_table(records::RECORDS)?).remove(&records::key(
                    DEVICE,
                    id,
                    &[],
                ))?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    /// Deletes a device unless it was replaced after the caller read `device`.
    ///
    /// # Errors
    /// Returns an error if the store cannot commit the deletion.
    pub async fn delete_device_if_unchanged(
        &self,
        id: u128,
        device: JsonObject,
    ) -> Result<(), StoreError> {
        self.worker()
            .call(move |worker| {
                let transaction = worker.database.begin_write()?;
                {
                    let mut records = Records(transaction.open_table(records::RECORDS)?);
                    let key = records::key(DEVICE, id, &[]);
                    if records
                        .get::<(u128, JsonObject)>(&key)?
                        .is_some_and(|(_, current)| current == device)
                    {
                        records.remove(&key)?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    /// Lists devices by ID.
    ///
    /// # Errors
    /// Returns an error if the store cannot be read.
    pub async fn devices(&self) -> Result<Vec<(u128, JsonObject)>, StoreError> {
        self.worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                Records(transaction.open_table(records::RECORDS)?)
                    .prefix::<(u128, JsonObject)>(&[DEVICE])?
                    .collect()
            })
            .await
    }
}
