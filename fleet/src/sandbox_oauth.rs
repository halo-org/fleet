use crate::{
    AuthenticationError,
    api::{ApiError, ResponseBody},
    oauth,
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
/// client reaches every endpoint in process.
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
    refresh_tokens: BTreeMap<String, String>,
}

struct Grant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
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
        form: Option<&[(&str, &str)]>,
    ) -> Result<oauth::Response, AuthenticationError> {
        let (status, body) = match (self.endpoint(url)?, form) {
            ("/token", Some(form)) => self.token(form),
            _ => (StatusCode::NOT_FOUND, json!({ "error": "not_found" })),
        };
        Ok(oauth::Response {
            status: status.as_u16(),
            body: body.to_string().into_bytes(),
        })
    }

    /// Returns the endpoint's path within the provider.
    ///
    /// # Errors
    ///
    /// Returns an error for URLs outside the provider.
    pub fn endpoint<'a>(&self, url: &'a Url) -> Result<&'a str, AuthenticationError> {
        (url.origin() == self.base.origin())
            .then(|| url.path().strip_prefix(self.base.path()))
            .flatten()
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
                scope: parameter("scope").unwrap_or_default().into(),
            });
        location.query_pairs_mut().append_pair("code", &code);
        if let Some(value) = parameter("state") {
            location.query_pairs_mut().append_pair("state", value);
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
}

impl State {
    fn authorize(&mut self, grant: Grant) -> String {
        self.codes_issued = self.codes_issued.saturating_add(1);
        let code = format!("sandbox-code-{}", self.codes_issued);
        self.codes.insert(code.clone(), grant);
        code
    }

    /// Issues tokens for a code or refresh token, returning their number and scope.
    fn token(&mut self, form: &BTreeMap<&str, &str>) -> Result<(u64, String), &'static str> {
        let scope = match form.get("grant_type").copied() {
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
                grant.scope
            }
            Some("refresh_token") => form
                .get("refresh_token")
                .and_then(|token| self.refresh_tokens.remove(*token))
                .ok_or("invalid_grant")?,
            _ => return Err("unsupported_grant_type"),
        };
        self.tokens_issued = self.tokens_issued.saturating_add(1);
        self.refresh_tokens.insert(
            format!("sandbox-refresh-{}", self.tokens_issued),
            scope.clone(),
        );
        Ok((self.tokens_issued, scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::OAuth;
    use fleet_kit::OAuthConfig;

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
        let response = provider.send(&url, Some(form)).unwrap();
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
        let config = |base: &str| OAuthConfig {
            authorization_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            client_id: "fleet".into(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            scopes: Vec::new(),
            parameters: Vec::new(),
            resource: None,
            issuer: None,
            registration: None,
            required_scopes: Vec::new(),
        };
        let oauth = OAuth::sandbox(provider.clone());
        let redirect_uri: Url = "http://127.0.0.1:9/callback".parse().unwrap();

        let response = provider.handle(&Method::GET, &external.parse().unwrap());
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let authorization = oauth
            .authorize(
                &config("http://127.0.0.1:1337/sandbox/oauth"),
                None,
                redirect_uri.clone(),
            )
            .unwrap();
        assert!(
            authorization
                .url
                .starts_with("http://127.0.0.1:1337/sandbox/oauth/authorize?")
        );
        let outside = oauth.authorize(&config("https://example.com"), None, redirect_uri);
        assert!(matches!(outside, Err(AuthenticationError::Invalid(_))));
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
                .send(&url.parse().unwrap(), Some(&form))
                .unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }
}
