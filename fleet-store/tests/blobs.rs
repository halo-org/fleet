//! Tests for immutable blobs, upload publication, and attachment access.

#![cfg(feature = "blobs")]

use core::assert_matches;
use fleet_core::{
    Agent, ClientStore as _, Configured, ContentBlock, EventPayload, JsonObject, Message,
    MessageMode, RuntimeStore as _,
};
use fleet_kit::{Blobs as _, ImageDimensions};
use fleet_store::{Store, StoreError};
use futures_util::TryStreamExt as _;

const IMAGE: &[u8] = include_bytes!("fixtures/image.png");

fn agent(id: u128) -> Agent {
    Agent {
        id: id.into(),
        role: Configured {
            name: "assistant".into(),
            config: JsonObject::new(),
        },
        extensions: JsonObject::new(),
    }
}

fn message(blob: u128) -> Message {
    Message {
        id: 1.into(),
        mode: MessageMode::Queue,
        content: vec![
            ContentBlock::Text("Describe this".into()),
            ContentBlock::Image(blob.into()),
        ],
    }
}

#[test]
fn blobs_are_immutable_and_reads_are_bounded() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        let metadata = store.put(1.into(), "image/png", IMAGE).await.unwrap();
        assert_eq!(metadata.size, IMAGE.len() as u64);
        assert_eq!(
            metadata.image,
            Some(ImageDimensions {
                width: 2,
                height: 1
            })
        );
        assert_eq!(
            store.put(1.into(), "image/png", IMAGE).await.unwrap(),
            metadata
        );
        assert_eq!(store.get(1.into()).await.unwrap(), Some(metadata.clone()));
        assert_eq!(store.read(1.into(), metadata.size).await.unwrap(), IMAGE);

        let error = store.read(1.into(), metadata.size - 1).await.unwrap_err();
        assert_matches!(error, StoreError::BlobTooLarge);
        let error = store
            .put(1.into(), "text/plain", b"different")
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::BlobConflict);
        let error = store.put(2.into(), "image/jpeg", IMAGE).await.unwrap_err();
        assert_matches!(error, StoreError::InvalidBlob);
        let error = store
            .put(2.into(), "image/png", b"not an image")
            .await
            .unwrap_err();
        assert_matches!(error, StoreError::InvalidBlob);
        assert_eq!(store.get(2.into()).await.unwrap(), None);
    });
}

#[test]
fn upload_offsets_and_publication_are_atomic() {
    let store = Store::memory().unwrap();
    let split = IMAGE.len().div_ceil(2);

    futures_executor::block_on(async {
        let upload = store
            .create_upload(1, "image/png", IMAGE.len() as u64)
            .await
            .unwrap();
        assert_eq!(
            store
                .create_upload(1, "image/png", upload.size)
                .await
                .unwrap(),
            upload
        );
        assert_matches!(
            store
                .create_upload(1, "image/jpeg", upload.size)
                .await
                .unwrap_err(),
            StoreError::UploadConflict
        );
        assert_matches!(
            store.complete_upload(1).await.unwrap_err(),
            StoreError::UploadConflict
        );
        assert_eq!(store.get(1.into()).await.unwrap(), None);

        let (first, second) = futures_util::future::join(
            store.append_upload(1, 0, &IMAGE[..split]),
            store.append_upload(1, 0, &IMAGE[..split]),
        )
        .await;
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_matches!(
            first.err().or_else(|| second.err()).unwrap(),
            StoreError::UploadConflict
        );
        assert_eq!(store.upload(1).await.unwrap().offset, split as u64);
        assert_eq!(store.get(1.into()).await.unwrap(), None);

        store
            .append_upload(1, split as u64, &IMAGE[split..])
            .await
            .unwrap();
        assert_eq!(store.complete_upload(1).await.unwrap(), 1.into());
        assert_eq!(store.complete_upload(1).await.unwrap(), 1.into());
        assert_eq!(store.upload(1).await.unwrap().blob, Some(1.into()));
        assert_matches!(
            store
                .append_upload(1, IMAGE.len() as u64, b"x")
                .await
                .unwrap_err(),
            StoreError::UploadConflict
        );
        store.delete_upload(1).await.unwrap();
        assert_matches!(
            store.upload(1).await.unwrap_err(),
            StoreError::UploadNotFound
        );
        assert_eq!(store.read(1.into(), u64::MAX).await.unwrap(), IMAGE);
    });
}

#[test]
fn invalid_images_never_become_readable_blobs() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        store.create_upload(1, "image/png", 3).await.unwrap();
        assert_matches!(
            store.append_upload(1, 0, b"long").await.unwrap_err(),
            StoreError::BlobTooLarge
        );
        assert_eq!(store.upload(1).await.unwrap().offset, 0);
        store.append_upload(1, 0, b"bad").await.unwrap();
        assert_matches!(
            store.complete_upload(1).await.unwrap_err(),
            StoreError::InvalidBlob
        );
        assert_eq!(store.get(1.into()).await.unwrap(), None);
        store.delete_upload(1).await.unwrap();
        store.delete_upload(1).await.unwrap();
    });
}

#[test]
fn messages_validate_references_and_grant_agent_access() {
    let store = Store::memory().unwrap();
    let agent = agent(1);
    let message = message(10);

    futures_executor::block_on(async {
        assert_matches!(
            store
                .create_agent("create", &agent, Some(&message))
                .await
                .unwrap_err(),
            StoreError::BlobNotFound
        );
        assert_eq!(store.agent(agent.id).await.unwrap(), None);
        store.put(10.into(), "image/png", IMAGE).await.unwrap();
        store
            .create_agent("create", &agent, Some(&message))
            .await
            .unwrap();
        store
            .create_agent("other", &self::agent(2), None)
            .await
            .unwrap();

        assert_eq!(
            store
                .agent_blobs(agent.id)
                .read(10.into(), u64::MAX)
                .await
                .unwrap(),
            IMAGE
        );
        assert_matches!(
            store
                .agent_blobs(2.into())
                .read(10.into(), u64::MAX)
                .await
                .unwrap_err(),
            StoreError::BlobDenied
        );
        assert_matches!(
            store
                .agent_blobs(2.into())
                .put(10.into(), "image/png", IMAGE)
                .await
                .unwrap_err(),
            StoreError::BlobDenied
        );
        store
            .agent_blobs(2.into())
            .put(11.into(), "image/png", IMAGE)
            .await
            .unwrap();
        assert_eq!(
            store
                .agent_blobs(2.into())
                .read(11.into(), u64::MAX)
                .await
                .unwrap(),
            IMAGE
        );

        store.consume_messages(agent.id, None, true).await.unwrap();
        let events = store
            .events(agent.id, 0.into(), None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            events[0].payload,
            EventPayload::Input(message.id, message.content)
        );
    });
}

#[test]
fn uploads_resume_after_reopening_the_database() {
    let path = std::env::temp_dir().join(format!(
        "fleet-upload-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let split = IMAGE.len().div_ceil(2);

    futures_executor::block_on(async {
        {
            let store = Store::open(&path).unwrap();
            store
                .create_upload(1, "image/png", IMAGE.len() as u64)
                .await
                .unwrap();
            store.append_upload(1, 0, &IMAGE[..split]).await.unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.upload(1).await.unwrap().offset, split as u64);
            store
                .append_upload(1, split as u64, &IMAGE[split..])
                .await
                .unwrap();
            store.complete_upload(1).await.unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.complete_upload(1).await.unwrap(), 1.into());
        assert_eq!(store.read(1.into(), u64::MAX).await.unwrap(), IMAGE);
    });

    std::fs::remove_file(path).unwrap();
}
