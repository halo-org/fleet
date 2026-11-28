use crate::{
    AuthenticationError, CredentialStore, IdleHandler, SandboxOAuth,
    oauth::{Authorization, Discovered, OAuth, Registration, Server, Tokens},
    oauth_client::AutomaticClient,
};
use core::{pin::pin, time::Duration};
use fleet_kit::{
    AuthenticationKind, Connection, Credential, Credentials, OAuthClient, OAuthConfig,
    OAuthRegisteredClient, OAuthServer,
};
use fleet_wasm::WasmOrigin;
use futures_util::future::{self, Either};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, RwLock},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
};
use url::Url;

/// Authentication for configured provider connections.
#[derive(Clone)]
pub struct Authentication {
    state: Arc<State>,
}

pub struct ProviderCredentials {
    authentication: Authentication,
    namespace: String,
}

struct State {
    store: CredentialStore,
    oauth: OAuth,
    client: Option<AutomaticClient>,
    host_id: String,
    connections: RwLock<BTreeMap<String, Arc<ConnectionState>>>,
    hosted: std::sync::Mutex<BTreeMap<String, String>>,
    idle_handler: IdleHandler,
}

struct ConnectionState {
    provider: String,
    origins: Vec<WasmOrigin>,
    definition: Connection,
    session: Mutex<Session>,
}

