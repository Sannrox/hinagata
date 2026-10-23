//! Promotion workflow with a caller-governed approval hook (#5).
//!
//! Hinagata owns no promotion policy. With an approval hook configured,
//! every stage change asks the hook and applies only on an explicit allow
//! that names its approver and decision reference. Without one, changes
//! outside production apply as before and promotion to production is
//! refused.
//!
//! A model can require gates for promotion to production, checked before
//! the hook is asked and again when the change applies: metric minimums,
//! signature compatibility with every version already in production, and
//! a signed manifest (the registry holds a signing key).
//!
//! The change applies only if the version has not changed stage since the
//! hook was asked and the gates still give the same results; otherwise it
//! is refused and the caller asks again. The request id is a digest of
//! everything the hook decides on (actor, version, stages, the version's
//! last stage change, gate results, manifest), so asking again about the
//! same change sends the same id and anything different sends another.

use crate::registry::{Actor, Registry, RegistryError, Stage, Version};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// The request the hook receives.
pub const REQUEST_FORMAT: &str = "hinagata.approval-request/v1";
/// Largest decision body read from the hook.
const MAX_DECISION_BYTES: usize = 64 * 1024;
const MAX_GATED_METRICS: usize = 64;

type Result<T> = std::result::Result<T, RegistryError>;

/// What a model requires before a version is promoted to production.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Gates {
    /// Each metric the version must record, at least at this value.
    pub min_metrics: BTreeMap<String, f64>,
    /// No breaking signature change against any version in production.
    pub require_compatible_signature: bool,
    /// The registry can sign the version's manifest.
    pub require_signed_manifest: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    /// `metric:<name>`, `compatible_signature` or `signed_manifest`.
    pub gate: String,
    pub passed: bool,
    pub detail: String,
}

/// The hook's answer. Anything but an allow with an approver and a
/// reference is a deny.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub allow: bool,
    pub approver: String,
    pub reference: String,
    pub reason: String,
}

/// A stage change ready to ask the hook about.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub version: Version,
    /// The sequence number of the version's last stage change, or 0: the
    /// change applies only while it is unchanged.
    pub stage_token: u64,
    pub gates: Vec<GateResult>,
}

/// What the audit trail records for a decided stage change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recorded<'a> {
    pub request_id: &'a str,
    pub from_stage: &'a str,
    pub to_stage: &'a str,
    pub approver: &'a str,
    pub reference: &'a str,
    pub reason: &'a str,
    pub gates: &'a [GateResult],
}

