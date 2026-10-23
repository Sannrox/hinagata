//! Signed export and import bundles for offline transfer (#13).
//!
//! A bundle is a directory (format in `docs/bundles.md`):
//!
//! - `bundle.json`: `{"format": "hinagata.bundle/v1"}`.
//! - `manifests/<model>-<version>.json`: the source registry's
//!   `GetVersionManifest` response, which signs the artifact digest and the
//!   metadata.
//! - `artifacts/<hex>`: each artifact, named by its SHA-256.
//!
//! [`export`] reads versions from a registry over gRPC. [`verify`] checks a
//! bundle offline against trusted keys: every signature, every metadata
//! digest and every artifact digest. [`import`] verifies first, asks the
//! target to validate every manifest, and only then uploads artifacts and
//! imports versions, so a bundle that fails any check changes nothing.

use crate::api::proto::hinagata_client::HinagataClient;
use crate::api::proto::{
    BeginArtifactUploadRequest, CompleteArtifactUploadRequest, CreateModelRequest,
    GetVersionManifestRequest, ImportVersionRequest, ReadArtifactRequest,
    UploadArtifactChunkRequest, VersionManifest,
};
use crate::signing::{Envelope, Manifest, TrustedKeys};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tonic::transport::Channel;

pub const BUNDLE_FORMAT: &str = "hinagata.bundle/v1";
const CHUNK_BYTES: usize = crate::artifacts::MAX_CHUNK_BYTES;

/// A registry client with the caller's bearer token.
pub struct Client {
    pub inner: HinagataClient<Channel>,
    pub token: String,
}

impl Client {
    /// Connect to `server` (`http://` or `https://`). HTTPS needs the CA
    /// certificate (PEM) that signed the server's certificate.
    pub async fn connect(server: &str, token: String, ca: Option<Vec<u8>>) -> Result<Self, String> {
        use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
        let mut endpoint = Endpoint::from_shared(server.to_string())
            .map_err(|error| format!("server: {error}"))?;
        match (server.starts_with("https://"), ca) {
            (true, Some(ca)) => {
                endpoint = endpoint
                    .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca)))
                    .map_err(|error| format!("TLS: {error}"))?;
            }
            (true, None) => return Err("an https server needs --ca <certificate file>".into()),
            (false, Some(_)) => return Err("--ca needs an https server".into()),
            (false, None) => {}
        }
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| format!("connect to {server}: {error}"))?;
        Ok(Self {
            inner: HinagataClient::new(channel),
            token: token.trim().to_string(),
        })
    }

    fn request<T>(&self, message: T) -> Result<tonic::Request<T>, String> {
        let mut request = tonic::Request::new(message);
        let value = format!("Bearer {}", self.token)
            .parse()
            .map_err(|_| "the token is not a valid header value")?;
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }
}

fn status(error: tonic::Status) -> String {
    format!("{:?}: {}", error.code(), error.message())
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |error| format!("{}: {error}", path.display())
}

fn hex_of(digest: &str) -> Result<&str, String> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("`{digest}` is not a sha256 digest"))
}

/// Write `versions` of `model` from the registry into a new bundle at
/// `dir`, which must not exist or be empty. Each artifact is checked
/// against its digest as it is written.
pub async fn export(
    client: &mut Client,
    model: &str,
    versions: &[u64],
    dir: &Path,
) -> Result<Vec<String>, String> {
    if dir.exists() && std::fs::read_dir(dir).map_err(io(dir))?.next().is_some() {
        return Err(format!("{} is not empty", dir.display()));
    }
    for sub in ["manifests", "artifacts"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(io(dir))?;
    }
    let mut written = Vec::new();
    for &version in versions {
        let request = client.request(GetVersionManifestRequest {
            model: model.into(),
            version,
        })?;
        let bundled = client
            .inner
            .get_version_manifest(request)
            .await
            .map_err(status)?
            .into_inner();
        let manifest: Manifest = serde_json::from_str(&bundled.manifest)
            .map_err(|error| format!("manifest: {error}"))?;
        let hex = hex_of(&manifest.artifact_digest)?;
        let artifact = dir.join("artifacts").join(hex);
        if !artifact.exists() {
            let request = client.request(ReadArtifactRequest {
                digest: manifest.artifact_digest.clone(),
            })?;
            let mut stream = client
                .inner
                .read_artifact(request)
                .await
                .map_err(status)?
                .into_inner();
            let partial = artifact.with_extension("partial");
            let mut file = std::fs::File::create(&partial).map_err(io(&partial))?;
            let mut hasher = Sha256::new();
            while let Some(chunk) = stream.message().await.map_err(status)? {
                hasher.update(&chunk.data);
                file.write_all(&chunk.data).map_err(io(&partial))?;
            }
            file.sync_all().map_err(io(&partial))?;
            if format!("{:x}", hasher.finalize()) != hex {
                return Err(format!("artifact {hex} did not match its digest"));
            }
            std::fs::rename(&partial, &artifact).map_err(io(&artifact))?;
        }
        let path = dir
            .join("manifests")
            .join(format!("{model}-{version}.json"));
        let body = serde_json::to_vec_pretty(&bundled).map_err(|error| error.to_string())?;
        std::fs::write(&path, body).map_err(io(&path))?;
        written.push(format!("{model} version {version}"));
    }
    let marker = dir.join("bundle.json");
    std::fs::write(
        &marker,
        serde_json::json!({ "format": BUNDLE_FORMAT }).to_string(),
    )
    .map_err(io(&marker))?;
    Ok(written)
}