struct Session {
    record: Record,
    dirty: bool,
    authorization: Option<PendingAuthorization>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Record {
    registrations: BTreeMap<String, Registration>,
    credential: Option<SavedCredential>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    clients: BTreeMap<String, Registration>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedCredential {
    method: String,
    secret: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server: Option<Discovered>,
}

#[derive(Clone, Serialize, Deserialize)]
enum Secret {
    ApiKey(String),
    OAuth(Tokens),
}

struct PendingAuthorization {
    info: AuthorizationInfo,
    attempt: Option<Attempt>,
}

struct Attempt {
    method: String,
    requested: Option<Url>,
    authorization: Authorization,
    identity: Option<Registration>,
    _cancelled: oneshot::Sender<()>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizationInfo {
    pub id: String,
    pub url: String,
    pub redirect_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    pub status: AuthorizationStatus,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Serialize)]
pub struct ConnectionInfo {
    pub id: String,
    pub provider: String,
    pub title: String,
    pub description: Option<String>,
    pub manage_url: Option<String>,
    pub connected: bool,
    pub email: Option<String>,
    pub methods: Vec<MethodInfo>,
}

#[derive(Serialize)]
pub struct MethodInfo {
    pub name: String,
    pub title: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl Authentication {
    /// Opens protected credential storage.
    ///
    /// Without an automatic client, automatic sign-in fails.
    pub async fn open(
        directory: &Path,
        client: Option<AutomaticClient>,
        idle_handler: IdleHandler,
    ) -> Result<Self, AuthenticationError> {
        Self::new(
            CredentialStore::open(directory)?,
            OAuth::new()?,
            client,
            idle_handler,
        )
        .await
    }

    /// Keeps credentials in memory and signs in only through the sandbox's OAuth provider.
    pub async fn sandbox(
        provider: SandboxOAuth,
        client: Option<AutomaticClient>,
        idle_handler: IdleHandler,
    ) -> Result<Self, AuthenticationError> {
        Self::new(
            CredentialStore::memory()?,
            OAuth::sandbox(provider),
            client,
            idle_handler,
        )
        .await
    }

    async fn new(
        store: CredentialStore,
        oauth: OAuth,
        client: Option<AutomaticClient>,
        idle_handler: IdleHandler,
    ) -> Result<Self, AuthenticationError> {
        let host_id = if let Some(id) = store.get("host_id")? {
            id
        } else {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes)
                .map_err(|_| AuthenticationError::Invalid("could not generate host ID"))?;
            let value = (u128::from_be_bytes(bytes) & 0xffff_ffff_ffff_0fff_3fff_ffff_ffff_ffff)
                | 0x0000_0000_0000_4000_8000_0000_0000_0000;
            let mut id = String::from("urn:uuid:");
            for (index, digit) in format!("{value:032x}").chars().enumerate() {
                if matches!(index, 8 | 12 | 16 | 20) {
                    id.push('-');
                }
                id.push(digit);
            }
            store.put("host_id".into(), &id).await?;
            id
        };
        Ok(Self {
            state: Arc::new(State {
                store,
                oauth,
                client,
                host_id,
                connections: RwLock::new(BTreeMap::new()),
                hosted: std::sync::Mutex::default(),
                idle_handler,
            }),
        })
    }

    /// Returns this installation's client ID metadata document, if it has one.
    pub fn client_metadata(&self) -> Option<serde_json::Value> {
        self.state.client.as_ref()?.document()
    }

    /// Returns this installation's persistent host ID.
    pub fn host_id(&self) -> &str {
        &self.state.host_id
    }

    /// Restricts credential access to one provider.
    pub fn scope(&self, namespace: &str) -> ProviderCredentials {
        ProviderCredentials {
            authentication: self.clone(),
            namespace: namespace.into(),
        }
    }

    /// Registers a provider's connections.
    ///
    /// The provider's HTTP grant allows discovered servers beyond public HTTPS. Automatic
    /// clients require a protected resource server.
    pub fn register(
        &self,
        namespace: &str,
        origins: &[WasmOrigin],
        definitions: Vec<Connection>,
    ) -> Result<(), AuthenticationError> {
        let mut connections = self
            .state
            .connections
            .write()
            .expect("connection lock is not poisoned");
        for definition in definitions {
            if !valid_name(namespace)
                || !valid_name(&definition.name)
                || definition.methods.is_empty()
            {
                return Err(AuthenticationError::Invalid(
                    "invalid connection definition",
                ));
            }
            let mut names = std::collections::BTreeSet::new();
            for method in &definition.methods {
                if !valid_name(&method.name) || !names.insert(&method.name) {
                    return Err(AuthenticationError::Invalid(
                        "invalid or duplicate sign-in method",
                    ));
                }
                if let AuthenticationKind::OAuth(config) = &method.kind
                    && let (OAuthClient::Automatic, OAuthServer::Endpoints(_)) =
                        (&config.client, &config.server)
                {
                    return Err(AuthenticationError::Invalid(
                        "automatic OAuth clients require a protected resource server",
                    ));
                }
            }
            let id = format!("{namespace}:{}", definition.name);
            if connections.contains_key(&id) {
                return Err(AuthenticationError::Invalid("duplicate connection"));
            }
            let record = self.state.store.get(&id)?.unwrap_or_default();
            connections.insert(
                id,
                Arc::new(ConnectionState {
                    provider: namespace.into(),
                    origins: origins.to_vec(),
                    definition,
                    session: Mutex::new(Session {
                        record,
                        dirty: false,
                        authorization: None,
                    }),
                }),
            );
        }
        drop(connections);
        Ok(())
    }

    /// Lists connections without exposing credentials.
    pub async fn connections(&self) -> Vec<ConnectionInfo> {
        let connections = self
            .state
            .connections
            .read()
            .expect("connection lock is not poisoned")
            .clone();
        let mut result = Vec::new();
        for (id, connection) in connections {
            let session = connection.session.lock().await;
            let credential = session.record.credential.as_ref();
            let email = credential
                .and_then(|credential| session.record.registrations.get(&credential.method))
                .and_then(|registration| registration.email.clone());
            let connected = credential.is_some_and(|credential| match &credential.secret {
                Secret::ApiKey(_) => true,
                Secret::OAuth(tokens) => {
                    oauth_config(&connection, &credential.method).is_ok_and(|config| {
                        authorized(config, tokens)
                            && server(&connection, config, credential.server.as_ref()).is_ok()
                    })
                }
            });
            drop(session);
            result.push(ConnectionInfo {
                id,
                provider: connection.provider.clone(),
                title: connection.definition.title.clone(),
                description: connection.definition.description.clone(),
                manage_url: connection.definition.manage_url.clone(),
                connected,
                email,
                methods: connection
                    .definition
                    .methods
                    .iter()
                    .map(|method| MethodInfo {
                        name: method.name.clone(),
                        title: method.title.clone(),
                        kind: match method.kind {
                            AuthenticationKind::ApiKey => "api_key",
                            AuthenticationKind::OAuth(_) => "oauth",
                        },
                        redirect_uri: match &method.kind {
                            AuthenticationKind::OAuth(config) => match &config.client {
                                OAuthClient::Registered(client) => {
                                    Some(client.redirect_uri.clone())
                                }
                                OAuthClient::Automatic => None,
                            },
                            AuthenticationKind::ApiKey => None,
                        },
                        resource: match &method.kind {
                            AuthenticationKind::OAuth(config) => match &config.server {
                                OAuthServer::Resource(resource) => Some(resource.clone()),
                                OAuthServer::Endpoints(_) => None,
                            },
                            AuthenticationKind::ApiKey => None,
                        },
                    })
                    .collect(),
            });
        }
        result
    }

    /// Starts a browser authorization.
    ///
    /// A client that receives the callback at a `requested` redirect URI delivers it with
    /// [`callback`](Self::callback). Without one, Fleet also listens at the registered URI.
    /// Automatic clients take no `requested` URI, since Fleet receives their callback with
    /// [`hosted_callback`](Self::hosted_callback). A pending attempt with the same method
    /// and redirect URI is returned instead. A protected resource's server is discovered,
    /// and an automatic client identified, without blocking the connection.
    pub async fn authorize(
        &self,
        id: &str,
        method: &str,
        requested: Option<&str>,
    ) -> Result<AuthorizationInfo, AuthenticationError> {
        let guard = self
            .state
            .idle_handler
            .track_mutation()
            .ok_or(AuthenticationError::Cancelled)?;
        let connection = self.connection(id)?;
        let config = oauth_config(&connection, method)?;
        let mut redirect_uri = match (&config.client, requested) {
            (OAuthClient::Registered(client), _) => {
                crate::oauth::redirect_uri(&client.redirect_uri, requested)?
            }
            (OAuthClient::Automatic, None) => self.automatic_client()?.redirect_uri().clone(),
            (OAuthClient::Automatic, Some(_)) => {
                return Err(AuthenticationError::Invalid(
                    "automatic OAuth clients use Fleet's own redirect URI",
                ));
            }
        };
        let requested = requested.map(|_| redirect_uri.clone());
        let mut session = connection.session.lock().await;
        self.flush(id, &mut session).await?;
        if let Some(info) = pending(&session, method, requested.as_ref()) {
            return Ok(info);
        }
        let stored = session.record.clients.get(method).cloned();
        drop(session);
        let metadata = self
            .state
            .oauth
            .discover(config, &connection.origins)
            .await?;
        let identity = match (&config.client, &metadata) {
            (OAuthClient::Automatic, Some(metadata)) => {
                let (client, origins) = (self.automatic_client()?, &connection.origins);
                let identity =
                    self.state
                        .oauth
                        .identify(client, metadata, stored.as_ref(), origins);
                Some(identity.await?)
            }
            _ => None,
        };
        let mut session = connection.session.lock().await;
        let identity = identity
            .map(|identity| session.keep_client(method, stored.as_ref(), identity))
            .transpose()?;
        self.flush(id, &mut session).await?;
        if let Some(info) = pending(&session, method, requested.as_ref()) {
            return Ok(info);
        }
        let listener = if let (OAuthClient::Registered(_), None) = (&config.client, &requested) {
            Some(crate::oauth_callback::bind(&mut redirect_uri).await?)
        } else {
            None
        };
        let authorization = self.state.oauth.authorize(
            config,
            metadata,
            identity
                .as_ref()
                .or_else(|| session.record.registrations.get(method)),
            redirect_uri,
        )?;
        let info = AuthorizationInfo {
            id: authorization.id.clone(),
            url: authorization.url.clone(),
            redirect_uri: authorization.redirect_uri.to_string(),
            issuer: authorization
                .server
                .as_ref()
                .map(|server| server.issuer.clone()),
            status: AuthorizationStatus::Pending,
            error: None,
        };
        if identity.is_some() {
            self.hosted().insert(info.id.clone(), id.to_owned());
        }
        let callback = listener.map(|listener| (listener, authorization.redirect_uri.clone()));
        let (cancelled, cancellation) = oneshot::channel();
        session.authorization = Some(PendingAuthorization {
            info: info.clone(),
            attempt: Some(Attempt {
                method: method.to_owned(),
                requested,
                authorization,
                identity,
                _cancelled: cancelled,
            }),
        });
        drop(session);
        let authentication = self.clone();
        let id = id.to_owned();
        let authorization_id = info.id.clone();
        tokio::spawn(async move {
            let _guard = guard;
            authentication
                .wait(&id, &authorization_id, callback, cancellation)
                .await;
        });
        Ok(info)
    }

    /// Completes an authorization with the callback URL a client received.
    ///
    /// Returns the result of an attempt that already finished.
    pub async fn callback(
        &self,
        id: &str,
        authorization_id: &str,
        url: &str,
    ) -> Result<AuthorizationInfo, AuthenticationError> {
        let connection = self.connection(id)?;
        let mut session = connection.session.lock().await;
        let pending = session
            .authorization
            .as_ref()
            .filter(|pending| pending.info.id == authorization_id)
            .ok_or(AuthenticationError::Cancelled)?;
        let Some(attempt) = &pending.attempt else {
            return Ok(pending.info.clone());
        };
        let parameters = crate::oauth_callback::received(
            &attempt.authorization.redirect_uri,
            url,
            authorization_id,
        )?;
        let info = self
            .receive(id, &connection, &mut session, Ok(parameters))
            .await;
        drop(session);
        Ok(info)
    }

    /// Completes an automatic client's authorization with a callback Fleet received.
    ///
    /// Returns nothing unless the callback's state names a pending attempt whose redirect
    /// URI is Fleet's own. Only such a callback counts as activity.
    pub async fn hosted_callback(&self, query: Option<&str>) -> Option<AuthorizationInfo> {
        let (_, state) = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
            .find(|(name, _)| name == "state")?;
        let id = self.hosted().get(state.as_ref())?.clone();
        let redirect_uri = self.state.client.as_ref()?.redirect_uri();
        let connection = self.connection(&id).ok()?;
        let mut session = connection.session.lock().await;
        session
            .authorization
            .as_ref()
            .filter(|pending| pending.info.id == state)
            .and_then(|pending| pending.attempt.as_ref())
            .filter(|attempt| attempt.authorization.redirect_uri == *redirect_uri)?;
        let parameters = crate::oauth_callback::parameters(query, &state).ok()?;
        let _guard = self.state.idle_handler.track_mutation()?;
        let info = self
            .receive(&id, &connection, &mut session, Ok(parameters))
            .await;
        drop(session);
        Some(info)
    }

    async fn wait(
        &self,
        id: &str,
        authorization_id: &str,
        callback: Option<(TcpListener, Url)>,
        cancellation: oneshot::Receiver<()>,
    ) {
        let parameters = {
            let listener = async {
                match &callback {
                    Some((listener, redirect_uri)) => {
                        crate::oauth_callback::wait(listener, redirect_uri, authorization_id).await
                    }
                    None => future::pending().await,
                }
            };
            let expiry = tokio::time::timeout(Duration::from_secs(600), listener);
            let shutdown = self.state.idle_handler.wait_for_shutdown();
            match future::select(
                pin!(cancellation),
                pin!(future::select(pin!(expiry), pin!(shutdown))),
            )
            .await
            {
                Either::Left(_) => None,
                Either::Right((Either::Left((Ok(parameters), _)), _)) => Some(parameters),
                Either::Right(_) => Some(Err(AuthenticationError::Cancelled)),
            }
        };
        drop(callback);
        self.hosted().remove(authorization_id);
        let Some(parameters) = parameters else {
            return;
        };
        let Ok(connection) = self.connection(id) else {
            return;
        };
        let mut session = connection.session.lock().await;
        if session
            .authorization
            .as_ref()
            .is_some_and(|pending| pending.info.id == authorization_id)
        {
            self.receive(id, &connection, &mut session, parameters)
                .await;
        }
    }

    async fn receive(
        &self,
        id: &str,
        connection: &ConnectionState,
        session: &mut Session,
        parameters: Result<BTreeMap<String, String>, AuthenticationError>,
    ) -> AuthorizationInfo {
        let pending = session
            .authorization
            .as_mut()
            .expect("received authorization is pending");
        let Some(attempt) = pending.attempt.take() else {
            return pending.info.clone();
        };
        let result = match parameters {
            Ok(parameters) => {
                self.complete(id, connection, session, &attempt, &parameters)
                    .await
            }
            Err(error) => Err(error),
        };
        let pending = session
            .authorization
            .as_mut()
            .expect("received authorization is pending");
        pending.info.status = if result.is_ok() {
            AuthorizationStatus::Complete
        } else {
            AuthorizationStatus::Failed
        };
        pending.info.error = result.err().map(|error| error.to_string());
        pending.info.clone()
    }

    async fn complete(
        &self,
        id: &str,
        connection: &ConnectionState,
        session: &mut Session,
        attempt: &Attempt,
        parameters: &BTreeMap<String, String>,
    ) -> Result<(), AuthenticationError> {
        let config = oauth_config(connection, &attempt.method)?;
        let server = server(connection, config, attempt.authorization.server.as_ref())?;
        match (parameters.get("iss"), server.issuer()) {
            (Some(issuer), expected) if expected != Some(issuer.as_str()) => {
                return Err(AuthenticationError::Invalid(
                    "callback issuer does not match",
                ));
            }
            (None, Some(_)) if self.state.oauth.requires_issuer_parameter(server).await? => {
                return Err(AuthenticationError::Invalid("callback issuer is missing"));
            }
            _ => {}
        }
        if parameters.contains_key("error") {
            return Err(AuthenticationError::Invalid("authorization was declined"));
        }
        let code = parameters
            .get("code")
            .filter(|code| !code.is_empty())
            .ok_or(AuthenticationError::Invalid(
                "authorization code is missing",
            ))?;
        let registration = match (&config.client, &attempt.identity) {
            (OAuthClient::Registered(client), _) => registration(
                client,
                session
                    .record
                    .registrations
                    .get(&attempt.method)
                    .filter(|registration| {
                        registration.issued_by(attempt.authorization.server.as_ref())
                    }),
                parameters,
                attempt.authorization.server.as_ref(),
            )?,
            (OAuthClient::Automatic, identity) => identity.clone().ok_or(
                AuthenticationError::Invalid("OAuth client was not identified"),
            )?,
        };
        if let OAuthClient::Registered(_) = config.client
            && session
                .record
                .credential
                .as_ref()
                .is_none_or(|credential| credential.method != attempt.method)
        {
            session
                .record
                .registrations
                .insert(attempt.method.clone(), registration.clone());
            session.dirty = true;
            self.flush(id, session).await?;
        }
        let mut registration = registration;
        let result = self
            .state
            .oauth
            .exchange(server, &mut registration, &attempt.authorization, code)
            .await;
        if matches!(result, Err(AuthenticationError::ClientRejected)) {
            session.reject_client(&attempt.method, &registration);
            self.flush(id, session).await?;
        }
        let tokens = result?;
        let authorized = authorized(config, &tokens);
        session
            .record
            .registrations
            .insert(attempt.method.clone(), registration);
        session.record.credential = Some(SavedCredential {
            method: attempt.method.clone(),
            secret: Secret::OAuth(tokens),
            server: attempt.authorization.server.clone(),
        });
        session.dirty = true;
        self.flush(id, session).await?;
        if !authorized {
            return Err(AuthenticationError::Unauthorized);
        }
        Ok(())
    }

    /// Reads an authorization attempt.
    pub async fn authorization(
        &self,
        id: &str,
        authorization_id: &str,
    ) -> Result<AuthorizationInfo, AuthenticationError> {
        let connection = self.connection(id)?;
        let session = connection.session.lock().await;
        session
            .authorization
            .as_ref()
            .filter(|pending| pending.info.id == authorization_id)
            .map(|pending| pending.info.clone())
            .ok_or(AuthenticationError::NotFound)
    }

    /// Saves an API key.
    pub async fn set_key(
        &self,
        id: &str,
        method: &str,
        key: String,
    ) -> Result<(), AuthenticationError> {
        let connection = self.connection(id)?;
        if key.trim().is_empty()
            || !connection.definition.methods.iter().any(|candidate| {
                candidate.name == method && matches!(candidate.kind, AuthenticationKind::ApiKey)
            })
        {
            return Err(AuthenticationError::Invalid(
                "invalid API key or sign-in method",
            ));
        }
        let mut session = connection.session.lock().await;
        session.authorization = None;
        session.record.credential = Some(SavedCredential {
            method: method.into(),
            secret: Secret::ApiKey(key),
            server: None,
        });
        session.dirty = true;
        let result = self.flush(id, &mut session).await;
        drop(session);
        result
    }

    /// Signs out while retaining the client registration.
    pub async fn sign_out(&self, id: &str) -> Result<bool, AuthenticationError> {
        let connection = self.connection(id)?;
        let mut session = connection.session.lock().await;
        session.authorization = None;
        self.flush(id, &mut session).await?;
        let revoked = if let Some(SavedCredential {
            method,
            secret: Secret::OAuth(tokens),
            server: discovered,
        }) = &session.record.credential
        {
            let config = oauth_config(&connection, method)?;
            let registration = session
                .record
                .registrations
                .get(method)
                .ok_or(AuthenticationError::Unauthorized)?;
            match server(&connection, config, discovered.as_ref()) {
                Ok(server) => self
                    .state
                    .oauth
                    .revoke(server, registration, tokens)
                    .await
                    .unwrap_or(false),
                Err(_) => false,
            }
        } else {
            true
        };
        session.record.credential = None;
        session.dirty = true;
        self.flush(id, &mut session).await?;
        drop(session);
        Ok(revoked)
    }

    fn automatic_client(&self) -> Result<&AutomaticClient, AuthenticationError> {
        self.state
            .client
            .as_ref()
            .ok_or(AuthenticationError::Invalid(
                "automatic sign-in needs public_url when Fleet listens on port 0",
            ))
    }

    /// Returns the connections of pending attempts whose callbacks Fleet receives.
    fn hosted(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.state
            .hosted
            .lock()
            .expect("hosted attempt lock is not poisoned")
    }

    fn connection(&self, id: &str) -> Result<Arc<ConnectionState>, AuthenticationError> {
        self.state
            .connections
            .read()
            .expect("connection lock is not poisoned")
            .get(id)
            .cloned()
            .ok_or(AuthenticationError::NotFound)
    }

    async fn flush(&self, id: &str, session: &mut Session) -> Result<(), AuthenticationError> {
        if session.dirty {
            self.state.store.put(id.into(), &session.record).await?;
            session.dirty = false;
        }
        Ok(())
    }

    async fn credential(
        &self,
        id: &str,
        rejected: Option<&str>,
    ) -> Result<Option<Credential>, AuthenticationError> {
        let connection = self.connection(id)?;
        let mut session = connection.session.lock().await;
        self.flush(id, &mut session).await?;
        let Some(credential) = session.record.credential.clone() else {
            return Ok(None);
        };
        match credential.secret {
            Secret::ApiKey(key) => Ok(Some(Credential::ApiKey(key))),
            Secret::OAuth(mut tokens) => {
                let config = oauth_config(&connection, &credential.method)?;
                let server = server(&connection, config, credential.server.as_ref())?;
                if !authorized(config, &tokens) {
                    return Err(AuthenticationError::Unauthorized);
                }
                if tokens
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= crate::oauth::now().saturating_add(60))
                    || rejected == Some(tokens.access_token.as_str())
                {
                    let mut registration = session
                        .record
                        .registrations
                        .get(&credential.method)
                        .cloned()
                        .ok_or(AuthenticationError::Unauthorized)?;
                    let result = self
                        .state
                        .oauth
                        .refresh(server, &mut registration, &tokens)
                        .await;
                    if matches!(result, Err(AuthenticationError::ClientRejected)) {
                        session.reject_client(&credential.method, &registration);
                        self.flush(id, &mut session).await?;
                    }
                    tokens = result?;
                    session
                        .record
                        .registrations
                        .insert(credential.method.clone(), registration);
                    session.record.credential = Some(SavedCredential {
                        method: credential.method.clone(),
                        secret: Secret::OAuth(tokens.clone()),
                        server: credential.server.clone(),
                    });
                    session.dirty = true;
                    self.flush(id, &mut session).await?;
                    if !authorized(config, &tokens) {
                        return Err(AuthenticationError::Unauthorized);
                    }
                }
                drop(session);
                Ok(Some(Credential::OAuth {
                    access_token: tokens.access_token,
                    id_token: tokens.id_token,
                }))
            }
        }
    }
}

impl Session {
    /// Returns the dynamic registration to use for a method, keeping a new one.
    ///
    /// A registration kept for the same issuer and redirect URI takes precedence. Reusing
    /// the `stored` registration an attempt read before discovery is cancelled if it has
    /// been discarded since.
    fn keep_client(
        &mut self,
        method: &str,
        stored: Option<&Registration>,
        identity: Registration,
    ) -> Result<Registration, AuthenticationError> {
        if identity.redirect_uri.is_none() {
            return Ok(identity);
        }
        let kept = self.record.clients.get(method);
        if stored == Some(&identity) && kept != stored {
            return Err(AuthenticationError::Cancelled);
        }
        if let Some(client) = kept
            && client.issuer == identity.issuer
            && client.redirect_uri == identity.redirect_uri
        {
            return Ok(client.clone());
        }
        self.record
            .clients
            .insert(method.to_owned(), identity.clone());
        self.dirty = true;
        Ok(identity)
    }

