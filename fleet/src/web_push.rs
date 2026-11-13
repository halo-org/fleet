use aws_lc_rs::{
    aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    agreement::{self, ECDH_P256, PrivateKey, UnparsedPublicKey},
    error::Unspecified,
    hkdf::{self, HKDF_SHA256, Salt},
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use core::{
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use url::{Host, Url};

const RECORD_SIZE: u32 = 4096;
const MESSAGE_SIZE: usize = 2048;
const HEADER_SIZE: usize = 86;
const TAG_SIZE: usize = 16;
const TTL: Duration = Duration::from_hours(12);

/// Sends encrypted messages to browser push services.
pub struct WebPush {
    client: reqwest::Client,
    key: EncodingKey,
    public_key: String,
}

/// A browser push subscription.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// The push service URL for this browser.
    pub endpoint: String,
    /// The browser's message encryption keys.
    pub keys: Keys,
}

/// The browser's message encryption keys, encoded as base64url.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    /// The browser's P-256 public key.
    pub p256dh: String,
    /// The browser's authentication secret.
    pub auth: String,
}

/// A failure to send a push message.
#[derive(Debug)]
pub enum WebPushError {
    /// The subscription has expired or been removed.
    Gone,
    /// The push service is unavailable or limiting requests.
    Unavailable(StatusCode),
    /// The push service rejected the message.
    Rejected(StatusCode),
    /// The push service could not be reached.
    Transport(reqwest::Error),
    /// The subscription is invalid.
    Invalid(&'static str),
    /// The payload is larger than [`WebPush::MAX_PAYLOAD`].
    TooLarge,
    /// The message could not be encrypted or signed.
    Encryption,
}

#[derive(Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sub: Option<&'a str>,
}

struct Length(usize);

impl WebPush {
    /// The largest payload in bytes. Every message is padded to the same size.
    pub const MAX_PAYLOAD: usize = MESSAGE_SIZE - HEADER_SIZE - 1 - TAG_SIZE;

    /// Generates a PKCS #8 application server key.
    pub fn generate_key() -> Result<Vec<u8>, Unspecified> {
        Ok(
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())?
                .as_ref()
                .to_vec(),
        )
    }

    /// Creates a sender with a PKCS #8 application server key.
    pub fn new(pkcs8: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client,
            key: EncodingKey::from_ec_der(pkcs8),
            public_key: URL_SAFE_NO_PAD.encode(key.public_key()),
        })
    }

    /// Returns the application server public key, encoded as base64url.
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Sends a message that replaces any undelivered message with the same topic.
    ///
    /// The subject identifies the sender to the push service.
    pub async fn send(
        &self,
        subscription: &Subscription,
        subject: Option<&str>,
        topic: &str,
        payload: &[u8],
    ) -> Result<(), WebPushError> {
        let endpoint = endpoint(&subscription.endpoint)?;
        let body = message(subscription, payload)?;
        let authorization = self.authorization(&endpoint, subject)?;
        let response = self
            .client
            .post(endpoint)
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_ENCODING, "aes128gcm")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("ttl", TTL.as_secs())
            .header("urgency", "normal")
            .header("topic", topic)
            .body(body)
            .send()
            .await
            .map_err(WebPushError::Transport)?;
        match response.status() {
            status if status.is_success() => Ok(()),
            StatusCode::NOT_FOUND | StatusCode::GONE => Err(WebPushError::Gone),
            status if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                Err(WebPushError::Unavailable(status))
            }
            status => Err(WebPushError::Rejected(status)),
        }
    }

    fn authorization(&self, endpoint: &Url, subject: Option<&str>) -> Result<String, WebPushError> {
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_add(TTL)
            .as_secs();
        let claims = Claims {
            aud: &endpoint.origin().ascii_serialization(),
            exp: expires,
            sub: subject,
        };
        let token = jsonwebtoken::encode(&Header::new(Algorithm::ES256), &claims, &self.key)
            .map_err(|_| WebPushError::Encryption)?;
        Ok(format!("vapid t={token}, k={}", self.public_key))
    }
}

