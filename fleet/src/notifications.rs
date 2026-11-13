use crate::{
    AuthenticationError, CredentialStore, ErrorChain,
    web_push::{Subscription, WebPush, WebPushError},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use core::time::Duration;
use fleet_core::{AgentId, EventPayload, JsonObject, ModelOutput, RoleName, RuntimeStore as _};
use fleet_store::{NotificationClaim, Store, StoreError};
use futures_util::{StreamExt as _, TryStreamExt as _, future};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::Path, sync::Arc};

const ATTEMPTS: u32 = 3;
const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 400;

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

struct Message {
    title: String,
    body: String,
    route: String,
    tag: String,
}

#[derive(Serialize)]
struct Payload<'a> {
    web_push: u16,
    notification: Notification<'a>,
}

#[derive(Serialize)]
struct Notification<'a> {
    title: &'a str,
    body: &'a str,
    navigate: &'a str,
    tag: &'a str,
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
            let events = store
                .events(claim.agent(), claim.cursor(), None)
                .take(2)
                .try_collect::<Vec<_>>()
                .await?;
            let output = events
                .into_iter()
                .find_map(|event| match event.payload {
                    EventPayload::ModelResponse(response) => Some(response.output),
                    _ => None,
                })
                .unwrap_or_default();
            let message = Message::new(
                claim.agent(),
                fleet_kit::agent_title(&agent.extensions),
                &agent.role.name,
                &output,
            );
            let topic = URL_SAFE_NO_PAD.encode(u128::from(claim.agent()).to_be_bytes());
            let devices = store.devices().await?;
            future::join_all(
                devices
                    .into_iter()
                    .map(|(id, device)| self.send(store, id, device, &topic, &message)),
            )
            .await;
        }
        store.complete_notification(claim).await
    }

    async fn send(
        &self,
        store: &Store,
        id: u128,
        value: JsonObject,
        topic: &str,
        message: &Message,
    ) {
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
        let payload = message.payload(origin.as_deref());
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=ATTEMPTS {
            match self
                .web_push
                .send(subscription, origin.as_deref(), topic, &payload)
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

impl Message {
    fn new(agent: AgentId, title: Option<&str>, role: &RoleName, output: &[ModelOutput]) -> Self {
        let id = u128::from(agent).to_string();
        let title = title.map_or_else(
            || {
                let suffix = id.get(id.len().saturating_sub(6)..).unwrap_or(&id);
                format!("{role} · {suffix}")
            },
            |title| crate::notifications::truncate(title, MAX_TITLE_CHARS).to_owned(),
        );
        let body = output
            .iter()
            .filter_map(|output| match output {
                ModelOutput::Text(text) => Some(text),
                _ => None,
            })
            .flat_map(|text| text.split(|c: char| c.is_whitespace() || c.is_control()))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            title,
            body: if body.is_empty() {
                "Finished".into()
            } else {
                body
            },
            route: format!("/#agent/{id}"),
            tag: format!("agent-{id}"),
        }
    }

    fn payload(&self, origin: Option<&str>) -> Vec<u8> {
        let navigate = format!("{}{}", origin.unwrap_or_default(), self.route);
        let mut chars = MAX_BODY_CHARS;
        loop {
            let payload = serde_json::to_vec(&Payload {
                web_push: 8030,
                notification: Notification {
                    title: &self.title,
                    body: &crate::notifications::excerpt(&self.body, chars),
                    navigate: &navigate,
                    tag: &self.tag,
                },
            })
            .expect("notification payload is serializable");
            let excess = payload.len().saturating_sub(WebPush::MAX_PAYLOAD);
            if excess == 0 || chars == 0 {
                return payload;
            }
            chars = chars.saturating_sub(excess);
        }
    }
}

fn excerpt(text: &str, chars: usize) -> Cow<'_, str> {
    if text.chars().nth(chars).is_none() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!(
            "{}…",
            crate::notifications::truncate(text, chars.saturating_sub(1))
        ))
    }
}

fn truncate(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(end, _)| text.get(..end).unwrap_or(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payload_shows_the_reply() {
        let message = Message::new(
            1_234_567.into(),
            None,
            &"coder".into(),
            &[
                ModelOutput::Text("Fixed the\nbuild.".into()),
                ModelOutput::Text("Tests\tpass.\u{7}".into()),
            ],
        );

        let payload = message.payload(Some("https://fleet.example"));

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap(),
            json!({
                "web_push": 8030,
                "notification": {
                    "title": "coder · 234567",
                    "body": "Fixed the build. Tests pass.",
                    "navigate": "https://fleet.example/#agent/1234567",
                    "tag": "agent-1234567"
                }
            })
        );
    }

    #[test]
    fn long_replies_are_shortened_to_fit() {
        let title = "題".repeat(300);
        let message = Message::new(
            1.into(),
            Some(&title),
            &"coder".into(),
            &[ModelOutput::Text("返信".repeat(1000))],
        );

        let payload = message.payload(None);

        assert!(payload.len() <= WebPush::MAX_PAYLOAD);
        let payload = serde_json::from_slice::<serde_json::Value>(&payload).unwrap();
        let notification = &payload["notification"];
        assert_eq!(notification["title"], "題".repeat(MAX_TITLE_CHARS));
        assert!(notification["body"].as_str().unwrap().ends_with('…'));
        assert_eq!(notification["navigate"], "/#agent/1");
    }
}
