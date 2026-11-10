use crate::AuthenticationError;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use core::time::Duration;
use fleet_kit::OAuthConfig;
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation,
    jwk::{JwkSet, KeyOperations, PublicKeyUse},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Clone)]
pub struct OAuth {
    client: reqwest::Client,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registration {
    pub client_id: String,
    pub subject: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at: u64,
    pub scopes: Vec<String>,
}

pub struct Authorization {
    pub id: String,
    pub url: String,
    pub redirect_uri: Url,
    pub verifier: String,
    pub nonce: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    id_token_signing_alg_values_supported: Vec<Algorithm>,
    revocation_endpoint: Option<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

#[derive(Deserialize)]
struct Identity {
    sub: String,
    email: Option<String>,
    nonce: Option<String>,
    aud: serde_json::Value,
    azp: Option<String>,
    iat: u64,
}

impl OAuth {
    pub fn new() -> Result<Self, AuthenticationError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(transport_error)?;
        Ok(Self { client })
    }

    pub async fn exchange(
        &self,
        config: &OAuthConfig,
        registration: &mut Registration,
        authorization: &Authorization,
        code: &str,
    ) -> Result<Tokens, AuthenticationError> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", registration.client_id.as_str()),
            ("redirect_uri", authorization.redirect_uri.as_str()),
            ("code_verifier", &authorization.verifier),
            ("code", code),
        ];
        if let Some(resource) = &config.resource {
            form.push(("resource", resource));
        }
        let response = self.token(config, &form).await?;
        if let Some(issuer) = &config.issuer {
            let token = response
                .id_token
                .as_deref()
                .ok_or(AuthenticationError::Invalid("identity token is missing"))?;
            self.verify(issuer, token, registration, Some(&authorization.nonce))
                .await?;
        }
        tokens(response, None)
    }

    pub async fn refresh(
        &self,
        config: &OAuthConfig,
        registration: &mut Registration,
        previous: &Tokens,
    ) -> Result<Tokens, AuthenticationError> {
        let refresh_token = previous
            .refresh_token
            .as_deref()
            .ok_or(AuthenticationError::Unauthorized)?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("client_id", registration.client_id.as_str()),
            ("refresh_token", refresh_token),
        ];
        if let Some(resource) = &config.resource {
            form.push(("resource", resource));
        }
        let response = self.token(config, &form).await?;
        if let (Some(issuer), Some(token)) = (&config.issuer, &response.id_token) {
            self.verify(issuer, token, registration, None).await?;
        }
        tokens(response, Some(previous))
    }

    pub async fn revoke(
        &self,
        config: &OAuthConfig,
        registration: &Registration,
        tokens: &Tokens,
    ) -> Result<bool, AuthenticationError> {
        let (Some(issuer), Some(token)) = (&config.issuer, &tokens.refresh_token) else {
            return Ok(false);
        };
        let Some(url) = self.discovery(issuer).await?.revocation_endpoint else {
            return Ok(false);
        };
        let response = self
            .client
            .post(endpoint(&url)?)
            .form(&[
                ("token", token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", &registration.client_id),
            ])
            .send()
            .await
            .map_err(transport_error)?;
        Ok(response.status().is_success())
    }

    async fn token(
        &self,
        config: &OAuthConfig,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, AuthenticationError> {
        let response = self
            .client
            .post(endpoint(&config.token_url)?)
            .form(form)
            .send()
            .await
            .map_err(transport_error)?;
        read_json(response).await
    }

    pub async fn requires_issuer_parameter(
        &self,
        issuer: &str,
    ) -> Result<bool, AuthenticationError> {
        Ok(self
            .discovery(issuer)
            .await?
            .authorization_response_iss_parameter_supported)
    }

    async fn discovery(&self, issuer: &str) -> Result<Discovery, AuthenticationError> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let response = self
            .client
            .get(endpoint(&url)?)
            .send()
            .await
            .map_err(transport_error)?;
        let discovery: Discovery = read_json(response).await?;
        if discovery.issuer != issuer {
            return Err(AuthenticationError::Invalid("OpenID issuer does not match"));
        }
        Ok(discovery)
    }

    async fn verify(
        &self,
        issuer: &str,
        token: &str,
        registration: &mut Registration,
        nonce: Option<&str>,
    ) -> Result<(), AuthenticationError> {
        let discovery = self.discovery(issuer).await?;
        let response = self
            .client
            .get(endpoint(&discovery.jwks_uri)?)
            .send()
            .await
            .map_err(transport_error)?;
        let keys: JwkSet = read_json(response).await?;
        let header = jsonwebtoken::decode_header(token).map_err(identity_error)?;
        if !discovery
            .id_token_signing_alg_values_supported
            .contains(&header.alg)
            || !matches!(
                header.alg,
                Algorithm::RS256
                    | Algorithm::RS384
                    | Algorithm::RS512
                    | Algorithm::PS256
                    | Algorithm::ES256
                    | Algorithm::ES384
                    | Algorithm::EdDSA
            )
        {
            return Err(AuthenticationError::Invalid(
                "unsupported identity signature algorithm",
            ));
        }
        let key = header.kid.as_deref().and_then(|id| keys.find(id)).ok_or(
            AuthenticationError::Invalid("identity signing key is missing"),
        )?;
        if key
            .common
            .key_algorithm
            .is_some_and(|algorithm| algorithm != header.alg.into())
            || key
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| *usage != PublicKeyUse::Signature)
            || key
                .common
                .key_operations
                .as_ref()
                .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
        {
            return Err(AuthenticationError::Invalid(
                "identity signing key cannot verify this token",
            ));
        }
        let key = DecodingKey::from_jwk(key).map_err(identity_error)?;
        let mut validation = Validation::new(header.alg);
        validation.set_audience(&[&registration.client_id]);
        validation.set_issuer(&[issuer]);
        validation.set_required_spec_claims(&["exp", "iat", "iss", "sub", "aud"]);
        validation.leeway = 5;
        validation.validate_nbf = true;
        let identity: Identity = jsonwebtoken::decode(token, &key, &validation)
            .map_err(identity_error)?
            .claims;
        let multiple_audiences = identity
            .aud
            .as_array()
            .is_some_and(|audiences| audiences.len() > 1);
        if identity.sub.is_empty()
            || identity.iat > now().saturating_add(5)
            || nonce.is_some_and(|nonce| identity.nonce.as_deref() != Some(nonce))
            || identity
                .azp
                .as_deref()
                .is_some_and(|client| client != registration.client_id)
            || (multiple_audiences && identity.azp.is_none())
            || registration
                .subject
                .as_ref()
                .is_some_and(|subject| subject != &identity.sub)
        {
            return Err(AuthenticationError::Invalid(
                "identity does not match the sign-in attempt",
            ));
        }
        registration.subject = Some(identity.sub);
        registration.email = identity.email;
        Ok(())
    }
}

