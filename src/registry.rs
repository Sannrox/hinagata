//! The model registry: models and immutable versions per namespace, version
//! stages, and an audit event for every change.
//!
//! Metadata lives in one embedded SQLite file owned by this module.
//! Artifacts stay in the artifact store; a version records only their
//! digest. Every write runs in one transaction with its audit event, so an
//! interrupted write leaves neither. The database refuses to change a
//! registered version's content (triggers back this up below the API), and
//! a model or version can never be deleted.
//!
//! Every call takes the caller's namespace; nothing reads across namespaces.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Schema migrations, applied in order and recorded in `user_version`.
/// Append only: an older database must always open with a newer binary.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE models (
    namespace TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (namespace, name)
);
CREATE TABLE versions (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    artifact_digest TEXT NOT NULL,
    framework TEXT NOT NULL,
    format TEXT NOT NULL,
    signature TEXT NOT NULL,
    metrics TEXT NOT NULL,
    tags TEXT NOT NULL,
    stage TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (namespace, model, version),
    UNIQUE (namespace, model, artifact_digest),
    FOREIGN KEY (namespace, model) REFERENCES models (namespace, name)
);
CREATE TRIGGER versions_are_immutable
BEFORE UPDATE OF namespace, model, version, artifact_digest, framework, format,
    signature, metrics, tags, created_at_ms, created_by ON versions
BEGIN
    SELECT RAISE(ABORT, 'a registered version is immutable');
END;
CREATE TRIGGER versions_are_never_deleted BEFORE DELETE ON versions
BEGIN
    SELECT RAISE(ABORT, 'a registered version is never deleted');
END;
CREATE TRIGGER models_are_never_deleted BEFORE DELETE ON models
BEGIN
    SELECT RAISE(ABORT, 'a model is never deleted');
END;
CREATE TABLE audit_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    before TEXT,
    after TEXT
);
CREATE INDEX audit_events_by_model ON audit_events (namespace, model, seq);
CREATE TRIGGER audit_events_are_append_only BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are append only');
END;
CREATE TRIGGER audit_events_are_never_deleted BEFORE DELETE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are never deleted');
END;
"#];

/// Most items one list call returns.
pub const MAX_PAGE: u32 = 1000;
const DEFAULT_PAGE: u32 = 100;

