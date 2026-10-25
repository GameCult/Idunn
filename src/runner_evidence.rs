//! The output tail of a failed runner step.
//!
//! One file, `runner-evidence.cc`, beside `control.cc`, holds one type: the
//! last bytes of a failed runner container's stdout and stderr, one record per
//! transaction. The engine's pre-fence failure path is its only writer, and
//! `idunn status --command` its only reader. It is never `control.cc`, which
//! refuses any type it does not know, and never `incidents.cc`, which is
//! published and carries no free text.
//!
//! The tail is raw runner output. It is kept unredacted and readable by root
//! alone (mode 0600): root can already read every secret the runner was given.
//! It never enters an error, a transaction record or the journal; the render
//! below escapes control characters because the status reader is a terminal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use cultcache_rs::{
    CultCacheEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
    TryCompareExchangeSnapshotOutcome,
};
use serde::{Deserialize, Serialize};

use crate::control_plane::{decode_record, require_id, typed_envelope};
use crate::drivers::{
    LockContended, RUNNER_OUTPUT_TAIL_BYTES, RunnerStepFailure, RunnerSubject,
    exchange_behind_private_lock, runner_status,
};

const RUNNER_FAILURE_SCHEMA: &str = "idunn.runner_failure.v1";

/// Records kept; the writer drops the oldest past this, so the store needs no
/// other owner.
pub(crate) const RUNNER_EVIDENCE_RECORDS: usize = 32;

/// `runner-evidence.cc`, beside the control store.
pub(crate) fn runner_evidence_store_path(state_store: &Path) -> PathBuf {
    state_store.with_file_name("runner-evidence.cc")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SubjectKind {
    Step,
    ExternalInput,
}

#[derive(Clone, PartialEq, Eq, DatabaseEntry)]
#[cultcache(type = "idunn.runner_failure", schema = "idunn.runner_failure.v1")]
pub(crate) struct RunnerFailureRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    #[cultcache(key = 1)]
    pub(crate) transaction_id: String,
    #[cultcache(key = 2)]
    pub(crate) command_id: String,
    #[cultcache(key = 3)]
    pub(crate) target: String,
    #[cultcache(key = 4)]
    pub(crate) subject_kind: SubjectKind,
    #[cultcache(key = 5)]
    pub(crate) subject_id: String,
    #[cultcache(key = 6)]
    pub(crate) runner: String,
    #[cultcache(key = 7)]
    pub(crate) exit_code: Option<i32>,
    #[cultcache(key = 8)]
    pub(crate) signal: Option<i32>,
    #[cultcache(key = 9)]
    pub(crate) stdout_tail: Vec<u8>,
    #[cultcache(key = 10)]
    pub(crate) stdout_total_bytes: u64,
    #[cultcache(key = 11)]
    pub(crate) stderr_tail: Vec<u8>,
    #[cultcache(key = 12)]
    pub(crate) stderr_total_bytes: u64,
    #[cultcache(key = 13)]
    pub(crate) recorded_at_unix_millis: u64,
}

/// Tails stay out of `Debug`, so a failed assertion or a logged record never
/// prints runner output.
impl std::fmt::Debug for RunnerFailureRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerFailureRecord")
            .field("transaction_id", &self.transaction_id)
            .field("subject_id", &self.subject_id)
            .finish_non_exhaustive()
    }
}

impl RunnerFailureRecord {
    pub(crate) fn from_failure(
        transaction_id: &str,
        command_id: &str,
        target: &str,
        failure: &RunnerStepFailure,
        now: u64,
    ) -> Self {
        let (subject_kind, subject_id) = match &failure.subject {
            RunnerSubject::Step(id) => (SubjectKind::Step, id),
            RunnerSubject::ExternalInput(id) => (SubjectKind::ExternalInput, id),
        };
        Self {
            schema_version: RUNNER_FAILURE_SCHEMA.to_owned(),
            transaction_id: transaction_id.to_owned(),
            command_id: command_id.to_owned(),
            target: target.to_owned(),
            subject_kind,
            subject_id: subject_id.clone(),
            runner: failure.runner.clone(),
            exit_code: failure.exit_code,
            signal: failure.signal,
            stdout_tail: failure.stdout_tail.clone(),
            stdout_total_bytes: failure.stdout_total_bytes,
            stderr_tail: failure.stderr_tail.clone(),
            stderr_total_bytes: failure.stderr_total_bytes,
            recorded_at_unix_millis: now,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == RUNNER_FAILURE_SCHEMA,
            "runner failure schema is unsupported"
        );
        require_id(&self.transaction_id, "runner failure transaction")?;
        require_id(&self.command_id, "runner failure command")?;
        require_id(&self.target, "runner failure target")?;
        require_id(&self.subject_id, "runner failure subject")?;
        require_id(&self.runner, "runner failure runner")?;
        for (tail, total, stream) in [
            (&self.stdout_tail, self.stdout_total_bytes, "stdout"),
            (&self.stderr_tail, self.stderr_total_bytes, "stderr"),
        ] {
            ensure!(
                tail.len() <= RUNNER_OUTPUT_TAIL_BYTES,
                "runner failure {stream} tail exceeds its bound"
            );
            ensure!(
                tail.len() as u64 <= total,
                "runner failure {stream} tail exceeds its stream"
            );
        }
        Ok(())
    }

