use alloc::{boxed::Box, string::String, vec::Vec};

/// A named connection to an external service.
#[derive(Clone, Debug)]
pub struct Connection {
    /// The connection name within its provider.
    pub name: String,
    /// The display name.
    pub title: String,
    /// A description shown with the connection.
    pub description: Option<String>,
    /// Where the user can manage the connection's usage.
    pub manage_url: Option<String>,
    /// The supported sign-in methods.
    pub methods: Vec<AuthenticationMethod>,
}

/// A way to authenticate a connection.
#[derive(Clone, Debug)]
pub struct AuthenticationMethod {
    /// The method name within its connection.
    pub name: String,
    /// The display name.
    pub title: String,
    /// How credentials are obtained.
    pub kind: AuthenticationKind,
}

/// How credentials are obtained.
#[derive(Clone, Debug)]
pub enum AuthenticationKind {
    /// A key supplied by the user.
    ApiKey,
    /// OAuth authorization with PKCE.
    OAuth(Box<OAuthConfig>),
}

/// Settings for OAuth authorization with PKCE.
#[derive(Clone, Debug)]
pub struct OAuthConfig {
    /// Where the authorization server comes from.
    pub server: OAuthServer,
    /// How Fleet is identified to the authorization server.
    pub client: OAuthClient,
    /// The requested scopes. Without any, a protected resource's advertised
    /// scopes are requested. Scopes a service asks for when rejecting a request
    /// take precedence.
    pub scopes: Vec<String>,
    /// Additional authorization parameters.
    pub parameters: Vec<(String, String)>,
    /// Scopes that must be granted before credentials can be used.
    pub required_scopes: Vec<String>,
}

/// Where the authorization server comes from.
#[derive(Clone, Debug)]
pub enum OAuthServer {
    /// Endpoints known in advance.
    Endpoints(OAuthEndpoints),
    /// A protected resource. Fleet discovers its authorization server.
    ///
    /// Discovered servers must use HTTPS at public addresses unless the
    /// provider's HTTP grant allows their origin.
    Resource(String),
}

/// Endpoints of an authorization server known in advance.
///
/// Endpoints should use HTTPS unless the network is trusted.
#[derive(Clone, Debug)]
pub struct OAuthEndpoints {
    /// The authorization endpoint.
    pub authorization_url: String,
    /// The token endpoint.
    pub token_url: String,
    /// The API resource requested during authorization and renewal.
    pub resource: Option<String>,
    /// The identity provider's issuer URL.
    pub issuer: Option<String>,
}

/// How Fleet is identified to the authorization server.
#[derive(Clone, Debug)]
pub enum OAuthClient {
    /// A client registered in advance.
    Registered(OAuthRegisteredClient),
    /// Fleet identifies itself. Requires a protected resource server.
    Automatic,
}

/// A client registered with the authorization server in advance.
#[derive(Clone, Debug)]
pub struct OAuthRegisteredClient {
    /// The public client ID.
    pub client_id: String,
    /// The registered redirect URI. Port 0 means any port is registered.
    pub redirect_uri: String,
    /// Client registration completed during authorization.
    pub registration: Option<OAuthRegistration>,
}

/// Client registration completed during authorization.
#[derive(Clone, Debug)]
pub struct OAuthRegistration {
    /// The callback parameter containing the issued client ID.
    pub client_id_parameter: String,
    /// Authorization parameters used only for a new registration.
    pub parameters: Vec<(String, String)>,
}

/// Credentials for a connection.
#[derive(Clone)]
pub enum Credential {
    /// A key supplied by the user.
    ApiKey(String),
    /// An OAuth access token and optional identity token.
    OAuth {
        /// The access token.
        access_token: String,
        /// The identity token returned by the provider.
        id_token: Option<String>,
    },
}

/// Provides credentials for named connections.
pub trait Credentials {
    /// An error returned by the provider.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Returns current credentials, refreshing expired tokens if needed.
    fn get(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Option<Credential>, Self::Error>> + Send;

    /// Reports that the service rejected a request.
    ///
    /// `token` is the access token the request carried, if any. `challenge` is the value
    /// of the response's `WWW-Authenticate` header, if any. Returns whether to retry with
    /// the credentials from [`get`](Self::get).
    fn reject(
        &self,
        name: &str,
        token: Option<&str>,
        challenge: Option<&str>,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
}