/// Version formats a version may declare. ONNX first.
const FORMATS: &[&str] = &["onnx"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Invalid(String),
    NotFound(String),
    /// The name or artifact is taken by different content.
    Conflict(String),
    /// The change is not allowed from the current state.
    Refused(String),
    Storage(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::Refused(message)
            | Self::Storage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<rusqlite::Error> for RegistryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Candidate,
    Staging,
    Production,
    Archived,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Staging => "staging",
            Self::Production => "production",
            Self::Archived => "archived",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            "archived" => Ok(Self::Archived),
            other => Err(RegistryError::Invalid(format!(
                "unknown stage `{other}`: expected candidate, staging, production or archived"
            ))),
        }
    }

    /// Whether a version may move from `self` to `to`. Production needs
    /// the caller-governed approval hook, which is not available yet, so
    /// every promotion to it is refused. Archived is final.
    fn check_transition(self, to: Self) -> Result<()> {
        match (self, to) {
            (_, Self::Production) => Err(RegistryError::Refused(
                "promotion to production needs an approval hook, and none is configured".into(),
            )),
            (Self::Archived, _) => Err(RegistryError::Refused(
                "an archived version stays archived".into(),
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Model {
    pub name: String,
    pub description: String,
    pub created_at_ms: i64,
    pub created_by: String,
}

/// One declared input or output of a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TensorSpec {
    pub name: String,
    pub dtype: String,
    /// Dimensions; -1 is a dynamic dimension.
    pub shape: Vec<i64>,
    pub nullable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Signature {
    pub inputs: Vec<TensorSpec>,
    pub outputs: Vec<TensorSpec>,
}

/// What a caller registers. Everything here is fixed once registered.
#[derive(Debug, Clone, PartialEq)]
pub struct NewVersion {
    pub artifact_digest: String,
    pub framework: String,
    pub format: String,
    pub signature: Signature,
    pub metrics: BTreeMap<String, f64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Version {
    pub model: String,
    pub version: u64,
    pub artifact_digest: String,
    pub framework: String,
    pub format: String,
    pub signature: Signature,
    pub metrics: BTreeMap<String, f64>,
    pub tags: BTreeMap<String, String>,
    pub stage: Stage,
    pub created_at_ms: i64,
    pub created_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub seq: u64,
    pub at_ms: i64,
    pub actor: String,
    pub action: String,
    pub model: String,
    /// Zero for model events.
    pub version: u64,
    /// JSON of the record before the change; `None` for a creation.
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Who acts and when. The API fills it from the authenticated identity.
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    pub namespace: &'a str,
    pub subject: &'a str,
    pub now_ms: i64,
}

pub struct Registry {
    conn: Mutex<Connection>,
}

impl Registry {
    /// Open (creating if needed) the registry database at `path` and bring
    /// its schema up to date. A database written by a newer schema is
    /// refused rather than read with the wrong layout.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// A private registry in memory, for tests and trials.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // Memory databases report `memory`; only files switch to WAL.
        let _: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn write<T>(&self, apply: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Storage("registry lock poisoned".into()))?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = apply(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    fn read<T>(&self, apply: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| RegistryError::Storage("registry lock poisoned".into()))?;
        apply(&conn)
    }

    /// Create a model. Creating it again with the same description returns
    /// the stored model unchanged, so a retried create is safe.
    pub fn create_model(&self, actor: Actor<'_>, name: &str, description: &str) -> Result<Model> {
        check_name("model name", name)?;
        check_text("description", description, 4096)?;
        self.write(|tx| {
            if let Some(stored) = load_model(tx, actor.namespace, name)? {
                return if stored.description == description {
                    Ok(stored)
                } else {
                    Err(RegistryError::Conflict(format!(
                        "model `{name}` already exists with another description"
                    )))
                };
            }
            let model = Model {
                name: name.into(),
                description: description.into(),
                created_at_ms: actor.now_ms,
                created_by: actor.subject.into(),
            };
            tx.execute(
                "INSERT INTO models (namespace, name, description, created_at_ms, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    actor.namespace,
                    model.name,
                    model.description,
                    model.created_at_ms,
                    model.created_by
                ],
            )?;
            audit(
                tx,
                actor,
                "model.created",
                name,
                0,
                None::<&Model>,
                Some(&model),
            )?;
            Ok(model)
        })
    }

    pub fn get_model(&self, namespace: &str, name: &str) -> Result<Model> {
        self.read(|conn| {
            load_model(conn, namespace, name)?
                .ok_or_else(|| RegistryError::NotFound(format!("model `{name}` does not exist")))
        })
    }

    /// Models in name order, after `after` when it is set.
    pub fn list_models(&self, namespace: &str, after: &str, limit: u32) -> Result<Vec<Model>> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT name, description, created_at_ms, created_by FROM models
                 WHERE namespace = ?1 AND name > ?2 ORDER BY name LIMIT ?3",
            )?;
            let models = statement
                .query_map(params![namespace, after, page(limit)], model_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(models)
        })
    }

    /// Register the next version of `model`, in stage candidate. Each
    /// artifact is one version per model: registering the same artifact
    /// with the same content returns the stored version, so a retried
    /// register is safe, and with different content it is a conflict.
    pub fn register_version(
        &self,
        actor: Actor<'_>,
        model: &str,
        new: NewVersion,
    ) -> Result<Version> {
        check_new_version(&new)?;
        self.write(|tx| {
            if load_model(tx, actor.namespace, model)?.is_none() {
                return Err(RegistryError::NotFound(format!(
                    "model `{model}` does not exist"
                )));
            }
            let existing = tx
                .query_row(
                    &format!("{VERSION_COLUMNS} WHERE namespace = ?1 AND model = ?2 AND artifact_digest = ?3"),
                    params![actor.namespace, model, new.artifact_digest],
                    version_row,
                )
                .optional()?
                .transpose()?;
            if let Some(stored) = existing {
                return if same_content(&stored, &new) {
                    Ok(stored)
                } else {
                    Err(RegistryError::Conflict(format!(
                        "artifact {} is already version {} of `{model}` with other metadata",
                        new.artifact_digest, stored.version
                    )))
                };
            }
            let next: u64 = tx.query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM versions WHERE namespace = ?1 AND model = ?2",
                params![actor.namespace, model],
                |row| row.get(0),
            )?;
            let version = Version {
                model: model.into(),
                version: next,
                artifact_digest: new.artifact_digest,
                framework: new.framework,
                format: new.format,
                signature: new.signature,
                metrics: new.metrics,
                tags: new.tags,
                stage: Stage::Candidate,
                created_at_ms: actor.now_ms,
                created_by: actor.subject.into(),
            };
            tx.execute(
                "INSERT INTO versions (namespace, model, version, artifact_digest, framework,
                     format, signature, metrics, tags, stage, created_at_ms, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    actor.namespace,
                    version.model,
                    version.version,
                    version.artifact_digest,
                    version.framework,
                    version.format,
                    json(&version.signature)?,
                    json(&version.metrics)?,
                    json(&version.tags)?,
                    version.stage.as_str(),
                    version.created_at_ms,
                    version.created_by,
                ],
            )?;
            audit(
                tx,
                actor,
                "version.registered",
                model,
                version.version,
                None::<&Version>,
                Some(&version),
            )?;
            Ok(version)
        })
    }

    pub fn get_version(&self, namespace: &str, model: &str, version: u64) -> Result<Version> {
        check_number("version", version)?;
        self.read(|conn| load_version(conn, namespace, model, version))
    }

    /// Versions of `model` in version order, after `after`.
    pub fn list_versions(
        &self,
        namespace: &str,
        model: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Version>> {
        check_number("after", after)?;
        self.read(|conn| {
            if load_model(conn, namespace, model)?.is_none() {
                return Err(RegistryError::NotFound(format!(
                    "model `{model}` does not exist"
                )));
            }
            let mut statement = conn.prepare(&format!(
                "{VERSION_COLUMNS} WHERE namespace = ?1 AND model = ?2 AND version > ?3
                 ORDER BY version LIMIT ?4"
            ))?;
            statement
                .query_map(params![namespace, model, after, page(limit)], version_row)?
                .map(|row| row?)
                .collect()
        })
    }

    /// Move a version to `stage`. With `expected`, the change applies only
    /// if the version is still in that stage, so two callers cannot both
    /// act on what they read. Moving to the current stage changes nothing
    /// and records nothing.
    pub fn set_stage(
        &self,
        actor: Actor<'_>,
        model: &str,
        version: u64,
        stage: Stage,
        expected: Option<Stage>,
    ) -> Result<Version> {
        check_number("version", version)?;
        self.write(|tx| {
            let before = load_version(tx, actor.namespace, model, version)?;
            if let Some(expected) = expected.filter(|expected| *expected != before.stage) {
                return Err(RegistryError::Refused(format!(
                    "version {version} of `{model}` is {}, not {}",
                    before.stage.as_str(),
                    expected.as_str()
                )));
            }
            if before.stage == stage {
                return Ok(before);
            }
            before.stage.check_transition(stage)?;
            tx.execute(
                "UPDATE versions SET stage = ?4 WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                params![actor.namespace, model, version, stage.as_str()],
            )?;
            let after = Version {
                stage,
                ..before.clone()
            };
            audit(
                tx,
                actor,
                "version.stage_changed",
                model,
                version,
                Some(&before),
                Some(&after),
            )?;
            Ok(after)
        })
    }

    /// Audit events in order, after `after_seq`, for one model when `model`
    /// is set.
    pub fn list_audit_events(
        &self,
        namespace: &str,
        model: Option<&str>,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<AuditEvent>> {
        check_number("after_seq", after_seq)?;
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT seq, at_ms, actor, action, model, version, before, after FROM audit_events
                 WHERE namespace = ?1 AND (?2 IS NULL OR model = ?2) AND seq > ?3
                 ORDER BY seq LIMIT ?4",
            )?;
            let events = statement
                .query_map(params![namespace, model, after_seq, page(limit)], |row| {
                    Ok(AuditEvent {
                        seq: row.get(0)?,
                        at_ms: row.get(1)?,
                        actor: row.get(2)?,
                        action: row.get(3)?,
                        model: row.get(4)?,
                        version: row.get(5)?,
                        before: row.get(6)?,
                        after: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(events)
        })
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: usize = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > MIGRATIONS.len() {
        return Err(RegistryError::Storage(format!(
            "the registry database has schema {current}, newer than this binary's {}",
            MIGRATIONS.len()
        )));
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(current) {
        tx.execute_batch(migration)?;
        tx.pragma_update(None, "user_version", index + 1)?;
    }
    tx.commit()?;
    Ok(())
}

fn page(limit: u32) -> u32 {
    match limit {
        0 => DEFAULT_PAGE,
        limit => limit.min(MAX_PAGE),
    }
}

fn json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| RegistryError::Storage(error.to_string()))
}

fn parse<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| RegistryError::Storage(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn audit(
    tx: &rusqlite::Transaction<'_>,
    actor: Actor<'_>,
    action: &str,
    model: &str,
    version: u64,
    before: Option<&impl Serialize>,
    after: Option<&impl Serialize>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO audit_events (namespace, at_ms, actor, action, model, version, before, after)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            actor.namespace,
            actor.now_ms,
            actor.subject,
            action,
            model,
            version,
            before.map(json).transpose()?,
            after.map(json).transpose()?,
        ],
    )?;
    Ok(())
}

fn model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Model> {
    Ok(Model {
        name: row.get(0)?,
        description: row.get(1)?,
        created_at_ms: row.get(2)?,
        created_by: row.get(3)?,
    })
}

fn load_model(conn: &Connection, namespace: &str, name: &str) -> Result<Option<Model>> {
    Ok(conn
        .query_row(
            "SELECT name, description, created_at_ms, created_by FROM models
             WHERE namespace = ?1 AND name = ?2",
            params![namespace, name],
            model_row,
        )
        .optional()?)
}

const VERSION_COLUMNS: &str =
    "SELECT model, version, artifact_digest, framework, format, signature,
    metrics, tags, stage, created_at_ms, created_by FROM versions";

/// A version row. The outer result is the SQLite read; the inner one parses
/// the stored JSON and stage.
fn version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Version>> {
    let signature: String = row.get(5)?;
    let metrics: String = row.get(6)?;
    let tags: String = row.get(7)?;
    let stage: String = row.get(8)?;
    let (model, version, artifact_digest, framework, format, created_at_ms, created_by) = (
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(9)?,
        row.get(10)?,
    );
    Ok((|| {
        Ok(Version {
            model,
            version,
            artifact_digest,
            framework,
            format,
            signature: parse(&signature)?,
            metrics: parse(&metrics)?,
            tags: parse(&tags)?,
            stage: Stage::parse(&stage)?,
            created_at_ms,
            created_by,
        })
    })())
}

fn load_version(conn: &Connection, namespace: &str, model: &str, version: u64) -> Result<Version> {
    conn.query_row(
        &format!("{VERSION_COLUMNS} WHERE namespace = ?1 AND model = ?2 AND version = ?3"),
        params![namespace, model, version],
        version_row,
    )
    .optional()?
    .unwrap_or_else(|| {
        Err(RegistryError::NotFound(format!(
            "version {version} of `{model}` does not exist"
        )))
    })
}

fn same_content(stored: &Version, new: &NewVersion) -> bool {
    stored.artifact_digest == new.artifact_digest
        && stored.framework == new.framework
        && stored.format == new.format
        && stored.signature == new.signature
        && stored.metrics == new.metrics
        && stored.tags == new.tags
}

/// Names: 1 to 128 of `A-Z a-z 0-9 . _ -`, starting with a letter or digit.
fn check_name(what: &str, name: &str) -> Result<()> {
    let valid = name.len() <= 128
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(RegistryError::Invalid(format!(
            "{what} `{name}` must be 1 to 128 letters, digits, `.`, `_` or `-`, starting with a letter or digit"
        )))
    }
}

