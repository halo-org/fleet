use crate::{
    AuthenticationError, SandboxOAuth,
    oauth_discovery::{ResourceMetadata, ServerMetadata},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use core::{
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use fleet_kit::{OAuthConfig, OAuthEndpoints, OAuthServer};
use fleet_wasm::WasmOrigin;
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
use url::{Host, Url};

#[derive(Clone)]
pub struct OAuth {
    transport: Transport,
}

#[derive(Clone)]
enum Transport {
    Network {
        client: reqwest::Client,
        public: reqwest::Client,
    },
    Sandbox(SandboxOAuth),
}

/// The authorization server of a sign-in.
#[derive(Clone, Copy)]
pub enum Server<'a> {
    /// Endpoints the plugin declared.
    Declared(&'a OAuthEndpoints),
    /// A server discovered for a protected resource, with the provider's HTTP grant.
    Discovered(&'a Discovered, &'a [WasmOrigin]),
}

/// An authorization server discovered for a protected resource.
#[derive(Clone, Serialize, Deserialize)]
pub struct Discovered {
    pub resource: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
}

/// What discovery found for a protected resource.
pub struct Metadata {
    server: Discovered,
    authorization_endpoint: Url,
    scopes: Vec<String>,
}

/// Which servers a request may reach.
#[derive(Clone, Copy)]
enum Reach<'a> {
    /// Servers the plugin declared.
    Declared,
    /// Public HTTPS servers and origins in the provider's HTTP grant.
    Public(&'a [WasmOrigin]),
}

/// Resolves host names to public addresses only.
struct PublicResolver;

/// A host name that resolves to no public address.
#[derive(Debug)]
struct NonPublicAddress;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registration {
    pub client_id: String,
    pub subject: Option<String>,
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    /// When the access token expires, unless the server did not say.
    pub expires_at: Option<u64>,
    pub scopes: Vec<String>,
}

pub struct Authorization {
    pub id: String,
    pub url: String,
    pub redirect_uri: Url,
    pub verifier: String,
    pub nonce: String,
    pub scope: Option<String>,
    pub server: Option<Discovered>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
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

impl Response {
    pub const fn is_success(&self) -> bool {
        matches!(self.status, 200..=299)
    }
}

impl Registration {
    /// Returns whether the registration was issued by a discovered server, or by none.
    pub fn issued_by(&self, server: Option<&Discovered>) -> bool {
        self.issuer.as_deref() == server.map(|server| server.issuer.as_str())
    }
}

impl<'a> Server<'a> {
    /// Returns the issuer expected in authorization responses.
    pub fn issuer(self) -> Option<&'a str> {
        match self {
            Self::Declared(endpoints) => endpoints.issuer.as_deref(),
            Self::Discovered(server, _) => Some(&server.issuer),
        }
    }

    fn token_url(self) -> &'a str {
        match self {
            Self::Declared(endpoints) => &endpoints.token_url,
            Self::Discovered(server, _) => &server.token_endpoint,
        }
    }

    fn resource(self) -> Option<&'a str> {
        match self {
            Self::Declared(endpoints) => endpoints.resource.as_deref(),
            Self::Discovered(server, _) => Some(&server.resource),
        }
    }

    const fn reach(self) -> Reach<'a> {
        match self {
            Self::Declared(_) => Reach::Declared,
            Self::Discovered(_, origins) => Reach::Public(origins),
        }
    }
}