    /// Discards a dynamic registration that the authorization server rejected, and any
    /// pending attempt that uses it.
    fn reject_client(&mut self, method: &str, rejected: &Registration) {
        let matches = |client: &Registration| {
            client.client_id == rejected.client_id
                && client.issuer == rejected.issuer
                && client.redirect_uri == rejected.redirect_uri
        };
        if self.record.clients.get(method).is_some_and(matches) {
            self.record.clients.remove(method);
            self.dirty = true;
        }
        if self
            .authorization
            .as_ref()
            .and_then(|pending| pending.attempt.as_ref())
            .is_some_and(|attempt| {
                attempt.method == method && attempt.identity.as_ref().is_some_and(matches)
            })
        {
            self.authorization = None;
        }
    }
}

impl Credentials for ProviderCredentials {
    type Error = AuthenticationError;

    async fn get(&self, name: &str) -> Result<Option<Credential>, Self::Error> {
        let guard = self
            .authentication
            .state
            .idle_handler
            .track_mutation()
            .ok_or(AuthenticationError::Cancelled)?;
        let authentication = self.authentication.clone();
        let id = format!("{}:{name}", self.namespace);
        crate::authentication::finish(async move {
            let _guard = guard;
            authentication.credential(&id, None).await
        })
        .await
    }

