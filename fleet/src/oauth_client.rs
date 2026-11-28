use crate::{AuthenticationError, oauth_discovery::ClientSupport};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::{Host, Url};

/// How Fleet identifies itself as an automatic OAuth client.
#[derive(Clone)]
pub struct AutomaticClient {
    public_url: String,
    redirect_uri: Url,
    shared_document: Option<String>,
}

/// How Fleet identifies itself to an authorization server.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientIdentity {
    /// The client ID is the URL of a client ID metadata document.
    Document(String),
    /// The client registers dynamically at this endpoint.
    Registration(String),
}

/// A dynamic client registration response, defined by RFC 7591.
#[derive(Deserialize)]
pub struct RegisteredClient {
    client_id: String,
    client_secret: Option<String>,
    token_endpoint_auth_method: Option<String>,
    redirect_uris: Option<Vec<String>>,
}

impl AutomaticClient {
    /// Creates a client for a public URL without a trailing `/`, with an optional shared
    /// metadata document.
    pub fn new(public_url: String, shared_document: Option<&Url>) -> Self {
        let redirect_uri = format!("{public_url}/oauth/callback")
            .parse()
            .expect("public URL forms a redirect URI");
        Self {
            public_url,
            redirect_uri,
            shared_document: shared_document.map(|url| url.as_str().to_owned()),
        }
    }

    /// Returns the redirect URI at which Fleet receives callbacks.
    pub const fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    /// Returns this installation's metadata document, which requires an HTTPS public URL.
    pub fn document(&self) -> Option<Value> {
        let mut document = self.metadata();
        document.insert("client_id".into(), self.document_url()?.into());
        Some(document.into())
    }

    /// Returns the body of a dynamic client registration request.
    pub fn registration(&self) -> Value {
        let application_type = if loopback(&self.redirect_uri) {
            "native"
        } else {
            "web"
        };
        let mut registration = self.metadata();
        registration.insert("application_type".into(), application_type.into());
        registration.into()
    }

    /// Chooses how to identify Fleet to an authorization server.
    ///
    /// This installation's metadata document comes first, then a shared one for a
    /// callback on 127.0.0.1, then dynamic registration.
    ///
    /// # Errors
    ///
    /// Returns an error if the server supports no method that applies.
    pub fn identity(&self, support: &ClientSupport) -> Result<ClientIdentity, AuthenticationError> {
        let shared = self
            .shared_document
            .clone()
            .filter(|_| shared_callback(&self.redirect_uri));
        if support.metadata_documents
            && let Some(url) = self.document_url().or(shared)
        {
            return Ok(ClientIdentity::Document(url));
        }
        match &support.registration_endpoint {
            Some(endpoint) => Ok(ClientIdentity::Registration(endpoint.clone())),
            None if support.metadata_documents => Err(AuthenticationError::Invalid(
                "authorization server requires a client metadata document, which needs an HTTPS public_url, or oauth_client_metadata_url and a public_url of http://127.0.0.1:<port>",
            )),
            None => Err(AuthenticationError::Invalid(
                "authorization server supports neither client metadata documents nor dynamic client registration",
            )),
        }
    }

    fn document_url(&self) -> Option<String> {
        self.public_url
            .starts_with("https://")
            .then(|| format!("{}/oauth/client.json", self.public_url))
    }

    fn metadata(&self) -> Map<String, Value> {
        [
            ("client_name", json!("Fleet")),
            ("redirect_uris", json!([self.redirect_uri])),
            (
                "grant_types",
                json!(["authorization_code", "refresh_token"]),
            ),
            ("response_types", json!(["code"])),
            ("token_endpoint_auth_method", json!("none")),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
    }
}

impl RegisteredClient {
    /// Returns the client ID of a public client registered for the redirect URI.
    ///
    /// # Errors
    ///
    /// Returns an error for a confidential client, redirect URIs without Fleet's own or
    /// an empty client ID.
    pub fn client_id(self, redirect_uri: &Url) -> Result<String, AuthenticationError> {
        if self.client_secret.is_some()
            || self
                .token_endpoint_auth_method
                .is_some_and(|method| method != "none")
        {
            return Err(AuthenticationError::Invalid(
                "authorization server registered a confidential client, which Fleet does not support",
            ));
        }
        if self
            .redirect_uris
            .is_some_and(|uris| !uris.iter().any(|uri| uri == redirect_uri.as_str()))
        {
            return Err(AuthenticationError::Invalid(
                "authorization server registered other redirect URIs",
            ));
        }
        if self.client_id.is_empty() {
            return Err(AuthenticationError::Invalid(
                "client registration returned no client ID",
            ));
        }
        Ok(self.client_id)
    }
}

/// Returns whether a URL is the callback that shared metadata documents list.
fn shared_callback(url: &Url) -> bool {
    url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.path() == "/oauth/callback"
}

fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::assert_matches;