impl Gates {
    pub fn validate(&self) -> Result<()> {
        if self.min_metrics.len() > MAX_GATED_METRICS {
            return Err(RegistryError::Invalid(format!(
                "at most {MAX_GATED_METRICS} metric gates"
            )));
        }
        for (name, min) in &self.min_metrics {
            if name.is_empty() || name.len() > 128 || !min.is_finite() {
                return Err(RegistryError::Invalid(
                    "a metric gate needs a name of 1 to 128 characters and a finite minimum".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Promotion gates (schema migration 7) on the registry database.
pub(crate) const MIGRATION: &str = r#"
CREATE TABLE promotion_gates (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    gates TEXT NOT NULL,
    PRIMARY KEY (namespace, model),
    FOREIGN KEY (namespace, model) REFERENCES models (namespace, name)
);
"#;

fn storage(error: impl ToString) -> RegistryError {
    RegistryError::Storage(error.to_string())
}

fn load_gates(conn: &rusqlite::Connection, namespace: &str, model: &str) -> Result<Gates> {
    conn.query_row(
        "SELECT gates FROM promotion_gates WHERE namespace = ?1 AND model = ?2",
        params![namespace, model],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|raw| serde_json::from_str(&raw).map_err(storage))
    .transpose()
    .map(Option::unwrap_or_default)
}

/// The gate results for promoting `version` to production.
fn evaluate(
    conn: &rusqlite::Connection,
    namespace: &str,
    version: &Version,
    gates: &Gates,
    signed: bool,
) -> Result<Vec<GateResult>> {
    let mut results = Vec::new();
    for (name, min) in &gates.min_metrics {
        let (passed, detail) = match version.metrics.get(name) {
            Some(value) if value >= min => (true, format!("{value} is at least {min}")),
            Some(value) => (false, format!("{value} is below {min}")),
            None => (false, "the version records no such metric".into()),
        };
        results.push(GateResult {
            gate: format!("metric:{name}"),
            passed,
            detail,
        });
    }
    if gates.require_compatible_signature {
        let mut statement = conn.prepare(
            "SELECT version FROM versions
             WHERE namespace = ?1 AND model = ?2 AND stage = 'production' AND version != ?3
             ORDER BY version",
        )?;
        let live = statement
            .query_map(params![namespace, version.model, version.version], |row| {
                row.get::<_, u64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut breaking = Vec::new();
        for number in &live {
            let current = crate::registry::load_version(conn, namespace, &version.model, *number)?;
            for change in crate::signature::breaking_changes(&current.signature, &version.signature)
            {
                breaking.push(format!("against version {number}: {change}"));
            }
        }
        results.push(GateResult {
            gate: "compatible_signature".into(),
            passed: breaking.is_empty(),
            detail: if live.is_empty() {
                "no version is in production".into()
            } else if breaking.is_empty() {
                "compatible with every version in production".into()
            } else {
                breaking.join("; ")
            },
        });
    }
    if gates.require_signed_manifest {
        results.push(GateResult {
            gate: "signed_manifest".into(),
            passed: signed,
            detail: if signed {
                "the manifest is signed".into()
            } else {
                "this registry has no signing key".into()
            },
        });
    }
    Ok(results)
}

/// The sequence number of the version's last stage change, or 0.
fn stage_token(conn: &rusqlite::Connection, namespace: &str, version: &Version) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM audit_events
         WHERE namespace = ?1 AND model = ?2 AND version = ?3 AND action = 'version.stage_changed'",
        params![namespace, version.model, version.version],
        |row| row.get(0),
    )?)
}

/// The request id for asking `subject` about moving `pending` to `to`
/// with `manifest`: a digest of every input to the decision.
pub fn request_id(
    namespace: &str,
    subject: &str,
    pending: &Pending,
    to: Stage,
    manifest: &serde_json::Value,
) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for part in [
        namespace,
        subject,
        &pending.version.model,
        &pending.version.version.to_string(),
        pending.version.stage.as_str(),
        to.as_str(),
        &pending.stage_token.to_string(),
        &serde_json::to_string(&pending.gates).unwrap_or_default(),
        &manifest.to_string(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

impl Registry {
    /// Set the gates a model requires for promotion to production, audited
    /// as `model.promotion_gates_set`. Setting the same gates is a no-op.
    pub fn put_promotion_gates(
        &self,
        actor: Actor<'_>,
        model: &str,
        gates: &Gates,
    ) -> Result<Gates> {
        gates.validate()?;
        let body = serde_json::to_string(gates).map_err(storage)?;
        self.write(|tx| {
            crate::registry::load_model(tx, actor.namespace, model)?.ok_or_else(|| {
                RegistryError::NotFound(format!("model `{model}` does not exist"))
            })?;
            let before = load_gates(tx, actor.namespace, model)?;
            if before == *gates {
                return Ok(before);
            }
            tx.execute(
                "INSERT INTO promotion_gates (namespace, model, gates) VALUES (?1, ?2, ?3)
                 ON CONFLICT (namespace, model) DO UPDATE SET gates = ?3",
                params![actor.namespace, model, body],
            )?;
            crate::registry::audit(
                tx,
                actor,
                "model.promotion_gates_set",
                model,
                0,
                Some(&before),
                Some(gates),
            )?;
            Ok(gates.clone())
        })
    }

    pub fn promotion_gates(&self, namespace: &str, model: &str) -> Result<Gates> {
        self.read(|conn| {
            crate::registry::load_model(conn, namespace, model)?.ok_or_else(|| {
                RegistryError::NotFound(format!("model `{model}` does not exist"))
            })?;
            load_gates(conn, namespace, model)
        })
    }

    /// Check a stage change before asking the hook: the version exists, is
    /// in `expected` when set, and may move to `to`; for production, the
    /// model's gates. A failed gate is refused here, so the hook is only
    /// asked about changes that can apply.
    pub fn prepare_stage_change(
        &self,
        namespace: &str,
        model: &str,
        version: u64,
        to: Stage,
        expected: Option<Stage>,
        signed: bool,
    ) -> Result<Pending> {
        crate::registry::check_number("version", version)?;
        self.read(|conn| {
            let current = crate::registry::load_version(conn, namespace, model, version)?;
            check_expected(&current, expected)?;
            if current.stage != to {
                current.stage.check_transition(to, true)?;
            }
            let gates = if to == Stage::Production && current.stage != to {
                let gates = evaluate(
                    conn,
                    namespace,
                    &current,
                    &load_gates(conn, namespace, model)?,
                    signed,
                )?;
                refuse_failed(&gates)?;
                gates
            } else {
                Vec::new()
            };
            Ok(Pending {
                stage_token: stage_token(conn, namespace, &current)?,
                version: current,
                gates,
            })
        })
    }

    /// Apply a stage change the hook allowed under `request_id`. It applies
    /// only if the version has not changed stage since `pending` and the
    /// gates give the same results; the decision is audited as
    /// `version.stage_approved` with the stage change.
    pub fn apply_approved_stage(
        &self,
        actor: Actor<'_>,
        pending: &Pending,
        request_id: &str,
        to: Stage,
        decision: &Decision,
        signed: bool,
    ) -> Result<Version> {
        if !decision.allow || decision.approver.is_empty() || decision.reference.is_empty() {
            return Err(RegistryError::Refused(
                "an approval needs an allow with an approver and a reference".into(),
            ));
        }
        let (model, number) = (pending.version.model.as_str(), pending.version.version);
        self.write(|tx| {
            let before = crate::registry::load_version(tx, actor.namespace, model, number)?;
            if before.stage != pending.version.stage
                || stage_token(tx, actor.namespace, &before)? != pending.stage_token
            {
                return Err(RegistryError::Refused(format!(
                    "version {number} of `{model}` changed while the approval was pending; ask again"
                )));
            }
            if before.stage == to {
                return Ok(before);
            }
            before.stage.check_transition(to, true)?;
            if to == Stage::Production {
                let gates = evaluate(tx, actor.namespace, &before, &load_gates(tx, actor.namespace, model)?, signed)?;
                if gates != pending.gates {
                    return Err(RegistryError::Refused(
                        "the promotion gates changed while the approval was pending; ask again".into(),
                    ));
                }
            }
            tx.execute(
                "UPDATE versions SET stage = ?4 WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                params![actor.namespace, model, number, to.as_str()],
            )?;
            let after = Version {
                stage: to,
                ..before.clone()
            };
            crate::registry::audit(
                tx,
                actor,
                "version.stage_changed",
                model,
                number,
                Some(&before),
                Some(&after),
            )?;
            crate::registry::audit(
                tx,
                actor,
                "version.stage_approved",
                model,
                number,
                None::<&Recorded<'_>>,
                Some(&recorded(pending, request_id, to, decision)),
            )?;
            Ok(after)
        })
    }

    /// Record a denied stage change, audited as `version.stage_denied`.
    pub fn record_stage_denial(
        &self,
        actor: Actor<'_>,
        pending: &Pending,
        request_id: &str,
        to: Stage,
        decision: &Decision,
    ) -> Result<()> {
        self.write(|tx| {
            crate::registry::audit(
                tx,
                actor,
                "version.stage_denied",
                &pending.version.model,
                pending.version.version,
                None::<&Recorded<'_>>,
                Some(&recorded(pending, request_id, to, decision)),
            )
        })
    }
}

fn recorded<'a>(
    pending: &'a Pending,
    request_id: &'a str,
    to: Stage,
    decision: &'a Decision,
) -> Recorded<'a> {
    Recorded {
        request_id,
        from_stage: pending.version.stage.as_str(),
        to_stage: to.as_str(),
        approver: &decision.approver,
        reference: &decision.reference,
        reason: &decision.reason,
        gates: &pending.gates,
    }
}

fn check_expected(current: &Version, expected: Option<Stage>) -> Result<()> {
    match expected.filter(|expected| *expected != current.stage) {
        Some(expected) => Err(RegistryError::Refused(format!(
            "version {} of `{}` is {}, not {}",
            current.version,
            current.model,
            current.stage.as_str(),
            expected.as_str()
        ))),
        None => Ok(()),
    }
}

fn refuse_failed(gates: &[GateResult]) -> Result<()> {
    let failed: Vec<_> = gates
        .iter()
        .filter(|gate| !gate.passed)
        .map(|gate| format!("{}: {}", gate.gate, gate.detail))
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(RegistryError::Refused(format!(
            "promotion gates failed: {}",
            failed.join("; ")
        )))
    }
}

/// The approval hook: an HTTPS endpoint that receives an
/// [`REQUEST_FORMAT`] request and answers with a decision. It fails closed:
/// a timeout, an error status, a redirect, an oversized or malformed body,
/// or an allow without an approver and a reference is a deny.
pub struct ApprovalHook {
    url: reqwest::Url,
    token: Option<String>,
    client: reqwest::Client,
}

impl ApprovalHook {
    /// `url` must be HTTPS, or HTTP to a loopback address. `token` is sent
    /// as a bearer credential so the hook can tell the registry's calls.
    pub fn new(
        url: &str,
        token: Option<String>,
        timeout: Duration,
    ) -> std::result::Result<Self, String> {
        let url =
            reqwest::Url::parse(url).map_err(|error| format!("approval hook URL: {error}"))?;
        let loopback = match url.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err("the approval hook must be HTTPS, or HTTP to a loopback address".into());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            url,
            token: token.map(|token| token.trim().to_string()),
            client,
        })
    }

    /// Ask the hook about `request`.
    pub async fn decide(&self, request: &serde_json::Value) -> Decision {
        match self.ask(request).await {
            Ok(decision) => decision,
            Err(reason) => Decision {
                allow: false,
                approver: String::new(),
                reference: String::new(),
                reason: format!("the approval hook did not decide: {reason}"),
            },
        }
    }

    async fn ask(&self, request: &serde_json::Value) -> std::result::Result<Decision, String> {
        #[derive(Deserialize)]
        struct Answer {
            decision: String,
            #[serde(default)]
            reason: String,
            #[serde(default)]
            approver: String,
            #[serde(default)]
            reference: String,
        }
        let mut call = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(request).map_err(|error| error.to_string())?);
        if let Some(token) = &self.token {
            call = call.bearer_auth(token);
        }
        let mut response = call.send().await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if body.len() + chunk.len() > MAX_DECISION_BYTES {
                return Err("the decision is too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        let answer: Answer =
            serde_json::from_slice(&body).map_err(|error| format!("decision: {error}"))?;
        let allow = match answer.decision.as_str() {
            "allow" => true,
            "deny" => false,
            other => return Err(format!("unknown decision `{other}`")),
        };
        if allow && (answer.approver.is_empty() || answer.reference.is_empty()) {
            return Err("an allow must name its approver and reference".into());
        }
        Ok(Decision {
            allow,
            approver: answer.approver,
            reference: answer.reference,
            reason: answer.reason,
        })
    }
}
