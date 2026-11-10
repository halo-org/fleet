use crate::{
    Fleet,
    api::{self, ApiError, ResponseBody},
};
use core::{convert::Infallible, error::Error};
use fleet_kit::Blobs as _;
use fleet_store::{MAX_BLOB_BYTES, MAX_UPLOAD_CHUNK_BYTES, StoreError, Upload};
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use hyper::{
    Method, Response, StatusCode,
    body::{Body, Bytes},
    header,
    http::request::Parts,
};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUpload {
    id: String,
    media_type: String,
    size: u64,
}

#[derive(Serialize)]
struct ApiUpload {
    id: String,
    media_type: String,
    size: u64,
    offset: u64,
    expires_at: u64,
    blob_id: Option<String>,
}

#[derive(Serialize)]
struct CompleteUploadResponse {
    blob_id: String,
}

pub async fn route<B>(
    fleet: &Fleet,
    parts: &Parts,
    segments: &[&str],
    body: B,
) -> Result<Response<ResponseBody>, ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    let response = match (&parts.method, segments) {
        (&Method::POST, ["", "uploads"]) => {
            let input: CreateUpload = api::read_json(&parts.headers, body).await?;
            let id = api::parse_id(&input.id)?;
            let upload = fleet
                .mutate(async move |fleet| {
                    fleet
                        .blob_store()
                        .create_upload(id, &input.media_type, input.size)
                        .await
                })
                .await
                .map_err(ApiError::Fleet)?;
            let response = api::json_response(StatusCode::CREATED, &ApiUpload::from(&upload));
            upload_response(&upload, response)
        }
        (&Method::HEAD, ["", "uploads", id]) => {
            let upload = fleet
                .blob_store()
                .upload(api::parse_id(id)?)
                .await
                .map_err(ApiError::Blob)?;
            upload_response(&upload, api::empty_response(StatusCode::OK))
        }
        (&Method::PATCH, ["", "uploads", id]) => {
            let id = api::parse_id(id)?;
            let (offset, content) = read_chunk(&parts.headers, body).await?;
            let upload = fleet
                .mutate(async move |fleet| {
                    fleet.blob_store().append_upload(id, offset, &content).await
                })
                .await
                .map_err(ApiError::Fleet)?;
            upload_response(&upload, api::empty_response(StatusCode::NO_CONTENT))
        }
        (&Method::POST, ["", "uploads", id, "complete"]) => {
            let _: serde_json::Map<String, serde_json::Value> =
                api::read_json(&parts.headers, body).await?;
            let id = api::parse_id(id)?;
            let blob = fleet
                .mutate(async move |fleet| fleet.blob_store().complete_upload(id).await)
                .await
                .map_err(ApiError::Fleet)?;
            api::json_response(
                StatusCode::OK,
                &CompleteUploadResponse {
                    blob_id: blob.to_string(),
                },
            )
        }
        (&Method::DELETE, ["", "uploads", id]) => {
            let id = api::parse_id(id)?;
            fleet
                .mutate(async move |fleet| fleet.blob_store().delete_upload(id).await)
                .await
                .map_err(ApiError::Fleet)?;
            api::empty_response(StatusCode::NO_CONTENT)
        }
        (&Method::GET | &Method::HEAD, ["", "blobs", id]) => {
            read_blob(fleet, api::parse_id(id)?, parts.method == Method::HEAD).await?
        }
        (_, ["", "uploads"] | ["", "uploads", _, "complete"]) => {
            return Err(ApiError::MethodNotAllowed("POST"));
        }
        (_, ["", "uploads", _]) => return Err(ApiError::MethodNotAllowed("HEAD, PATCH, DELETE")),
        (_, ["", "blobs", _]) => return Err(ApiError::MethodNotAllowed("GET, HEAD")),
        _ => return Err(ApiError::NotFound),
    };
    Ok(secure_response(response))
}

async fn read_chunk<B>(headers: &hyper::HeaderMap, body: B) -> Result<(u64, Bytes), ApiError>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/offset+octet-stream")
        || headers.contains_key(header::CONTENT_ENCODING)
    {
        return Err(ApiError::BadRequest(
            "Expected unencoded application/offset+octet-stream".into(),
        ));
    }
    let mut offsets = headers.get_all("upload-offset").iter();
    let offset = offsets
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| ApiError::BadRequest("Upload-Offset is required".into()))?;
    if offsets.next().is_some() {
        return Err(ApiError::BadRequest("Upload-Offset must occur once".into()));
    }
    let content = Limited::new(body, MAX_UPLOAD_CHUNK_BYTES)
        .collect()
        .await
        .map_err(|error| {
            if error.is::<LengthLimitError>() {
                ApiError::BodyTooLarge
            } else {
                ApiError::BadRequest("Could not read request body".into())
            }
        })?
        .to_bytes();
    Ok((offset, content))
}

fn secure_response(mut response: Response<ResponseBody>) -> Response<ResponseBody> {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "cross-origin-resource-policy",
        header::HeaderValue::from_static("same-origin"),
    );
    response
}

async fn read_blob(
    fleet: &Fleet,
    id: fleet_core::BlobId,
    head: bool,
) -> Result<Response<ResponseBody>, ApiError> {
    let info = fleet
        .blob_store()
        .get(id)
        .await
        .map_err(ApiError::Blob)?
        .ok_or(ApiError::Blob(StoreError::BlobNotFound))?;
    let mut response = if head {
        api::empty_response(StatusCode::OK)
    } else {
        let content = fleet
            .blob_store()
            .read(id, MAX_BLOB_BYTES)
            .await
            .map_err(ApiError::Blob)?;
        Response::new(
            Full::new(Bytes::from(content))
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        )
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        info.media_type
            .parse()
            .map_err(|_| ApiError::Blob(StoreError::InvalidBlob))?,
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, info.size.into());
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static(if info.image.is_some() {
            "inline"
        } else {
            "attachment"
        }),
    );
    response.headers_mut().insert(
        "content-security-policy",
        header::HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    Ok(response)
}

fn upload_response(
    upload: &Upload,
    mut response: Response<ResponseBody>,
) -> Response<ResponseBody> {
    response
        .headers_mut()
        .insert("upload-offset", upload.offset.into());
    response
        .headers_mut()
        .insert("upload-length", upload.size.into());
    response.headers_mut().insert(
        header::LOCATION,
        format!("/uploads/{}", upload.id)
            .parse()
            .expect("upload ID is a header"),
    );
    response
}

impl From<&Upload> for ApiUpload {
    fn from(upload: &Upload) -> Self {
        Self {
            id: upload.id.to_string(),
            media_type: upload.media_type.clone(),
            size: upload.size,
            offset: upload.offset,
            expires_at: upload.expires_at,
            blob_id: upload.blob.map(|id| id.to_string()),
        }
    }
}

pub const fn error_details(error: &StoreError) -> (StatusCode, &'static str, &'static str) {
    match error {
        StoreError::BlobNotFound | StoreError::UploadNotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "Blob or upload not found",
        ),
        StoreError::BlobDenied => (StatusCode::FORBIDDEN, "denied", "Blob access denied"),
        StoreError::BlobTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "limit_exceeded",
            "Image or upload capacity limit exceeded",
        ),
        StoreError::InvalidBlob => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_image",
            "Invalid, unsupported, or oversized image",
        ),
        StoreError::BlobConflict | StoreError::UploadConflict => (
            StatusCode::CONFLICT,
            "conflict",
            "Upload offset or content conflicts with stored state",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "Blob storage failed",
        ),
    }
}
