use super::{BlobId, ContentBlock, WasmBlobs, WasmState, blobs};
use crate::WasmEnvironment;
use core::assert_matches;
use fleet_core::ClientStore as _;
use fleet_core::{Agent, Configured, JsonObject};
use fleet_kit::Blobs as _;
use fleet_store::Store as BlobStore;
use wasmtime::{Config, Engine, Store, component::HasSelf};

const IMAGE: &[u8] = include_bytes!("../../../fleet-store/tests/fixtures/image.png");

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn state(blobs: Option<WasmBlobs>) -> Store<WasmState> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    Store::new(
        &Engine::new(&config).unwrap(),
        WasmState::new(WasmEnvironment::default(), None).with_blobs(blobs),
    )
}

#[test]
fn model_host_reads_existing_blobs_but_cannot_write() {
    runtime().block_on(async {
        let blobs = BlobStore::memory().unwrap();
        blobs.put(1.into(), "image/png", IMAGE).await.unwrap();
        let host = WasmBlobs::new(blobs);
        let mut store = state(Some(host.for_model()));

        store
            .run_concurrent(async |accessor| {
                let bytes = <HasSelf<WasmState> as blobs::HostWithStore<WasmState>>::read(
                    accessor,
                    BlobId::from(1).into(),
                    1024,
                )
                .await
                .unwrap();
                assert_eq!(bytes, IMAGE);

                let error = <HasSelf<WasmState> as blobs::HostWithStore<WasmState>>::put(
                    accessor,
                    BlobId::from(1).into(),
                    "image/png".into(),
                    IMAGE.to_vec(),
                )
                .await
                .unwrap_err();
                assert_matches!(error.kind, blobs::BlobsErrorKind::Denied);
            })
            .await
            .unwrap();
    });
}

#[test]
fn tool_host_uses_agent_authority_and_validates_output_references() {
    runtime().block_on(async {
        let blobs = BlobStore::memory().unwrap();
        for id in [1_u128, 2] {
            blobs
                .create_agent(
                    &id.to_string(),
                    &Agent {
                        id: id.into(),
                        role: Configured {
                            name: "assistant".into(),
                            config: JsonObject::new(),
                        },
                        extensions: JsonObject::new(),
                    },
                    None,
                )
                .await
                .unwrap();
        }
        let host = WasmBlobs::new(blobs.clone()).with_agents(move |id| blobs.agent_blobs(id));
        let first = host.for_agent(1.into()).unwrap();
        first
            .provider
            .put(3.into(), "image/png".into(), IMAGE.to_vec())
            .await
            .unwrap();
        let content = [ContentBlock::Image(3.into())];

        first.validate_content(&content).await.unwrap();

        let error = host
            .for_agent(2.into())
            .unwrap()
            .validate_content(&content)
            .await
            .unwrap_err();
        assert_matches!(error.kind, blobs::BlobsErrorKind::Denied);
    });
}