    fn envelope(&self) -> Result<CultCacheEnvelope> {
        self.validate()?;
        typed_envelope(
            &self.transaction_id,
            Self::TYPE,
            RUNNER_FAILURE_SCHEMA,
            self,
            self.recorded_at_unix_millis,
        )
    }
}

/// One read of the store: the envelopes exactly as stored, and the records
/// decoded from them in the same order.
fn snapshot(path: &Path) -> Result<(Vec<CultCacheEnvelope>, Vec<RunnerFailureRecord>)> {
    let envelopes = SingleFileMessagePackBackingStore::new(path)
        .pull_all_read_only_snapshot()
        .context("reading the runner evidence store")?;
    let mut records = Vec::new();
    for envelope in &envelopes {
        ensure!(
            envelope.r#type == RunnerFailureRecord::TYPE
                && envelope.schema_id.as_deref() == Some(RUNNER_FAILURE_SCHEMA),
            "the runner evidence store holds a document that is not a runner failure"
        );
        let record: RunnerFailureRecord = decode_record(envelope)?;
        record.validate()?;
        ensure!(
            envelope.key == record.transaction_id,
            "runner evidence store key differs from the record's transaction"
        );
        records.push(record);
    }
    Ok((envelopes, records))
}

/// The one writer. Upsert by transaction id; past `RUNNER_EVIDENCE_RECORDS` the
/// oldest records go in the same exchange. The file ends at mode 0600.
pub(crate) fn record(path: &Path, record: &RunnerFailureRecord) -> Result<()> {
    let (envelopes, records) = snapshot(path)?;
    let mut kept: Vec<(u64, CultCacheEnvelope)> = envelopes
        .iter()
        .zip(&records)
        .filter(|(_, stored)| stored.transaction_id != record.transaction_id)
        .map(|(envelope, stored)| (stored.recorded_at_unix_millis, envelope.clone()))
        .collect();
    kept.push((record.recorded_at_unix_millis, record.envelope()?));
    // Stable: records of one instant keep their stored order, so the record
    // being written survives a tie.
    kept.sort_by_key(|(recorded_at, _)| *recorded_at);
    let excess = kept.len().saturating_sub(RUNNER_EVIDENCE_RECORDS);
    let replacements: Vec<_> = kept
        .into_iter()
        .skip(excess)
        .map(|(_, envelope)| envelope)
        .collect();
    match exchange_behind_private_lock(path, &envelopes, &replacements)? {
        TryCompareExchangeSnapshotOutcome::Exchanged => {}
        TryCompareExchangeSnapshotOutcome::Mismatch => {
            bail!("the runner evidence store changed while it was being written")
        }
        TryCompareExchangeSnapshotOutcome::LockContended => {
            return Err(LockContended {
                store: "runner evidence store",
            }
            .into());
        }
    }
    keep_private(path)
}

#[cfg(unix)]
fn keep_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context("making the runner evidence store private")
}

#[cfg(not(unix))]
fn keep_private(_path: &Path) -> Result<()> {
    Ok(())
}

/// The one reader. An absent store holds nothing.
pub(crate) fn for_transaction(
    path: &Path,
    transaction_id: &str,
) -> Result<Option<RunnerFailureRecord>> {
    Ok(snapshot(path)?
        .1
        .into_iter()
        .find(|stored| stored.transaction_id == transaction_id))
}

/// The record as `idunn status --command` prints it. The store keeps raw
/// bytes; every control character except newline and tab is escaped here, so
/// build output cannot drive the operator's terminal.
pub(crate) fn render(record: &RunnerFailureRecord) -> Vec<String> {
    let noun = match record.subject_kind {
        SubjectKind::Step => "step",
        SubjectKind::ExternalInput => "external input",
    };
    let mut lines = vec![
        format!("    failed {noun} {}", escape(&record.subject_id)),
        format!("    runner {}", escape(&record.runner)),
        format!("    status {}", runner_status(record.exit_code, record.signal)),
    ];
    for (stream, tail, total) in [
        ("stdout", &record.stdout_tail, record.stdout_total_bytes),
        ("stderr", &record.stderr_tail, record.stderr_total_bytes),
    ] {
        lines.push(format!(
            "      {stream} (last {} of {total} bytes)",
            tail.len()
        ));
        let text = escape(&String::from_utf8_lossy(tail));
        let text = text.strip_suffix('\n').unwrap_or(&text);
        if !text.is_empty() {
            lines.extend(text.split('\n').map(|line| format!("      | {line}")));
        }
    }
    lines
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\n' && character != '\t' {
            escaped.push_str(&format!("\\u{{{:x}}}", u32::from(character)));
        } else {
            escaped.push(character);
        }
    }
    escaped
}
