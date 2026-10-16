//! Local files that Markdown in a transcript embeds or links to, served wherever they live on this
//! machine. The machine token already lets its holder direct agents that read any file, so these
//! routes add no reach. They sit behind the same cookie as every other API route.

use super::api::{ApiError, AppState, secure, secure_json};
use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderValue, Response, StatusCode, header},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tokio::{fs, io::AsyncReadExt as _};

/// The largest local image the transcript will display.
const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

/// The largest text file the viewer opens. Rendering, not transfer, is what a larger file would
/// make slow.
const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct FileQuery {
    path: String,
    /// Relative paths are the session's workspace's, or the default workspace's without one.
    session: Option<String>,
}

impl FileQuery {
    fn resolve(&self, state: &AppState) -> PathBuf {
        let workspace = self
            .session
            .as_deref()
            .and_then(|session| state.hub.session_workspace(session))
            .unwrap_or_else(|| state.workspace.clone());
        workspace.join(self.path.strip_prefix("file://").unwrap_or(&self.path))
    }
}

/// Serves an image that an agent's Markdown refers to, as the terminal transcript displays such
/// images. Only files whose bytes are a raster image the browser renders inertly are served, so
/// this reads no other file.
pub(super) async fn image(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FileQuery>,
) -> Result<Response<Body>, ApiError> {
    let missing = || ApiError::new(StatusCode::NOT_FOUND, "invalid_request", "no such image");
    let path = query.resolve(&state);
    let bytes = read_regular_file(&path, MAX_IMAGE_BYTES)
        .await
        .map_err(|_| missing())?;
    let media_type = sniff_image(&bytes).ok_or_else(missing)?;
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(media_type));
    secure(&mut response);
    Ok(response)
}

/// The media type of a PNG, JPEG, GIF, or WebP file, judged by its leading bytes.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

/// A text file for the viewer. The text travels as JSON and the page renders it as sanitized
/// Markdown or plain text, so nothing in the file can run in the page.
#[derive(Serialize)]
struct Document {
    /// The canonical absolute path, which the viewer resolves the document's own links against.
    path: String,
    text: String,
}

/// Why a document cannot be shown. Each case is a state the viewer explains differently.
#[derive(Debug, Error)]
enum DocumentError {
    #[error("No file exists at this path.")]
    NotFound,
    #[error("This path is not a regular file.")]
    NotAFile,
    #[error("This file is larger than the {} MiB the viewer opens.", MAX_DOCUMENT_BYTES / 1024 / 1024)]
    TooLarge,
    #[error("This file is not UTF-8 text.")]
    Binary,
    #[error("This file cannot be read: {0}")]
    Unreadable(#[source] io::Error),
}

impl From<io::Error> for DocumentError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            _ => Self::Unreadable(error),
        }
    }
}

impl From<DocumentError> for ApiError {
    fn from(error: DocumentError) -> Self {
        let (status, code) = match error {
            DocumentError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            DocumentError::NotAFile => (StatusCode::UNPROCESSABLE_ENTITY, "not_a_file"),
            DocumentError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            DocumentError::Binary => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "binary"),
            DocumentError::Unreadable(_) => (StatusCode::FORBIDDEN, "unreadable"),
        };
        Self::new(status, code, error.to_string())
    }
}

/// Serves a text file that an agent's Markdown links to, for the viewer.
pub(super) async fn document(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FileQuery>,
) -> Result<Response<Body>, ApiError> {
    // Use the canonical location for both the read and the viewer's relative links.
    let path = fs::canonicalize(query.resolve(&state))
        .await
        .map_err(DocumentError::from)?;
    let text = read_text(&path).await?;
    let document = Document {
        path: path.to_string_lossy().into_owned(),
        text,
    };
    Ok(secure_json(StatusCode::OK, document))
}

async fn read_text(path: &Path) -> Result<String, DocumentError> {
    let bytes = read_regular_file(path, MAX_DOCUMENT_BYTES).await?;

    // Valid UTF-8 can still be binary data, and NUL never occurs in text.
    let text = String::from_utf8(bytes).map_err(|_| DocumentError::Binary)?;
    if text.contains('\0') {
        return Err(DocumentError::Binary);
    }
    Ok(text)
}