    async fn refresh(&self, name: &str, rejected: &str) -> Result<bool, Self::Error> {
        let guard = self
            .authentication
            .state
            .idle_handler
            .track_mutation()
            .ok_or(AuthenticationError::Cancelled)?;
        let authentication = self.authentication.clone();
        let id = format!("{}:{name}", self.namespace);
        let rejected = rejected.to_owned();
        crate::authentication::finish(async move {
            let _guard = guard;
            Ok(matches!(authentication.credential(&id, Some(&rejected)).await?, Some(Credential::OAuth { access_token, .. }) if access_token != rejected))
        }).await
    }
}

pub async fn finish<T: Send + 'static>(operation: impl Future<Output = T> + Send + 'static) -> T {
    tokio::spawn(operation).await.unwrap_or_else(|error| {
        let panic = error
            .try_into_panic()
            .expect("authentication task was not cancelled");
        std::panic::resume_unwind(panic)
    })
}

fn oauth_config<'a>(
    connection: &'a ConnectionState,
    method: &str,
) -> Result<&'a OAuthConfig, AuthenticationError> {
    connection
        .definition
        .methods
        .iter()
        .find_map(|candidate| match &candidate.kind {
            AuthenticationKind::OAuth(config) if candidate.name == method => Some(config.as_ref()),
            _ => None,
        })
        .ok_or(AuthenticationError::NotFound)
}

/// Returns the server a credential's tokens belong to.
fn server<'a>(
    connection: &'a ConnectionState,
    config: &'a OAuthConfig,
    discovered: Option<&'a Discovered>,
) -> Result<Server<'a>, AuthenticationError> {
    match (&config.server, discovered) {
        (OAuthServer::Endpoints(endpoints), None) => Ok(Server::Declared(endpoints)),
        (OAuthServer::Resource(resource), Some(discovered)) if *resource == discovered.resource => {
            Ok(Server::Discovered(discovered, &connection.origins))
        }
        _ => Err(AuthenticationError::Unauthorized),
    }
}