impl Subscription {
    /// Checks that the subscription can be used to send messages.
    pub fn validate(&self) -> Result<(), &'static str> {
        endpoint(&self.endpoint).map_err(|_| "endpoint must be a public HTTPS URL")?;
        decode::<65>(&self.keys.p256dh)
            .filter(|key| key.first() == Some(&4))
            .ok_or("p256dh must be an uncompressed P-256 public key")?;
        decode::<16>(&self.keys.auth).ok_or("auth must be a 16-byte secret")?;
        Ok(())
    }
}

impl WebPushError {
    /// Returns whether sending again later may succeed.
    pub const fn is_temporary(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Transport(_))
    }
}

impl fmt::Display for WebPushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone => f.write_str("push subscription is gone"),
            Self::Unavailable(status) => write!(f, "push service returned HTTP {status}"),
            Self::Rejected(status) => {
                write!(f, "push service rejected the message with HTTP {status}")
            }
            Self::Transport(_) => f.write_str("could not reach push service"),
            Self::Invalid(message) => f.write_str(message),
            Self::TooLarge => f.write_str("push message is too large"),
            Self::Encryption => f.write_str("could not encrypt or sign push message"),
        }
    }
}

impl Error for WebPushError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

fn endpoint(value: &str) -> Result<Url, WebPushError> {
    let url = Url::parse(value).map_err(|_| WebPushError::Invalid("invalid endpoint"))?;
    let public = match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain != "localhost" && !domain.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => is_public(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => is_public(IpAddr::V6(address)),
        None => false,
    };
    if url.scheme() != "https" || !public || !url.username().is_empty() || url.password().is_some()
    {
        return Err(WebPushError::Invalid("endpoint is not a public HTTPS URL"));
    }
    Ok(url)
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or_else(|| is_public_v6(address), is_public_v4),
    }
}

const fn is_public_v4(address: Ipv4Addr) -> bool {
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast())
}

const fn is_public_v6(address: Ipv6Addr) -> bool {
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_unique_local()
        || address.is_unicast_link_local())
}

fn decode<const N: usize>(value: &str) -> Option<[u8; N]> {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .ok()?
        .try_into()
        .ok()
}

fn message(subscription: &Subscription, payload: &[u8]) -> Result<Vec<u8>, WebPushError> {
    let ua_public = decode::<65>(&subscription.keys.p256dh)
        .ok_or(WebPushError::Invalid("invalid p256dh key"))?;
    let auth = decode::<16>(&subscription.keys.auth)
        .ok_or(WebPushError::Invalid("invalid auth secret"))?;
    let padding = WebPush::MAX_PAYLOAD
        .checked_sub(payload.len())
        .ok_or(WebPushError::TooLarge)?;
    let mut salt = [0; 16];
    aws_lc_rs::rand::fill(&mut salt)
        .and_then(|()| PrivateKey::generate(&ECDH_P256))
        .and_then(|key| encrypt(&ua_public, &auth, &key, salt, payload, padding))
        .map_err(|_| WebPushError::Encryption)
}

