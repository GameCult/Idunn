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
//! It never enters an error, a transaction record or the journal. The status
//! reader is a terminal, so everything it prints from this store goes through
//! one allowlist sink (`emit`): printable ASCII passes, all else is escaped.

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

/// How a write left the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kept {
    Stored,
    /// The store held bytes that do not decode; it was moved to
    /// `unreadable_store_path` and the record opened a fresh one. `replaced`
    /// is the last-written time (unix milliseconds) of an earlier set-aside
    /// store this one displaced, which is gone: one slot, and it says so.
    SetAside { replaced: Option<u64> },
}

/// Where an undecodable store is kept. One slot: the next one replaces it, so
/// the damage a bad store can cost is one extra file.
pub(crate) fn unreadable_store_path(path: &Path) -> PathBuf {
    path.with_file_name("runner-evidence.unreadable.cc")
}

/// Moves the store over the previous set-aside one, root-only, and reports
/// what it displaced. Nothing of the store's content is named: a decode error
/// can quote stored values, and none is carried here.
fn set_aside_unreadable(path: &Path) -> Result<Option<u64>> {
    let aside = unreadable_store_path(path);
    let replaced = std::fs::symlink_metadata(&aside).ok().map(|earlier| {
        earlier
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_millis() as u64)
    });
    std::fs::rename(path, &aside)
        .context("the runner evidence store is undecodable and could not be set aside")?;
    keep_private(&aside)?;
    Ok(replaced)
}

/// How the writer reads the store's bytes; tests inject an I/O error here.
type ReadStore<'a> = &'a dyn Fn(&Path) -> std::io::Result<Vec<u8>>;

fn unreadable_fault(error: &std::io::Error) -> anyhow::Error {
    anyhow::anyhow!("the runner evidence store could not be read: {}", error.kind())
}

/// The one writer. Upsert by transaction id; past `RUNNER_EVIDENCE_RECORDS` the
/// oldest records go in the same exchange. The file ends at mode 0600.
///
/// A store is set aside only when bytes that were read do not decode. An I/O
/// error leaves the store in place, keeps no evidence for that failure, and
/// the error names its kind for the journal.
pub(crate) fn record(path: &Path, record: &RunnerFailureRecord) -> Result<Kept> {
    record_reading(path, record, &|path| std::fs::read(path))
}

fn record_reading(path: &Path, record: &RunnerFailureRecord, read: ReadStore) -> Result<Kept> {
    let first = match read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(unreadable_fault(&error)),
    };
    let (envelopes, records, how) = match snapshot(path) {
        Ok((envelopes, records)) => (envelopes, records, Kept::Stored),
        Err(_) => {
            let Some(first) = first else {
                bail!("the runner evidence store could not be decoded");
            };
            // Undecodable only if the same bytes are still there to read: a
            // second read that fails, or finds other bytes, is not a verdict.
            match read(path) {
                Ok(second) if second == first => {}
                Ok(_) => bail!("the runner evidence store changed while it was being read"),
                Err(error) => return Err(unreadable_fault(&error)),
            }
            let replaced = set_aside_unreadable(path)?;
            (Vec::new(), Vec::new(), Kept::SetAside { replaced })
        }
    };
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
    keep_private(path)?;
    Ok(how)
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

/// The record as `idunn status --command` shows it, as raw lines. They are
/// not text yet: `show_status` is the only caller, and `emit` the only
/// writer.
fn render(record: &RunnerFailureRecord) -> Vec<Vec<u8>> {
    let noun = match record.subject_kind {
        SubjectKind::Step => "step",
        SubjectKind::ExternalInput => "external input",
    };
    let mut lines: Vec<Vec<u8>> = vec![
        format!("    failed {noun} {}", record.subject_id).into_bytes(),
        format!("    runner {}", record.runner).into_bytes(),
        format!("    status {}", runner_status(record.exit_code, record.signal)).into_bytes(),
    ];
    for (stream, tail, total) in [
        ("stdout", &record.stdout_tail, record.stdout_total_bytes),
        ("stderr", &record.stderr_tail, record.stderr_total_bytes),
    ] {
        lines.push(format!("      {stream} (last {} of {total} bytes)", tail.len()).into_bytes());
        let text = tail.strip_suffix(b"\n").unwrap_or(tail);
        if !text.is_empty() {
            lines.extend(text.split(|byte| *byte == b'\n').map(|line| {
                let mut shown = b"      | ".to_vec();
                shown.extend_from_slice(line);
                shown
            }));
        }
    }
    lines
}