/// Reads `path` if it is a regular file of at most `limit` bytes.
async fn read_regular_file(path: &Path, limit: u64) -> Result<Vec<u8>, DocumentError> {
    // Opening a FIFO waits for a writer unless the open is nonblocking. The type and size are then
    // read from the opened descriptor, so whatever the path names later cannot change the answer.
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(DocumentError::NotAFile);
    }
    if metadata.len() > limit {
        return Err(DocumentError::TooLarge);
    }

    // The file can grow after the size check, so the read itself is bounded too.
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(DocumentError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{super::testing::Harness, DocumentError, MAX_DOCUMENT_BYTES, read_text};
    use axum::{
        body::Body,
        http::{HeaderMap, Method, Request, StatusCode, header},
    };
    use std::{fs, path::Path};
    use url::form_urlencoded::Serializer;

    /// An authenticated GET of `route` with the given query parameters, percent-encoded as the
    /// browser sends them.
    async fn get(
        harness: &Harness,
        route: &str,
        parameters: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let query = Serializer::new(String::new())
            .extend_pairs(parameters)
            .finish();
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("{route}?{query}"))
            .header(header::COOKIE, harness.cookie())
            .body(Body::empty())
            .unwrap();
        harness.send(request).await
    }

    async fn document(
        harness: &Harness,
        parameters: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let (status, _, body) = get(harness, "/api/document", parameters).await;
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn canonical(path: &Path) -> String {
        fs::canonicalize(path).unwrap().to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn only_real_images_are_served_from_disk() {
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let png = directory.path().join("shot.png");
        fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let disguised = directory.path().join("notes.png");
        fs::write(&disguised, b"api key: hunter2").unwrap();
        let absent = directory.path().join("absent.png");

        let (status, headers, body) =
            get(&harness, "/api/file", &[("path", png.to_str().unwrap())]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "image/png");
        assert!(body.starts_with(b"\x89PNG"));
        let (status, _, _) = get(
            &harness,
            "/api/file",
            &[("path", disguised.to_str().unwrap())],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a non-image is never served");
        let (status, _, _) =
            get(&harness, "/api/file", &[("path", absent.to_str().unwrap())]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_document_anywhere_on_disk_is_served_as_json_text_with_its_canonical_path() {
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("scratch pad").join("report.md");
        fs::create_dir(report.parent().unwrap()).unwrap();
        fs::write(&report, "# Report\n\n<script>alert(1)</script>\n").unwrap();

        let (status, headers, body) = get(
            &harness,
            "/api/document",
            &[("path", report.to_str().unwrap())],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["path"], canonical(&report));
        assert_eq!(body["text"], "# Report\n\n<script>alert(1)</script>\n");
    }

    #[tokio::test]
    async fn relative_paths_resolve_against_the_session_or_default_workspace() {
        let harness = Harness::new();
        harness.open_session("s1").await;
        let docs = harness.workspace.path().join("docs");
        fs::create_dir(&docs).unwrap();
        fs::write(docs.join("guide.md"), "guide\n").unwrap();
        fs::write(harness.workspace.path().join("notes.md"), "notes\n").unwrap();
        let guide = canonical(&docs.join("guide.md"));

        let (status, body) =
            document(&harness, &[("path", "docs/guide.md"), ("session", "s1")]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["text"], "guide\n");
        assert_eq!(body["path"], guide);

        let (status, body) = document(&harness, &[("path", "notes.md")]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["text"], "notes\n");
        assert_eq!(
            body["path"],
            canonical(&harness.workspace.path().join("notes.md"))
        );
    }

    #[tokio::test]
    async fn documents_that_cannot_be_shown_explain_why() {
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("blob.bin");
        fs::write(&binary, b"\x00\x01\x02text").unwrap();
        let latin1 = directory.path().join("latin1.txt");
        fs::write(&latin1, b"caf\xe9\n").unwrap();
        let large = directory.path().join("large.log");
        let length = usize::try_from(MAX_DOCUMENT_BYTES).unwrap() + 1;
        fs::write(&large, "a".repeat(length)).unwrap();
        let cases = [
            (
                directory.path().join("absent.md"),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (
                directory.path().to_owned(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "not_a_file",
            ),
            (binary, StatusCode::UNSUPPORTED_MEDIA_TYPE, "binary"),
            (latin1, StatusCode::UNSUPPORTED_MEDIA_TYPE, "binary"),
            (large, StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
        ];

        for (path, expected_status, expected_code) in cases {
            let (status, body) = document(&harness, &[("path", path.to_str().unwrap())]).await;
            assert_eq!(status, expected_status, "{}", path.display());
            assert_eq!(body["code"], expected_code, "{}", path.display());
            assert!(body.get("text").is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_document_is_reported_without_its_contents() {
        use std::os::unix::fs::PermissionsExt as _;
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let secret = directory.path().join("secret.txt");
        fs::write(&secret, "hunter2\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

        // A superuser reads the file regardless of its mode, so the case cannot arise.
        if fs::read(&secret).is_ok() {
            return;
        }

        let (status, body) = document(&harness, &[("path", secret.to_str().unwrap())]).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "unreadable");
        assert!(!body.to_string().contains("hunter2"));
    }

    /// A FIFO swapped in after the path was resolved must not hold the request open waiting for
    /// a writer, so the read itself refuses it without any earlier check.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fifo_is_refused_at_open_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("pipe.md");
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(created.success());

        let read = tokio::time::timeout(std::time::Duration::from_secs(5), read_text(&fifo)).await;

        // A blocked open would also block the runtime's shutdown, so a writer releases it first
        // and the failure is reported instead of hanging.
        if read.is_err() {
            drop(fs::OpenOptions::new().write(true).open(&fifo));
        }
        let read = read.expect("opening a FIFO without a writer must not block");
        assert!(matches!(read, Err(DocumentError::NotAFile)), "{read:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_reports_the_target_location_for_relative_links() {
        let harness = Harness::new();
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report");
        fs::write(&report, "# Report\n").unwrap();
        let link = directory.path().join("alias.md");
        std::os::unix::fs::symlink(&report, &link).unwrap();

        let (status, body) = document(&harness, &[("path", link.to_str().unwrap())]).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["text"], "# Report\n");
        assert_eq!(body["path"], canonical(&report));
    }
}
