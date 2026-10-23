//! Content-addressed artifact bytes on local disk or S3-compatible storage.
//!
//! An artifact lives once at `artifacts/sha256/<hex>`. Uploads arrive as
//! chunks staged at `uploads/<upload id>/<offset>`; completing an upload
//! hashes the staged chunks and only then writes the artifact, so a
//! mismatched or partial upload never becomes one. Every read hashes the
//! artifact first and refuses it if the bytes do not match their name.
//!
//! Which namespaces may read an artifact, and how far an upload has come,
//! is registry state (`crate::registry`); this module only holds bytes.

use bytes::Bytes;
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Largest chunk one call may send, under the gRPC frame limit.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Default cap on one artifact (`HINAGATA_MAX_ARTIFACT_BYTES`).
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactError {
    NotFound,
    /// The stored bytes do not hash to the artifact's name.
    Corrupt(String),
    /// The staged upload does not match its declared digest and size.
    Mismatch(String),
    Storage(String),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("artifact not found"),
            Self::Corrupt(message) | Self::Mismatch(message) | Self::Storage(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for ArtifactError {}

fn storage(error: object_store::Error) -> ArtifactError {
    match error {
        object_store::Error::NotFound { .. } => ArtifactError::NotFound,
        other => ArtifactError::Storage(other.to_string()),
    }
}

/// A chunk the registry accepted for an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChunk {
    pub offset: u64,
    pub key: String,
}

pub struct ArtifactStore {
    store: Arc<dyn ObjectStore>,
    max_bytes: u64,
    /// The directory of a local store: a missing one lists as empty, so
    /// the health check looks at it directly.
    local_root: Option<std::path::PathBuf>,
}

impl ArtifactStore {
    pub fn new(store: Arc<dyn ObjectStore>, max_bytes: u64) -> Self {
        Self {
            store,
            max_bytes,
            local_root: None,
        }
    }

    /// Artifacts under a local directory, created if missing.
    pub fn local(dir: &std::path::Path, max_bytes: u64) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        // Writes are fsynced: the registry records an upload's progress
        // durably, so the bytes it points to must be durable first.
        let store = object_store::local::LocalFileSystem::new_with_prefix(dir)
            .map_err(|error| error.to_string())?
            .with_fsync(true);
        Ok(Self {
            local_root: Some(dir.to_path_buf()),
            ..Self::new(Arc::new(store), max_bytes)
        })
    }

    /// Artifacts in an S3-compatible bucket. Credentials and region come
    /// from the standard `AWS_*` environment; `endpoint` selects a
    /// non-AWS store, and plain `http://` endpoints are allowed for it.
    pub fn s3(bucket: &str, endpoint: Option<&str>, max_bytes: u64) -> Result<Self, String> {
        let mut builder = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
        if let Some(endpoint) = endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        let store = builder.build().map_err(|error| error.to_string())?;
        Ok(Self::new(Arc::new(store), max_bytes))
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether the store answers a listing: the health check's storage
    /// probe. For S3 this also proves the bucket and credentials work.
    pub async fn health(&self) -> Result<(), ArtifactError> {
        if let Some(root) = &self.local_root
            && !root.is_dir()
        {
            return Err(ArtifactError::Storage(format!(
                "the artifact directory {} is gone",
                root.display()
            )));
        }
        let prefix = StorePath::from("artifacts");
        match self.store.list(Some(&prefix)).next().await {
            None | Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(storage(error)),
        }
    }

    fn artifact_path(hex: &str) -> StorePath {
        StorePath::from(format!("artifacts/sha256/{hex}"))
    }

    fn upload_prefix(upload_id: &str) -> StorePath {
        StorePath::from(format!("uploads/{upload_id}"))
    }

    /// Stage one chunk of an upload at `offset` and return its key. The key
    /// names the offset and the content, so a different chunk sent for the
    /// same offset never replaces one the registry accepted.
    pub async fn stage_chunk(
        &self,
        upload_id: &str,
        attempt: u64,
        offset: u64,
        data: Bytes,
    ) -> Result<String, ArtifactError> {
        let content = format!("{:x}", Sha256::digest(&data));
        let key = format!(
            "uploads/{upload_id}/{attempt:020}/{offset:020}-{}",
            &content[..32]
        );
        self.store
            .put(&StorePath::from(key.as_str()), PutPayload::from(data))
            .await
            .map_err(storage)?;
        Ok(key)
    }

    /// Everything staged for an upload's attempts up to `attempt`,
    /// accepted or not.
    async fn staged(&self, upload_id: &str, attempt: u64) -> Result<Vec<StorePath>, ArtifactError> {
        let mut paths = Vec::new();
        let mut listing = self.store.list(Some(&Self::upload_prefix(upload_id)));
        while let Some(meta) = listing.next().await {
            let location = meta.map_err(storage)?.location;
            let staged_attempt = location
                .parts()
                .nth(2)
                .and_then(|part| part.as_ref().parse::<u64>().ok());
            if staged_attempt.is_none_or(|staged| staged <= attempt) {
                paths.push(location);
            }
        }
        Ok(paths)
    }

    /// Drop what an upload staged in attempts up to `attempt`; a later
    /// attempt's chunks are left alone.
    pub async fn discard_upload(&self, upload_id: &str, attempt: u64) -> Result<(), ArtifactError> {
        for path in self.staged(upload_id, attempt).await? {
            match self.store.delete(&path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(storage(error)),
            }
        }
        Ok(())
    }

    async fn chunk_bytes(&self, key: &str) -> Result<Bytes, ArtifactError> {
        self.store
            .get(&StorePath::from(key))
            .await
            .map_err(storage)?
            .bytes()
            .await
            .map_err(storage)
    }

    /// Check the accepted chunks, in offset order, against `hex` and `size`,
    /// then write them as the artifact. An artifact already stored is kept
    /// if it verifies and replaced if it does not. A mismatch writes
    /// nothing. Staged chunks stay until [`Self::discard_upload`], so the
    /// caller records the outcome durably before dropping them.
    pub async fn publish(
        &self,
        chunks: &[StagedChunk],
        hex: &str,
        size: u64,
    ) -> Result<(), ArtifactError> {
        let mut expected = 0;
        let mut hasher = Sha256::new();
        for chunk in chunks {
            if chunk.offset != expected {
                return Err(ArtifactError::Mismatch(format!(
                    "the upload has a gap at byte {expected}"
                )));
            }
            let bytes = self.chunk_bytes(&chunk.key).await?;
            expected += bytes.len() as u64;
            hasher.update(&bytes);
        }
        let actual = format!("{:x}", hasher.finalize());
        if expected != size || actual != hex {
            return Err(ArtifactError::Mismatch(format!(
                "the upload holds {expected} bytes with sha256:{actual}, not {size} bytes with sha256:{hex}; send it again"
            )));
        }
        match self.verify(hex).await {
            Ok(_) => {}
            Err(ArtifactError::NotFound | ArtifactError::Corrupt(_)) => {
                self.write_artifact(chunks, hex).await?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// Write `chunks` as the artifact, hashing again on the way: bytes that
    /// changed after the check abort the write instead of becoming the
    /// artifact. At most a few parts are in flight, so a slow store never
    /// holds a whole artifact in memory.
    async fn write_artifact(&self, chunks: &[StagedChunk], hex: &str) -> Result<(), ArtifactError> {
        let target = Self::artifact_path(hex);
        let mut writer =
            WriteMultipart::new(self.store.put_multipart(&target).await.map_err(storage)?);
        let mut rehash = Sha256::new();
        let mut failed = None;
        for chunk in chunks {
            if let Err(error) = writer.wait_for_capacity(4).await {
                failed = Some(storage(error));
                break;
            }
            match self.chunk_bytes(&chunk.key).await {
                Ok(bytes) => {
                    rehash.update(&bytes);
                    writer.write(&bytes);
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        if failed.is_none() && format!("{:x}", rehash.finalize()) != hex {
            failed = Some(ArtifactError::Mismatch(
                "the upload changed while it was published; send it again".into(),
            ));
        }
        // A failed write is aborted, so it leaves no unfinished parts.
        if let Some(error) = failed {
            let _ = writer.abort().await;
            return Err(error);
        }
        writer.finish().await.map_err(storage)?;
        Ok(())
    }

    /// Hash a stored artifact, refusing it unless it reads back as `hex`.
    pub async fn verify(&self, hex: &str) -> Result<u64, ArtifactError> {
        let mut stream = self
            .store
            .get(&Self::artifact_path(hex))
            .await
            .map_err(storage)?
            .into_stream();
        let mut hasher = Sha256::new();
        let mut size = 0;
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.map_err(storage)?;
            size += bytes.len() as u64;
            hasher.update(&bytes);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != hex {
            return Err(ArtifactError::Corrupt(format!(
                "artifact sha256:{hex} reads back as sha256:{actual}; it is refused"
            )));
        }
        Ok(size)
    }

    /// The artifact's bytes in chunks of at most [`MAX_CHUNK_BYTES`]. The
    /// artifact is verified before the first byte; the stream hashes again
    /// as it goes and ends in an error if the bytes changed in between.
    pub async fn read_verified(
        &self,
        hex: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<Bytes, ArtifactError>>, ArtifactError>
    {
        self.verify(hex).await?;
        let inner = self
            .store
            .get(&Self::artifact_path(hex))
            .await
            .map_err(storage)?
            .into_stream();
        struct State {
            inner: futures::stream::BoxStream<'static, object_store::Result<Bytes>>,
            hasher: Option<Sha256>,
            pending: Bytes,
            hex: String,
        }
        let state = State {
            inner,
            hasher: Some(Sha256::new()),
            pending: Bytes::new(),
            hex: hex.to_string(),
        };
        Ok(futures::stream::unfold(state, |mut state| async move {
            loop {
                if !state.pending.is_empty() {
                    let take = state.pending.len().min(MAX_CHUNK_BYTES);
                    let chunk = state.pending.split_to(take);
                    return Some((Ok(chunk), state));
                }
                let hasher = state.hasher.as_mut()?;
                match state.inner.next().await {
                    Some(Ok(bytes)) => {
                        hasher.update(&bytes);
                        state.pending = bytes;
                    }
                    Some(Err(error)) => {
                        state.hasher = None;
                        return Some((Err(storage(error)), state));
                    }
                    None => {
                        let actual = format!("{:x}", state.hasher.take()?.finalize());
                        if actual == state.hex {
                            return None;
                        }
                        let error = ArtifactError::Corrupt(format!(
                            "artifact sha256:{} changed while it was read",
                            state.hex
                        ));
                        return Some((Err(error), state));
                    }
                }
            }
        })
        .boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn stores() -> Vec<(ArtifactStore, Option<tempfile::TempDir>)> {
        let dir = tempfile::tempdir().unwrap();
        vec![
            (
                ArtifactStore::new(Arc::new(object_store::memory::InMemory::new()), 1 << 20),
                None,
            ),
            (
                ArtifactStore::local(dir.path(), 1 << 20).unwrap(),
                Some(dir),
            ),
        ]
    }

    async fn stage(store: &ArtifactStore, upload: &str, offset: u64, data: &[u8]) -> StagedChunk {
        let key = store
            .stage_chunk(upload, 0, offset, Bytes::copy_from_slice(data))
            .await
            .unwrap();
        StagedChunk { offset, key }
    }

    async fn read_all(store: &ArtifactStore, hex: &str) -> Vec<u8> {
        let chunks: Vec<Bytes> = store
            .read_verified(hex)
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;
        chunks.concat()
    }

    #[tokio::test]
    async fn a_missing_local_root_is_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("artifacts-root");
        let store = ArtifactStore::local(&root, 1 << 20).unwrap();
        assert_eq!(store.health().await, Ok(()));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(store.health().await.is_err());
    }

    #[tokio::test]
    async fn a_staged_upload_publishes_only_when_it_matches() {
        for (store, _dir) in stores() {
            let body = b"model weights".to_vec();
            let first = stage(&store, "u1", 0, b"model ").await;
            // A different chunk sent for an accepted offset lands beside it.
            let stray = stage(&store, "u1", 0, b"MODEL ").await;
            assert_ne!(first.key, stray.key);
            let second = stage(&store, "u1", 6, b"weights").await;
            store
                .publish(&[first, second], &hex(&body), body.len() as u64)
                .await
                .unwrap();
            assert_eq!(read_all(&store, &hex(&body)).await, body);
            store.discard_upload("u1", 0).await.unwrap();
            assert!(store.staged("u1", u64::MAX).await.unwrap().is_empty());

            let wrong = stage(&store, "u2", 0, b"tampered").await;
            let refused = store
                .publish(&[wrong], &hex(b"other"), 8)
                .await
                .unwrap_err();
            assert!(matches!(refused, ArtifactError::Mismatch(_)), "{refused:?}");
            assert_eq!(
                store.verify(&hex(b"other")).await,
                Err(ArtifactError::NotFound)
            );

            let tail = stage(&store, "u3", 4, b"tail").await;
            assert!(matches!(
                store.publish(&[tail], &hex(b"headtail"), 8).await,
                Err(ArtifactError::Mismatch(message)) if message.contains("gap")
            ));
        }
    }

    #[tokio::test]
    async fn a_corrupted_artifact_is_refused_on_read_and_repaired_by_a_valid_upload() {
        for (store, _dir) in stores() {
            let body = b"weights".to_vec();
            let chunk = stage(&store, "u1", 0, &body).await;
            store
                .publish(&[chunk], &hex(&body), body.len() as u64)
                .await
                .unwrap();
            store
                .store
                .put(
                    &ArtifactStore::artifact_path(&hex(&body)),
                    PutPayload::from_static(b"weightz"),
                )
                .await
                .unwrap();
            assert!(matches!(
                store.read_verified(&hex(&body)).await.err(),
                Some(ArtifactError::Corrupt(_))
            ));
            let chunk = stage(&store, "u2", 0, &body).await;
            store
                .publish(&[chunk], &hex(&body), body.len() as u64)
                .await
                .unwrap();
            assert_eq!(read_all(&store, &hex(&body)).await, body);
        }
    }
}
