use crate::{
    Fleet,
    api::{self, ApiError, ResponseBody},
    notifications::Device,
    web_push::{Keys, Subscription},
};
use core::error::Error;
use hyper::{
    Method, Response, StatusCode,
    body::{Body, Bytes},
    header::{self, HeaderValue},
    http::request::Parts,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct NotificationsResponse<'a> {
    web_push: WebPushResponse<'a>,
}

#[derive(Serialize)]
struct WebPushResponse<'a> {
    public_key: &'a str,
}

#[derive(Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
enum PutDeviceRequest {
    WebPush {
        label: String,
        endpoint: String,
        keys: Keys,
    },
}

pub async fn route<B>(
    fleet: &Fleet,
    parts: &Parts,
    segments: &[&str],
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let notifications = fleet.notifications().ok_or(ApiError::NotFound)?;
    if parts.uri.query().is_some() {
        return Err(ApiError::BadRequest(
            "Notification APIs do not accept query parameters".into(),
        ));
    }
    let mut response = match (&parts.method, segments) {
        (&Method::GET, ["", "notifications"]) => api::json_response(
            StatusCode::OK,
            &NotificationsResponse {
                web_push: WebPushResponse {
                    public_key: notifications.web_push_key(),
                },
            },
        ),
        (&Method::PUT, ["", "notifications", "devices", id]) => {
            let id = api::parse_id(id)?;
            let PutDeviceRequest::WebPush {
                label,
                endpoint,
                keys,
            } = api::read_json(&parts.headers, body).await?;
            let origin = parts
                .headers
                .get(header::ORIGIN)
                .and_then(|origin| origin.to_str().ok())
                .map(String::from);
            let device = Device::web_push(label, origin, Subscription { endpoint, keys })
                .map_err(|message| ApiError::BadRequest(message.into()))?;
            fleet
                .put_device(id, device.into_json())
                .await
                .map_err(ApiError::Fleet)?;
            api::empty_response(StatusCode::NO_CONTENT)
        }
        (&Method::DELETE, ["", "notifications", "devices", id]) => {
            fleet
                .delete_device(api::parse_id(id)?)
                .await
                .map_err(ApiError::Fleet)?;
            api::empty_response(StatusCode::NO_CONTENT)
        }
        (_, ["", "notifications"]) => return Err(ApiError::MethodNotAllowed("GET")),
        (_, ["", "notifications", "devices", _]) => {
            return Err(ApiError::MethodNotAllowed("PUT, DELETE"));
        }
        _ => return Err(ApiError::NotFound),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Notifications, web_push::WebPush};
    use fleet_store::Store;
    use http_body_util::{BodyExt as _, Full};
    use hyper::Request;
    use serde_json::{Value, json};

    const P256DH: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";

    async fn call(fleet: &Fleet, method: &str, path: &str, body: &Value) -> Response<ResponseBody> {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "https://fleet.example")
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        crate::api::handle(fleet.clone(), request).await.unwrap()
    }

    #[test]
    fn devices_are_registered_and_removed() {
        let store = Store::memory().unwrap();
        let notifications = Notifications::new(&WebPush::generate_key().unwrap()).unwrap();
        let fleet = Fleet::new(store.clone(), None).with_notifications(notifications.clone());
        let device = json!({
            "label": "Firefox on Linux",
            "transport": "web_push",
            "endpoint": "https://push.example/send/abc",
            "keys": {"p256dh": P256DH, "auth": "BTBZMqHH6r4Tts7J_aSIgg"},
        });

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response = call(&fleet, "GET", "/notifications", &json!({})).await;
                let body = response.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"web_push": {"public_key": notifications.web_push_key()}}),
                );

                let response = call(&fleet, "PUT", "/notifications/devices/7", &device).await;
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                assert_eq!(
                    Value::Object(store.devices().await.unwrap().pop().unwrap().1),
                    json!({
                        "label": "Firefox on Linux",
                        "transport": "web_push",
                        "origin": "https://fleet.example",
                        "endpoint": "https://push.example/send/abc",
                        "keys": {"p256dh": P256DH, "auth": "BTBZMqHH6r4Tts7J_aSIgg"},
                    }),
                );

                for (field, value) in [
                    ("endpoint", json!("https://127.0.0.1/send/abc")),
                    ("keys", json!({"p256dh": P256DH, "auth": "c2hvcnQ"})),
                    ("extra", json!(true)),
                ] {
                    let mut device = device.clone();
                    device[field] = value;
                    let response = call(&fleet, "PUT", "/notifications/devices/8", &device).await;
                    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{field}");
                }

                for _ in 0..2 {
                    let response =
                        call(&fleet, "DELETE", "/notifications/devices/7", &json!({})).await;
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                }
                assert_eq!(store.devices().await.unwrap(), []);
            });
    }
}
