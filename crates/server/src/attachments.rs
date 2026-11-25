use crate::{
    ServerState,
    routes::{ApiError, ApiResult},
    storage_io,
};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State, multipart::MultipartError},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::Deserialize;
use std::{io, path::Path as FsPath};
use t3_core::{Attachment, CoreError, Store};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub(crate) const MAX_BYTES: usize = 25 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct UploadQuery {
    thread_id: Option<Uuid>,
}

pub(crate) async fn upload(
    State(state): State<ServerState>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, UploadError> {
    let mut file = None;
    let mut thread_id = query.thread_id;
    while let Some(mut field) = multipart.next_field().await.map_err(multipart_error)? {
        match field.name() {
            Some("thread_id") => {
                if thread_id.is_some() {
                    return Err(invalid("duplicate thread_id"));
                }
                let id = field.text().await.map_err(multipart_error)?;
                thread_id = Some(id.parse().map_err(|_| invalid("invalid thread_id"))?);
            }
            Some("file") if file.is_none() => {
                let filename = field
                    .file_name()
                    .filter(|name| valid_filename(name))
                    .ok_or_else(|| invalid("invalid attachment filename"))?
                    .to_owned();
                let mime = valid_mime(field.content_type().unwrap_or("application/octet-stream"))
                    .ok_or_else(|| invalid("invalid attachment MIME type"))?;
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if chunk.len() > MAX_BYTES - bytes.len() {
                        return Err(UploadError::TooLarge);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                file = Some((filename, mime, bytes));
            }
            Some("file") => return Err(invalid("one file per upload is supported")),
            _ => {}
        }
    }
    let (filename, mime_type, bytes) = file.ok_or_else(|| invalid("file is required"))?;
    if let Some(id) = thread_id {
        state.store.get_thread(id).await.map_err(api_error)?;
    }
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let dir = state.config.attachments_dir();
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&dir).await.map_err(io_error)?;
    let dir = tokio::fs::canonicalize(dir).await.map_err(io_error)?;
    let storage_path = dir.join(&hash);
    save_blob(&dir, &storage_path, &bytes)
        .await
        .map_err(io_error)?;
    let attachment = state
        .store
        .save_attachment(Attachment {
            id: Uuid::new_v4(),
            thread_id,
            filename,
            mime_type,
            size_bytes: bytes.len() as u64,
            blake3_hash: hash,
            storage_path,
            created_at: Utc::now(),
        })
        .await
        .map_err(api_error)?;
    Ok(Json(attachment))
}

async fn save_blob(dir: &FsPath, target: &FsPath, bytes: &[u8]) -> io::Result<()> {
    if valid_blob(target, bytes.len() as u64)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    let temp = dir.join(format!(".{}.upload", Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temp).await?;
    let result = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        match tokio::fs::hard_link(&temp, target).await {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && valid_blob(target, bytes.len() as u64).await? =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    .await;
    let _ = tokio::fs::remove_file(temp).await;
    result
}

async fn valid_blob(path: &FsPath, size: u64) -> io::Result<bool> {
    let info = tokio::fs::symlink_metadata(path).await?;
    Ok(info.is_file() && info.len() == size)
}

pub(crate) async fn download(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let attachment = state.store.get_attachment(id).await?;
    let hash = &attachment.blake3_hash;
    if hash.len() != 64
        || !hash.bytes().all(|ch| ch.is_ascii_hexdigit())
        || attachment.size_bytes > MAX_BYTES as u64
    {
        return Err(CoreError::NotFound("attachment".into()).into());
    }
    let path = state.config.attachments_dir().join(hash);
    if !valid_blob(&path, attachment.size_bytes)
        .await
        .unwrap_or(false)
    {
        return Err(CoreError::NotFound("attachment blob".into()).into());
    }
    let bytes = tokio::fs::read(path).await.map_err(storage_io)?;
    let mime =
        valid_mime(&attachment.mime_type).unwrap_or_else(|| "application/octet-stream".into());
    let disposition = content_disposition(&attachment.filename);
    Ok((
        [
            (header::CONTENT_TYPE, mime.as_str()),
            (header::CONTENT_DISPOSITION, disposition.as_str()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; sandbox; frame-ancestors 'none'",
            ),
        ],
        Body::from(bytes),
    )
        .into_response())
}

fn content_disposition(name: &str) -> String {
    use std::fmt::Write;
    let clean: String = name
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | '"') {
                '_'
            } else {
                ch
            }
        })
        .collect();
    let clean = if matches!(clean.as_str(), "" | "." | "..") {
        "download"
    } else {
        &clean
    };
    let ascii: String = clean
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let mut value = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''");
    for byte in clean.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_') {
            value.push(char::from(byte));
        } else {
            write!(&mut value, "%{byte:02X}").expect("write to String");
        }
    }
    value
}

fn valid_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
        && name.len() <= 255
}

fn valid_mime(value: &str) -> Option<String> {
    let (kind, subtype) = value.split(';').next()?.trim().split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|ch| {
                ch.is_ascii_alphanumeric()
                    || matches!(
                        ch,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    (value.len() <= 255 && valid(kind) && valid(subtype)).then(|| {
        let mime = format!("{kind}/{subtype}").to_ascii_lowercase();
        // The PWA opens downloads as object URLs. Serve active documents as data.
        if mime == "text/html" || mime.ends_with("/xml") || mime.ends_with("+xml") {
            "application/octet-stream".into()
        } else {
            mime
        }
    })
}

pub(crate) enum UploadError {
    Api(ApiError),
    Multipart(MultipartError),
    TooLarge,
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(error) => error.into_response(),
            Self::Multipart(error) => (
                error.status(),
                Json(serde_json::json!({"error": error.body_text()})),
            )
                .into_response(),
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        }
    }
}

fn invalid(message: &str) -> UploadError {
    api_error(CoreError::InvalidArgument(message.into()))
}

fn io_error(error: io::Error) -> UploadError {
    api_error(storage_io(error))
}

fn api_error(error: CoreError) -> UploadError {
    UploadError::Api(error.into())
}

fn multipart_error(error: MultipartError) -> UploadError {
    UploadError::Multipart(error)
}