fn encrypt(
    ua_public: &[u8; 65],
    auth: &[u8; 16],
    as_private: &PrivateKey,
    salt: [u8; 16],
    payload: &[u8],
    padding: usize,
) -> Result<Vec<u8>, Unspecified> {
    let as_public = as_private.compute_public_key()?;
    let ikm = agreement::agree(
        as_private,
        UnparsedPublicKey::new(&ECDH_P256, ua_public),
        Unspecified,
        |secret| {
            let mut ikm = [0; 32];
            Salt::new(HKDF_SHA256, auth)
                .extract(secret)
                .expand(
                    &[b"WebPush: info\0", ua_public, as_public.as_ref()],
                    Length(ikm.len()),
                )?
                .fill(&mut ikm)?;
            Ok(ikm)
        },
    )?;
    let prk = Salt::new(HKDF_SHA256, &salt).extract(&ikm);
    let mut cek = [0; 16];
    prk.expand(&[b"Content-Encoding: aes128gcm\0"], Length(cek.len()))?
        .fill(&mut cek)?;
    let mut nonce = [0; 12];
    prk.expand(&[b"Content-Encoding: nonce\0"], Length(nonce.len()))?
        .fill(&mut nonce)?;

    let mut record = payload.to_vec();
    record.push(2);
    record.resize(record.len().saturating_add(padding), 0);
    LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek)?).seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut record,
    )?;
    if u32::try_from(record.len()).map_or(true, |length| length > RECORD_SIZE) {
        return Err(Unspecified);
    }

    let mut body = salt.to_vec();
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(u8::try_from(as_public.as_ref().len()).map_err(|_| Unspecified)?);
    body.extend_from_slice(as_public.as_ref());
    body.extend_from_slice(&record);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::assert_matches;

    fn base64(value: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(value).unwrap()
    }

    #[test]
    fn encrypts_rfc_8291_example() {
        let as_private = PrivateKey::from_private_key(
            &ECDH_P256,
            &base64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        )
        .unwrap();
        let body = encrypt(
            &decode("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap(),
            &decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap(),
            &as_private,
            decode("DGv6ra1nlYgDCS1FRnbzlw").unwrap(),
            b"When I grow up, I want to be a watermelon",
            0,
        )
        .unwrap();

        let header = base64(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let ciphertext = base64(
            "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ",
        );
        assert_eq!(body, [header, ciphertext].concat());
    }

    #[test]
    fn messages_are_padded_to_one_size() {
        let subscription = Subscription {
            endpoint: "https://push.example/send/abc".into(),
            keys: Keys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            },
        };

        let short = message(&subscription, b"{}").unwrap();
        let long = message(&subscription, &[b'a'; WebPush::MAX_PAYLOAD]).unwrap();
        let error = message(&subscription, &[b'a'; WebPush::MAX_PAYLOAD + 1]).unwrap_err();

        assert_eq!(short.len(), MESSAGE_SIZE);
        assert_eq!(long.len(), MESSAGE_SIZE);
        assert_matches!(error, WebPushError::TooLarge);
    }

    #[test]
    fn authorization_is_signed_for_the_push_service() {
        let web_push = WebPush::new(&WebPush::generate_key().unwrap()).unwrap();
        let endpoint = Url::parse("https://push.example/send/abc").unwrap();
        let authorization = web_push
            .authorization(&endpoint, Some("https://fleet.example"))
            .unwrap();

        let (token, key) = authorization
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(key, web_push.public_key());
        let key = base64(key);
        let key = jsonwebtoken::DecodingKey::from_ec_components(
            &URL_SAFE_NO_PAD.encode(&key[1..33]),
            &URL_SAFE_NO_PAD.encode(&key[33..]),
        )
        .unwrap();
        let mut validation = jsonwebtoken::Validation::new(Algorithm::ES256);
        validation.set_audience(&["https://push.example"]);
        let claims = jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
            .unwrap()
            .claims;
        assert_eq!(claims["sub"], "https://fleet.example");
    }

    #[test]
    fn endpoints_must_be_public_https_urls() {
        for endpoint in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://web.push.apple.com/abc",
            "https://8.8.8.8/push",
            "https://[2001:4860:4860::8888]/push",
        ] {
            assert!(super::endpoint(endpoint).is_ok(), "{endpoint}");
        }
        for endpoint in [
            "http://fcm.googleapis.com/fcm/send/abc",
            "https://localhost/push",
            "https://push.localhost./push",
            "https://user@push.example/push",
            "https://127.0.0.1/push",
            "https://10.0.0.1/push",
            "https://169.254.169.254/latest",
            "https://[::1]/push",
            "https://[fd00::1]/push",
            "https://[::ffff:192.168.0.1]/push",
            "not a url",
        ] {
            assert!(super::endpoint(endpoint).is_err(), "{endpoint}");
        }
    }
}