pub fn authorize(
    config: &OAuthConfig,
    registration: Option<&Registration>,
    redirect_uri: Url,
) -> Result<Authorization, AuthenticationError> {
    endpoint(&config.token_url)?;
    let id = random_value()?;
    let verifier = random_value()?;
    let nonce = random_value()?;
    let mut url = endpoint(&config.authorization_url)?;
    if url.query().is_some() {
        return Err(AuthenticationError::Invalid(
            "OAuth authorization endpoint must not contain a query",
        ));
    }
    let mut parameters = BTreeMap::from([
        ("response_type".to_owned(), "code".to_owned()),
        (
            "client_id".into(),
            registration
                .map_or(&config.client_id, |registration| &registration.client_id)
                .clone(),
        ),
        ("redirect_uri".into(), redirect_uri.to_string()),
        ("scope".into(), config.scopes.join(" ")),
        ("state".into(), id.clone()),
        ("nonce".into(), nonce.clone()),
        ("code_challenge_method".into(), "S256".into()),
        (
            "code_challenge".into(),
            URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier)),
        ),
    ]);
    if let Some(resource) = &config.resource {
        parameters.insert("resource".into(), resource.clone());
    }
    let initial = registration
        .is_none()
        .then_some(config.registration.as_ref())
        .flatten();
    for (name, value) in config.parameters.iter().chain(
        initial
            .into_iter()
            .flat_map(|registration| &registration.parameters),
    ) {
        if parameters.insert(name.clone(), value.clone()).is_some() {
            return Err(AuthenticationError::Invalid(
                "OAuth parameters contain a reserved or duplicate name",
            ));
        }
    }
    url.query_pairs_mut().extend_pairs(parameters);
    Ok(Authorization {
        id,
        url: url.into(),
        redirect_uri,
        verifier,
        nonce,
    })
}

/// Returns the redirect URI for an authorization.
///
/// A requested URI must match the registered URI apart from its port, which can differ only
/// when the registered port is 0. Without a request, the registered URI is returned.
pub fn redirect_uri(registered: &str, requested: Option<&str>) -> Result<Url, AuthenticationError> {
    let registered = loopback(registered)?;
    let Some(requested) = requested else {
        return Ok(registered);
    };
    let requested = loopback(requested)?;
    let port = registered.port_or_known_default();
    if requested.path() != registered.path()
        || requested.port_or_known_default() == Some(0)
        || (port != Some(0) && requested.port_or_known_default() != port)
    {
        return Err(AuthenticationError::Invalid(
            "redirect URI does not match the registered URI",
        ));
    }
    Ok(requested)
}

