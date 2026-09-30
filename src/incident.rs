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

use anyhow::{Context, Result, ensure};
use cultcache_rs::{
    CultCacheEnvelope, CultCacheExpectedEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
};
use serde::{Deserialize, Serialize};

use crate::control_plane::{decode_record, require_id, typed_envelope};
use crate::drivers::publish_projection_mode;

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

struct Stored {
    envelope: CultCacheEnvelope,
    record: IncidentRecord,
}

/// The incident file and its history sibling. Every write is a compare-exchange
/// against exactly what was read, and every write is followed by the
/// world-readable publication mode.
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

    fn backing(&self) -> SingleFileMessagePackBackingStore {
        SingleFileMessagePackBackingStore::new(&self.path)
    }

    /// Every record, oldest key first. An absent file is empty. The file
    /// refuses any document that is not an operator incident.
    pub(crate) fn read(&self) -> Result<Vec<IncidentRecord>> {
        Ok(self
            .read_stored()?
            .into_iter()
            .map(|stored| stored.record)
            .collect())
    }

    fn read_stored(&self) -> Result<Vec<Stored>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let mut stored = Vec::new();
        for envelope in self
            .backing()
            .pull_all_read_only_snapshot()
            .context("reading the incident store")?
        {
            ensure!(
                envelope.r#type == IncidentRecord::TYPE
                    && envelope.schema_id.as_deref() == Some(INCIDENT_SCHEMA),
                "the incident store holds a document that is not an operator incident"
            );
            let record: IncidentRecord = decode_record(&envelope)?;
            record.validate()?;
            ensure!(
                envelope.key == record.incident_key,
                "incident store key differs from the incident's identity"
            );
            stored.push(Stored { envelope, record });
        }
        stored.sort_by(|left, right| left.record.incident_key.cmp(&right.record.incident_key));
        Ok(stored)
    }

    fn open_record<'a>(
        stored: &'a [Stored],
        condition: IncidentCondition,
        subject: &str,
    ) -> Option<&'a Stored> {
        stored.iter().find(|stored| {
            stored.record.is_open()
                && stored.record.condition == condition
                && stored.record.subject == subject
        })
    }

    /// Open an incident unless one is already open for `(condition, subject)`.
    /// `Ok(true)` means this call wrote it.
    pub(crate) fn open(
        &self,
        condition: IncidentCondition,
        subject: &str,
        now: u64,
    ) -> Result<bool> {
        let stored = self.read_stored()?;
        if Self::open_record(&stored, condition, subject).is_some() {
            return Ok(false);
        }
        let record = IncidentRecord::opened(condition, subject, now);
        let written = self.backing().compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: IncidentRecord::TYPE.into(),
                key: record.incident_key.clone(),
                current: None,
            }],
            &[record.envelope(now)?],
        )?;
        if written {
            publish_projection_mode(&self.path)?;
        }
        Ok(written)
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
        let stored = self.read_stored()?;
        let Some(open) = Self::open_record(&stored, condition, subject) else {
            return Ok(false);
        };
        let mut closed = open.record.clone();
        // A clock stepped back must not close an incident before it opened.
        closed.closed_at_unix_millis = Some(now.max(closed.opened_at_unix_millis));
        closed.close_reason = Some(reason);
        let written = self.backing().compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: IncidentRecord::TYPE.into(),
                key: closed.incident_key.clone(),
                current: Some(open.envelope.clone()),
            }],
            &[closed.envelope(now)?],
        )?;
        if written {
            publish_projection_mode(&self.path)?;
        }
        Ok(written)
    }

    /// Move every incident closed more than `INCIDENT_RETENTION_MILLIS` ago to
    /// the history file, then delete it here. History first: a crash between
    /// the two leaves the record in both, and `insert_entry_if_absent` makes
    /// the repeat a no-op. An open incident never moves.
    pub(crate) fn retire_closed(&self, now: u64) -> Result<bool> {
        let retiring = self
            .read_stored()?
            .into_iter()
            .filter(|stored| {
                stored.record.closed_at_unix_millis.is_some_and(|closed_at| {
                    now.saturating_sub(closed_at) > INCIDENT_RETENTION_MILLIS
                })
            })
            .map(|stored| stored.envelope)
            .collect::<Vec<_>>();
        if retiring.is_empty() {
            return Ok(false);
        }
        let history = SingleFileMessagePackBackingStore::new(&self.history_path());
        for envelope in &retiring {
            history
                .insert_entry_if_absent(envelope.clone())
                .context("archiving a closed incident")?;
        }
        let deleted = self
            .backing()
            .delete_batch_if_unchanged(&retiring)
            .context("retiring a closed incident")?;
        if deleted {
            publish_projection_mode(&self.path)?;
        }
        Ok(deleted)
    }
}

/// The lines `idunn status` prints for one target's incidents: each open one,
/// and each closed one still inside retention.
pub(crate) fn render_incidents(records: &[IncidentRecord], target: &str, now: u64) -> Vec<String> {
    records
        .iter()
        .filter(|record| record.subject == target)
        .filter(|record| {
            record.closed_at_unix_millis.is_none_or(|closed_at| {
                now.saturating_sub(closed_at) <= INCIDENT_RETENTION_MILLIS
            })
        })
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
mod tests {
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

    /// Idunn runs with `UMask=027`, so a file it creates lands 0640 unless the
    /// store publishes it. The umask is process-wide, so it is held only around
    /// each write and only one test at a time may hold it.
    #[cfg(unix)]
    fn under_service_umask<T>(write: impl FnOnce() -> T) -> T {
        static UMASK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = UMASK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: umask only swaps the process file-creation mask.
        let previous = unsafe { libc::umask(0o027) };
        let result = write();
        unsafe { libc::umask(previous) };
        result
    }

    #[cfg(unix)]
    #[test]
    fn published_incident_store_is_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("incidents.cc");
        let lock = temp.path().join("incidents.cc.lock");
        let store = IncidentStore::new(&path);
        let condition = IncidentCondition::ContinuityExhausted;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

        assert!(under_service_umask(|| store.open(condition, "service", OPENED)).unwrap());
        assert_eq!((mode(&path), mode(&lock)), (0o644, 0o644), "after open");

        // Each write replaces the file, and the replacement is published too.
        assert!(
            under_service_umask(|| store.close(condition, "service", CloseReason::Recovered, CLOSED))
                .unwrap()
        );
        assert_eq!((mode(&path), mode(&lock)), (0o644, 0o644), "after close");

        assert!(
            under_service_umask(|| store.retire_closed(CLOSED + INCIDENT_RETENTION_MILLIS + 1))
                .unwrap()
        );
        assert_eq!(mode(&path), 0o644, "after retire");
    }
}
