//! Operator incidents: the durable record that a condition needing a human
//! opened, and that it closed.
//!
//! One file, `incidents.cc`, holds one type. Idunn's daemon is its only writer,
//! at the site that decides the condition; it is also the file external
//! readers open, so no second copy exists to drift. It is never `control.cc`,
//! which refuses any type it does not know: a type added there would stop
//! continuity for every target on a rollback.
//!
//! A record carries no free text. A workload's error never enters it, so the
//! published file leaks nothing a reader should not have.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use cultcache_rs::{
    CultCacheEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
    TryCompareExchangeSnapshotOutcome,
};
use serde::{Deserialize, Serialize};

use crate::control_plane::{decode_record, require_id, typed_envelope};
use crate::drivers::{exchange_behind_private_lock, publish_file_mode};

const INCIDENT_SCHEMA: &str = "idunn.operator_incident.v1";

/// How long a closed incident stays in the published file before it retires to
/// the incident history.
pub(crate) const INCIDENT_RETENTION_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IncidentCondition {
    ContinuityExhausted,
}

impl IncidentCondition {
    /// The spelling in the key and on `idunn status`. Pinned equal to the
    /// serialized spelling by `the_incident_fixture_is_what_idunn_reads`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::ContinuityExhausted => "continuity-exhausted",
        }
    }

    /// Whether the subject is an admitted target, so that the target leaving
    /// admission ends the condition. A condition about anything else (a
    /// transaction, a store) answers false and is closed by its own owner.
    pub(crate) const fn ends_with_admission(self) -> bool {
        match self {
            Self::ContinuityExhausted => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CloseReason {
    Recovered,
    NoLongerAdmitted,
}

impl CloseReason {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Recovered => "recovered",
            Self::NoLongerAdmitted => "no-longer-admitted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(type = "idunn.operator_incident", schema = "idunn.operator_incident.v1")]
pub(crate) struct IncidentRecord {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    /// `<condition>:<subject>:<opened_at>`. Injective because at most one
    /// incident per `(condition, subject)` is open at a time.
    #[cultcache(key = 1)]
    pub(crate) incident_key: String,
    #[cultcache(key = 2)]
    pub(crate) condition: IncidentCondition,
    /// The target the condition is about.
    #[cultcache(key = 3)]
    pub(crate) subject: String,
    #[cultcache(key = 4)]
    pub(crate) opened_at_unix_millis: u64,
    #[cultcache(key = 5)]
    pub(crate) closed_at_unix_millis: Option<u64>,
    #[cultcache(key = 6)]
    pub(crate) close_reason: Option<CloseReason>,
}

impl IncidentRecord {
    fn opened(condition: IncidentCondition, subject: &str, now: u64) -> Self {
        Self {
            schema_version: INCIDENT_SCHEMA.to_owned(),
            incident_key: format!("{}:{subject}:{now}", condition.name()),
            condition,
            subject: subject.to_owned(),
            opened_at_unix_millis: now,
            closed_at_unix_millis: None,
            close_reason: None,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.closed_at_unix_millis.is_none()
    }

    /// Whether the record still belongs in the published file: open, or closed
    /// no more than `INCIDENT_RETENTION_MILLIS` ago. The one predicate behind
    /// both retirement and `idunn status`.
    pub(crate) fn inside_retention(&self, now: u64) -> bool {
        self.closed_at_unix_millis
            .is_none_or(|closed_at| now.saturating_sub(closed_at) <= INCIDENT_RETENTION_MILLIS)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == INCIDENT_SCHEMA,
            "incident schema is unsupported"
        );
        require_id(&self.subject, "incident subject")?;
        ensure!(
            self.incident_key
                == format!(
                    "{}:{}:{}",
                    self.condition.name(),
                    self.subject,
                    self.opened_at_unix_millis
                ),
            "incident key is not its condition, subject and opening time"
        );
        ensure!(
            self.closed_at_unix_millis.is_some() == self.close_reason.is_some(),
            "incident has a closing time or a reason but not both"
        );
        if let Some(closed_at) = self.closed_at_unix_millis {
            ensure!(
                closed_at >= self.opened_at_unix_millis,
                "incident closes before it opens"
            );
        }
        Ok(())
    }

    fn envelope(&self, now: u64) -> Result<CultCacheEnvelope> {
        self.validate()?;
        typed_envelope(
            &self.incident_key,
            Self::TYPE,
            INCIDENT_SCHEMA,
            self,
            now,
        )
    }
}

/// One read of the incident file: its envelopes exactly as stored, and the
/// records decoded from them, in the same order. Every write expects exactly
/// these envelopes, so a write only lands on the file it was decided against.
struct Snapshot {
    envelopes: Vec<CultCacheEnvelope>,
    records: Vec<IncidentRecord>,
}

impl Snapshot {
    fn open_record(&self, condition: IncidentCondition, subject: &str) -> Option<usize> {
        self.records.iter().position(|record| {
            record.is_open() && record.condition == condition && record.subject == subject
        })
    }
}

/// The incident file and its history sibling. Incidents are written from the
/// scheduler tick, so no write waits: each is one nonblocking compare-exchange
/// of the whole file against the snapshot it was decided on. A lost race or a
/// held lock is an error and a no-op, retried on a later pass. The lock beside
/// the file is not published and is Idunn's alone
/// (`exchange_behind_private_lock`): the readers of `incidents.cc` take no
/// lock, so only Idunn can ever hold it.
pub(crate) struct IncidentStore {
    path: PathBuf,
}

impl IncidentStore {
    pub(crate) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    fn history_path(&self) -> PathBuf {
        self.path.with_file_name("incident-history.cc")
    }

    /// Every record, oldest key first. An absent file is empty. The file
    /// refuses any document that is not an operator incident.
    pub(crate) fn read(&self) -> Result<Vec<IncidentRecord>> {
        let mut records = self.snapshot()?.records;
        records.sort_by(|left, right| left.incident_key.cmp(&right.incident_key));
        Ok(records)
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let envelopes = SingleFileMessagePackBackingStore::new(&self.path)
            .pull_all_read_only_snapshot()
            .context("reading the incident store")?;
        let mut records = Vec::new();
        for envelope in &envelopes {
            ensure!(
                envelope.r#type == IncidentRecord::TYPE
                    && envelope.schema_id.as_deref() == Some(INCIDENT_SCHEMA),
                "the incident store holds a document that is not an operator incident"
            );
            let record: IncidentRecord = decode_record(envelope)?;
            record.validate()?;
            ensure!(
                envelope.key == record.incident_key,
                "incident store key differs from the incident's identity"
            );
            records.push(record);
        }
        Ok(Snapshot { envelopes, records })
    }

    /// Replace `path`'s whole content with `replacements` if it is still
    /// exactly `expected`. Never waits for the lock.
    fn exchange(
        path: &Path,
        expected: &[CultCacheEnvelope],
        replacements: &[CultCacheEnvelope],
    ) -> Result<()> {
        match exchange_behind_private_lock(path, expected, replacements)? {
            TryCompareExchangeSnapshotOutcome::Exchanged => Ok(()),
            TryCompareExchangeSnapshotOutcome::Mismatch => {
                bail!("the incident store changed while it was being written")
            }
            TryCompareExchangeSnapshotOutcome::LockContended => {
                bail!("the incident store is locked by another holder")
            }
        }
    }

    /// Open an incident unless one is already open for `(condition, subject)`.
    /// `Ok(true)` means this call wrote it.
    pub(crate) fn open(
        &self,
        condition: IncidentCondition,
        subject: &str,
        now: u64,
    ) -> Result<bool> {
        self.open_on(&self.snapshot()?, condition, subject, now)
    }

    fn open_on(
        &self,
        snapshot: &Snapshot,
        condition: IncidentCondition,
        subject: &str,
        now: u64,
    ) -> Result<bool> {
        if snapshot.open_record(condition, subject).is_some() {
            return Ok(false);
        }
        let mut replacements = snapshot.envelopes.clone();
        replacements.push(IncidentRecord::opened(condition, subject, now).envelope(now)?);
        Self::exchange(&self.path, &snapshot.envelopes, &replacements)?;
        publish_file_mode(&self.path)?;
        Ok(true)
    }

    /// Close the open incident for `(condition, subject)`, if there is one.
    /// `Ok(true)` means this call wrote the closure.
    pub(crate) fn close(
        &self,
        condition: IncidentCondition,
        subject: &str,
        reason: CloseReason,
        now: u64,
    ) -> Result<bool> {
        let snapshot = self.snapshot()?;
        let Some(index) = snapshot.open_record(condition, subject) else {
            return Ok(false);
        };
        let mut closed = snapshot.records[index].clone();
        // A clock stepped back must not close an incident before it opened.
        closed.closed_at_unix_millis = Some(now.max(closed.opened_at_unix_millis));
        closed.close_reason = Some(reason);
        let mut replacements = snapshot.envelopes.clone();
        replacements[index] = closed.envelope(now)?;
        Self::exchange(&self.path, &snapshot.envelopes, &replacements)?;
        publish_file_mode(&self.path)?;
        Ok(true)
    }

    /// Move every incident that has left retention to the history file, then
    /// drop it here. History first: a crash between the two leaves the record
    /// in both, and the repeat finds it already archived. An open incident
    /// never moves.
    pub(crate) fn retire_closed(&self, now: u64) -> Result<bool> {
        let snapshot = self.snapshot()?;
        let (kept, retiring): (Vec<_>, Vec<_>) = snapshot
            .envelopes
            .iter()
            .zip(&snapshot.records)
            .partition(|(_, record)| record.inside_retention(now));
        if retiring.is_empty() {
            return Ok(false);
        }
        let history_path = self.history_path();
        let archived = SingleFileMessagePackBackingStore::new(&history_path)
            .pull_all_read_only_snapshot()
            .context("reading the incident history")?;
        let mut history = archived.clone();
        history.extend(
            retiring
                .iter()
                .filter(|(envelope, _)| !archived.iter().any(|held| held.key == envelope.key))
                .map(|(envelope, _)| (*envelope).clone()),
        );
        if history.len() != archived.len() {
            Self::exchange(&history_path, &archived, &history)?;
        }
        let kept = kept
            .into_iter()
            .map(|(envelope, _)| envelope.clone())
            .collect::<Vec<_>>();
        Self::exchange(&self.path, &snapshot.envelopes, &kept)?;
        publish_file_mode(&self.path)?;
        Ok(true)
    }
}

/// The lines `idunn status` prints for one target's incidents: each open one,
/// and each closed one still inside retention.
pub(crate) fn render_incidents(records: &[IncidentRecord], target: &str, now: u64) -> Vec<String> {
    records
        .iter()
        .filter(|record| record.subject == target && record.inside_retention(now))
        .map(|record| {
            let mut line = format!(
                "  incident {} opened-at {}",
                record.condition.name(),
                record.opened_at_unix_millis
            );
            if let (Some(closed_at), Some(reason)) =
                (record.closed_at_unix_millis, record.close_reason)
            {
                line.push_str(&format!(" closed-at {closed_at} {}", reason.name()));
            }
            line
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;

    const OPENED: u64 = 1_700_000_000_000;
    const CLOSED: u64 = 1_700_000_100_000;
    const FIXTURE: &str = "tests/fixtures/idunn.operator_incident.v1.cc";

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
    }

    /// The two records the fixture holds, spelled independently of the store:
    /// one open, one closed.
    fn fixture_records() -> [IncidentRecord; 2] {
        let closed_key = format!("continuity-exhausted:closed-target:{OPENED}");
        let open_key = format!("continuity-exhausted:open-target:{CLOSED}");
        [
            IncidentRecord {
                schema_version: "idunn.operator_incident.v1".into(),
                incident_key: closed_key,
                condition: IncidentCondition::ContinuityExhausted,
                subject: "closed-target".into(),
                opened_at_unix_millis: OPENED,
                closed_at_unix_millis: Some(CLOSED),
                close_reason: Some(CloseReason::Recovered),
            },
            IncidentRecord {
                schema_version: "idunn.operator_incident.v1".into(),
                incident_key: open_key,
                condition: IncidentCondition::ContinuityExhausted,
                subject: "open-target".into(),
                opened_at_unix_millis: CLOSED,
                closed_at_unix_millis: None,
                close_reason: None,
            },
        ]
    }

    #[test]
    fn incident_records_validate_their_key_and_closure() {
        let [closed, open] = fixture_records();
        closed.validate().unwrap();
        open.validate().unwrap();

        let mut wrong_key = open.clone();
        wrong_key.incident_key.push('x');
        assert!(wrong_key.validate().is_err());

        let mut reason_missing = open.clone();
        reason_missing.closed_at_unix_millis = Some(CLOSED + 1);
        assert!(reason_missing.validate().is_err());

        let mut closing_missing = open.clone();
        closing_missing.close_reason = Some(CloseReason::Recovered);
        assert!(closing_missing.validate().is_err());

        let mut before_open = closed.clone();
        before_open.closed_at_unix_millis = Some(OPENED - 1);
        assert!(before_open.validate().is_err());

        let mut empty_subject = open.clone();
        empty_subject.subject = String::new();
        empty_subject.incident_key = format!("continuity-exhausted::{CLOSED}");
        assert!(empty_subject.validate().is_err());

        let mut wrong_schema = open;
        wrong_schema.schema_version = "idunn.operator_incident.v0".into();
        assert!(wrong_schema.validate().is_err());
    }

    #[test]
    fn the_incident_fixture_is_what_idunn_reads() {
        if std::env::var_os("IDUNN_WRITE_FIXTURES").is_some() {
            let temp = TempDir::new().unwrap();
            let store = IncidentStore::new(&temp.path().join("incidents.cc"));
            assert!(
                store
                    .open(IncidentCondition::ContinuityExhausted, "closed-target", OPENED)
                    .unwrap()
            );
            assert!(
                store
                    .close(
                        IncidentCondition::ContinuityExhausted,
                        "closed-target",
                        CloseReason::Recovered,
                        CLOSED
                    )
                    .unwrap()
            );
            assert!(
                store
                    .open(IncidentCondition::ContinuityExhausted, "open-target", CLOSED)
                    .unwrap()
            );
            std::fs::copy(temp.path().join("incidents.cc"), fixture_path()).unwrap();
        }
        // A copy, so reading the committed file can never leave a lock beside it.
        let temp = TempDir::new().unwrap();
        let copy = temp.path().join("incidents.cc");
        std::fs::copy(fixture_path(), &copy).unwrap();
        assert_eq!(
            IncidentStore::new(&copy).read().unwrap(),
            fixture_records().to_vec()
        );
        // The wire spelling readers match on is the spelling the key and
        // status use.
        let payload = rmp_serde::to_vec(&IncidentCondition::ContinuityExhausted).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<String>(&payload).unwrap(),
            IncidentCondition::ContinuityExhausted.name()
        );
        let payload = rmp_serde::to_vec(&CloseReason::NoLongerAdmitted).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<String>(&payload).unwrap(),
            CloseReason::NoLongerAdmitted.name()
        );
    }

    #[test]
    fn the_store_opens_once_closes_once_and_reopens_as_a_new_record() {
        let temp = TempDir::new().unwrap();
        let store = IncidentStore::new(&temp.path().join("incidents.cc"));
        let condition = IncidentCondition::ContinuityExhausted;
        assert!(store.read().unwrap().is_empty());
        assert!(!store.close(condition, "service", CloseReason::Recovered, OPENED).unwrap());

        assert!(store.open(condition, "service", OPENED).unwrap());
        assert!(!store.open(condition, "service", OPENED + 5).unwrap());
        assert!(store.open(condition, "other", OPENED).unwrap());

        assert!(store.close(condition, "service", CloseReason::Recovered, CLOSED).unwrap());
        assert!(!store.close(condition, "service", CloseReason::Recovered, CLOSED).unwrap());

        assert!(store.open(condition, "service", CLOSED + 1).unwrap());
        let records = store.read().unwrap();
        assert_eq!(records.len(), 3);
        let keys = records.iter().map(|r| r.incident_key.as_str()).collect::<Vec<_>>();
        assert!(keys.contains(&format!("continuity-exhausted:service:{OPENED}").as_str()));
        assert!(keys.contains(&format!("continuity-exhausted:service:{}", CLOSED + 1).as_str()));

        // A clock stepped back closes at the opening time, never before it.
        assert!(store.close(condition, "other", CloseReason::NoLongerAdmitted, OPENED - 9).unwrap());
        let other = store.read().unwrap().into_iter().find(|r| r.subject == "other").unwrap();
        assert_eq!(other.closed_at_unix_millis, Some(OPENED));
        assert_eq!(other.close_reason, Some(CloseReason::NoLongerAdmitted));
    }

    #[test]
    fn closed_incidents_retire_to_history_after_retention() {
        // The ruled retention: seven days.
        assert_eq!(INCIDENT_RETENTION_MILLIS, 604_800_000);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        for subject in ["old", "young", "open"] {
            assert!(store.open(condition, subject, OPENED).unwrap());
        }
        assert!(store.close(condition, "old", CloseReason::Recovered, OPENED + 1).unwrap());
        let young_closed = OPENED + INCIDENT_RETENTION_MILLIS;
        assert!(store.close(condition, "young", CloseReason::Recovered, young_closed).unwrap());

        // Exactly at the boundary nothing has aged out; one millisecond later
        // only the older closure has.
        let boundary = OPENED + 1 + INCIDENT_RETENTION_MILLIS;
        assert!(!store.retire_closed(boundary).unwrap());
        assert!(store.retire_closed(boundary + 1).unwrap());
        assert!(!store.retire_closed(boundary + 1).unwrap());

        let live = store.read().unwrap();
        let mut subjects = live.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>();
        subjects.sort_unstable();
        assert_eq!(subjects, ["open", "young"]);

        let history = SingleFileMessagePackBackingStore::new(&temp.path().join("incident-history.cc"))
            .pull_all_read_only_snapshot()
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].key, format!("continuity-exhausted:old:{OPENED}"));

        // Later the younger one retires too; the open one never does.
        assert!(store.retire_closed(young_closed + INCIDENT_RETENTION_MILLIS + 1).unwrap());
        let live = store.read().unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].subject, "open");
    }

    #[test]
    fn the_store_refuses_any_document_that_is_not_an_incident() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let foreign = CultCacheEnvelope {
            key: "x".into(),
            r#type: "idunn.target_supervision".into(),
            payload: vec![0x90],
            stored_at: "2026-01-01T00:00:00+00:00".into(),
            schema_id: Some("idunn.target_supervision.v1".into()),
        };
        SingleFileMessagePackBackingStore::new(&path)
            .insert_entry_if_absent(foreign)
            .unwrap();
        let store = IncidentStore::new(&path);
        assert!(store.read().is_err());
        assert!(
            store
                .open(IncidentCondition::ContinuityExhausted, "service", OPENED)
                .is_err()
        );
    }

    #[test]
    fn status_renders_open_and_recently_closed_incidents_for_their_target() {
        let [closed, open] = fixture_records();
        let records = [closed, open];
        let lines = render_incidents(&records, "closed-target", CLOSED + 1);
        assert_eq!(
            lines,
            [format!(
                "  incident continuity-exhausted opened-at {OPENED} closed-at {CLOSED} recovered"
            )]
        );
        assert_eq!(
            render_incidents(&records, "open-target", CLOSED + 1),
            [format!("  incident continuity-exhausted opened-at {CLOSED}")]
        );
        assert!(render_incidents(&records, "unrelated", CLOSED + 1).is_empty());
        // A closed incident past retention is history, not status.
        assert!(
            render_incidents(&records, "closed-target", CLOSED + INCIDENT_RETENTION_MILLIS + 1)
                .is_empty()
        );
        assert_eq!(
            render_incidents(&records, "closed-target", CLOSED + INCIDENT_RETENTION_MILLIS).len(),
            1
        );
    }

    /// Idunn runs with `UMask=027`. The umask is process-wide, so it is held
    /// only around each call and only one test at a time may hold it.
    #[cfg(target_os = "linux")]
    pub(crate) fn under_service_umask<T>(write: impl FnOnce() -> T) -> T {
        static UMASK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = UMASK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: umask only swaps the process file-creation mask.
        let previous = unsafe { libc::umask(0o027) };
        let result = write();
        unsafe { libc::umask(previous) };
        result
    }

    /// The body's condition: a directory carrying the default ACL
    /// `u::rw g::r o::r` that Yggdrasil's idunn-projection directory has. Under
    /// it the umask is ignored and a file created with mode 0666 lands 0644.
    #[cfg(target_os = "linux")]
    pub(crate) fn give_default_acl(dir: &Path) {
        use std::os::unix::ffi::OsStrExt;

        // posix_acl_xattr: version 2, then (tag, perm, id) entries. The tags
        // are ACL_USER_OBJ, ACL_GROUP_OBJ and ACL_OTHER; the id is undefined.
        let mut value = 2u32.to_le_bytes().to_vec();
        for (tag, perm) in [(0x01u16, 6u16), (0x04, 4), (0x20, 4)] {
            value.extend(tag.to_le_bytes());
            value.extend(perm.to_le_bytes());
            value.extend(u32::MAX.to_le_bytes());
        }
        let dir = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
        // SAFETY: both strings are NUL-terminated and the value is a live buffer.
        let rc = unsafe {
            libc::setxattr(
                dir.as_ptr(),
                c"system.posix_acl_default".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(
            rc,
            0,
            "the test filesystem must accept a default POSIX ACL: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Whether uid 65534 can read `path`; `None` when the uid cannot be
    /// switched (not root, or no setpriv), which leaves the question unproven.
    #[cfg(target_os = "linux")]
    pub(crate) fn readable_by_another_uid(path: &Path) -> Option<bool> {
        std::process::Command::new("setpriv")
            .args(["--reuid=65534", "--regid=65534", "--clear-groups", "cat"])
            .arg(path)
            .output()
            .ok()
            .map(|output| output.status.success())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn under_a_default_acl_the_store_is_published_and_its_locks_are_private() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        give_default_acl(dir);
        let path = dir.join("incidents.cc");
        let lock = dir.join("incidents.cc.lock");
        let history_lock = dir.join("incident-history.cc.lock");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

        // Control: the lock CultCache would have made here, open(2) mode 0666
        // under Idunn's umask. Without the ACL in force this reads 0640 and the
        // test below proves nothing about the body.
        let control = dir.join("control.lock");
        under_service_umask(|| {
            std::fs::OpenOptions::new().create(true).write(true).mode(0o666).open(&control)
        })
        .unwrap();
        assert_eq!(mode(&control), 0o644, "the default ACL is not in force");

        assert!(under_service_umask(|| store.open(condition, "service", OPENED)).unwrap());
        assert_eq!(mode(&path), 0o644, "after open");
        assert_eq!(mode(&lock), 0o600, "the lock after open");

        assert!(
            under_service_umask(|| store.close(condition, "service", CloseReason::Recovered, CLOSED))
                .unwrap()
        );
        assert_eq!(mode(&path), 0o644, "after close");
        assert_eq!(mode(&lock), 0o600, "the lock after close");

        assert!(
            under_service_umask(|| store.retire_closed(CLOSED + INCIDENT_RETENTION_MILLIS + 1))
                .unwrap()
        );
        assert_eq!(mode(&path), 0o644, "after retire");
        assert_eq!(mode(&lock), 0o600, "the lock after retire");
        assert_eq!(mode(&history_lock), 0o600, "the history lock after retire");

        // The consequence: another uid cannot open the lock, so it cannot hold
        // it. Provable only as root with setpriv, and only if the same uid can
        // read the control file, which shows the switch itself works.
        if readable_by_another_uid(&control) == Some(true) {
            assert_eq!(readable_by_another_uid(&lock), Some(false));
            assert_eq!(readable_by_another_uid(&history_lock), Some(false));
        } else {
            eprintln!("UNPROVEN: no uid switch here; the lock modes above stand alone");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_left_wider_by_an_earlier_run_is_replaced_before_it_is_used() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let lock = temp.path().join("incidents.cc.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
        let before = std::fs::metadata(&lock).unwrap().ino();

        assert!(
            IncidentStore::new(&path)
                .open(IncidentCondition::ContinuityExhausted, "service", OPENED)
                .unwrap()
        );
        let after = std::fs::metadata(&lock).unwrap();
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_ne!(after.ino(), before, "the wider lock was chmodded, not replaced");
    }

    /// The lock beside a store, spelled independently of the code under test.
    #[cfg(target_os = "linux")]
    fn lock_beside(store: &Path) -> PathBuf {
        let mut name = store.file_name().unwrap().to_owned();
        name.push(".lock");
        store.with_file_name(name)
    }

    /// An exclusive flock on a store's lock, held by a SEPARATE PROCESS
    /// (`flock -x <lock> sleep`), so the holder shares no descriptor, thread
    /// or address space with the code under test. A watch on the lock counts
    /// every open of it from the moment it is held.
    #[cfg(target_os = "linux")]
    pub(crate) struct HeldLock {
        holder: std::sync::Arc<std::sync::Mutex<std::process::Child>>,
        watch: std::os::fd::OwnedFd,
    }

    /// Hold the lock of `store`. An absent lock is created as production keeps
    /// it (0600, ours); a lock a test put there first is held as it is, never
    /// chmodded.
    #[cfg(target_os = "linux")]
    pub(crate) fn hold_lock_of(store: &Path) -> HeldLock {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let lock = lock_beside(store);
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock)
            .unwrap();
        let mut holder = std::process::Command::new("flock")
            .args(["--no-fork", "--exclusive"])
            .arg(&lock)
            .args(["sleep", "3600"])
            .spawn()
            .expect("flock (util-linux) must be installed to hold a lock from another process");

        // Held once a nonblocking exclusive flock from here is refused.
        let deadline = Instant::now() + STUCK_AFTER;
        loop {
            assert!(holder.try_wait().unwrap().is_none(), "the lock holder exited");
            let probe = std::fs::File::open(&lock).unwrap();
            // SAFETY: flock on a descriptor this function owns.
            let refused = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
            if refused {
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                break;
            }
            drop(probe);
            assert!(Instant::now() < deadline, "the lock was never held");
            std::thread::sleep(Duration::from_millis(10));
        }

        // Open and close alternate, so two opens are never identical adjacent
        // events for the kernel to coalesce into one.
        // SAFETY: plain syscalls on descriptors and strings this function owns.
        let watch = unsafe {
            let fd = libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK);
            assert!(fd >= 0, "{}", std::io::Error::last_os_error());
            let watch = OwnedFd::from_raw_fd(fd);
            let path = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
            let wd = libc::inotify_add_watch(
                fd,
                path.as_ptr(),
                libc::IN_OPEN | libc::IN_CLOSE_WRITE | libc::IN_CLOSE_NOWRITE,
            );
            assert!(wd >= 0, "{}", std::io::Error::last_os_error());
            watch
        };
        HeldLock {
            holder: std::sync::Arc::new(std::sync::Mutex::new(holder)),
            watch,
        }
    }

    #[cfg(target_os = "linux")]
    impl HeldLock {
        /// Whether the holder process is still running and so still holds.
        pub(crate) fn is_held(&self) -> bool {
            self.holder.lock().unwrap().try_wait().unwrap().is_none()
        }

        /// How many times the lock has been opened since it was held.
        fn opens(&self) -> usize {
            use std::os::unix::io::AsRawFd;

            let mut opens = 0;
            let mut buffer = [0u8; 4096];
            loop {
                // SAFETY: reads into a live buffer of the stated length.
                let read = unsafe {
                    libc::read(self.watch.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                };
                if read <= 0 {
                    return opens;
                }
                let read = usize::try_from(read).unwrap();
                let mut at = 0;
                // struct inotify_event: wd i32, mask u32, cookie u32, len u32, name[len].
                while at + 16 <= read {
                    let word = |offset: usize| {
                        u32::from_ne_bytes(buffer[at + offset..at + offset + 4].try_into().unwrap())
                    };
                    if word(4) & libc::IN_OPEN != 0 {
                        opens += 1;
                    }
                    at += 16 + usize::try_from(word(12)).unwrap();
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for HeldLock {
        fn drop(&mut self) {
            let mut holder = self.holder.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = holder.kill();
            let _ = holder.wait();
        }
    }

    /// How long a test lets a held lock, or a released lock's last straggler,
    /// stand in its way before it calls the store stuck. Nothing correct ever
    /// spends it: the code under test finishes in milliseconds, and the limit
    /// is only what stops a blocked write from hanging the suite. No
    /// assertion compares elapsed time to a bound the machine's load can move.
    #[cfg(target_os = "linux")]
    pub(crate) const STUCK_AFTER: Duration = Duration::from_secs(30);

    /// Run `work` while `held` is held, and fail if it waited on it. The rule
    /// is observed where it is decided, at the lock, not on a clock: every
    /// exchange opens the lock exactly once, so `exchanges` is the number of
    /// exchanges `work` makes against this lock, and a wait of any length (a
    /// sleep and retry, a poll, a second attempt) opens it more often. A wait
    /// that never ends is freed by the holder's release after `STUCK_AFTER`,
    /// and is then seen to have waited, so the test fails instead of hanging.
    /// The holder is still holding when `work` returns, and is released on the
    /// way out.
    #[cfg(target_os = "linux")]
    pub(crate) fn run_while_held<T>(
        held: HeldLock,
        exchanges: usize,
        work: impl FnOnce() -> T,
    ) -> T {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, mpsc};

        let (finished, work_done) = mpsc::channel::<()>();
        let gave_up = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let gave_up = Arc::clone(&gave_up);
            let holder = Arc::clone(&held.holder);
            std::thread::spawn(move || {
                if work_done.recv_timeout(STUCK_AFTER) == Err(mpsc::RecvTimeoutError::Timeout) {
                    gave_up.store(true, Ordering::SeqCst);
                    let _ = holder.lock().unwrap().kill();
                }
            })
        };
        let result = work();
        let still_held = held.is_held();
        let _ = finished.send(());
        watchdog.join().unwrap();
        assert!(!gave_up.load(Ordering::SeqCst), "the work waited on a held lock");
        assert!(still_held, "the holder let go before the work returned");
        assert_eq!(
            held.opens(),
            exchanges,
            "the work opened the held lock other than once per exchange: it waited or retried"
        );
        result
    }

    /// Retry `attempt` until it lands. A lock just released can take a moment
    /// to be seen free.
    #[cfg(target_os = "linux")]
    pub(crate) fn eventually<T>(mut attempt: impl FnMut() -> Result<T>) -> T {
        let deadline = Instant::now() + STUCK_AFTER;
        loop {
            match attempt() {
                Ok(value) => return value,
                Err(error) if Instant::now() >= deadline => panic!("never landed: {error:#}"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_incident_write_waits_on_a_held_lock_and_each_lands_when_it_is_freed() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        assert!(store.open(condition, "closing", OPENED).unwrap());
        assert!(store.close(condition, "closing", CloseReason::Recovered, OPENED + 1).unwrap());
        assert!(store.open(condition, "open", OPENED).unwrap());
        let before = std::fs::read(&path).unwrap();
        let retired = OPENED + 1 + INCIDENT_RETENTION_MILLIS + 1;

        // One exchange each: open, close, and the retirement's store delete.
        let outcomes = run_while_held(hold_lock_of(&path), 3, || {
            (
                store.open(condition, "new", CLOSED),
                store.close(condition, "open", CloseReason::Recovered, CLOSED),
                store.retire_closed(retired),
            )
        });
        for outcome in [outcomes.0.map(drop), outcomes.1.map(drop), outcomes.2.map(drop)] {
            let error = outcome.unwrap_err().to_string();
            assert_eq!(error, "the incident store is locked by another holder");
        }
        assert_eq!(std::fs::read(&path).unwrap(), before, "a contended write changed the file");

        // Nothing was lost by the refusal: the same calls land once it is free.
        // (Retirement archives first, so its refused delete left the record in
        // history too; the repeat archives nothing twice.)
        assert!(eventually(|| store.open(condition, "new", CLOSED)));
        assert!(eventually(|| store.close(condition, "open", CloseReason::Recovered, CLOSED)));
        assert!(eventually(|| store.retire_closed(retired)));
        let history = SingleFileMessagePackBackingStore::new(&temp.path().join("incident-history.cc"));
        assert_eq!(history.pull_all_read_only_snapshot().unwrap().len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_held_history_lock_keeps_the_incident_where_it_is() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let history = temp.path().join("incident-history.cc");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        assert!(store.open(condition, "old", OPENED).unwrap());
        assert!(store.close(condition, "old", CloseReason::Recovered, OPENED + 1).unwrap());
        let before = std::fs::read(&path).unwrap();
        let retired = OPENED + 1 + INCIDENT_RETENTION_MILLIS + 1;

        let outcome = run_while_held(hold_lock_of(&history), 1, || store.retire_closed(retired));
        assert!(outcome.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(eventually(|| store.retire_closed(retired)));
    }

    #[test]
    fn a_write_decided_on_a_stale_snapshot_does_not_land() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;

        // Two writers read the file with no incident open for the subject.
        let stale = store.snapshot().unwrap();
        assert!(store.open(condition, "service", OPENED).unwrap());

        // The slower one must lose: one open incident per subject.
        let error = store
            .open_on(&stale, condition, "service", OPENED + 1)
            .unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        let records = store.read().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].opened_at_unix_millis, OPENED);
    }

    #[test]
    fn retiring_after_a_crash_between_history_and_delete_archives_once() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        assert!(store.open(condition, "old", OPENED).unwrap());
        assert!(store.close(condition, "old", CloseReason::Recovered, OPENED + 1).unwrap());
        // History already holds the record: the delete never ran.
        let envelope = store.snapshot().unwrap().envelopes[0].clone();
        let history = SingleFileMessagePackBackingStore::new(&temp.path().join("incident-history.cc"));
        assert!(history.insert_entry_if_absent(envelope).unwrap());

        assert!(store.retire_closed(OPENED + 1 + INCIDENT_RETENTION_MILLIS + 1).unwrap());
        assert!(store.read().unwrap().is_empty());
        assert_eq!(history.pull_all_read_only_snapshot().unwrap().len(), 1);
    }
}