/// What `idunn status --command` prints for a transaction's runner evidence:
/// its record, or one line naming why the store is unreadable. Nothing is
/// written when the transaction has no record.
pub(crate) fn show_status(
    out: &mut impl std::io::Write,
    store: &Path,
    transaction_id: &str,
) -> std::io::Result<()> {
    let lines = match for_transaction(store, transaction_id) {
        Ok(Some(record)) => render(&record),
        Ok(None) => return Ok(()),
        Err(error) => {
            vec![format!("    runner evidence unreadable: {}", error.root_cause()).into_bytes()]
        }
    };
    emit(out, &lines)
}

/// The one sink: every byte the status read prints from the evidence store,
/// or from an error about it, is written here and nowhere else. An allowlist:
/// printable ASCII and tab pass; every other code point is a visible
/// `\u{..}` escape and every invalid UTF-8 byte a `\x..` escape. A newline
/// inside a line is escaped too: this function writes the only ones.
fn emit(out: &mut impl std::io::Write, lines: &[Vec<u8>]) -> std::io::Result<()> {
    for line in lines {
        out.write_all(escape(line).as_bytes())?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn escape(bytes: &[u8]) -> String {
    let mut shown = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                ' '..='~' | '\t' => shown.push(character),
                _ => shown.push_str(&format!("\\u{{{:x}}}", u32::from(character))),
            }
        }
        for byte in chunk.invalid() {
            shown.push_str(&format!("\\x{byte:02x}"));
        }
    }
    shown
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn failure(subject: RunnerSubject, stdout: &[u8], stderr: &[u8]) -> RunnerStepFailure {
        RunnerStepFailure {
            subject,
            runner: "rust".into(),
            exit_code: Some(3),
            signal: None,
            stdout_tail: stdout.to_vec(),
            stdout_total_bytes: stdout.len() as u64,
            stderr_tail: stderr.to_vec(),
            stderr_total_bytes: stderr.len() as u64,
        }
    }

    /// What status writes for a record, through the one sink.
    fn shown(record: &RunnerFailureRecord) -> String {
        let mut written = Vec::new();
        emit(&mut written, &render(record)).expect("a Vec takes any write");
        String::from_utf8(written).expect("the sink writes ASCII only")
    }

    fn shown_lines(record: &RunnerFailureRecord) -> Vec<String> {
        shown(record).lines().map(str::to_owned).collect()
    }

    fn record_at(transaction: &str, now: u64) -> RunnerFailureRecord {
        RunnerFailureRecord::from_failure(
            transaction,
            "up-service",
            "service",
            &failure(RunnerSubject::Step("build".into()), b"out\n", b"err\n"),
            now,
        )
    }

    #[test]
    fn the_evidence_store_keeps_the_newest_32() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        // The literal, not the constant: the spec's bound is 32.
        for index in 0..=32 {
            record(&path, &record_at(&format!("tx-{index:02}"), 1000 + index as u64))?;
        }
        let kept: Vec<_> = snapshot(&path)?.1.into_iter().map(|stored| stored.transaction_id).collect();
        assert_eq!(kept.len(), 32);
        assert!(!kept.contains(&"tx-00".to_owned()), "the oldest survived");
        assert!(kept.contains(&"tx-32".to_owned()), "the newest was evicted");
        assert!(kept.contains(&"tx-01".to_owned()), "the second oldest was evicted early");
        // Rewriting a resident transaction replaces it and drops nothing.
        let mut again = record_at("tx-05", 5000);
        again.stdout_tail = b"rewritten\n".to_vec();
        again.stdout_total_bytes = 10;
        record(&path, &again)?;
        let kept = snapshot(&path)?.1;
        assert_eq!(kept.len(), 32);
        assert!(kept.iter().any(|stored| stored.transaction_id == "tx-01"));
        let stored = for_transaction(&path, "tx-05")?.context("rewritten record is gone")?;
        assert_eq!(stored.stdout_tail, b"rewritten\n");
        assert_eq!(for_transaction(&path, "tx-00")?, None);
        Ok(())
    }

    #[test]
    fn a_record_round_trips_byte_for_byte() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        let written = RunnerFailureRecord::from_failure(
            "tx-a",
            "up-service",
            "service",
            &failure(RunnerSubject::ExternalInput("toolchain".into()), &[0, 1, 255, 10], b""),
            77,
        );
        record(&path, &written)?;
        assert_eq!(for_transaction(&path, "tx-a")?, Some(written));
        assert_eq!(for_transaction(&path, "tx-b")?, None);
        Ok(())
    }

    #[test]
    fn the_evidence_store_is_private() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        // Whatever the umask, the mode is the store's own rule, on every write.
        for index in 0..2 {
            record(&path, &record_at(&format!("tx-{index}"), 10 + index))?;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn a_tail_over_its_bound_or_its_stream_is_refused() {
        let mut oversize = record_at("tx-a", 1);
        oversize.stdout_tail = vec![b'x'; RUNNER_OUTPUT_TAIL_BYTES + 1];
        oversize.stdout_total_bytes = u64::MAX;
        assert!(oversize.validate().is_err());
        let mut impossible = record_at("tx-a", 1);
        impossible.stderr_total_bytes = 1;
        assert!(impossible.validate().is_err());
        let mut exact = record_at("tx-a", 1);
        exact.stdout_tail = vec![b'x'; RUNNER_OUTPUT_TAIL_BYTES];
        exact.stdout_total_bytes = RUNNER_OUTPUT_TAIL_BYTES as u64;
        assert!(exact.validate().is_ok());
    }

    #[test]
    fn status_renders_the_tail_with_control_bytes_escaped() {
        let mut stored = record_at("tx-a", 1);
        stored.stdout_tail = b"first\n\x1b[2Jsecond\x00end\tTab\nlast\n".to_vec();
        stored.stdout_total_bytes = 5000;
        stored.stderr_tail = b"bad\r\x07".to_vec();
        stored.stderr_total_bytes = 6;
        let lines = shown_lines(&stored);
        let text = lines.join("\n");
        assert!(!text.contains('\u{1b}') && !text.contains('\0') && !text.contains('\r'));
        assert!(!text.contains('\u{7}'));
        assert_eq!(
            lines,
            vec![
                "    failed step build",
                "    runner rust",
                "    status exit status 3",
                "      stdout (last 30 of 5000 bytes)",
                "      | first",
                "      | \\u{1b}[2Jsecond\\u{0}end\tTab",
                "      | last",
                "      stderr (last 5 of 6 bytes)",
                "      | bad\\u{d}\\u{7}",
            ]
        );
    }

    #[test]
    fn debug_names_the_record_and_prints_no_tail() {
        let mut stored = record_at("tx-a", 1);
        stored.stdout_tail = b"SECRET-OUTPUT".to_vec();
        stored.stdout_total_bytes = 13;
        let debug = format!("{stored:?}");
        assert!(debug.contains("tx-a") && !debug.contains("SECRET"), "{debug}");
    }

    #[test]
    fn an_empty_tail_renders_only_its_header() {
        let mut stored = record_at("tx-a", 1);
        stored.stdout_tail = Vec::new();
        stored.stdout_total_bytes = 0;
        let lines = shown_lines(&stored);
        assert!(lines.contains(&"      stdout (last 0 of 0 bytes)".to_owned()));
        assert!(!lines.iter().any(|line| line == "      | "));
    }

    #[test]
    fn the_reader_names_an_unreadable_store_as_an_error() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        std::fs::write(&path, b"\xc1 not a store")?;
        assert!(for_transaction(&path, "tx-a").is_err());
        Ok(())
    }

    fn files_beside(path: &Path) -> Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(path.parent().context("no parent")?)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        names.retain(|name| name.starts_with("runner-evidence") && !name.contains("lock"));
        names.sort();
        Ok(names)
    }

    /// A store a newer Idunn wrote: well-formed, but a schema this one does not know.
    fn write_future_store(path: &Path) -> Result<()> {
        let mut future = record_at("tx-future", 1);
        future.schema_version = "idunn.runner_failure.v2".into();
        let envelope = typed_envelope(
            &future.transaction_id,
            RunnerFailureRecord::TYPE,
            "idunn.runner_failure.v2",
            &future,
            1,
        )?;
        let outcome = exchange_behind_private_lock(path, &[], &[envelope])?;
        ensure!(
            matches!(outcome, TryCompareExchangeSnapshotOutcome::Exchanged),
            "the future store was not written"
        );
        Ok(())
    }

    #[test]
    fn a_corrupt_store_is_set_aside_and_the_next_failure_keeps_its_evidence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        let damage = b"\xc1 not a store \x00 damage";
        std::fs::write(&path, damage)?;
        let kept = record(&path, &record_at("tx-a", 5))?;
        assert_eq!(kept, Kept::SetAside { replaced: None });
        // Kept, not deleted: the damage is where the journal says, root-only.
        let aside = unreadable_store_path(&path);
        assert_eq!(std::fs::read(&aside)?, damage);
        assert_eq!(std::fs::metadata(&aside)?.permissions().mode() & 0o777, 0o600);
        // The failure's evidence is in a fresh, private store.
        assert_eq!(
            for_transaction(&path, "tx-a")?.map(|stored| stored.transaction_id),
            Some("tx-a".to_owned())
        );
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        // A healthy store is not set aside again.
        assert_eq!(record(&path, &record_at("tx-b", 6))?, Kept::Stored);
        assert_eq!(std::fs::read(&aside)?, damage);
        Ok(())
    }

    #[test]
    fn a_future_version_store_is_set_aside_and_the_next_failure_keeps_its_evidence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        write_future_store(&path)?;
        let before = std::fs::read(&path)?;
        assert!(for_transaction(&path, "tx-future").is_err());
        assert_eq!(
            record(&path, &record_at("tx-a", 5))?,
            Kept::SetAside { replaced: None }
        );
        assert_eq!(std::fs::read(unreadable_store_path(&path))?, before);
        assert!(for_transaction(&path, "tx-a")?.is_some());
        assert_eq!(for_transaction(&path, "tx-future")?, None);
        Ok(())
    }

    #[test]
    fn at_most_one_unreadable_store_is_kept() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        let damages: [&[u8]; 3] = [b"\xc1 first", b"\xc1 second", b"\xc1 third"];
        for (index, damage) in damages.into_iter().enumerate() {
            std::fs::write(&path, damage)?;
            record(&path, &record_at(&format!("tx-{index}"), 5 + index as u64))?;
            assert_eq!(
                std::fs::read(unreadable_store_path(&path))?,
                damage,
                "the newest damage is the one kept"
            );
        }
        assert_eq!(
            files_beside(&path)?,
            vec![
                "runner-evidence.cc".to_owned(),
                "runner-evidence.unreadable.cc".to_owned()
            ]
        );
        Ok(())
    }

    #[test]
    fn a_store_that_cannot_be_read_is_left_in_place_and_the_write_fails() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        // A directory where the store should be: unreadable, not undecodable.
        std::fs::create_dir(&path)?;
        assert!(record(&path, &record_at("tx-a", 1)).is_err());
        assert!(path.is_dir());
        assert!(!unreadable_store_path(&path).exists());
        Ok(())
    }

    #[test]
    fn every_code_point_but_printable_ascii_is_a_visible_escape() {
        // The allowlist, exhaustively: no class list decides anything.
        for code in 0..=0x10ffffu32 {
            let Some(character) = char::from_u32(code) else {
                continue;
            };
            let text = character.to_string();
            let shown = escape(text.as_bytes());
            if matches!(character, ' '..='~' | '\t') {
                assert_eq!(shown, text, "U+{code:04X} should pass");
            } else {
                assert_eq!(shown, format!("\\u{{{code:x}}}"), "U+{code:04X}");
            }
        }
        // Invalid UTF-8 is escaped byte by byte; valid text round it passes.
        assert_eq!(escape(b"a\xc1\xffb\xe2\x82"), "a\\xc1\\xffb\\xe2\\x82");
        assert_eq!(escape(b"ok \xe2\x82\xac"), "ok \\u{20ac}");
    }

    #[test]
    fn a_newline_inside_a_line_cannot_forge_a_line() {
        let mut stored = record_at("tx-a", 1);
        stored.subject_id = "build\n    status exit status 0".into();
        let lines = shown_lines(&stored);
        assert_eq!(lines[0], "    failed step build\\u{a}    status exit status 0");
        assert_eq!(
            lines.iter().filter(|line| line.starts_with("    status ")).count(),
            1
        );
    }

    /// A store whose first record is well-formed except that its subject kind
    /// holds `marker`: the decode error quotes it.
    fn store_with_hostile_subject_kind(path: &Path, marker: &[u8; 4]) -> Result<()> {
        record(path, &record_at("tx-a", 1))?;
        let mut bytes = std::fs::read(path)?;
        let at = bytes
            .windows(4)
            .position(|window| window == b"step")
            .context("no step marker")?;
        bytes[at..at + 4].copy_from_slice(marker);
        std::fs::write(path, bytes)?;
        Ok(())
    }

    #[test]
    fn the_unreadable_store_line_is_escaped_with_the_rest() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        store_with_hostile_subject_kind(&path, b"\x1b[2J")?;
        let mut written = Vec::new();
        show_status(&mut written, &path, "tx-a")?;
        assert!(!written.contains(&0x1b), "an escape byte reached the terminal");
        assert!(written.iter().all(|byte| matches!(byte, b' '..=b'~' | b'\t' | b'\n')));
        let text = String::from_utf8(written)?;
        assert!(text.starts_with("    runner evidence unreadable: "), "{text}");
        assert!(text.contains("\\u{1b}[2J"), "the quoted value was not shown escaped: {text}");
        Ok(())
    }

    #[test]
    fn a_failed_set_aside_names_itself_keeps_the_store_and_echoes_nothing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        store_with_hostile_subject_kind(&path, b"CNRY")?;
        let damage = std::fs::read(&path)?;
        // Sanity: this damage decodes to an error that quotes the canary.
        assert!(format!("{:#}", snapshot(&path).unwrap_err()).contains("CNRY"));
        // A non-empty directory where the aside goes: rename fails even as root.
        let aside = unreadable_store_path(&path);
        std::fs::create_dir(&aside)?;
        std::fs::write(aside.join("occupied"), b"x")?;
        let error = record(&path, &record_at("tx-b", 2)).unwrap_err();
        for text in [format!("{error:#}"), format!("{error:?}")] {
            assert!(text.contains("could not be set aside"), "{text}");
            assert!(!text.contains("CNRY"), "stored content reached the error: {text}");
        }
        // The failure cost nothing: the store is as it was, with no evidence
        // written over it.
        assert_eq!(std::fs::read(&path)?, damage);
        assert!(aside.join("occupied").exists());
        Ok(())
    }

    #[test]
    fn an_io_error_leaves_the_store_in_place_and_keeps_no_evidence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        record(&path, &record_at("tx-a", 1))?;
        let before = std::fs::read(&path)?;
        let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        // The first read fails on a healthy store.
        let error = record_reading(&path, &record_at("tx-b", 2), &|_| Err(denied())).unwrap_err();
        assert!(format!("{error:#}").contains("permission denied"), "{error:#}");
        assert_eq!(std::fs::read(&path)?, before, "the store changed");
        assert!(!unreadable_store_path(&path).exists());
        assert!(for_transaction(&path, "tx-b")?.is_none());
        // The second read, the one that confirms the bytes do not decode, fails.
        std::fs::write(&path, b"\xc1 not a store")?;
        let calls = std::cell::Cell::new(0usize);
        let read = |path: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                return Err(denied());
            }
            std::fs::read(path)
        };
        let error = record_reading(&path, &record_at("tx-b", 2), &read).unwrap_err();
        assert!(format!("{error:#}").contains("permission denied"), "{error:#}");
        assert_eq!(std::fs::read(&path)?, b"\xc1 not a store");
        assert!(!unreadable_store_path(&path).exists());
        // Bytes that changed between the reads are no verdict either.
        let calls = std::cell::Cell::new(0usize);
        let read = |path: &Path| -> std::io::Result<Vec<u8>> {
            calls.set(calls.get() + 1);
            let mut bytes = std::fs::read(path)?;
            bytes.push(calls.get() as u8);
            Ok(bytes)
        };
        assert!(record_reading(&path, &record_at("tx-b", 2), &read).is_err());
        assert_eq!(std::fs::read(&path)?, b"\xc1 not a store");
        assert!(!unreadable_store_path(&path).exists());
        Ok(())
    }

    #[test]
    fn a_second_set_aside_says_it_replaced_the_first() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = runner_evidence_store_path(&temp.path().join("control.cc"));
        std::fs::write(&path, b"\xc1 first")?;
        assert_eq!(
            record(&path, &record_at("tx-a", 1))?,
            Kept::SetAside { replaced: None }
        );
        std::fs::write(&path, b"\xc1 second")?;
        let Kept::SetAside { replaced: Some(_) } = record(&path, &record_at("tx-b", 2))? else {
            bail!("a displaced set-aside store went unreported");
        };
        assert_eq!(std::fs::read(unreadable_store_path(&path))?, b"\xc1 second");
        Ok(())
    }
}