/// SQLite integers are signed; larger numbers are refused as input.
fn check_number(what: &str, value: u64) -> Result<()> {
    if i64::try_from(value).is_err() {
        return Err(RegistryError::Invalid(format!(
            "{what} must be at most {}",
            i64::MAX
        )));
    }
    Ok(())
}

fn check_text(what: &str, value: &str, max: usize) -> Result<()> {
    if value.len() > max || value.chars().any(char::is_control) {
        return Err(RegistryError::Invalid(format!(
            "{what} must be at most {max} bytes without control characters"
        )));
    }
    Ok(())
}

fn check_new_version(new: &NewVersion) -> Result<()> {
    let hex = new
        .artifact_digest
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    if hex.is_none() {
        return Err(RegistryError::Invalid(format!(
            "artifact_digest `{}` must be sha256:<64 lowercase hex>",
            new.artifact_digest
        )));
    }
    if new.framework.is_empty() {
        return Err(RegistryError::Invalid("framework is required".into()));
    }
    check_text("framework", &new.framework, 64)?;
    if !FORMATS.contains(&new.format.as_str()) {
        return Err(RegistryError::Invalid(format!(
            "format `{}` is not supported: expected one of {}",
            new.format,
            FORMATS.join(", ")
        )));
    }
    let specs = new.signature.inputs.iter().chain(&new.signature.outputs);
    for spec in specs {
        check_name("signature field", &spec.name)?;
        check_text("dtype", &spec.dtype, 64)?;
        if spec.dtype.is_empty() || spec.shape.iter().any(|dim| *dim < -1) {
            return Err(RegistryError::Invalid(format!(
                "signature field `{}` needs a dtype and dimensions of -1 or more",
                spec.name
            )));
        }
    }
    if new.signature.inputs.len() + new.signature.outputs.len() > 1024 {
        return Err(RegistryError::Invalid(
            "a signature has at most 1024 fields".into(),
        ));
    }
    if new.metrics.len() > 256 || new.tags.len() > 256 {
        return Err(RegistryError::Invalid(
            "a version has at most 256 metrics and 256 tags".into(),
        ));
    }
    for (name, value) in &new.metrics {
        check_name("metric", name)?;
        if !value.is_finite() {
            return Err(RegistryError::Invalid(format!(
                "metric `{name}` must be a finite number"
            )));
        }
    }
    for (key, value) in &new.tags {
        check_name("tag", key)?;
        check_text("tag value", value, 1024)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Another valid artifact digest, all `hex`.
    fn digest(hex: char) -> String {
        format!("sha256:{}", hex.to_string().repeat(64))
    }

    type Change = fn(&mut NewVersion);

    fn actor(namespace: &str) -> Actor<'_> {
        Actor {
            namespace,
            subject: "svc-test",
            now_ms: 1_700_000_000_000,
        }
    }

    fn new_version(digest: &str) -> NewVersion {
        NewVersion {
            artifact_digest: digest.into(),
            framework: "xgboost".into(),
            format: "onnx".into(),
            signature: Signature {
                inputs: vec![TensorSpec {
                    name: "features".into(),
                    dtype: "float32".into(),
                    shape: vec![-1, 8],
                    nullable: false,
                }],
                outputs: vec![TensorSpec {
                    name: "score".into(),
                    dtype: "float32".into(),
                    shape: vec![-1],
                    nullable: false,
                }],
            },
            metrics: BTreeMap::from([("auc".into(), 0.91)]),
            tags: BTreeMap::from([("team".into(), "risk".into())]),
        }
    }

    fn registry_with_version() -> Registry {
        let registry = Registry::open_in_memory().unwrap();
        registry.create_model(actor("a"), "churn", "").unwrap();
        registry
            .register_version(actor("a"), "churn", new_version(DIGEST))
            .unwrap();
        registry
    }

    #[test]
    fn a_registered_version_cannot_be_changed_or_removed() {
        let registry = registry_with_version();
        let mut changed = new_version(DIGEST);
        changed.metrics.insert("auc".into(), 0.99);
        assert!(matches!(
            registry.register_version(actor("a"), "churn", changed),
            Err(RegistryError::Conflict(_))
        ));
        let conn = registry.conn.lock().unwrap();
        for statement in [
            "UPDATE versions SET metrics = '{}'",
            "UPDATE versions SET artifact_digest = 'sha256:00'",
            "DELETE FROM versions",
            "DELETE FROM models",
            "UPDATE audit_events SET actor = 'someone else'",
            "DELETE FROM audit_events",
        ] {
            let error = conn.execute(statement, []).unwrap_err().to_string();
            assert!(
                error.contains("immutable") || error.contains("never") || error.contains("append"),
                "{statement}: {error}"
            );
        }
        drop(conn);
        let stored = registry.get_version("a", "churn", 1).unwrap();
        assert_eq!(stored.metrics["auc"], 0.91);
    }

    #[test]
    fn retried_writes_return_what_is_stored() {
        let registry = registry_with_version();
        let again = registry
            .register_version(actor("a"), "churn", new_version(DIGEST))
            .unwrap();
        assert_eq!(again.version, 1);
        assert_eq!(
            registry.create_model(actor("a"), "churn", "").unwrap().name,
            "churn"
        );
        assert!(matches!(
            registry.create_model(actor("a"), "churn", "other"),
            Err(RegistryError::Conflict(_))
        ));
        let second = registry
            .register_version(actor("a"), "churn", new_version(&digest('b')))
            .unwrap();
        assert_eq!(second.version, 2);
        // Two creations, two registrations: retries record nothing.
        assert_eq!(
            registry.list_audit_events("a", None, 0, 0).unwrap().len(),
            3
        );
    }

    #[test]
    fn metrics_read_back_exactly() {
        let registry = registry_with_version();
        let mut new = new_version(&digest('d'));
        // A value that a lossy float parser reads back differently.
        new.metrics.insert("rmse".into(), 51.248178375505404);
        registry
            .register_version(actor("a"), "churn", new.clone())
            .unwrap();
        let stored = registry.get_version("a", "churn", 2).unwrap();
        assert_eq!(
            stored.metrics["rmse"].to_bits(),
            51.248178375505404_f64.to_bits()
        );
        assert_eq!(
            registry
                .register_version(actor("a"), "churn", new)
                .unwrap()
                .version,
            2
        );
    }

    #[test]
    fn stage_changes_follow_the_rules_and_are_recorded() {
        let registry = registry_with_version();
        let staged = registry
            .set_stage(
                actor("a"),
                "churn",
                1,
                Stage::Staging,
                Some(Stage::Candidate),
            )
            .unwrap();
        assert_eq!(staged.stage, Stage::Staging);
        assert!(matches!(
            registry.set_stage(
                actor("a"),
                "churn",
                1,
                Stage::Archived,
                Some(Stage::Candidate)
            ),
            Err(RegistryError::Refused(_))
        ));
        assert!(matches!(
            registry.set_stage(actor("a"), "churn", 1, Stage::Production, None),
            Err(RegistryError::Refused(message)) if message.contains("approval hook")
        ));
        registry
            .set_stage(actor("a"), "churn", 1, Stage::Archived, None)
            .unwrap();
        assert!(matches!(
            registry.set_stage(actor("a"), "churn", 1, Stage::Candidate, None),
            Err(RegistryError::Refused(_))
        ));
        // Repeating the current stage is a no-op.
        registry
            .set_stage(actor("a"), "churn", 1, Stage::Archived, None)
            .unwrap();
        let changes: Vec<_> = registry
            .list_audit_events("a", Some("churn"), 0, 0)
            .unwrap()
            .into_iter()
            .filter(|event| event.action == "version.stage_changed")
            .collect();
        assert_eq!(changes.len(), 2);
        let before: serde_json::Value =
            serde_json::from_str(changes[0].before.as_deref().unwrap()).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(changes[0].after.as_deref().unwrap()).unwrap();
        assert_eq!(
            (before["stage"].as_str(), after["stage"].as_str()),
            (Some("candidate"), Some("staging"))
        );
        assert_eq!(changes[0].actor, "svc-test");
        assert_eq!(changes[1].version, 1);
    }

    #[test]
    fn namespaces_never_see_each_other() {
        let registry = registry_with_version();
        assert!(matches!(
            registry.get_model("b", "churn"),
            Err(RegistryError::NotFound(_))
        ));
        assert!(matches!(
            registry.get_version("b", "churn", 1),
            Err(RegistryError::NotFound(_))
        ));
        assert!(matches!(
            registry.set_stage(actor("b"), "churn", 1, Stage::Staging, None),
            Err(RegistryError::NotFound(_))
        ));
        assert!(registry.list_models("b", "", 0).unwrap().is_empty());
        assert!(
            registry
                .list_audit_events("b", None, 0, 0)
                .unwrap()
                .is_empty()
        );
        // The same name in another namespace is a different model.
        registry
            .create_model(actor("b"), "churn", "theirs")
            .unwrap();
        registry
            .register_version(actor("b"), "churn", new_version(DIGEST))
            .unwrap();
        assert_eq!(registry.get_model("a", "churn").unwrap().description, "");
        assert_eq!(registry.list_versions("a", "churn", 0, 0).unwrap().len(), 1);
    }

    #[test]
    fn registrations_are_validated() {
        let registry = registry_with_version();
        let cases: [(&str, Change); 5] = [
            ("digest", |v| v.artifact_digest = "sha256:XYZ".into()),
            ("format", |v| v.format = "pickle".into()),
            ("framework", |v| v.framework.clear()),
            ("metric", |v| {
                v.metrics.insert("loss".into(), f64::NAN);
            }),
            ("shape", |v| v.signature.inputs[0].shape = vec![-2]),
        ];
        for (what, change) in cases {
            let mut new = new_version(&digest('c'));
            change(&mut new);
            assert!(
                matches!(
                    registry.register_version(actor("a"), "churn", new),
                    Err(RegistryError::Invalid(_))
                ),
                "{what}"
            );
        }
        // Unchanged, the same registration is valid.
        registry
            .register_version(actor("a"), "churn", new_version(&digest('c')))
            .unwrap();
        assert!(matches!(
            registry.register_version(actor("a"), "missing", new_version(DIGEST)),
            Err(RegistryError::NotFound(_))
        ));
        assert!(matches!(
            registry.create_model(actor("a"), "../x", ""),
            Err(RegistryError::Invalid(_))
        ));
        assert!(matches!(
            registry.list_versions("a", "churn", u64::MAX, 0),
            Err(RegistryError::Invalid(_))
        ));
        assert!(matches!(
            registry.get_version("a", "churn", u64::MAX),
            Err(RegistryError::Invalid(_))
        ));
    }

    #[test]
    fn a_database_from_an_older_schema_opens_and_a_newer_one_is_refused() {
        let path = std::env::temp_dir().join(format!(
            "hinagata-registry-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        // An empty file is schema 0, the oldest there is.
        Connection::open(&path).unwrap();
        let registry = Registry::open(&path).unwrap();
        registry.create_model(actor("a"), "churn", "").unwrap();
        drop(registry);
        // Reopening keeps the data and the schema.
        let registry = Registry::open(&path).unwrap();
        assert_eq!(registry.get_model("a", "churn").unwrap().name, "churn");
        drop(registry);
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() + 1)
            .unwrap();
        drop(conn);
        let Err(error) = Registry::open(&path) else {
            panic!("a newer schema opened");
        };
        assert!(error.to_string().contains("newer"), "{error}");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
