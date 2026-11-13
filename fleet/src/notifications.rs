use crate::{
    AuthenticationError, CredentialStore, ErrorChain,
    web_push::{Subscription, WebPush, WebPushError},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use core::time::Duration;
use fleet_core::{JsonObject, RuntimeStore as _};
use fleet_store::{NotificationClaim, Store, StoreError};
use futures_util::future;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

const ATTEMPTS: u32 = 3;
const MAX_TITLE_CHARS: usize = 200;

/// Delivers notifications to registered devices.
#[derive(Clone)]
pub struct Notifications {
    web_push: Arc<WebPush>,
}

/// A device that receives notifications.
#[derive(Serialize, Deserialize)]
pub struct Device {
    label: String,
    #[serde(flatten)]
    transport: Transport,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
enum Transport {
    WebPush {
        origin: Option<String>,
        #[serde(flatten)]
        subscription: Subscription,
    },
}

#[derive(Serialize)]
struct Payload<'a> {
    kind: &'static str,
    agent: String,
    title: Option<&'a str>,
    role: &'a str,
}

impl Notifications {
    /// Opens notification delivery, creating its push key on first use.
    pub async fn open(directory: &Path) -> Result<Self, AuthenticationError> {
        let store = CredentialStore::open(directory)?;
        let key = if let Some(key) = store.get::<String>("web_push_key")? {
            URL_SAFE_NO_PAD
                .decode(key)
                .map_err(|error| AuthenticationError::source("could not decode push key", error))?
        } else {
            let key = WebPush::generate_key()
                .map_err(|_| AuthenticationError::Invalid("could not generate push key"))?;
            store
                .put("web_push_key".into(), &URL_SAFE_NO_PAD.encode(&key))
                .await?;
            key
        };
        Self::new(&key)
    }

    /// Creates notification delivery with a PKCS #8 push key.
    pub fn new(key: &[u8]) -> Result<Self, AuthenticationError> {
        let web_push = WebPush::new(key)
            .map_err(|error| AuthenticationError::Other("could not load push key", error))?;
        Ok(Self {
            web_push: Arc::new(web_push),
        })
    }

    /// Returns the Web Push application server key, encoded as base64url.
    pub fn web_push_key(&self) -> &str {
        self.web_push.public_key()
    }

    /// Sends a claimed notification to every device and completes it.
    ///
    /// Delivery to each device is best effort. Devices that no longer exist are removed.
    pub async fn deliver(&self, store: &Store, claim: NotificationClaim) -> Result<(), StoreError> {
        if let Some(agent) = store.agent(claim.agent()).await? {
            let title = fleet_kit::agent_title(&agent.extensions).map(|title| {
                title
                    .char_indices()
                    .nth(MAX_TITLE_CHARS)
                    .map_or(title, |(end, _)| title.get(..end).unwrap_or(title))
            });
            let payload = serde_json::to_vec(&Payload {
                kind: "turn_completed",
                agent: u128::from(claim.agent()).to_string(),
                title,
                role: agent.role.name.as_ref(),
            })
            .expect("notification payload is serializable");
            let topic = URL_SAFE_NO_PAD.encode(u128::from(claim.agent()).to_be_bytes());
            let devices = store.devices().await?;
            future::join_all(
                devices
                    .into_iter()
                    .map(|(id, device)| self.send(store, id, device, &topic, &payload)),
            )
            .await;
        }
        store.complete_notification(claim).await
    }

    async fn send(&self, store: &Store, id: u128, value: JsonObject, topic: &str, payload: &[u8]) {
        let device = match serde_json::from_value::<Device>(value.clone().into()) {
            Ok(device) => device,
            Err(error) => {
                eprintln!(
                    "Could not read notification device {id}: {}",
                    ErrorChain(&error)
                );
                return;
            }
        };
        let Transport::WebPush {
            origin,
            subscription,
        } = &device.transport;
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=ATTEMPTS {
            match self
                .web_push
                .send(subscription, origin.as_deref(), topic, payload)
                .await
            {
                Ok(()) => return,
                Err(WebPushError::Gone) => {
                    if let Err(error) = store.delete_device_if_unchanged(id, value).await {
                        eprintln!(
                            "Could not delete notification device {id}: {}",
                            ErrorChain(&error)
                        );
                    }
                    return;
                }
                Err(error) if error.is_temporary() && attempt < ATTEMPTS => {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
                Err(error) => {
                    eprintln!(
                        "Could not notify device {id} ({}): {}",
                        device.label,
                        ErrorChain(&error)
                    );
                    return;
                }
            }
        }
    }
}

impl Device {
    /// Creates a Web Push device. The origin identifies this server to the push service.
    pub fn web_push(
        label: String,
        origin: Option<String>,
        subscription: Subscription,
    ) -> Result<Self, &'static str> {
        subscription.validate()?;
        Ok(Self {
            label,
            transport: Transport::WebPush {
                origin: origin.filter(|origin| origin.starts_with("https://")),
                subscription,
            },
        })
    }

    /// Converts the device to its stored form.
    pub fn into_json(self) -> JsonObject {
        let serde_json::Value::Object(device) =
            serde_json::to_value(self).expect("device is serializable")
        else {
            unreachable!("device serializes to an object");
        };
        device
    }
}
