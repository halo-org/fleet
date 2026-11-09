use crate::{
    AuthenticationError, CredentialStore, IdleHandler,
    oauth::{OAuth, Registration, Tokens},
};
use fleet_kit::{AuthenticationKind, Connection, Credential, Credentials, OAuthConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, RwLock},
};
use tokio::sync::{Mutex, oneshot};

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
    host_id: String,
    connections: RwLock<BTreeMap<String, Arc<ConnectionState>>>,
    idle_handler: IdleHandler,
}

struct ConnectionState {
    provider: String,
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
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedCredential {
    method: String,
    secret: Secret,
}

#[derive(Clone, Serialize, Deserialize)]
enum Secret {
    ApiKey(String),
    OAuth(Tokens),
}

struct PendingAuthorization {
    info: AuthorizationInfo,
    cancelled: Option<oneshot::Sender<()>>,
}

#[derive(Clone, Serialize)]
pub struct AuthorizationInfo {
    pub id: String,
    pub url: String,
    pub status: AuthorizationStatus,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
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
}

impl Authentication {
    /// Opens protected credential storage.
    pub async fn open(
        directory: &Path,
        idle_handler: IdleHandler,
    ) -> Result<Self, AuthenticationError> {
        let store = CredentialStore::open(directory)?;
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
                oauth: OAuth::new()?,
                host_id,
                connections: RwLock::new(BTreeMap::new()),
                idle_handler,
            }),
        })
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
    pub fn register(
        &self,
        namespace: &str,
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
                Secret::OAuth(tokens) => oauth_config(&connection, &credential.method)
                    .is_ok_and(|config| authorized(config, tokens)),
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
                    })
                    .collect(),
            });
        }
        result
    }

    /// Starts a browser authorization.
    pub async fn authorize(
        &self,
        id: &str,
        method: &str,
    ) -> Result<AuthorizationInfo, AuthenticationError> {
        let guard = self
            .state
            .idle_handler
            .track_mutation()
            .ok_or(AuthenticationError::Cancelled)?;
        let connection = self.connection(id)?;
        let config = oauth_config(&connection, method)?.clone();
        let mut session = connection.session.lock().await;
        self.flush(id, &mut session).await?;
        if let Some(authorization) = &session.authorization
            && authorization.info.status == AuthorizationStatus::Pending
        {
            return Ok(authorization.info.clone());
        }
        let authorization = self
            .state
            .oauth
            .authorize(&config, session.record.registrations.get(method))
            .await?;
        let info = AuthorizationInfo {
            id: authorization.id.clone(),
            url: authorization.url.clone(),
            status: AuthorizationStatus::Pending,
            error: None,
        };
        let (cancelled, cancellation) = oneshot::channel();
        session.authorization = Some(PendingAuthorization {
            info: info.clone(),
            cancelled: Some(cancelled),
        });
        drop(session);
        let authentication = self.clone();
        let id = id.to_owned();
        let method = method.to_owned();
        tokio::spawn(async move {
            let _guard = guard;
            authentication
                .complete_authorization(
                    &id,
                    &method,
                    &config,
                    &authorization,
                    &connection,
                    cancellation,
                )
                .await;
        });
        Ok(info)
    }

    async fn complete_authorization(
        &self,
        id: &str,
        method: &str,
        config: &OAuthConfig,
        authorization: &crate::oauth::Authorization,
        connection: &ConnectionState,
        cancellation: oneshot::Receiver<()>,
    ) {
        let callback = {
            let callback =
                core::pin::pin!(crate::oauth_callback::wait(authorization, cancellation));
            let shutdown = core::pin::pin!(self.state.idle_handler.wait_for_shutdown());
            match futures_util::future::select(callback, shutdown).await {
                futures_util::future::Either::Left((result, _)) => result,
                futures_util::future::Either::Right(_) => Err(AuthenticationError::Cancelled),
            }
        };
        let mut session = connection.session.lock().await;
        if session
            .authorization
            .as_ref()
            .is_none_or(|pending| pending.info.id != authorization.id)
        {
            return;
        }
        let result = async {
            let parameters = callback?;
            if parameters.contains_key("error") {
                return Err(AuthenticationError::Invalid("authorization was declined"));
            }
            if parameters
                .get("iss")
                .is_some_and(|issuer| config.issuer.as_ref() != Some(issuer))
            {
                return Err(AuthenticationError::Invalid(
                    "callback issuer does not match",
                ));
            }
            let code = parameters
                .get("code")
                .filter(|code| !code.is_empty())
                .ok_or(AuthenticationError::Invalid(
                    "authorization code is missing",
                ))?;
            let registration = registration(
                config,
                session.record.registrations.get(method),
                &parameters,
            )?;
            session
                .record
                .registrations
                .insert(method.to_owned(), registration.clone());
            session.dirty = true;
            self.flush(id, &mut session).await?;
            let mut registration = registration;
            let tokens = self
                .state
                .oauth
                .exchange(config, &mut registration, authorization, code)
                .await?;
            let authorized = authorized(config, &tokens);
            session
                .record
                .registrations
                .insert(method.to_owned(), registration);
            session.record.credential = Some(SavedCredential {
                method: method.into(),
                secret: Secret::OAuth(tokens),
            });
            session.dirty = true;
            self.flush(id, &mut session).await?;
            if !authorized {
                return Err(AuthenticationError::Unauthorized);
            }
            Ok(())
        }
        .await;
        if let Some(pending) = &mut session.authorization {
            pending.cancelled = None;
            pending.info.status = if result.is_ok() {
                AuthorizationStatus::Complete
            } else {
                AuthorizationStatus::Failed
            };
            pending.info.error = result.err().map(|error| error.to_string());
        }
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
        }) = &session.record.credential
        {
            let config = oauth_config(&connection, method)?;
            let registration = session
                .record
                .registrations
                .get(method)
                .ok_or(AuthenticationError::Unauthorized)?;
            self.state
                .oauth
                .revoke(config, registration, tokens)
                .await
                .unwrap_or(false)
        } else {
            true
        };
        session.record.credential = None;
        session.dirty = true;
        self.flush(id, &mut session).await?;
        drop(session);
        Ok(revoked)
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
                if !authorized(config, &tokens) {
                    return Err(AuthenticationError::Unauthorized);
                }
                if tokens.expires_at <= crate::oauth::now().saturating_add(60)
                    || rejected == Some(tokens.access_token.as_str())
                {
                    let mut registration = session
                        .record
                        .registrations
                        .get(&credential.method)
                        .cloned()
                        .ok_or(AuthenticationError::Unauthorized)?;
                    tokens = self
                        .state
                        .oauth
                        .refresh(config, &mut registration, &tokens)
                        .await?;
                    session
                        .record
                        .registrations
                        .insert(credential.method.clone(), registration);
                    session.record.credential = Some(SavedCredential {
                        method: credential.method,
                        secret: Secret::OAuth(tokens.clone()),
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
    config: &OAuthConfig,
    existing: Option<&Registration>,
    parameters: &BTreeMap<String, String>,
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
        ..Registration::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_kit::{AuthenticationMethod, OAuthRegistration};

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

    fn config() -> OAuthConfig {
        OAuthConfig {
            authorization_url: "https://example.com/authorize".into(),
            token_url: "https://example.com/token".into(),
            client_id: "register".into(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
            scopes: vec![],
            parameters: vec![],
            resource: None,
            issuer: None,
            registration: Some(OAuthRegistration {
                client_id_parameter: "client_id".into(),
                parameters: vec![],
            }),
            required_scopes: vec![],
        }
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
                let authentication = Authentication::open(&directory, IdleHandler::new(None))
                    .await
                    .unwrap();
                authentication
                    .register("first", vec![connection()])
                    .unwrap();
                authentication
                    .register("second", vec![connection()])
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
            let authentication = Authentication::open(&directory, IdleHandler::new(None))
                .await
                .unwrap();
            authentication
                .register("first", vec![connection()])
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
    fn registration_requires_an_issued_client_id() {
        let config = config();
        for parameters in [
            BTreeMap::new(),
            BTreeMap::from([("client_id".into(), "register".into())]),
        ] {
            let error = registration(&config, None, &parameters).unwrap_err();
            assert!(matches!(error, AuthenticationError::Invalid(_)));
        }

        let parameters = BTreeMap::from([("client_id".into(), "issued".into())]);
        let registration = registration(&config, None, &parameters).unwrap();

        assert_eq!(registration.client_id, "issued");
    }

    #[test]
    fn registration_preserves_the_selected_account() {
        let config = config();
        let existing = Registration {
            client_id: "issued".into(),
            subject: Some("account".into()),
            email: None,
        };
        let registration = super::registration(&config, Some(&existing), &BTreeMap::new()).unwrap();

        assert_eq!(registration.client_id, "issued");
        assert_eq!(registration.subject.as_deref(), Some("account"));

        let parameters = BTreeMap::from([("client_id".into(), "different".into())]);
        let error = super::registration(&config, Some(&existing), &parameters)
            .err()
            .unwrap();

        assert!(matches!(error, AuthenticationError::Invalid(_)));
    }
}
