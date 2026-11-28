use crate::{
    AuthenticationError,
    api::{ApiError, ResponseBody},
    oauth::{self, Body},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hyper::{Method, Response, StatusCode, Uri, header};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use url::Url;

/// A simulated OAuth provider that approves every authorization.
///
/// Browsers reach the authorization endpoint through Fleet's HTTP server. Fleet's OAuth
/// client reaches every endpoint in process. The provider is also the authorization
/// server of the protected resource at `resource` and publishes metadata for both. It
/// registers clients dynamically and accepts any other client ID, including metadata
/// document URLs, without fetching documents. Callbacks for the resource name the
/// issuer.
#[derive(Clone)]
pub struct SandboxOAuth {
    base: Url,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    codes_issued: u64,
    tokens_issued: u64,
    codes: BTreeMap<String, Grant>,
    refresh_tokens: BTreeMap<String, Access>,
    clients: Vec<String>,
}

struct Grant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    access: Access,
}

#[derive(Clone)]
struct Access {
    scope: String,
    resource: Option<String>,
}

impl SandboxOAuth {
    /// Creates a provider whose endpoints start with the base URL.
    pub fn new(base: Url) -> Self {
        Self {
            base,
            state: Arc::default(),
        }
    }

    /// Handles a browser request to the authorization endpoint.
    pub fn handle(&self, method: &Method, uri: &Uri) -> Response<ResponseBody> {
        let authorization = match (method, uri.path().strip_prefix(self.base.path())) {
            (&Method::GET, Some("/authorize")) => self.authorize(uri.query().unwrap_or_default()),
            (_, Some("/authorize")) => return ApiError::MethodNotAllowed("GET").into_response(),
            _ => return ApiError::NotFound.into_response(),
        };
        match authorization {
            Ok(location) => {
                let mut response = crate::api::empty_response(StatusCode::FOUND);
                response.headers_mut().insert(
                    header::LOCATION,
                    location
                        .as_str()
                        .parse()
                        .expect("redirect URL is a valid header"),
                );
                response
            }
            Err(error) => {
                crate::api::json_response(StatusCode::BAD_REQUEST, &json!({ "error": error }))
            }
        }
    }

    /// Answers a request from Fleet's OAuth client.
    ///
    /// # Errors
    ///
    /// Returns an error for URLs outside the provider.
    pub fn send(
        &self,
        url: &Url,
        body: Option<Body<'_>>,
    ) -> Result<oauth::Response, AuthenticationError> {
        let base = self.base.as_str();
        let (status, body) = match (self.endpoint(url)?.as_str(), body) {
            ("/token", Some(Body::Form(form))) => self.token(form),
            ("/revoke", Some(Body::Form(form))) => self.revoke(form),
            ("/register", Some(Body::Json(metadata))) => self.register(metadata),
            ("/.well-known/oauth-protected-resource/resource", None) => (
                StatusCode::OK,
                json!({
                    "resource": format!("{base}/resource"),
                    "authorization_servers": [base],
                    "scopes_supported": ["sandbox"],
                }),
            ),
            ("/.well-known/oauth-authorization-server", None) => (
                StatusCode::OK,
                json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "revocation_endpoint": format!("{base}/revoke"),
                    "registration_endpoint": format!("{base}/register"),
                    "code_challenge_methods_supported": ["S256"],
                    "authorization_response_iss_parameter_supported": true,
                    "client_id_metadata_document_supported": true,
                }),
            ),
            _ => (StatusCode::NOT_FOUND, json!({ "error": "not_found" })),
        };
        Ok(oauth::Response {
            status: status.as_u16(),
            body: body.to_string().into_bytes(),
        })
    }

    /// Returns the endpoint's path within the provider.
    ///
    /// Metadata paths keep their well-known prefix.
    ///
    /// # Errors
    ///
    /// Returns an error for URLs outside the provider.
    pub fn endpoint(&self, url: &Url) -> Result<String, AuthenticationError> {
        let path = url.path();
        let (prefix, path) = [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
        ]
        .into_iter()
        .find_map(|prefix| Some((prefix, path.strip_prefix(prefix)?)))
        .unwrap_or(("", path));
        (url.origin() == self.base.origin())
            .then(|| path.strip_prefix(self.base.path()))
            .flatten()
            .map(|path| format!("{prefix}{path}"))
            .ok_or(AuthenticationError::Invalid(
                "the sandbox only allows its own OAuth provider",
            ))
    }

    fn authorize(&self, query: &str) -> Result<Url, &'static str> {
        let parameters: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).collect();
        let parameter = |name: &str| parameters.get(name).map(AsRef::as_ref);
        let (Some("code"), Some("S256"), Some(client_id), Some(redirect_uri), Some(challenge)) = (
            parameter("response_type"),
            parameter("code_challenge_method"),
            parameter("client_id"),
            parameter("redirect_uri"),
            parameter("code_challenge"),
        ) else {
            return Err("invalid_request");
        };
        let mut location = crate::oauth::loopback(redirect_uri).map_err(|_| "invalid_request")?;
        let code = self
            .state
            .lock()
            .expect("sandbox OAuth lock is not poisoned")
            .authorize(Grant {
                client_id: client_id.into(),
                redirect_uri: redirect_uri.into(),
                code_challenge: challenge.into(),
                access: Access {
                    scope: parameter("scope").unwrap_or_default().into(),
                    resource: parameter("resource").map(Into::into),
                },
            })?;
        location.query_pairs_mut().append_pair("code", &code);
        if let Some(value) = parameter("state") {
            location.query_pairs_mut().append_pair("state", value);
        }
        if parameter("resource").is_some() {
            location
                .query_pairs_mut()
                .append_pair("iss", self.base.as_str());
        }
        Ok(location)
    }

    fn token(&self, form: &[(&str, &str)]) -> (StatusCode, Value) {
        let form: BTreeMap<_, _> = form.iter().copied().collect();
        let tokens = self
            .state
            .lock()
            .expect("sandbox OAuth lock is not poisoned")
            .token(&form);
        match tokens {
            Ok((issued, scope)) => (
                StatusCode::OK,
                json!({
                    "access_token": format!("sandbox-access-{issued}"),
                    "token_type": "Bearer",
                    "refresh_token": format!("sandbox-refresh-{issued}"),
                    "expires_in": 3600,
                    "scope": scope,
                }),
            ),
            Err(error) => (StatusCode::BAD_REQUEST, json!({ "error": error })),
        }
    }

    fn register(&self, metadata: &Value) -> (StatusCode, Value) {
        let Some(redirect_uri) = metadata["redirect_uris"]
            .as_array()
            .and_then(|uris| uris.first())
            .and_then(Value::as_str)
            .filter(|uri| crate::oauth::loopback(uri).is_ok())
        else {
            return (
                StatusCode::BAD_REQUEST,
                json!({ "error": "invalid_redirect_uri" }),
            );
        };
        let mut state = self
            .state
            .lock()
            .expect("sandbox OAuth lock is not poisoned");
        state.clients.push(redirect_uri.into());
        let client_id = format!("sandbox-client-{}", state.clients.len());
        drop(state);
        (
            StatusCode::CREATED,
            json!({
                "client_id": client_id,
                "redirect_uris": [redirect_uri],
                "token_endpoint_auth_method": "none",
            }),
        )
    }

    fn revoke(&self, form: &[(&str, &str)]) -> (StatusCode, Value) {
        if let Some((_, token)) = form.iter().find(|(name, _)| *name == "token") {
            self.state
                .lock()
                .expect("sandbox OAuth lock is not poisoned")
                .refresh_tokens
                .remove(*token);
        }
        (StatusCode::OK, json!({}))
    }
}

