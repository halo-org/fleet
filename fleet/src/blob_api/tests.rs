use super::*;
use fleet_store::Store;
use hyper::Request;
use serde_json::{Value, json};

const IMAGE: &[u8] = include_bytes!("../../../fleet-store/tests/fixtures/image.png");

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn call(fleet: &Fleet, method: &str, path: &str, body: Value) -> Response<ResponseBody> {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    crate::api::handle(fleet.clone(), request).await.unwrap()
}

async fn patch(fleet: &Fleet, offset: u64, bytes: &[u8]) -> Response<ResponseBody> {
    let request = Request::builder()
        .method("PATCH")
        .uri("/uploads/1")
        .header(header::CONTENT_TYPE, "application/offset+octet-stream")
        .header("upload-offset", offset)
        .body(Full::new(Bytes::copy_from_slice(bytes)))
        .unwrap();
    crate::api::handle(fleet.clone(), request).await.unwrap()
}

#[test]
fn uploads_publish_only_after_completion_and_return_safe_image_responses() {
    let fleet = Fleet::new(Store::memory().unwrap(), None);

    runtime().block_on(async {
        let response = call(
            &fleet,
            "POST",
            "/uploads",
            json!({"id": "1", "media_type": "image/png", "size": IMAGE.len()}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["upload-offset"], "0");
        assert_eq!(response.headers()[header::LOCATION], "/uploads/1");
        assert_eq!(
            call(&fleet, "GET", "/blobs/1", json!({})).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&fleet, "POST", "/uploads/1/complete", json!({}))
                .await
                .status(),
            StatusCode::CONFLICT
        );

        let response = patch(&fleet, 0, IMAGE).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["upload-offset"], IMAGE.len().to_string());
        assert_eq!(patch(&fleet, 0, IMAGE).await.status(), StatusCode::CONFLICT);
        let response = call(&fleet, "HEAD", "/uploads/1", json!({})).await;
        assert_eq!(response.headers()["upload-offset"], IMAGE.len().to_string());
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );

        for _ in 0..2 {
            let response = call(&fleet, "POST", "/uploads/1/complete", json!({})).await;
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body, json!({"blob_id": "1"}));
        }
        let response = call(&fleet, "GET", "/blobs/1", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            IMAGE
        );
        assert_eq!(
            call(&fleet, "DELETE", "/uploads/1", json!({}))
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&fleet, "GET", "/blobs/1", json!({})).await.status(),
            StatusCode::OK
        );
    });
}

#[test]
fn uploads_reject_invalid_content_and_oversized_chunks_without_advancing() {
    let fleet = Fleet::new(Store::memory().unwrap(), None);

    runtime().block_on(async {
        for input in [
            json!({"id": "1", "media_type": "image/svg+xml", "size": 3}),
            json!({"id": "1", "media_type": "image/png", "size": MAX_BLOB_BYTES + 1}),
            json!({"id": "1", "media_type": "image/png", "size": 0}),
        ] {
            assert!(
                call(&fleet, "POST", "/uploads", input)
                    .await
                    .status()
                    .is_client_error()
            );
        }
        call(
            &fleet,
            "POST",
            "/uploads",
            json!({"id": "1", "media_type": "image/png", "size": 3}),
        )
        .await;
        let response = patch(&fleet, 0, &vec![0; MAX_UPLOAD_CHUNK_BYTES + 1]).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(fleet.blob_store().upload(1).await.unwrap().offset, 0);
        patch(&fleet, 0, b"bad").await;
        assert_eq!(
            call(&fleet, "POST", "/uploads/1/complete", json!({}))
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            call(&fleet, "GET", "/blobs/1", json!({})).await.status(),
            StatusCode::NOT_FOUND
        );
    });
}