/// One verified version of a bundle.
pub struct Entry {
    pub bundled: VersionManifest,
    pub manifest: Manifest,
    pub artifact: PathBuf,
}

/// Check every manifest of the bundle at `dir` against `keys`, its
/// metadata against the manifest, and its artifact against the digest.
/// Returns the versions in source order: by model, then version.
pub fn verify(keys: &TrustedKeys, dir: &Path) -> Result<Vec<Entry>, String> {
    #[derive(serde::Deserialize)]
    struct Marker {
        format: String,
    }
    let marker = dir.join("bundle.json");
    let marker: Marker = serde_json::from_slice(&std::fs::read(&marker).map_err(io(&marker))?)
        .map_err(|error| format!("bundle.json: {error}"))?;
    if marker.format != BUNDLE_FORMAT {
        return Err(format!("bundle format must be {BUNDLE_FORMAT}"));
    }
    let manifests = dir.join("manifests");
    let mut entries = Vec::new();
    for file in std::fs::read_dir(&manifests).map_err(io(&manifests))? {
        let path = file.map_err(io(&manifests))?.path();
        let raw = std::fs::read(&path).map_err(io(&path))?;
        let bundled: VersionManifest =
            serde_json::from_slice(&raw).map_err(|error| format!("{}: {error}", path.display()))?;
        let manifest = keys
            .verify(&Envelope {
                format: bundled.format.clone(),
                key_id: bundled.key_id.clone(),
                manifest: bundled.manifest.clone(),
                signature: bundled.signature.clone(),
            })
            .map_err(|error| format!("{}: {error}", path.display()))?;
        crate::signing::check_contents(&manifest, None, Some(bundled.metadata.as_bytes()))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let artifact = dir
            .join("artifacts")
            .join(hex_of(&manifest.artifact_digest)?);
        let mut file = std::fs::File::open(&artifact).map_err(io(&artifact))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            let read = file.read(&mut buffer).map_err(io(&artifact))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if format!("sha256:{:x}", hasher.finalize()) != manifest.artifact_digest {
            return Err(format!(
                "{}: the artifact does not match the signed digest",
                artifact.display()
            ));
        }
        entries.push(Entry {
            bundled,
            manifest,
            artifact,
        });
    }
    entries.sort_by(|a, b| {
        (&a.manifest.model, a.manifest.version).cmp(&(&b.manifest.model, b.manifest.version))
    });
    Ok(entries)
}

/// Verify the bundle at `dir`, have the target validate every manifest,
/// then upload each artifact and import each version. Importing a bundle
/// again is a no-op for versions already imported.
pub async fn import(
    client: &mut Client,
    keys: &TrustedKeys,
    dir: &Path,
) -> Result<Vec<String>, String> {
    let entries = verify(keys, dir)?;
    for entry in &entries {
        let request = client.request(ImportVersionRequest {
            manifest: Some(entry.bundled.clone()),
            validate_only: true,
        })?;
        client
            .inner
            .import_version(request)
            .await
            .map_err(|error| format!("{}: {}", entry.manifest.model, status(error)))?;
    }
    let mut imported = Vec::new();
    for entry in entries {
        let request = client.request(CreateModelRequest {
            name: entry.manifest.model.clone(),
            description: String::new(),
        })?;
        match client.inner.create_model(request).await {
            Ok(_) => {}
            Err(error) if error.code() == tonic::Code::AlreadyExists => {}
            Err(error) => return Err(status(error)),
        }
        upload(client, &entry.artifact, &entry.manifest.artifact_digest).await?;
        let request = client.request(ImportVersionRequest {
            manifest: Some(entry.bundled),
            validate_only: false,
        })?;
        let version = client
            .inner
            .import_version(request)
            .await
            .map_err(status)?
            .into_inner()
            .version
            .ok_or("the registry returned no version")?;
        imported.push(format!(
            "{} version {} as version {}",
            entry.manifest.model, entry.manifest.version, version.version
        ));
    }
    Ok(imported)
}

/// Upload `path` as `digest`, resuming where the registry's upload is.
async fn upload(client: &mut Client, path: &Path, digest: &str) -> Result<(), String> {
    let size = std::fs::metadata(path).map_err(io(path))?.len();
    let request = client.request(BeginArtifactUploadRequest {
        digest: digest.into(),
        size_bytes: size,
    })?;
    let mut upload = client
        .inner
        .begin_artifact_upload(request)
        .await
        .map_err(status)?
        .into_inner();
    let mut file = std::fs::File::open(path).map_err(io(path))?;
    let mut buffer = vec![0; CHUNK_BYTES];
    while !upload.complete && upload.received_bytes < size {
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(upload.received_bytes))
            .map_err(io(path))?;
        let read = file.read(&mut buffer).map_err(io(path))?;
        if read == 0 {
            return Err(format!("{} ended early", path.display()));
        }
        let request = client.request(UploadArtifactChunkRequest {
            upload_id: upload.upload_id.clone(),
            offset: upload.received_bytes,
            data: buffer[..read].to_vec(),
        })?;
        upload = client
            .inner
            .upload_artifact_chunk(request)
            .await
            .map_err(status)?
            .into_inner();
    }
    if !upload.complete {
        let request = client.request(CompleteArtifactUploadRequest {
            upload_id: upload.upload_id,
        })?;
        client
            .inner
            .complete_artifact_upload(request)
            .await
            .map_err(status)?;
    }
    Ok(())
}