impl State {
    /// Issues a code, requiring a registered client's own redirect URI.
    fn authorize(&mut self, grant: Grant) -> Result<String, &'static str> {
        let registered = grant
            .client_id
            .strip_prefix("sandbox-client-")
            .and_then(|number| number.parse::<usize>().ok())
            .and_then(|number| self.clients.get(number.checked_sub(1)?));
        if registered.is_some_and(|uri| *uri != grant.redirect_uri) {
            return Err("invalid_request");
        }
        self.codes_issued = self.codes_issued.saturating_add(1);
        let code = format!("sandbox-code-{}", self.codes_issued);
        self.codes.insert(code.clone(), grant);
        Ok(code)
    }

    /// Issues tokens for a code or refresh token, returning their number and scope.
    fn token(&mut self, form: &BTreeMap<&str, &str>) -> Result<(u64, String), &'static str> {
        let resource = form.get("resource").copied();
        let access = match form.get("grant_type").copied() {
            Some("authorization_code") => {
                let grant = form
                    .get("code")
                    .and_then(|code| self.codes.remove(*code))
                    .ok_or("invalid_grant")?;
                let challenge = form
                    .get("code_verifier")
                    .map(|verifier| URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())));
                if form.get("client_id").copied() != Some(grant.client_id.as_str())
                    || form.get("redirect_uri").copied() != Some(grant.redirect_uri.as_str())
                    || challenge.as_deref() != Some(grant.code_challenge.as_str())
                {
                    return Err("invalid_grant");
                }
                if grant.access.resource.as_deref() != resource {
                    return Err("invalid_target");
                }
                grant.access
            }
            Some("refresh_token") => {
                let token = form.get("refresh_token").copied().unwrap_or_default();
                if self
                    .refresh_tokens
                    .get(token)
                    .is_some_and(|access| access.resource.as_deref() != resource)
                {
                    return Err("invalid_target");
                }
                self.refresh_tokens.remove(token).ok_or("invalid_grant")?
            }
            _ => return Err("unsupported_grant_type"),
        };
        self.tokens_issued = self.tokens_issued.saturating_add(1);
        self.refresh_tokens.insert(
            format!("sandbox-refresh-{}", self.tokens_issued),
            access.clone(),
        );
        Ok((self.tokens_issued, access.scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::OAuth;
    use fleet_kit::{OAuthClient, OAuthConfig, OAuthEndpoints, OAuthRegisteredClient, OAuthServer};
    use futures_util::FutureExt as _;

    fn provider() -> SandboxOAuth {
        SandboxOAuth::new("http://127.0.0.1:1337/sandbox/oauth".parse().unwrap())
    }

    fn authorize(provider: &SandboxOAuth, verifier: &str) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier));
        let uri = format!(
            "/sandbox/oauth/authorize?response_type=code&client_id=fleet&scope=a+b\
             &redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcallback&state=xyz\
             &code_challenge_method=S256&code_challenge={challenge}"
        );
        let response = provider.handle(&Method::GET, &uri.parse().unwrap());
        assert_eq!(response.status(), StatusCode::FOUND);
        let location: Url = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(location.path(), "/callback");
        let parameters: BTreeMap<_, _> = location.query_pairs().collect();
        assert_eq!(parameters["state"], "xyz");
        parameters["code"].to_string()
    }

    fn token(provider: &SandboxOAuth, form: &[(&str, &str)]) -> (u16, Value) {
        let url = "http://127.0.0.1:1337/sandbox/oauth/token".parse().unwrap();
        let response = provider.send(&url, Some(Body::Form(form))).unwrap();
        (
            response.status,
            serde_json::from_slice(&response.body).unwrap(),
        )
    }

    #[test]
    fn codes_exchange_once_with_their_verifier_and_refresh_tokens_rotate() {
        let provider = provider();
        let code = authorize(&provider, "verifier");
        let exchange = |verifier| {
            [
                ("grant_type", "authorization_code"),
                ("client_id", "fleet"),
                ("redirect_uri", "http://127.0.0.1:9/callback"),
                ("code", code.as_str()),
                ("code_verifier", verifier),
            ]
        };

        let (status, tokens) = token(&provider, &exchange("verifier"));
        assert_eq!(status, 200);
        assert_eq!(
            tokens,
            json!({
                "access_token": "sandbox-access-1",
                "token_type": "Bearer",
                "refresh_token": "sandbox-refresh-1",
                "expires_in": 3600,
                "scope": "a b",
            })
        );
        assert_eq!(token(&provider, &exchange("verifier")).0, 400);

        let refresh = [
            ("grant_type", "refresh_token"),
            ("client_id", "fleet"),
            ("refresh_token", "sandbox-refresh-1"),
        ];
        let (status, tokens) = token(&provider, &refresh);
        assert_eq!(status, 200);
        assert_eq!(tokens["access_token"], "sandbox-access-2");
        assert_eq!(tokens["scope"], "a b");
        assert_eq!(token(&provider, &refresh).0, 400);
    }

    #[test]
    fn authorizations_stay_within_the_sandbox() {
        let provider = provider();
        let external = "/sandbox/oauth/authorize?response_type=code&client_id=fleet\
                       &redirect_uri=https%3A%2F%2Fexample.com%2Fcallback\
                       &code_challenge_method=S256&code_challenge=x";
        let config = |server| OAuthConfig {
            server,
            client: OAuthClient::Registered(OAuthRegisteredClient {
                client_id: "fleet".into(),
                redirect_uri: "http://127.0.0.1:0/callback".into(),
                registration: None,
            }),
            scopes: Vec::new(),
            parameters: Vec::new(),
            required_scopes: Vec::new(),
        };
        let endpoints = |base: &str| {
            config(OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: format!("{base}/authorize"),
                token_url: format!("{base}/token"),
                resource: None,
                issuer: None,
            }))
        };
        let resource = |base: &str| config(OAuthServer::Resource(format!("{base}/resource")));
        let oauth = OAuth::sandbox(provider.clone());
        let authorize = |config| {
            let metadata = oauth.discover(&config, &[], None).now_or_never().unwrap()?;
            let redirect_uri = "http://127.0.0.1:9/callback".parse().unwrap();
            oauth.authorize(&config, metadata, &[], None, redirect_uri)
        };

        let response = provider.handle(&Method::GET, &external.parse().unwrap());
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        for config in [
            endpoints("http://127.0.0.1:1337/sandbox/oauth"),
            resource("http://127.0.0.1:1337/sandbox/oauth"),
        ] {
            let authorization = authorize(config).unwrap();
            assert!(
                authorization
                    .url
                    .starts_with("http://127.0.0.1:1337/sandbox/oauth/authorize?")
            );
        }
        for config in [
            endpoints("https://example.com"),
            resource("https://example.com"),
        ] {
            let error = authorize(config).err().unwrap();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }

    #[test]
    fn wrong_verifiers_and_other_hosts_are_rejected() {
        let provider = provider();
        let code = authorize(&provider, "verifier");
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", "fleet"),
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("code", code.as_str()),
            ("code_verifier", "other"),
        ];

        assert_eq!(token(&provider, &form).0, 400);
        for url in [
            "https://auth.example.com/sandbox/oauth/token",
            "http://127.0.0.1:1337/other/token",
        ] {
            let error = provider
                .send(&url.parse().unwrap(), Some(Body::Form(&form)))
                .unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }
}