impl OAuth {
    pub fn new() -> Result<Self, AuthenticationError> {
        let builder = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
        };
        let client = builder().build().map_err(transport_error)?;
        let public = builder()
            .no_proxy()
            .dns_resolver(PublicResolver)
            .build()
            .map_err(transport_error)?;
        Ok(Self {
            transport: Transport::Network { client, public },
        })
    }

    /// Sends every request to the sandbox's provider instead of the network.
    pub const fn sandbox(provider: SandboxOAuth) -> Self {
        Self {
            transport: Transport::Sandbox(provider),
        }
    }

    /// Starts an authorization through endpoints this client can reach.
    ///
    /// A protected resource's server must be [discovered](Self::discover) first.
    pub fn authorize(
        &self,
        config: &OAuthConfig,
        metadata: Option<Metadata>,
        registration: Option<&Registration>,
        redirect_uri: Url,
    ) -> Result<Authorization, AuthenticationError> {
        let mut parameters = BTreeMap::new();
        let (url, server) = match (&config.server, metadata) {
            (OAuthServer::Endpoints(endpoints), None) => {
                self.endpoint(&endpoints.token_url, Reach::Declared)?;
                parameters.insert("scope".to_owned(), config.scopes.join(" "));
                if let Some(resource) = &endpoints.resource {
                    parameters.insert("resource".into(), resource.clone());
                }
                let url = self.endpoint(&endpoints.authorization_url, Reach::Declared)?;
                (url, None)
            }
            (OAuthServer::Resource(resource), Some(metadata)) => {
                let scopes = if config.scopes.is_empty() {
                    &metadata.scopes
                } else {
                    &config.scopes
                };
                if !scopes.is_empty() {
                    parameters.insert("scope".into(), scopes.join(" "));
                }
                parameters.insert("resource".into(), resource.clone());
                (metadata.authorization_endpoint, Some(metadata.server))
            }
            _ => {
                return Err(AuthenticationError::Invalid(
                    "authorization server was not discovered",
                ));
            }
        };
        let registration =
            registration.filter(|registration| registration.issued_by(server.as_ref()));
        authorization(config, registration, redirect_uri, url, parameters, server)
    }

    pub async fn exchange(
        &self,
        server: Server<'_>,
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
        if let Some(resource) = server.resource() {
            form.push(("resource", resource));
        }
        let mut response = self.token(server, &form).await?;
        if let Server::Discovered(..) = server
            && response.scope.is_none()
        {
            response.scope.clone_from(&authorization.scope);
        }
        if let Server::Declared(OAuthEndpoints {
            issuer: Some(issuer),
            ..
        }) = server
        {
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
        server: Server<'_>,
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
        if let Some(resource) = server.resource() {
            form.push(("resource", resource));
        }
        let response = self.token(server, &form).await?;
        if let Server::Declared(OAuthEndpoints {
            issuer: Some(issuer),
            ..
        }) = server
            && let Some(token) = &response.id_token
        {
            self.verify(issuer, token, registration, None).await?;
        }
        tokens(response, Some(previous))
    }

    pub async fn revoke(
        &self,
        server: Server<'_>,
        registration: &Registration,
        tokens: &Tokens,
    ) -> Result<bool, AuthenticationError> {
        let url = match server {
            Server::Declared(endpoints) => {
                let (Some(issuer), Some(_)) = (&endpoints.issuer, &tokens.refresh_token) else {
                    return Ok(false);
                };
                self.discovery(issuer).await?.revocation_endpoint
            }
            Server::Discovered(server, _) => server.revocation_endpoint.clone(),
        };
        let Some(url) = url else {
            return Ok(false);
        };
        let (token, hint) = tokens
            .refresh_token
            .as_ref()
            .map_or((&tokens.access_token, "access_token"), |token| {
                (token, "refresh_token")
            });
        let form = [
            ("token", token.as_str()),
            ("token_type_hint", hint),
            ("client_id", &registration.client_id),
        ];
        Ok(self
            .send(&url, Some(&form), server.reach())
            .await?
            .is_success())
    }

    async fn token(
        &self,
        server: Server<'_>,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, AuthenticationError> {
        read_json(
            &self
                .send(server.token_url(), Some(form), server.reach())
                .await?,
        )
    }

    /// Returns whether authorization responses must name the issuer.
    ///
    /// Discovered servers always must, since their metadata can give another server's
    /// authorization endpoint. A declared issuer must when its discovery document
    /// advertises support.
    pub async fn requires_issuer_parameter(
        &self,
        server: Server<'_>,
    ) -> Result<bool, AuthenticationError> {
        match server {
            Server::Declared(OAuthEndpoints {
                issuer: Some(issuer),
                ..
            }) => Ok(self
                .discovery(issuer)
                .await?
                .authorization_response_iss_parameter_supported),
            Server::Declared(_) => Ok(false),
            Server::Discovered(..) => Ok(true),
        }
    }

    async fn discovery(&self, issuer: &str) -> Result<Discovery, AuthenticationError> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: Discovery = read_json(&self.send(&url, None, Reach::Declared).await?)?;
        if discovery.issuer != issuer {
            return Err(AuthenticationError::Invalid("OpenID issuer does not match"));
        }
        Ok(discovery)
    }

    /// Discovers the authorization server of a protected resource.
    ///
    /// Returns nothing for declared endpoints.
    pub async fn discover(
        &self,
        config: &OAuthConfig,
        origins: &[WasmOrigin],
    ) -> Result<Option<Metadata>, AuthenticationError> {
        let OAuthServer::Resource(resource) = &config.server else {
            return Ok(None);
        };
        let reach = Reach::Public(origins);
        let url = self.endpoint(resource, reach)?;
        let (issuer, scopes) = self
            .metadata(
                crate::oauth_discovery::resource_metadata_urls(&url),
                reach,
                "protected resource metadata was not found",
                "protected resource metadata is invalid",
                |metadata: ResourceMetadata| metadata.issuer(resource),
            )
            .await?;
        let url = self.endpoint(&issuer, reach)?;
        if url.query().is_some() {
            return Err(AuthenticationError::Invalid(
                "authorization server issuer must not contain a query",
            ));
        }
        let (server, authorization_endpoint) = self
            .metadata(
                crate::oauth_discovery::server_metadata_urls(&url),
                reach,
                "authorization server metadata was not found",
                "authorization server metadata is invalid",
                |metadata: ServerMetadata| metadata.server(resource, &issuer),
            )
            .await?;
        self.endpoint(&server.token_endpoint, reach)?;
        if let Some(url) = &server.revocation_endpoint {
            self.endpoint(url, reach)?;
        }
        Ok(Some(Metadata {
            server,
            authorization_endpoint: self.endpoint(&authorization_endpoint, reach)?,
            scopes,
        }))
    }

    /// Reads the first metadata document that is valid.
    ///
    /// Reports the first failure other than a missing document. Redirects are failures.
    async fn metadata<T: DeserializeOwned, U>(
        &self,
        urls: Vec<Url>,
        reach: Reach<'_>,
        missing: &'static str,
        invalid: &'static str,
        validate: impl Fn(T) -> Result<U, AuthenticationError>,
    ) -> Result<U, AuthenticationError> {
        let mut failure = AuthenticationError::Invalid(missing);
        for url in urls {
            let result = match self.send(url.as_str(), None, reach).await {
                Ok(response) if response.is_success() => serde_json::from_slice(&response.body)
                    .map_err(|_| AuthenticationError::Invalid(invalid))
                    .and_then(&validate),
                Ok(response) if response.status == 404 => {
                    Err(AuthenticationError::Invalid(missing))
                }
                Ok(response) => Err(AuthenticationError::Rejected(response.status)),
                Err(error) => Err(error),
            };
            match result {
                Ok(metadata) => return Ok(metadata),
                Err(error) => {
                    if matches!(failure, AuthenticationError::Invalid(message) if message == missing)
                    {
                        failure = error;
                    }
                }
            }
        }
        Err(failure)
    }

    /// Posts the form, or gets the URL without one.
    async fn send(
        &self,
        url: &str,
        form: Option<&[(&str, &str)]>,
        reach: Reach<'_>,
    ) -> Result<Response, AuthenticationError> {
        let url = self.endpoint(url, reach)?;
        let client = match (&self.transport, reach) {
            (Transport::Network { public, .. }, Reach::Public(origins))
                if !crate::oauth::granted(&url, origins)? =>
            {
                public
            }
            (Transport::Network { client, .. }, _) => client,
            (Transport::Sandbox(provider), _) => return provider.send(&url, form),
        };
        let request = match form {
            Some(form) => client.post(url).form(form),
            None => client.get(url),
        };
        let mut response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        let mut body = Vec::new();
        if status.is_success() {
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if body.len().saturating_add(chunk.len()) > 1_048_576 {
                    return Err(AuthenticationError::Invalid(
                        "OAuth response exceeds the size limit",
                    ));
                }
                body.extend_from_slice(&chunk);
            }
        }
        Ok(Response {
            status: status.as_u16(),
            body,
        })
    }

    fn endpoint(&self, value: &str, reach: Reach<'_>) -> Result<Url, AuthenticationError> {
        let url = endpoint(value)?;
        match (&self.transport, reach) {
            (Transport::Sandbox(provider), _) => {
                provider.endpoint(&url)?;
            }
            (Transport::Network { .. }, Reach::Public(origins)) => {
                crate::oauth::granted(&url, origins)?;
            }
            (Transport::Network { .. }, Reach::Declared) => {}
        }
        Ok(url)
    }
    async fn verify(
        &self,
        issuer: &str,
        token: &str,
        registration: &mut Registration,
        nonce: Option<&str>,
    ) -> Result<(), AuthenticationError> {
        let discovery = self.discovery(issuer).await?;
        let keys: JwkSet = read_json(
            &self
                .send(&discovery.jwks_uri, None, Reach::Declared)
                .await?,
        )?;
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

fn authorization(
    config: &OAuthConfig,
    registration: Option<&Registration>,
    redirect_uri: Url,
    mut url: Url,
    mut parameters: BTreeMap<String, String>,
    server: Option<Discovered>,
) -> Result<Authorization, AuthenticationError> {
    let id = random_value()?;
    let verifier = random_value()?;
    let nonce = random_value()?;
    parameters.extend([
        ("response_type".to_owned(), "code".to_owned()),
        (
            "client_id".into(),
            registration
                .map_or(&config.client_id, |registration| &registration.client_id)
                .clone(),
        ),
        ("redirect_uri".into(), redirect_uri.to_string()),
        ("state".into(), id.clone()),
        ("code_challenge_method".into(), "S256".into()),
        (
            "code_challenge".into(),
            URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier)),
        ),
    ]);
    if server.is_none() {
        parameters.insert("nonce".into(), nonce.clone());
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
        if matches!(name.as_str(), "scope" | "nonce")
            || parameters.insert(name.clone(), value.clone()).is_some()
        {
            return Err(AuthenticationError::Invalid(
                "OAuth parameters contain a reserved or duplicate name",
            ));
        }
    }
    if url
        .query_pairs()
        .any(|(name, _)| parameters.contains_key(name.as_ref()))
    {
        return Err(AuthenticationError::Invalid(
            "OAuth authorization endpoint query contains a parameter Fleet sets",
        ));
    }
    url.query_pairs_mut().extend_pairs(parameters);
    let scope = url
        .query_pairs()
        .find(|(name, _)| name == "scope")
        .map(|(_, scope)| scope.into_owned());
    Ok(Authorization {
        id,
        url: url.into(),
        redirect_uri,
        verifier,
        nonce,
        scope,
        server,
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

/// Parses a redirect URI that uses HTTP on 127.0.0.1.
pub fn loopback(value: &str) -> Result<Url, AuthenticationError> {
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
        || response.expires_in == Some(0)
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
        expires_at: response
            .expires_in
            .map(|expires_in| now().saturating_add(expires_in)),
        scopes,
    })
}

