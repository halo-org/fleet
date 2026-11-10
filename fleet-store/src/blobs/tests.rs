use super::*;
use core::assert_matches;
use redb::ReadableTableMetadata as _;

const IMAGE: &[u8] = include_bytes!("../../tests/fixtures/image.png");

#[test]
fn expired_sessions_are_reclaimed_without_removing_published_blobs() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        for id in [1, 2] {
            store
                .create_upload(id, "image/png", IMAGE.len() as u64)
                .await
                .unwrap();
            store.append_upload(id, 0, IMAGE).await.unwrap();
        }
        store.complete_upload(2).await.unwrap();
        store
            .worker()
            .call(|worker| {
                let transaction = worker.database.begin_write()?;
                {
                    let mut table = transaction.open_table(blob_records::UPLOADS)?;
                    for id in [1, 2] {
                        let mut record = upload(&table, id)?;
                        record.expires_at = 0;
                        table.insert(id, serde_json::to_vec(&record)?.as_slice())?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();

        assert_matches!(
            store.upload(1).await.unwrap_err(),
            StoreError::UploadNotFound
        );
        store
            .create_upload(3, "image/png", IMAGE.len() as u64)
            .await
            .unwrap();
        let chunks = store
            .worker()
            .call(|worker| {
                let transaction = worker.database.begin_read()?;
                Ok(transaction.open_table(blob_records::CHUNKS)?.len()?)
            })
            .await
            .unwrap();
        assert_eq!(chunks, 0);
        assert_eq!(store.get(1.into()).await.unwrap(), None);
        assert_eq!(store.read(2.into(), u64::MAX).await.unwrap(), IMAGE);
    });
}

#[test]
fn reads_detect_corruption_without_returning_partial_content() {
    let store = Store::memory().unwrap();

    futures_executor::block_on(async {
        store.put(1.into(), "image/png", IMAGE).await.unwrap();
        store
            .worker()
            .call(|worker| {
                let transaction = worker.database.begin_write()?;
                transaction
                    .open_table(blob_records::BYTES)?
                    .insert(1, b"corrupt".as_slice())?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();

        assert_matches!(
            store.read(1.into(), u64::MAX).await.unwrap_err(),
            StoreError::InvalidBlob
        );
        assert_matches!(
            store.put(1.into(), "image/png", IMAGE).await.unwrap_err(),
            StoreError::InvalidBlob
        );
    });
}