fn loopback(value: &str) -> Result<Url, AuthenticationError> {
    let url = Url::parse(value)
        .map_err(|_| AuthenticationError::Invalid("invalid OAuth redirect URI"))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AuthenticationError::Invalid(
            "OAuth redirect URI must use HTTP on 127.0.0.1",
        ));
    }
    Ok(url)
}

fn tokens(
    response: TokenResponse,
    previous: Option<&Tokens>,
) -> Result<Tokens, AuthenticationError> {
    if response.access_token.is_empty()
        || !response.token_type.eq_ignore_ascii_case("bearer")
        || response.expires_in == 0
    {
        return Err(AuthenticationError::Invalid("invalid OAuth token response"));
    }
    let scopes = response
        .scope
        .map(|scope| scope.split_whitespace().map(str::to_owned).collect())
        .or_else(|| previous.map(|tokens| tokens.scopes.clone()))
        .unwrap_or_default();
    Ok(Tokens {
        access_token: response.access_token,
        refresh_token: response
            .refresh_token
            .or_else(|| previous.and_then(|tokens| tokens.refresh_token.clone())),
        id_token: response
            .id_token
            .or_else(|| previous.and_then(|tokens| tokens.id_token.clone())),
        expires_at: now().saturating_add(response.expires_in),
        scopes,
    })
}

fn endpoint(value: &str) -> Result<Url, AuthenticationError> {
    let url =
        Url::parse(value).map_err(|_| AuthenticationError::Invalid("invalid OAuth endpoint"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AuthenticationError::Invalid(
            "OAuth endpoints must use HTTPS without credentials",
        ));
    }
    Ok(url)
}

async fn read_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, AuthenticationError> {
    if !response.status().is_success() {
        return Err(AuthenticationError::Rejected(response.status().as_u16()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
            return Err(AuthenticationError::Invalid(
                "OAuth response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| AuthenticationError::Invalid("invalid OAuth response"))
}

pub fn random_value() -> Result<String, AuthenticationError> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| AuthenticationError::Invalid("could not generate sign-in randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn transport_error(error: reqwest::Error) -> AuthenticationError {
    AuthenticationError::source("OAuth request failed", error.without_url())
}

fn identity_error(_: jsonwebtoken::errors::Error) -> AuthenticationError {
    AuthenticationError::Invalid("identity token could not be verified")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_preserve_omitted_refresh_fields() {
        let previous = Tokens {
            access_token: "old".into(),
            refresh_token: Some("refresh".into()),
            id_token: Some("identity".into()),
            expires_at: 0,
            scopes: vec!["invoke".into()],
        };
        let response = TokenResponse {
            access_token: "new".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            id_token: None,
            expires_in: 3600,
            scope: None,
        };
        let tokens = tokens(response, Some(&previous)).unwrap();

        assert_eq!(tokens.access_token, "new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(tokens.id_token.as_deref(), Some("identity"));
        assert_eq!(tokens.scopes, ["invoke"]);
        assert!(tokens.expires_at > now());
    }

    #[test]
    fn tokens_replace_rotated_refresh_tokens_and_scopes() {
        let previous = Tokens {
            access_token: "old".into(),
            refresh_token: Some("old-refresh".into()),
            id_token: None,
            expires_at: 0,
            scopes: vec!["invoke".into()],
        };
        let response = TokenResponse {
            access_token: "new".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("new-refresh".into()),
            id_token: None,
            expires_in: 3600,
            scope: Some("openid".into()),
        };
        let tokens = tokens(response, Some(&previous)).unwrap();

        assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(tokens.scopes, ["openid"]);
    }

    #[test]
    fn redirect_uris_match_the_registered_path_and_port() {
        let any = "http://127.0.0.1:0/callback";
        let fixed = "http://127.0.0.1:1455/callback";
        let client = "http://127.0.0.1:3000/callback";

        assert_eq!(redirect_uri(any, None).unwrap().as_str(), any);
        assert_eq!(redirect_uri(any, Some(client)).unwrap().as_str(), client);
        assert_eq!(redirect_uri(fixed, Some(fixed)).unwrap().as_str(), fixed);

        for (registered, requested) in [
            (any, "http://127.0.0.1:0/callback"),
            (any, "http://127.0.0.1:3000/other"),
            (any, "http://localhost:3000/callback"),
            (any, "http://127.0.0.1:3000/callback?next=/"),
            (fixed, client),
        ] {
            let error = redirect_uri(registered, Some(requested)).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }
}
