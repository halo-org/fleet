use crate::{
    Sprites,
    bindings::exports::fleet::providers::authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Guest,
    },
};

impl Guest for Sprites {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn connections(_host_id: String) -> Result<Vec<Connection>, String> {
        Ok(vec![Connection {
            name: "sprites".into(),
            title: "Sprites".into(),
            description: Some("Runs workspaces in Sprites VMs.".into()),
            manage_url: Some("https://sprites.dev".into()),
            methods: vec![AuthenticationMethod {
                name: "api-key".into(),
                title: "API key".into(),
                kind: AuthenticationKind::ApiKey,
            }],
        }])
    }
}
