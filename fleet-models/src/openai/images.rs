use crate::OpenAiError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use fleet_core::{BlobId, ModelsErrorKind};
use fleet_kit::{Blobs, BlobsErrorKind};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 40 * 1024 * 1024;

fn image_urls(values: &[Value]) -> Result<Vec<String>, OpenAiError> {
    fn visit(value: &Value, urls: &mut Vec<String>) -> Result<(), OpenAiError> {
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("input_image") {
                    let url = object
                        .get("image_url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("image has no usable URL"))?;
                    urls.push(url.into());
                } else {
                    for value in object.values() {
                        visit(value, urls)?;
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, urls)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut urls = Vec::new();
    for value in values {
        visit(value, &mut urls)?;
    }
    Ok(urls)
}

fn replace(values: &mut [Value], replacements: &BTreeMap<String, String>) {
    fn visit(value: &mut Value, replacements: &BTreeMap<String, String>) {
        match value {
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("input_image") {
                    if let Some(url) = object.get_mut("image_url")
                        && let Some(replacement) =
                            url.as_str().and_then(|url| replacements.get(url))
                    {
                        *url = replacement.clone().into();
                    }
                } else {
                    for value in object.values_mut() {
                        visit(value, replacements);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, replacements);
                }
            }
            _ => {}
        }
    }
    for value in values {
        visit(value, replacements);
    }
}

pub async fn resolve<B: Blobs + Sync>(
    values: &mut [Value],
    blobs: &B,
) -> Result<BTreeMap<String, String>, OpenAiError>
where
    B::Error: Send + Sync,
{
    let urls = image_urls(values)?;
    if urls.len() > 20 {
        return Err(invalid("too many images in model input"));
    }
    let mut replacements = BTreeMap::new();
    let mut size = 0_u64;
    for url in urls {
        let id = url
            .strip_prefix("fleet-blob:")
            .and_then(|id| id.parse::<u128>().ok())
            .ok_or_else(|| invalid("image has no durable blob reference"))?;
        let info = blobs
            .get(id.into())
            .await
            .map_err(|error| blob_error(id.into(), error))?
            .ok_or_else(|| blob_error(id.into(), BlobsErrorKind::NotFound))?;
        if info.image.is_none()
            || !matches!(
                info.media_type.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        {
            return Err(invalid("blob is not a supported image"));
        }
        size = size.saturating_add(info.size);
        if info.size > MAX_IMAGE_BYTES || size > MAX_REQUEST_BYTES {
            return Err(invalid("image input exceeds request limits"));
        }
        if replacements.contains_key(&url) {
            continue;
        }
        let bytes = blobs
            .read(id.into(), MAX_IMAGE_BYTES)
            .await
            .map_err(|error| blob_error(id.into(), error))?;
        if u64::try_from(bytes.len()).ok() != Some(info.size) {
            return Err(invalid("image blob size changed"));
        }
        replacements.insert(
            url,
            format!("data:{};base64,{}", info.media_type, STANDARD.encode(bytes)),
        );
    }
    replace(values, &replacements);
    Ok(replacements
        .into_iter()
        .map(|(reference, data)| (data, reference))
        .collect())
}

pub fn retain(
    values: &mut [Value],
    references: &BTreeMap<String, String>,
    continuation: bool,
) -> Result<(), OpenAiError> {
    for url in image_urls(values)? {
        references.get(&url).ok_or_else(|| {
            OpenAiError::new(
                if continuation {
                    ModelsErrorKind::InvalidContinuation
                } else {
                    ModelsErrorKind::Other
                },
                "compaction retained an image without a durable blob reference",
            )
        })?;
    }
    replace(values, references);
    Ok(())
}

const fn invalid(message: &'static str) -> OpenAiError {
    OpenAiError::new(ModelsErrorKind::Other, message)
}

fn blob_error(
    id: BlobId,
    error: impl fleet_core::Error<BlobsErrorKind> + Send + Sync + 'static,
) -> OpenAiError {
    OpenAiError::blob(ModelsErrorKind::Other, id, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::Error as _;
    use serde_json::json;

    #[test]
    fn compaction_rejects_unresolved_images_and_requests_full_replay_when_possible() {
        for (continuation, expected) in [
            (true, ModelsErrorKind::InvalidContinuation),
            (false, ModelsErrorKind::Other),
        ] {
            let mut output = [json!({
                "type": "input_image",
                "image_url": "data:image/png;base64,unknown",
            })];
            let error = retain(&mut output, &BTreeMap::new(), continuation).unwrap_err();

            assert_eq!(error.kind(), expected);
        }
        let mut output = [json!({
            "type": "input_image",
            "file_id": "ephemeral",
        })];
        let error = retain(&mut output, &BTreeMap::new(), false).unwrap_err();

        assert_eq!(error.kind(), ModelsErrorKind::Other);
    }

    #[test]
    fn blob_errors_preserve_the_source() {
        use core::error::Error as _;

        let error = blob_error(1.into(), BlobsErrorKind::NotFound);
        let source = error.source().unwrap();

        assert_eq!(error.kind(), ModelsErrorKind::Other);
        assert_eq!(error.to_string(), "could not load image 1");
        assert_eq!(source.to_string(), "blob not found");
    }

    #[test]
    fn excessive_images_fail_before_blob_access() {
        futures_executor::block_on(async {
            let mut values = vec![
                json!({
                    "type": "input_image",
                    "image_url": "fleet-blob:1",
                });
                21
            ];
            let error = resolve(&mut values, &fleet_kit::NoBlobs).await.unwrap_err();

            assert_eq!(error.to_string(), "too many images in model input");
        });
    }
}