fn authorized(config: &OAuthConfig, tokens: &Tokens) -> bool {
    config
        .required_scopes
        .iter()
        .all(|scope| tokens.scopes.contains(scope))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn registration(
    config: &OAuthRegisteredClient,
    existing: Option<&Registration>,
    parameters: &BTreeMap<String, String>,
    server: Option<&Discovered>,
) -> Result<Registration, AuthenticationError> {
    let issued = config
        .registration
        .as_ref()
        .and_then(|registration| parameters.get(&registration.client_id_parameter));
    if let Some(existing) = existing {
        if issued.is_some_and(|issued| issued != &existing.client_id) {
            return Err(AuthenticationError::Invalid(
                "client registration does not match",
            ));
        }
        return Ok(existing.clone());
    }
    let client_id = if config.registration.is_some() {
        issued
            .filter(|id| !id.is_empty() && *id != &config.client_id)
            .cloned()
            .ok_or(AuthenticationError::Invalid("issued client ID is missing"))?
    } else {
        config.client_id.clone()
    };
    Ok(Registration {
        client_id,
        issuer: server.map(|server| server.issuer.clone()),
        ..Registration::default()
    })
}

fn pending(session: &Session, method: &str, requested: Option<&Url>) -> Option<AuthorizationInfo> {
    let pending = session.authorization.as_ref()?;
    let attempt = pending.attempt.as_ref()?;
    (attempt.method == method && attempt.requested.as_ref() == requested)
        .then(|| pending.info.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_kit::{AuthenticationMethod, OAuthEndpoints, OAuthRegistration};
    use http_body_util::{BodyExt as _, Full};
    use hyper::{
        Request, Response, StatusCode,
        body::{Bytes, Incoming},
        header,
    };
    use hyper_util::rt::TokioIo;
    use serde_json::{Value, json};

    type Requests = Arc<std::sync::Mutex<Vec<(String, BTreeMap<String, String>)>>>;

    fn connection() -> Connection {
        Connection {
            name: "account".into(),
            title: "Account".into(),
            description: None,
            manage_url: None,
            methods: vec![AuthenticationMethod {
                name: "key".into(),
                title: "API key".into(),
                kind: AuthenticationKind::ApiKey,
            }],
        }
    }

    fn client() -> OAuthRegisteredClient {
        OAuthRegisteredClient {
            client_id: "register".into(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            registration: Some(OAuthRegistration {
                client_id_parameter: "client_id".into(),
                parameters: vec![],
            }),
        }
    }

    fn config() -> OAuthConfig {
        OAuthConfig {
            server: OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: "https://example.com/authorize".into(),
                token_url: "https://example.com/token".into(),
                resource: None,
                issuer: None,
            }),
            client: OAuthClient::Registered(client()),
            scopes: vec![],
            parameters: vec![],
            required_scopes: vec![],
        }
    }

    fn resource(name: &str, url: String, scopes: &[&str]) -> AuthenticationMethod {
        let client = OAuthClient::Registered(OAuthRegisteredClient {
            client_id: "fleet".into(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            registration: None,
        });
        method(name, OAuthServer::Resource(url), client, scopes)
    }

    fn automatic(name: &str, url: String) -> AuthenticationMethod {
        method(
            name,
            OAuthServer::Resource(url),
            OAuthClient::Automatic,
            &[],
        )
    }

    fn method(
        name: &str,
        server: OAuthServer,
        client: OAuthClient,
        scopes: &[&str],
    ) -> AuthenticationMethod {
        AuthenticationMethod {
            name: name.into(),
            title: "Sign in".into(),
            kind: AuthenticationKind::OAuth(Box::new(OAuthConfig {
                server,
                client,
                scopes: scopes.iter().copied().map(Into::into).collect(),
                parameters: vec![],
                required_scopes: vec![],
            })),
        }
    }

    /// Serves protected resources and their authorization servers, recording requests.
    async fn serve() -> (String, Requests) {
        let listener = TcpListener::bind((core::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Requests::default();
        let state = (origin.clone(), requests.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (origin, requests) = state.clone();
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let (origin, requests) = (origin.clone(), requests.clone());
                    async move {
                        let target = request.uri().to_string();
                        if target.ends_with("/slow") {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                        let body = request.into_body().collect().await?.to_bytes();
                        let form = form_urlencoded::parse(&body).into_owned().collect();
                        let mut requests = requests.lock().unwrap();
                        requests.push((target.clone(), form));
                        Ok::<_, hyper::Error>(respond(&origin, &target, requests.len()))
                    }
                });
                tokio::spawn(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service),
                );
            }
        });
        (origin, requests)
    }

    /// Returns how the automatic client's server `name` lets clients identify themselves.
    fn clients(origin: &str, name: &str) -> Option<Value> {
        let register = format!("{origin}/register");
        Some(match name {
            "dcr" => json!({ "registration_endpoint": register }),
            "cimd" => json!({
                "registration_endpoint": register,
                "client_id_metadata_document_supported": true,
            }),
            "remote" => json!({ "registration_endpoint": "http://localhost:9/register" }),
            "secret" => json!({ "registration_endpoint": format!("{origin}/secret") }),
            "rejected" => json!({
                "registration_endpoint": register,
                "token_endpoint": format!("{origin}/rejecting"),
            }),
            _ => return None,
        })
    }

    fn respond(origin: &str, target: &str, count: usize) -> Response<Full<Bytes>> {
        let server = |issuer: String, token_path: &str| {
            json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{origin}/authorize"),
                "token_endpoint": format!("{origin}{token_path}"),
                "revocation_endpoint": format!("{origin}/revoke"),
                "code_challenge_methods_supported": ["S256"],
            })
        };
        if let Some((kind, name)) = target
            .strip_prefix("/.well-known/")
            .and_then(|path| path.split_once('/'))
            && let Some(clients) = clients(origin, name)
        {
            let body = if kind == "oauth-protected-resource" {
                json!({
                    "resource": format!("{origin}/{name}"),
                    "authorization_servers": [format!("{origin}/{name}")],
                })
            } else {
                let mut metadata = server(format!("{origin}/{name}"), "/token");
                for (field, value) in clients.as_object().unwrap() {
                    metadata[field] = value.clone();
                }
                metadata
            };
            return Response::new(Full::new(body.to_string().into()));
        }
        let body = match target {
            "/.well-known/oauth-protected-resource/mcp" => json!({
                "resource": format!("{origin}/mcp"),
                "authorization_servers": [origin],
                "scopes_supported": ["read"],
            }),
            "/.well-known/oauth-protected-resource/bare" => json!({
                "resource": format!("{origin}/bare"),
                "authorization_servers": [origin],
            }),
            "/.well-known/oauth-protected-resource/failing" => json!({
                "resource": format!("{origin}/failing"),
                "authorization_servers": [format!("{origin}/failing")],
            }),
            "/.well-known/oauth-authorization-server" => server(origin.into(), "/token"),
            "/.well-known/oauth-authorization-server/failing" => {
                server(format!("{origin}/failing"), "/failing")
            }
            "/.well-known/oauth-protected-resource/moved" => {
                let mut response = Response::new(Full::default());
                *response.status_mut() = StatusCode::FOUND;
                response.headers_mut().insert(
                    header::LOCATION,
                    header::HeaderValue::from_static("/.well-known/oauth-protected-resource/mcp"),
                );
                return response;
            }
            "/.well-known/oauth-protected-resource/down" => {
                let mut response = Response::new(Full::default());
                *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
                return response;
            }
            "/.well-known/oauth-protected-resource/large" => {
                return Response::new(Full::new(vec![b' '; 2_000_000].into()));
            }
            "/token" => json!({
                "access_token": format!("access-{count}"),
                "token_type": "Bearer",
                "refresh_token": format!("refresh-{count}"),
                "expires_in": 3600,
            }),
            "/lasting" => json!({
                "access_token": format!("access-{count}"),
                "token_type": "Bearer",
                "refresh_token": format!("refresh-{count}"),
            }),
            "/register" => json!({ "client_id": format!("client-{count}") }),
            "/secret" => json!({ "client_id": "client", "client_secret": "secret" }),
            "/rejecting" => {
                let body = json!({ "error": "invalid_client" }).to_string();
                let mut response = Response::new(Full::new(body.into()));
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                return response;
            }
            "/failing" => {
                let body = json!({ "error": "server_error" }).to_string();
                let mut response = Response::new(Full::new(body.into()));
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                return response;
            }
            "/revoke" => json!({}),
            _ => {
                let mut response = Response::new(Full::default());
                *response.status_mut() = StatusCode::NOT_FOUND;
                return response;
            }
        };
        Response::new(Full::new(body.to_string().into()))
    }

    fn automatic_client() -> AutomaticClient {
        AutomaticClient::new("http://127.0.0.1:1337".into(), None)
    }

    async fn memory() -> Authentication {
        open(CredentialStore::memory().unwrap(), Some(automatic_client())).await
    }

    async fn open(store: CredentialStore, client: Option<AutomaticClient>) -> Authentication {
        Authentication::new(store, OAuth::new().unwrap(), client, IdleHandler::new(None))
            .await
            .unwrap()
    }

    fn query(url: &str) -> BTreeMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn credentials_are_private_scoped_and_persistent() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let directory = std::env::temp_dir().join(format!(
            "fleet-auth-{}",
            crate::oauth::random_value().unwrap()
        ));

        runtime.block_on(async {
            let host_id = {
                let authentication = Authentication::open(
                    &directory,
                    Some(automatic_client()),
                    IdleHandler::new(None),
                )
                .await
                .unwrap();
                authentication
                    .register("first", &[], vec![connection()])
                    .unwrap();
                authentication
                    .register("second", &[], vec![connection()])
                    .unwrap();
                authentication
                    .set_key("first:account", "key", "test-secret".into())
                    .await
                    .unwrap();
                let first = authentication
                    .scope("first")
                    .get("account")
                    .await
                    .unwrap()
                    .unwrap();
                let second = authentication.scope("second").get("account").await.unwrap();
                let connections =
                    serde_json::to_string(&authentication.connections().await).unwrap();

                assert!(matches!(first, Credential::ApiKey(key) if key == "test-secret"));
                assert!(second.is_none());
                assert!(!connections.contains("test-secret"));

                authentication.host_id().to_owned()
            };
            let authentication =
                Authentication::open(&directory, Some(automatic_client()), IdleHandler::new(None))
                    .await
                    .unwrap();
            authentication
                .register("first", &[], vec![connection()])
                .unwrap();
            let credential = authentication
                .scope("first")
                .get("account")
                .await
                .unwrap()
                .unwrap();

            assert_eq!(authentication.host_id(), host_id);
            assert!(matches!(credential, Credential::ApiKey(key) if key == "test-secret"));

            authentication.sign_out("first:account").await.unwrap();
            let credential = authentication.scope("first").get("account").await.unwrap();

            assert!(credential.is_none());
            assert_eq!(authentication.host_id(), host_id);
        });
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn callbacks_complete_only_their_current_attempt() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let directory = std::env::temp_dir().join(format!(
            "fleet-auth-{}",
            crate::oauth::random_value().unwrap()
        ));
        let mut definition = connection();
        definition.methods.push(AuthenticationMethod {
            name: "oauth".into(),
            title: "Sign in".into(),
            kind: AuthenticationKind::OAuth(Box::new(config())),
        });

        runtime.block_on(async {
            let authentication =
                Authentication::open(&directory, Some(automatic_client()), IdleHandler::new(None))
                    .await
                    .unwrap();
            authentication
                .register("first", &[], vec![definition])
                .unwrap();
            let first = authentication
                .authorize(
                    "first:account",
                    "oauth",
                    Some("http://127.0.0.1:3000/callback"),
                )
                .await
                .unwrap();
            let repeated = authentication
                .authorize(
                    "first:account",
                    "oauth",
                    Some("http://127.0.0.1:3000/callback"),
                )
                .await
                .unwrap();
            let second = authentication
                .authorize(
                    "first:account",
                    "oauth",
                    Some("http://127.0.0.1:3001/callback"),
                )
                .await
                .unwrap();

            assert_eq!(repeated.id, first.id);
            assert_ne!(second.id, first.id);
            assert_eq!(first.redirect_uri, "http://127.0.0.1:3000/callback");

            let replaced = format!(
                "http://127.0.0.1:3000/callback?state={}&error=access_denied",
                first.id
            );
            let error = authentication
                .callback("first:account", &first.id, &replaced)
                .await
                .unwrap_err();

            assert!(matches!(error, AuthenticationError::Cancelled));

            let mismatched = format!(
                "http://127.0.0.1:3000/callback?state={}&error=access_denied",
                second.id
            );
            let error = authentication
                .callback("first:account", &second.id, &mismatched)
                .await
                .unwrap_err();

            assert!(matches!(error, AuthenticationError::Invalid(_)));

            let declined = format!(
                "http://127.0.0.1:3001/callback?state={}&error=access_denied",
                second.id
            );
            let failed = authentication
                .callback("first:account", &second.id, &declined)
                .await
                .unwrap();
            let repeated = authentication
                .callback("first:account", &second.id, &declined)
                .await
                .unwrap();

            assert_eq!(failed.status, AuthorizationStatus::Failed);
            assert_eq!(failed.error.as_deref(), Some("authorization was declined"));
            assert_eq!(repeated.status, AuthorizationStatus::Failed);
        });
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn registration_requires_an_issued_client_id() {
        let config = client();
        for parameters in [
            BTreeMap::new(),
            BTreeMap::from([("client_id".into(), "register".into())]),
        ] {
            let error = registration(&config, None, &parameters, None).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }

        let parameters = BTreeMap::from([("client_id".into(), "issued".into())]);
        let registration = registration(&config, None, &parameters, None).unwrap();

        assert_eq!(registration.client_id, "issued");
    }

    #[test]
    fn registration_preserves_the_selected_account() {
        let config = client();
        let existing = Registration {
            client_id: "issued".into(),
            subject: Some("account".into()),
            ..Registration::default()
        };
        let registration =
            super::registration(&config, Some(&existing), &BTreeMap::new(), None).unwrap();

        assert_eq!(registration.client_id, "issued");
        assert_eq!(registration.subject.as_deref(), Some("account"));

        let parameters = BTreeMap::from([("client_id".into(), "different".into())]);
        let error = super::registration(&config, Some(&existing), &parameters, None)
            .err()
            .unwrap();

        assert!(matches!(error, AuthenticationError::Invalid(_)));
    }

    #[test]
    fn resources_sign_in_through_their_discovered_server() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut method = resource("mcp", format!("{origin}/mcp"), &[]);
            if let AuthenticationKind::OAuth(config) = &mut method.kind {
                config.required_scopes = vec!["read".into()];
            }
            let mut definition = connection();
            definition.methods = vec![method];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let connections = authentication.connections().await;
            let info = authentication
                .authorize(
                    "first:account",
                    "mcp",
                    Some("http://127.0.0.1:3000/callback"),
                )
                .await
                .unwrap();
            let parameters = query(&info.url);

            assert_eq!(
                connections[0].methods[0].resource,
                Some(format!("{origin}/mcp"))
            );
            assert_eq!(info.issuer.as_deref(), Some(origin.as_str()));
            assert!(info.url.starts_with(&format!("{origin}/authorize?")));
            assert_eq!(parameters["resource"], format!("{origin}/mcp"));
            assert_eq!(parameters["scope"], "read");
            assert_eq!(parameters["client_id"], "fleet");
            assert!(!parameters.contains_key("nonce"));

            let callback = format!(
                "http://127.0.0.1:3000/callback?state={}&code=granted&iss={origin}",
                info.id
            );
            let completed = authentication
                .callback("first:account", &info.id, &callback)
                .await
                .unwrap();
            let credentials = authentication.scope("first");
            let first = credentials.get("account").await.unwrap().unwrap();
            let refreshed = credentials.refresh("account", "access-3").await.unwrap();
            let second = credentials.get("account").await.unwrap().unwrap();
            let revoked = authentication.sign_out("first:account").await.unwrap();

            assert_eq!(completed.status, AuthorizationStatus::Complete);
            assert!(matches!(first, Credential::OAuth { access_token, .. } if access_token == "access-3"));
            assert!(refreshed);
            assert!(matches!(second, Credential::OAuth { access_token, .. } if access_token == "access-4"));
            assert!(revoked);

            let requests = requests.lock().unwrap().clone();
            let targets: Vec<_> = requests.iter().map(|(target, _)| target.as_str()).collect();
            assert_eq!(
                targets,
                [
                    "/.well-known/oauth-protected-resource/mcp",
                    "/.well-known/oauth-authorization-server",
                    "/token",
                    "/token",
                    "/revoke",
                ]
            );
            for (_, form) in &requests[2..4] {
                assert_eq!(form["resource"], format!("{origin}/mcp"));
            }
            assert_eq!(requests[2].1["grant_type"], "authorization_code");
            assert_eq!(requests[3].1["grant_type"], "refresh_token");
            assert_eq!(requests[4].1["token"], "refresh-4");
        });
    }

    #[test]
    fn resources_request_declared_or_advertised_scopes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, _) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut definition = connection();
            definition.methods = vec![
                resource("declared", format!("{origin}/mcp"), &["write", "admin"]),
                resource("advertised", format!("{origin}/mcp"), &[]),
                resource("bare", format!("{origin}/bare"), &[]),
            ];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let mut scopes = Vec::new();
            for method in ["declared", "advertised", "bare"] {
                let info = authentication
                    .authorize(
                        "first:account",
                        method,
                        Some("http://127.0.0.1:3000/callback"),
                    )
                    .await
                    .unwrap();
                scopes.push(query(&info.url).remove("scope"));
            }

            assert_eq!(
                scopes,
                [Some("write admin".into()), Some("read".into()), None]
            );
        });
    }

    #[test]
    fn callbacks_from_discovered_servers_carry_their_issuer() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, _) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut definition = connection();
            definition.methods = vec![resource("mcp", format!("{origin}/mcp"), &[])];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let mut results = Vec::new();
            for (issuer, response) in [
                (Some(origin.clone()), "code"),
                (Some(format!("{origin}/other")), "code"),
                (None, "code"),
                (Some(origin.clone()), "error"),
                (None, "error"),
            ] {
                let info = authentication
                    .authorize(
                        "first:account",
                        "mcp",
                        Some("http://127.0.0.1:3000/callback"),
                    )
                    .await
                    .unwrap();
                let mut callback = Url::parse("http://127.0.0.1:3000/callback").unwrap();
                callback
                    .query_pairs_mut()
                    .append_pair(response, "granted")
                    .append_pair("state", &info.id);
                if let Some(issuer) = issuer {
                    callback.query_pairs_mut().append_pair("iss", &issuer);
                }
                let info = authentication
                    .callback("first:account", &info.id, callback.as_str())
                    .await
                    .unwrap();
                results.push(info.error);
            }

            assert_eq!(
                results,
                [
                    None,
                    Some("callback issuer does not match".into()),
                    Some("callback issuer is missing".into()),
                    Some("authorization was declined".into()),
                    Some("callback issuer is missing".into()),
                ]
            );
        });
    }

    #[test]
    fn discovery_refuses_redirects_large_responses_and_ungranted_servers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut granted = connection();
            granted.methods = vec![
                resource("moved", format!("{origin}/moved"), &[]),
                resource("down", format!("{origin}/down"), &[]),
                resource("large", format!("{origin}/large"), &[]),
            ];
            let mut ungranted = connection();
            ungranted.methods = vec![resource("mcp", format!("{origin}/mcp"), &[])];
            authentication
                .register("first", &grant, vec![granted])
                .unwrap();
            authentication
                .register("second", &[], vec![ungranted])
                .unwrap();
            let mut errors = Vec::new();
            for (id, method) in [
                ("first:account", "moved"),
                ("first:account", "down"),
                ("first:account", "large"),
                ("second:account", "mcp"),
            ] {
                let error = authentication
                    .authorize(id, method, Some("http://127.0.0.1:3000/callback"))
                    .await
                    .unwrap_err();
                errors.push(error.to_string());
            }

            assert_eq!(
                errors,
                [
                    "authorization server returned HTTP 302",
                    "authorization server returned HTTP 503",
                    "OAuth response exceeds the size limit",
                    "discovered OAuth servers must use HTTPS unless the provider's HTTP grant allows them",
                ]
            );
            let targets: Vec<_> = requests
                .lock()
                .unwrap()
                .iter()
                .map(|(target, _)| target.clone())
                .collect();
            assert_eq!(
                targets,
                [
                    "/.well-known/oauth-protected-resource/moved",
                    "/.well-known/oauth-protected-resource",
                    "/.well-known/oauth-protected-resource/down",
                    "/.well-known/oauth-protected-resource",
                    "/.well-known/oauth-protected-resource/large",
                    "/.well-known/oauth-protected-resource",
                ]
            );
        });
    }

    #[test]
    fn records_saved_before_discovery_still_refresh() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let store = CredentialStore::memory().unwrap();
            let record = json!({
                "registrations": {
                    "oauth": {
                        "client_id": "fleet",
                        "subject": null,
                        "email": null,
                    },
                },
                "credential": {
                    "method": "oauth",
                    "secret": {
                        "OAuth": {
                            "access_token": "saved",
                            "refresh_token": "saved-refresh",
                            "id_token": null,
                            "expires_at": 0,
                            "scopes": [],
                        },
                    },
                },
            });
            store.put("first:account".into(), &record).await.unwrap();
            let authentication = open(store, Some(automatic_client())).await;
            let mut definition = connection();
            let mut config = config();
            config.server = OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: format!("{origin}/authorize"),
                token_url: format!("{origin}/token"),
                resource: None,
                issuer: None,
            });
            definition.methods.push(AuthenticationMethod {
                name: "oauth".into(),
                title: "Sign in".into(),
                kind: AuthenticationKind::OAuth(Box::new(config)),
            });
            authentication
                .register("first", &[], vec![definition])
                .unwrap();
            let credential = authentication
                .scope("first")
                .get("account")
                .await
                .unwrap()
                .unwrap();

            assert!(matches!(credential, Credential::OAuth { access_token, .. } if access_token == "access-1"));
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests[0].0, "/token");
            assert_eq!(requests[0].1["refresh_token"], "saved-refresh");
            assert!(!requests[0].1.contains_key("resource"));
        });
    }

    #[test]
    fn tokens_without_an_expiry_refresh_only_when_rejected() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let authentication = memory().await;
            let mut config = config();
            config.server = OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: format!("{origin}/authorize"),
                token_url: format!("{origin}/lasting"),
                resource: None,
                issuer: None,
            });
            config.client = OAuthClient::Registered(OAuthRegisteredClient {
                registration: None,
                ..client()
            });
            let mut definition = connection();
            definition.methods = vec![AuthenticationMethod {
                name: "oauth".into(),
                title: "Sign in".into(),
                kind: AuthenticationKind::OAuth(Box::new(config)),
            }];
            authentication
                .register("first", &[], vec![definition])
                .unwrap();
            let info = authentication
                .authorize(
                    "first:account",
                    "oauth",
                    Some("http://127.0.0.1:3000/callback"),
                )
                .await
                .unwrap();
            let callback = format!(
                "http://127.0.0.1:3000/callback?state={}&code=granted",
                info.id
            );
            let completed = authentication
                .callback("first:account", &info.id, &callback)
                .await
                .unwrap();
            let credentials = authentication.scope("first");
            let first = credentials.get("account").await.unwrap().unwrap();
            let second = credentials.get("account").await.unwrap().unwrap();
            let refreshed = credentials.refresh("account", "access-1").await.unwrap();
            let third = credentials.get("account").await.unwrap().unwrap();

            assert_eq!(completed.status, AuthorizationStatus::Complete);
            assert!(matches!(first, Credential::OAuth { access_token, .. } if access_token == "access-1"));
            assert!(matches!(second, Credential::OAuth { access_token, .. } if access_token == "access-1"));
            assert!(refreshed);
            assert!(matches!(third, Credential::OAuth { access_token, .. } if access_token == "access-2"));
            assert_eq!(requests.lock().unwrap().len(), 2);
        });
    }

    #[test]
    fn discovery_leaves_connections_readable() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, _) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut definition = connection();
            definition.methods = vec![resource("slow", format!("{origin}/slow"), &[])];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let pending = authentication.clone();
            tokio::spawn(async move {
                pending
                    .authorize(
                        "first:account",
                        "slow",
                        Some("http://127.0.0.1:3000/callback"),
                    )
                    .await
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            let connections =
                tokio::time::timeout(Duration::from_secs(5), authentication.connections()).await;

            assert_eq!(connections.unwrap().len(), 1);
        });
    }

    #[test]
    fn automatic_clients_register_once_and_receive_their_callback() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut definition = connection();
            definition.methods = vec![automatic("dcr", format!("{origin}/dcr"))];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let connections = authentication.connections().await;
            let first = authentication
                .authorize("first:account", "dcr", None)
                .await
                .unwrap();
            let parameters = query(&first.url);

            assert_eq!(connections[0].methods[0].redirect_uri, None);
            assert_eq!(first.redirect_uri, "http://127.0.0.1:1337/oauth/callback");
            assert_eq!(parameters["client_id"], "client-3");
            assert_eq!(
                parameters["redirect_uri"],
                "http://127.0.0.1:1337/oauth/callback"
            );

            let callback = format!("state={}&code=granted&iss={origin}/dcr", first.id);
            let completed = authentication.hosted_callback(Some(&callback)).await;
            let repeated = authentication.hosted_callback(Some(&callback)).await;
            let second = authentication
                .authorize("first:account", "dcr", None)
                .await
                .unwrap();
            let pasted = format!(
                "http://127.0.0.1:1337/oauth/callback?state={}&code=granted&iss={origin}/dcr",
                second.id
            );
            let pasted = authentication
                .callback("first:account", &second.id, &pasted)
                .await
                .unwrap();
            let credential = authentication
                .scope("first")
                .get("account")
                .await
                .unwrap()
                .unwrap();

            assert_eq!(completed.unwrap().status, AuthorizationStatus::Complete);
            assert!(repeated.is_none());
            assert_eq!(query(&second.url)["client_id"], "client-3");
            assert_eq!(pasted.status, AuthorizationStatus::Complete);
            assert!(matches!(credential, Credential::OAuth { access_token, .. } if access_token == "access-7"));
            let requests = requests.lock().unwrap().clone();
            let targets: Vec<_> = requests.iter().map(|(target, _)| target.as_str()).collect();
            assert_eq!(
                targets,
                [
                    "/.well-known/oauth-protected-resource/dcr",
                    "/.well-known/oauth-authorization-server/dcr",
                    "/register",
                    "/token",
                    "/.well-known/oauth-protected-resource/dcr",
                    "/.well-known/oauth-authorization-server/dcr",
                    "/token",
                ]
            );
            assert_eq!(requests[3].1["client_id"], "client-3");
            assert_eq!(
                requests[3].1["redirect_uri"],
                "http://127.0.0.1:1337/oauth/callback"
            );
        });
    }

    #[test]
    fn dynamic_registrations_belong_to_their_issuer_and_redirect_uri() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, _) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let hosted = "http://127.0.0.1:1337/oauth/callback";
            let mut client_ids = Vec::new();
            for (issuer, redirect_uri) in [
                (format!("{origin}/dcr"), hosted),
                (format!("{origin}/other"), hosted),
                (
                    format!("{origin}/dcr"),
                    "http://127.0.0.1:1338/oauth/callback",
                ),
            ] {
                let store = CredentialStore::memory().unwrap();
                let record = json!({
                    "registrations": {},
                    "credential": null,
                    "clients": {
                        "dcr": {
                            "client_id": "stored",
                            "subject": null,
                            "email": null,
                            "issuer": issuer,
                            "redirect_uri": redirect_uri,
                        },
                    },
                });
                store.put("first:account".into(), &record).await.unwrap();
                let authentication = open(store, Some(automatic_client())).await;
                let mut definition = connection();
                definition.methods = vec![automatic("dcr", format!("{origin}/dcr"))];
                authentication
                    .register("first", &grant, vec![definition])
                    .unwrap();
                let info = authentication
                    .authorize("first:account", "dcr", None)
                    .await
                    .unwrap();
                client_ids.push(query(&info.url).remove("client_id").unwrap());
            }

            assert_eq!(client_ids, ["stored", "client-5", "client-8"]);
        });
    }

    #[test]
    fn metadata_documents_identify_fleet_at_an_https_public_url() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let mut parameters = Vec::new();
            for public_url in ["https://fleet.example.com", "http://127.0.0.1:1337"] {
                let client = AutomaticClient::new(public_url.into(), None);
                let authentication = open(CredentialStore::memory().unwrap(), Some(client)).await;
                let mut definition = connection();
                definition.methods = vec![automatic("cimd", format!("{origin}/cimd"))];
                authentication
                    .register("first", &grant, vec![definition])
                    .unwrap();
                let info = authentication
                    .authorize("first:account", "cimd", None)
                    .await
                    .unwrap();
                let mut query = query(&info.url);
                parameters.push((
                    query.remove("client_id").unwrap(),
                    query.remove("redirect_uri").unwrap(),
                ));
            }

            assert_eq!(
                parameters,
                [
                    (
                        "https://fleet.example.com/oauth/client.json".into(),
                        "https://fleet.example.com/oauth/callback".into(),
                    ),
                    (
                        "client-5".into(),
                        "http://127.0.0.1:1337/oauth/callback".into(),
                    ),
                ]
            );
            let registrations = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, _)| target == "/register")
                .count();
            assert_eq!(registrations, 1);
        });
    }

    #[test]
    fn hosted_callbacks_complete_only_pending_automatic_attempts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut first = connection();
            first.methods = vec![automatic("dcr", format!("{origin}/dcr"))];
            let mut second = connection();
            let endpoints = OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: format!("{origin}/authorize"),
                token_url: format!("{origin}/token"),
                resource: None,
                issuer: None,
            });
            let client = OAuthClient::Registered(OAuthRegisteredClient {
                client_id: "fleet".into(),
                redirect_uri: "http://127.0.0.1:0/oauth/callback".into(),
                registration: None,
            });
            second.methods = vec![method("oauth", endpoints, client, &[])];
            authentication
                .register("first", &grant, vec![first])
                .unwrap();
            authentication
                .register("second", &[], vec![second])
                .unwrap();
            let automatic = authentication
                .authorize("first:account", "dcr", None)
                .await
                .unwrap();
            let registered = authentication
                .authorize(
                    "second:account",
                    "oauth",
                    Some("http://127.0.0.1:1337/oauth/callback"),
                )
                .await
                .unwrap();
            let activity = authentication.state.idle_handler.last_activity();
            for query in [
                None,
                Some("code=granted".into()),
                Some("state=unknown&code=granted".into()),
                Some(format!("state={}&code=granted", registered.id)),
                Some(format!("state={0}&state={0}&code=granted", automatic.id)),
            ] {
                assert!(
                    authentication
                        .hosted_callback(query.as_deref())
                        .await
                        .is_none()
                );
            }
            let first = authentication
                .authorization("first:account", &automatic.id)
                .await
                .unwrap();
            let second = authentication
                .authorization("second:account", &registered.id)
                .await
                .unwrap();

            assert_eq!(first.status, AuthorizationStatus::Pending);
            assert_eq!(second.status, AuthorizationStatus::Pending);
            assert_eq!(authentication.state.idle_handler.last_activity(), activity);

            let callback = format!("state={}&code=granted&iss={origin}/dcr", automatic.id);
            let completed = authentication.hosted_callback(Some(&callback)).await;
            assert_ne!(authentication.state.idle_handler.last_activity(), activity);
            let expiring = authentication
                .authorize("first:account", "dcr", None)
                .await
                .unwrap();
            tokio::time::pause();
            tokio::time::sleep(Duration::from_secs(601)).await;
            let expired = authentication
                .authorization("first:account", &expiring.id)
                .await
                .unwrap();
            let callback = format!("state={}&code=granted&iss={origin}/dcr", expiring.id);
            let late = authentication.hosted_callback(Some(&callback)).await;

            assert_eq!(completed.unwrap().status, AuthorizationStatus::Complete);
            assert_eq!(expired.status, AuthorizationStatus::Failed);
            assert!(late.is_none());
            let tokens = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, _)| target == "/token")
                .count();
            assert_eq!(tokens, 1);
        });
    }

    #[test]
    fn dynamic_registrations_rejected_at_exchange_are_discarded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, _) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let mut definition = connection();
            definition.methods = vec![automatic("rejected", format!("{origin}/rejected"))];
            let authentication = memory().await;
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let first = authentication
                .authorize("first:account", "rejected", None)
                .await
                .unwrap();
            let callback = format!("state={}&code=granted&iss={origin}/rejected", first.id);
            let failed = authentication
                .hosted_callback(Some(&callback))
                .await
                .unwrap();
            let second = authentication
                .authorize("first:account", "rejected", None)
                .await
                .unwrap();

            assert_eq!(failed.status, AuthorizationStatus::Failed);
            assert_eq!(
                failed.error.unwrap(),
                AuthenticationError::ClientRejected.to_string()
            );
            assert_eq!(query(&first.url)["client_id"], "client-3");
            assert_eq!(query(&second.url)["client_id"], "client-7");
        });
    }

    #[test]
    fn dynamic_registrations_rejected_at_refresh_are_discarded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let mut definition = connection();
            definition.methods = vec![automatic("rejected", format!("{origin}/rejected"))];

            let mut errors = Vec::new();
            let client = json!({
                "client_id": "stored",
                "subject": null,
                "email": null,
                "issuer": format!("{origin}/rejected"),
                "redirect_uri": "http://127.0.0.1:1337/oauth/callback",
            });
            for dynamic in [true, false] {
                let mut registration = client.clone();
                if !dynamic {
                    registration["redirect_uri"] = Value::Null;
                }
                let record = json!({
                    "registrations": { "rejected": registration },
                    "credential": {
                        "method": "rejected",
                        "secret": {
                            "OAuth": {
                                "access_token": "saved",
                                "refresh_token": "saved-refresh",
                                "id_token": null,
                                "expires_at": 0,
                                "scopes": [],
                            },
                        },
                        "server": {
                            "resource": format!("{origin}/rejected"),
                            "issuer": format!("{origin}/rejected"),
                            "token_endpoint": format!("{origin}/rejecting"),
                            "revocation_endpoint": null,
                        },
                    },
                    "clients": { "rejected": client },
                });
                let store = CredentialStore::memory().unwrap();
                store.put("first:account".into(), &record).await.unwrap();
                let authentication = open(store, Some(automatic_client())).await;
                authentication
                    .register("first", &grant, vec![definition.clone()])
                    .unwrap();
                let pending = authentication
                    .authorize("first:account", "rejected", None)
                    .await
                    .unwrap();
                let error = authentication
                    .scope("first")
                    .get("account")
                    .await
                    .err()
                    .unwrap();
                let info = authentication
                    .authorize("first:account", "rejected", None)
                    .await
                    .unwrap();
                assert_eq!(query(&pending.url)["client_id"], "stored");
                assert_eq!(info.id == pending.id, !dynamic);
                errors.push((error.to_string(), query(&info.url).remove("client_id")));
            }

            assert_eq!(
                errors,
                [
                    (
                        AuthenticationError::ClientRejected.to_string(),
                        Some("client-6".into())
                    ),
                    (
                        "authorization server returned HTTP 401".into(),
                        Some("stored".into())
                    ),
                ]
            );
            let registrations = requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, _)| target == "/register")
                .count();
            assert_eq!(registrations, 1);
        });
    }

    #[test]
    fn failed_sign_ins_keep_the_credential_registration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let resource = format!("{origin}/failing");
            let store = CredentialStore::memory().unwrap();
            let record = json!({
                "registrations": {
                    "oauth": {
                        "client_id": "stored",
                        "subject": null,
                        "email": null,
                        "issuer": origin,
                    },
                },
                "credential": {
                    "method": "oauth",
                    "secret": {
                        "OAuth": {
                            "access_token": "saved",
                            "refresh_token": "saved-refresh",
                            "id_token": null,
                            "expires_at": 0,
                            "scopes": [],
                        },
                    },
                    "server": {
                        "resource": resource,
                        "issuer": origin,
                        "token_endpoint": format!("{origin}/token"),
                        "revocation_endpoint": null,
                    },
                },
            });
            store.put("first:account".into(), &record).await.unwrap();
            let authentication = open(store, Some(automatic_client())).await;
            let mut config = config();
            config.server = OAuthServer::Resource(resource.clone());
            let mut definition = connection();
            definition.methods = vec![AuthenticationMethod {
                name: "oauth".into(),
                title: "Sign in".into(),
                kind: AuthenticationKind::OAuth(Box::new(config)),
            }];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let info = authentication
                .authorize(
                    "first:account",
                    "oauth",
                    Some("http://127.0.0.1:3000/callback"),
                )
                .await
                .unwrap();
            let callback = format!(
                "http://127.0.0.1:3000/callback?state={}&code=granted&client_id=issued&iss={resource}",
                info.id
            );
            let failed = authentication
                .callback("first:account", &info.id, &callback)
                .await
                .unwrap();
            let credential = authentication
                .scope("first")
                .get("account")
                .await
                .unwrap()
                .unwrap();

            assert_eq!(query(&info.url)["client_id"], "register");
            assert_eq!(failed.status, AuthorizationStatus::Failed);
            assert!(matches!(credential, Credential::OAuth { access_token, .. } if access_token == "access-4"));
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests[2].0, "/failing");
            assert_eq!(requests[2].1["client_id"], "issued");
            assert_eq!(requests[3].0, "/token");
            assert_eq!(requests[3].1["client_id"], "stored");
        });
    }

    #[test]
    fn failed_automatic_sign_ins_keep_the_credential_registration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let issuer = format!("{origin}/rejected");
            let registration = json!({
                "client_id": "stored",
                "subject": null,
                "email": null,
                "issuer": issuer,
                "redirect_uri": "http://127.0.0.1:1338/oauth/callback",
            });
            let record = json!({
                "registrations": { "rejected": registration },
                "credential": {
                    "method": "rejected",
                    "secret": {
                        "OAuth": {
                            "access_token": "saved",
                            "refresh_token": "saved-refresh",
                            "id_token": null,
                            "expires_at": 0,
                            "scopes": [],
                        },
                    },
                    "server": {
                        "resource": issuer,
                        "issuer": issuer,
                        "token_endpoint": format!("{origin}/token"),
                        "revocation_endpoint": null,
                    },
                },
                "clients": { "rejected": registration },
            });
            let store = CredentialStore::memory().unwrap();
            store.put("first:account".into(), &record).await.unwrap();
            let authentication = open(store, Some(automatic_client())).await;
            let mut definition = connection();
            definition.methods = vec![automatic("rejected", issuer.clone())];
            authentication
                .register("first", &grant, vec![definition])
                .unwrap();
            let info = authentication
                .authorize("first:account", "rejected", None)
                .await
                .unwrap();
            let callback = format!("state={}&code=granted&iss={issuer}", info.id);
            let failed = authentication.hosted_callback(Some(&callback)).await;
            let credential = authentication
                .scope("first")
                .get("account")
                .await
                .unwrap()
                .unwrap();

            assert_eq!(query(&info.url)["client_id"], "client-3");
            assert_eq!(failed.unwrap().status, AuthorizationStatus::Failed);
            assert!(matches!(credential, Credential::OAuth { access_token, .. } if access_token == "access-5"));
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests[4].0, "/token");
            assert_eq!(requests[4].1["client_id"], "stored");
        });
    }

    #[test]
    fn sessions_keep_one_dynamic_registration_per_binding() {
        let client = |client_id: &str, issuer: &str| Registration {
            client_id: client_id.into(),
            issuer: Some(issuer.into()),
            redirect_uri: Some("http://127.0.0.1:1337/oauth/callback".into()),
            ..Registration::default()
        };
        let mut session = Session {
            record: Record::default(),
            dirty: false,
            authorization: None,
        };
        let kept = [
            client("first", "https://a.example.com"),
            client("second", "https://a.example.com"),
            client("first", "https://b.example.com"),
        ]
        .map(|identity| session.keep_client("dcr", None, identity).unwrap());

        assert_eq!(kept.map(|kept| kept.client_id), ["first", "first", "first"]);
        assert_eq!(
            session.record.clients["dcr"].issuer.as_deref(),
            Some("https://b.example.com")
        );

        session.reject_client("dcr", &client("first", "https://a.example.com"));
        assert!(session.record.clients.contains_key("dcr"));
        session.reject_client("dcr", &client("first", "https://b.example.com"));
        assert!(session.record.clients.is_empty());

        let stored = client("first", "https://b.example.com");
        let error = session
            .keep_client("dcr", Some(&stored), stored.clone())
            .unwrap_err();
        assert!(matches!(error, AuthenticationError::Cancelled));
        assert!(session.record.clients.is_empty());
    }

    #[test]
    fn automatic_clients_refuse_what_they_cannot_use() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let (origin, requests) = serve().await;
            let grant = [origin.parse::<WasmOrigin>().unwrap()];
            let authentication = memory().await;
            let mut declared = connection();
            let endpoints = OAuthServer::Endpoints(OAuthEndpoints {
                authorization_url: format!("{origin}/authorize"),
                token_url: format!("{origin}/token"),
                resource: None,
                issuer: None,
            });
            declared.methods = vec![method("oauth", endpoints, OAuthClient::Automatic, &[])];
            let error = authentication
                .register("first", &grant, vec![declared])
                .unwrap_err();

            assert_eq!(
                error.to_string(),
                "automatic OAuth clients require a protected resource server"
            );

            let mut definition = connection();
            definition.methods = vec![
                automatic("dcr", format!("{origin}/dcr")),
                automatic("remote", format!("{origin}/remote")),
                automatic("secret", format!("{origin}/secret")),
            ];
            authentication
                .register("second", &grant, vec![definition.clone()])
                .unwrap();
            let unconfigured = open(CredentialStore::memory().unwrap(), None).await;
            unconfigured
                .register("second", &grant, vec![definition])
                .unwrap();
            let mut errors = Vec::new();
            for (authentication, method, redirect_uri) in [
                (&authentication, "dcr", Some("http://127.0.0.1:1337/oauth/callback")),
                (&authentication, "remote", None),
                (&authentication, "secret", None),
                (&unconfigured, "dcr", None),
            ] {
                let error = authentication
                    .authorize("second:account", method, redirect_uri)
                    .await
                    .unwrap_err();
                errors.push(error.to_string());
            }

            assert_eq!(
                errors,
                [
                    "automatic OAuth clients use Fleet's own redirect URI",
                    "discovered OAuth servers must use HTTPS unless the provider's HTTP grant allows them",
                    "authorization server registered a confidential client, which Fleet does not support",
                    "automatic sign-in needs public_url when Fleet listens on port 0",
                ]
            );
            assert!(
                !requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(target, _)| target.ends_with("/dcr"))
            );
        });
    }

    #[test]
    fn registrations_belong_to_their_issuer() {
        let server = |issuer: &str| Discovered {
            resource: "https://api.example.com/mcp".into(),
            issuer: issuer.into(),
            token_endpoint: "https://auth.example.com/token".into(),
            revocation_endpoint: None,
        };
        let parameters = BTreeMap::from([("client_id".into(), "issued".into())]);
        let first = server("https://auth.example.com");
        let registration = registration(&client(), None, &parameters, Some(&first)).unwrap();

        assert!(registration.issued_by(Some(&first)));
        assert!(!registration.issued_by(Some(&server("https://other.example.com"))));
        assert!(!registration.issued_by(None));
    }
}