    const SHARED: &str = "https://fleet.example.org/oauth/client.json";

    fn client(public_url: &str, shared: bool) -> AutomaticClient {
        let shared = shared.then(|| SHARED.parse().unwrap());
        AutomaticClient::new(public_url.into(), shared.as_ref())
    }

    #[test]
    fn documents_describe_fleet_at_its_https_public_url() {
        let document = client("https://fleet.example.com/api", false).document();

        assert_eq!(
            document.unwrap(),
            json!({
                "client_id": "https://fleet.example.com/api/oauth/client.json",
                "client_name": "Fleet",
                "redirect_uris": ["https://fleet.example.com/api/oauth/callback"],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
            })
        );
        assert_eq!(client("http://127.0.0.1:1337", true).document(), None);
        assert_eq!(client("http://fleet.example.com", false).document(), None);
    }

    #[test]
    fn identities_prefer_documents_then_registration() {
        let documents = |registration: Option<&str>| ClientSupport {
            metadata_documents: true,
            registration_endpoint: registration.map(Into::into),
        };
        let registration = ClientSupport {
            metadata_documents: false,
            registration_endpoint: Some("https://auth.example.com/register".into()),
        };
        let document = |url: &str| ClientIdentity::Document(url.into());
        let register = || ClientIdentity::Registration("https://auth.example.com/register".into());
        let https = "https://fleet.example.com";
        let own = "https://fleet.example.com/oauth/client.json";
        let loopback = "http://127.0.0.1:1337";

        for (public_url, shared, support, expected) in [
            (https, false, documents(None), document(own)),
            (https, true, documents(None), document(own)),
            (https, false, registration.clone(), register()),
            (loopback, true, documents(None), document(SHARED)),
            (loopback, true, registration, register()),
            (
                loopback,
                false,
                documents(Some("https://auth.example.com/register")),
                register(),
            ),
            (
                "http://localhost:1337",
                true,
                documents(Some("https://auth.example.com/register")),
                register(),
            ),
            (
                "http://127.0.0.1:1337/api",
                true,
                documents(Some("https://auth.example.com/register")),
                register(),
            ),
        ] {
            let identity = client(public_url, shared).identity(&support).unwrap();
            assert_eq!(identity, expected, "{public_url} {shared}");
        }

        for (public_url, support, message) in [
            (
                loopback,
                documents(None),
                "authorization server requires a client metadata document, which needs an HTTPS public_url, or oauth_client_metadata_url and a public_url of http://127.0.0.1:<port>",
            ),
            (
                https,
                ClientSupport::default(),
                "authorization server supports neither client metadata documents nor dynamic client registration",
            ),
        ] {
            let error = client(public_url, false).identity(&support).unwrap_err();
            assert_matches!(error, AuthenticationError::Invalid(error) if error == message);
        }
    }

    #[test]
    fn registrations_are_native_on_loopback() {
        let registration = client("http://127.0.0.1:1337", false).registration();

        assert_eq!(
            registration,
            json!({
                "client_name": "Fleet",
                "redirect_uris": ["http://127.0.0.1:1337/oauth/callback"],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                "application_type": "native",
            })
        );
        for (public_url, application_type) in [
            ("http://localhost:1337", "native"),
            ("http://[::1]:1337", "native"),
            ("https://fleet.example.com", "web"),
            ("http://fleet.internal", "web"),
        ] {
            let registration = client(public_url, false).registration();
            assert_eq!(
                registration["application_type"], application_type,
                "{public_url}"
            );
        }
    }

    #[test]
    fn registrations_must_issue_public_clients() {
        let redirect_uri = "http://127.0.0.1:1337/oauth/callback".parse().unwrap();
        let client = |value| {
            serde_json::from_value::<RegisteredClient>(value)
                .unwrap()
                .client_id(&redirect_uri)
        };

        assert_eq!(client(json!({ "client_id": "issued" })).unwrap(), "issued");
        assert_eq!(
            client(json!({
                "client_id": "issued",
                "token_endpoint_auth_method": "none",
                "redirect_uris": ["http://127.0.0.1:1337/oauth/callback"],
                "registration_access_token": "ignored",
            }))
            .unwrap(),
            "issued"
        );
        for value in [
            json!({ "client_id": "issued", "client_secret": "secret" }),
            json!({ "client_id": "issued", "token_endpoint_auth_method": "client_secret_basic" }),
            json!({ "client_id": "issued", "redirect_uris": ["http://127.0.0.1:9/callback"] }),
            json!({ "client_id": "" }),
        ] {
            let error = client(value).unwrap_err();
            assert_matches!(error, AuthenticationError::Invalid(_));
        }
    }
}
