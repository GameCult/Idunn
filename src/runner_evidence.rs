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

/// How a write left the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kept {
    Stored,
    /// The store could not be decoded; it was moved to
    /// `unreadable_store_path` and the record opened a fresh one.
    AfterSettingAsideAnUnreadableStore,
}

/// Where an undecodable store is kept. One slot: the next one replaces it, so
/// the damage a bad store can cost is one extra file.
pub(crate) fn unreadable_store_path(path: &Path) -> PathBuf {
    path.with_file_name("runner-evidence.unreadable.cc")
}

/// An undecodable store (a corrupt file, or one a newer Idunn wrote) must not
/// stop later failures from keeping their evidence. Move it aside, root-only,
/// over the previous one. Nothing of its content is named: a decode error can
/// quote stored values. A store that cannot even be read is left in place.
fn set_aside_unreadable(path: &Path) -> Result<()> {
    std::fs::read(path).context("the runner evidence store could not be read")?;
    let aside = unreadable_store_path(path);
    std::fs::rename(path, &aside)
        .context("the runner evidence store is undecodable and could not be set aside")?;
    keep_private(&aside)
}

/// The one writer. Upsert by transaction id; past `RUNNER_EVIDENCE_RECORDS` the
/// oldest records go in the same exchange. The file ends at mode 0600. A store
/// that cannot be decoded is set aside first and the record opens a new one.
pub(crate) fn record(path: &Path, record: &RunnerFailureRecord) -> Result<Kept> {
    let (envelopes, records, how) = match snapshot(path) {
        Ok((envelopes, records)) => (envelopes, records, Kept::Stored),
        Err(_) => {
            set_aside_unreadable(path)?;
            (Vec::new(), Vec::new(), Kept::AfterSettingAsideAnUnreadableStore)
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

/// Format characters that reorder what a terminal shows: the set the mind's
/// Title rule refuses.
fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if (character.is_control() || is_bidi_control(character))
            && character != '\n'
            && character != '\t'
        {
            escaped.push_str(&format!("\\u{{{:x}}}", u32::from(character)));
        } else {
            escaped.push(character);
        }
    }
    escaped
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
        let lines = render(&stored);
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
        let lines = render(&stored);
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
        assert_eq!(kept, Kept::AfterSettingAsideAnUnreadableStore);
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
            Kept::AfterSettingAsideAnUnreadableStore
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
    fn status_escapes_bidi_controls_in_tails_and_in_names() {
        for bidi in [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            let mut stored = record_at("tx-a", 1);
            let tail = format!("ok {bidi}gpj.exe\n");
            stored.stdout_tail = tail.clone().into_bytes();
            stored.stdout_total_bytes = tail.len() as u64;
            stored.stderr_tail = tail.clone().into_bytes();
            stored.stderr_total_bytes = tail.len() as u64;
            stored.subject_id = format!("step{bidi}x");
            stored.runner = format!("rust{bidi}");
            let text = render(&stored).join("\n");
            let escaped = format!("\\u{{{:x}}}", bidi as u32);
            assert!(!text.contains(bidi), "U+{:04X} reached the terminal", bidi as u32);
            // Escaped in every place it can appear: two tails, the subject, the runner.
            assert_eq!(text.matches(&escaped).count(), 4, "{text}");
        }
        // Format characters either side of the set pass.
        let mut stored = record_at("tx-a", 1);
        stored.stdout_tail = "caf\u{e9} \u{200d}\u{202f}\u{2065}\u{206a}\n".as_bytes().to_vec();
        stored.stdout_total_bytes = stored.stdout_tail.len() as u64;
        let text = render(&stored).join("\n");
        assert!(text.contains("caf\u{e9} \u{200d}\u{202f}\u{2065}\u{206a}"), "{text}");
    }

    #[test]
    fn status_escapes_the_c1_controls() {
        for code in 0x80u32..=0x9f {
            let character = char::from_u32(code).unwrap();
            let mut stored = record_at("tx-a", 1);
            stored.stdout_tail = format!("a{character}b\n").into_bytes();
            stored.stdout_total_bytes = stored.stdout_tail.len() as u64;
            let text = render(&stored).join("\n");
            assert!(!text.contains(character), "U+{code:04X} reached the terminal");
            assert!(text.contains(&format!("a\\u{{{code:x}}}b")), "U+{code:04X}: {text}");
        }
        // U+00A0 is not a control character and passes.
        let mut stored = record_at("tx-a", 1);
        stored.stdout_tail = "a\u{a0}b\n".as_bytes().to_vec();
        stored.stdout_total_bytes = stored.stdout_tail.len() as u64;
        assert!(render(&stored).join("\n").contains("a\u{a0}b"));
    }
}