fn endpoint(value: &str) -> Result<Url, AuthenticationError> {
    let url =
        Url::parse(value).map_err(|_| AuthenticationError::Invalid("invalid OAuth endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AuthenticationError::Invalid(
            "OAuth endpoints must use HTTP or HTTPS without credentials",
        ));
    }
    Ok(url)
}

/// Returns whether the provider's HTTP grant allows a discovered URL.
///
/// URLs outside the grant must use HTTPS. Their IP address hosts must be public.
fn granted(url: &Url, origins: &[WasmOrigin]) -> Result<bool, AuthenticationError> {
    let uri = url
        .as_str()
        .parse()
        .map_err(|_| AuthenticationError::Invalid("invalid OAuth endpoint"))?;
    if origins.iter().any(|origin| origin.allows(&uri)) {
        return Ok(true);
    }
    if url.scheme() != "https" {
        return Err(AuthenticationError::Invalid(
            "discovered OAuth servers must use HTTPS unless the provider's HTTP grant allows them",
        ));
    }
    let address = match url.host() {
        Some(Host::Ipv4(address)) => Some(IpAddr::V4(address)),
        Some(Host::Ipv6(address)) => Some(IpAddr::V6(address)),
        _ => None,
    };
    if address.is_some_and(|address| !crate::oauth::public(address)) {
        return Err(non_public());
    }
    Ok(false)
}

/// Returns whether an address is on the public internet.
///
/// Special-purpose IPv4 ranges are not public. IPv6 addresses must be global unicast
/// outside the IETF protocol assignment and documentation ranges. IPv4-mapped and
/// NAT64 addresses are judged by their IPv4 address.
fn public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => !matches!(
            address.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        IpAddr::V6(address) => match (address.to_ipv4_mapped(), address.segments()) {
            (Some(address), _) => crate::oauth::public(address.into()),
            (None, [0x64, 0xff9b, 0, 0, 0, 0, ..]) => {
                let [.., a, b, c, d] = address.octets();
                crate::oauth::public(Ipv4Addr::new(a, b, c, d).into())
            }
            (None, [0x2001, 0..=0x1ff | 0xdb8, ..] | [0x3fff, 0..=0xfff, ..]) => false,
            (None, [first, ..]) => first & 0xe000 == 0x2000,
        },
    }
}

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|address| crate::oauth::public(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(NonPublicAddress.into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl fmt::Display for NonPublicAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("host resolves to no public address")
    }
}

impl Error for NonPublicAddress {}

fn read_json<T: DeserializeOwned>(response: &Response) -> Result<T, AuthenticationError> {
    if !response.is_success() {
        return Err(AuthenticationError::Rejected(response.status));
    }
    serde_json::from_slice(&response.body)
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
    let mut source: Option<&(dyn Error + 'static)> = Some(&error);
    while let Some(error) = source {
        if error.is::<NonPublicAddress>() {
            return non_public();
        }
        source = error.source();
    }
    AuthenticationError::source("OAuth request failed", error.without_url())
}

const fn non_public() -> AuthenticationError {
    AuthenticationError::Invalid(
        "discovered OAuth servers must be at public addresses unless the provider's HTTP grant allows them",
    )
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
            expires_at: Some(0),
            scopes: vec!["invoke".into()],
        };
        let response = TokenResponse {
            access_token: "new".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(3600),
            scope: None,
        };
        let tokens = tokens(response, Some(&previous)).unwrap();

        assert_eq!(tokens.access_token, "new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(tokens.id_token.as_deref(), Some("identity"));
        assert_eq!(tokens.scopes, ["invoke"]);
        assert!(tokens.expires_at.unwrap() > now());
    }

    #[test]
    fn tokens_replace_rotated_refresh_tokens_and_scopes() {
        let previous = Tokens {
            access_token: "old".into(),
            refresh_token: Some("old-refresh".into()),
            id_token: None,
            expires_at: Some(0),
            scopes: vec!["invoke".into()],
        };
        let response = TokenResponse {
            access_token: "new".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("new-refresh".into()),
            id_token: None,
            expires_in: Some(3600),
            scope: Some("openid".into()),
        };
        let tokens = tokens(response, Some(&previous)).unwrap();

        assert_eq!(tokens.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(tokens.scopes, ["openid"]);
    }

    #[test]
    fn tokens_without_a_lifetime_have_no_known_expiry() {
        let response = |expires_in| TokenResponse {
            access_token: "new".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            id_token: None,
            expires_in,
            scope: None,
        };

        assert_eq!(tokens(response(None), None).unwrap().expires_at, None);
        let error = tokens(response(Some(0)), None).err().unwrap();
        assert!(matches!(error, AuthenticationError::Invalid(_)));
    }

    #[test]
    fn endpoints_accept_http_and_https_without_credentials() {
        for url in ["https://example.com/token", "http://example.com:8080/token"] {
            assert_eq!(endpoint(url).unwrap().as_str(), url);
        }

        for url in [
            "/token",
            "ftp://example.com/token",
            "https://user@example.com/token",
            "https://example.com/token#fragment",
        ] {
            let error = endpoint(url).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
    }

    #[test]
    fn authorization_parameters_cannot_replace_reserved_names() {
        let server = Discovered {
            resource: "https://api.example.com/mcp".into(),
            issuer: "https://auth.example.com".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            revocation_endpoint: None,
        };
        let authorize = |parameters: &[(&str, &str)], url: &str| {
            let config = OAuthConfig {
                server: OAuthServer::Resource(server.resource.clone()),
                client_id: "fleet".into(),
                redirect_uri: "http://127.0.0.1:0/callback".into(),
                scopes: Vec::new(),
                parameters: parameters
                    .iter()
                    .map(|(name, value)| ((*name).into(), (*value).into()))
                    .collect(),
                registration: None,
                required_scopes: Vec::new(),
            };
            authorization(
                &config,
                None,
                "http://127.0.0.1:9/callback".parse().unwrap(),
                url.parse().unwrap(),
                BTreeMap::from([("resource".into(), server.resource.clone())]),
                Some(server.clone()),
            )
        };

        let authorization = authorize(
            &[("prompt", "login")],
            "https://auth.example.com/authorize?tenant=a&tenant=b&scope=read",
        )
        .unwrap();
        let url = Url::parse(&authorization.url).unwrap();
        let pairs: Vec<_> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs[..2],
            [("tenant".into(), "a".into()), ("tenant".into(), "b".into())]
        );
        assert!(pairs.contains(&("prompt".into(), "login".into())));
        assert_eq!(authorization.scope.as_deref(), Some("read"));
        assert!(pairs.contains(&("state".into(), authorization.id)));

        for name in ["scope", "nonce", "resource", "state"] {
            let error = authorize(&[(name, "value")], "https://auth.example.com/authorize")
                .err()
                .unwrap();
            assert!(matches!(error, AuthenticationError::Invalid(_)), "{name}");
        }
        for query in [
            "state=value",
            "resource=value",
            "client_id=value",
            "prompt=none",
        ] {
            let url = format!("https://auth.example.com/authorize?{query}");
            let error = authorize(&[("prompt", "login")], &url).err().unwrap();
            assert!(matches!(error, AuthenticationError::Invalid(_)), "{query}");
        }
    }

    #[test]
    fn addresses_outside_the_public_internet_are_not_public() {
        for address in [
            "93.184.215.14",
            "100.63.255.255",
            "100.128.0.0",
            "172.32.0.1",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:93.184.215.14",
            "64:ff9b::93.184.215.14",
        ] {
            assert!(public(address.parse().unwrap()), "{address}");
        }
        for address in [
            "0.0.0.0",
            "0.1.2.3",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:100.64.0.1",
            "::ffff:0.0.0.0",
            "64:ff9b::10.0.0.1",
            "::127.0.0.1",
            "fdf::1",
            "fec0::1",
            "2001:db8::1",
            "2001::1",
            "2001:2::1",
            "3fff::1",
        ] {
            assert!(!public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn discovered_urls_need_public_https_or_the_grant() {
        let grant = [
            "http://127.0.0.1:8080".parse::<WasmOrigin>().unwrap(),
            "https://10.0.0.1".parse().unwrap(),
            "http://auth.internal".parse().unwrap(),
        ];
        let granted = |url: &str, origins| super::granted(&url.parse().unwrap(), origins);

        assert!(!granted("https://auth.example.com/token", &grant).unwrap());
        assert!(!granted("https://93.184.215.14/token", &[]).unwrap());
        for url in [
            "http://127.0.0.1:8080/token",
            "https://10.0.0.1/token",
            "http://auth.internal/token",
        ] {
            assert!(granted(url, &grant).unwrap(), "{url}");
            let error = granted(url, &[]).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }
        for url in [
            "http://auth.example.com/token",
            "http://127.0.0.1:8081/token",
            "https://127.0.0.1:8080/token",
            "https://10.0.0.2/token",
            "https://[::1]/token",
            "https://[::ffff:192.168.0.1]/token",
            "https://169.254.169.254/latest",
        ] {
            let error = granted(url, &grant).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)), "{url}");
        }
    }

    #[test]
    fn hosts_resolving_to_private_addresses_are_refused() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let oauth = OAuth::new().unwrap();

        let error = runtime
            .block_on(oauth.send("https://localhost:9/token", None, Reach::Public(&[])))
            .unwrap_err();

        assert_eq!(error.to_string(), non_public().to_string());
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
