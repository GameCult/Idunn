//! The deployment stop: the one reader and the one writer of the record that
//! halts deployments of a runtime. Engaged halts; absent or clear admits. Who
//! engaged it and why are on the record, and nothing is signed, issued, bound
//! to a release or consumed: the brake is a stop, not a per-deploy gate (ruling
//! brake-is-a-stop). The lifecycle brake is a different record and a different
//! store; nothing here touches it.
//!
//! The reader fails closed. A store that is present but foreign, ambiguous,
//! noncanonical, for another runtime, a symlink, or owned by anyone but the
//! expected uid reads as `Unreadable`, which does not admit.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{CultCacheEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore};
use serde::{Deserialize, Serialize};

pub(crate) const DEPLOYMENT_STOP_SCHEMA: &str = "idunn.deployment_stop.v1";
pub(crate) const DEPLOYMENT_STOP_KEY: &str = "deployment-stop";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StopState {
    Engaged,
    Clear,
}

impl StopState {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Engaged => "engaged",
            Self::Clear => "clear",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(type = "idunn.deployment_stop", schema = "idunn.deployment_stop.v1")]
pub(crate) struct DeploymentStop {
    #[cultcache(key = 0)]
    pub(crate) schema_version: String,
    #[cultcache(key = 1)]
    pub(crate) runtime_id: String,
    #[cultcache(key = 2)]
    pub(crate) state: StopState,
    /// The login name of the user who ran the verb, read from the process uid.
    #[cultcache(key = 3)]
    pub(crate) by: String,
    #[cultcache(key = 4)]
    pub(crate) reason: String,
    /// The verb's clock.
    #[cultcache(key = 5)]
    pub(crate) at_unix_millis: u64,
    /// The previous record's sequence plus one; 1 for the first.
    #[cultcache(key = 6)]
    pub(crate) sequence: u64,
}

fn bounded(value: &str, label: &str, max: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "deployment stop {label} is empty, oversized, or contains control characters"
    );
    Ok(())
}

impl DeploymentStop {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == DEPLOYMENT_STOP_SCHEMA,
            "deployment stop schema is unsupported"
        );
        bounded(&self.runtime_id, "runtime id", 256)?;
        bounded(&self.by, "author", 256)?;
        bounded(&self.reason, "reason", 1024)?;
        ensure!(
            self.at_unix_millis > 0 && self.sequence > 0,
            "deployment stop time or sequence is zero"
        );
        Ok(())
    }

    fn envelope(&self) -> Result<CultCacheEnvelope> {
        self.validate()?;
        let stored_at = chrono::DateTime::from_timestamp_millis(i64::try_from(self.at_unix_millis)?)
            .ok_or_else(|| anyhow!("deployment stop time is out of range"))?
            .to_rfc3339();
        Ok(CultCacheEnvelope {
            key: DEPLOYMENT_STOP_KEY.into(),
            r#type: Self::TYPE.into(),
            payload: rmp_serde::to_vec(self)?,
            stored_at,
            schema_id: Some(DEPLOYMENT_STOP_SCHEMA.into()),
        })
    }

    fn decode(envelope: &CultCacheEnvelope) -> Result<Self> {
        ensure!(
            envelope.r#type == Self::TYPE
                && envelope.key == DEPLOYMENT_STOP_KEY
                && envelope.schema_id.as_deref() == Some(DEPLOYMENT_STOP_SCHEMA),
            "deployment stop store holds a foreign record"
        );
        let record: Self = rmp_serde::from_slice(&envelope.payload)?;
        ensure!(
            rmp_serde::to_vec(&record)? == envelope.payload,
            "deployment stop is noncanonical"
        );
        record.validate()?;
        Ok(record)
    }
}

/// What one store says about a runtime's deployments.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// No store, or an empty one: nothing halts.
    Absent,
    Clear(DeploymentStop),
    Engaged(DeploymentStop),
    /// Present but not a record this runtime can trust. Does not admit.
    Unreadable(String),
}

impl Stop {
    pub(crate) fn admits(&self) -> bool {
        matches!(self, Self::Absent | Self::Clear(_))
    }
}

#[cfg(unix)]
pub(crate) fn process_uid() -> u32 {
    // SAFETY: geteuid has no failure mode and no arguments.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub(crate) fn process_uid() -> u32 {
    0
}

#[cfg(unix)]
fn login_name(uid: u32) -> Result<String> {
    // SAFETY: getpwuid returns null or a pointer into libc's static buffer,
    // copied before any other call; the verbs are single threaded.
    unsafe {
        let entry = libc::getpwuid(uid);
        ensure!(!entry.is_null(), "uid {uid} has no login name");
        Ok(std::ffi::CStr::from_ptr((*entry).pw_name)
            .to_string_lossy()
            .into_owned())
    }
}

#[cfg(not(unix))]
fn login_name(_: u32) -> Result<String> {
    std::env::var("USERNAME").context("USERNAME is unset")
}

/// Open without following a final symlink and judge the file by fstat before
/// anything is read from it.
#[cfg(unix)]
fn check_file(path: &Path, expected_owner_uid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening deployment stop store {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "deployment stop store is not a regular file"
    );
    ensure!(
        metadata.uid() == expected_owner_uid,
        "deployment stop store is owned by uid {}, not {expected_owner_uid}",
        metadata.uid()
    );
    Ok(())
}

#[cfg(not(unix))]
fn check_file(path: &Path, _: u32) -> Result<()> {
    ensure!(
        std::fs::metadata(path)?.is_file(),
        "deployment stop store is not a regular file"
    );
    Ok(())
}

/// Absent means exactly this: the final component does not exist and its
/// parent is an existing directory. Every other failure to look (permission,
/// a parent that is a file, a symlink loop) is a store this runtime cannot
/// judge, and fails closed.
fn is_absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
            let parent = parent.unwrap_or_else(|| Path::new("."));
            ensure!(
                std::fs::metadata(parent).is_ok_and(|m| m.is_dir()),
                "deployment stop store directory {} does not exist",
                parent.display()
            );
            Ok(true)
        }
        Err(error) => Err(error)
            .with_context(|| format!("looking for deployment stop store {}", path.display())),
    }
}

fn read_checked(path: &Path, runtime_id: &str, owner: u32) -> Result<Option<DeploymentStop>> {
    if is_absent(path)? {
        return Ok(None);
    }
    check_file(path, owner)?;
    ensure!(
        std::fs::metadata(path)?.len() > 0,
        "deployment stop store is zero length"
    );
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    match entries.as_slice() {
        [] => bail!("deployment stop store holds no record"),
        [entry] => {
            let record = DeploymentStop::decode(entry)?;
            ensure!(
                record.runtime_id == runtime_id,
                "deployment stop belongs to runtime {}",
                record.runtime_id
            );
            Ok(Some(record))
        }
        _ => bail!("deployment stop store is ambiguous"),
    }
}

pub(crate) fn read_stop(path: &Path, runtime_id: &str, expected_owner_uid: u32) -> Stop {
    match read_checked(path, runtime_id, expected_owner_uid) {
        Ok(None) => Stop::Absent,
        Ok(Some(record)) if record.state == StopState::Clear => Stop::Clear(record),
        Ok(Some(record)) => Stop::Engaged(record),
        Err(error) => Stop::Unreadable(format!(
            "{error:#}; to replace it run: idunn-provision deployment-brake-clear \
             --store {} --runtime-id {runtime_id} --reason <text> (or deployment-brake-engage)",
            path.display()
        )),
    }
}

/// Write the next record under a compare-exchange of the whole store, so a
/// concurrent verb or a store that moved since it was read is refused. The
/// author is the login name of the process uid and the sequence is the
/// predecessor's plus one (1 when there is none or it cannot be decoded);
/// neither is an argument.
///
/// This is also the operator's break-glass. A predecessor that is a retired v1
/// brake, another runtime's record, ambiguous, or zero length is replaced. A
/// file that is not a CultCache store at all (junk bytes) is replaced only
/// when it is a regular file owned by the caller, by removing it first. A
/// symlink or another user's file is refused. The sequence is not validated
/// beyond being positive: nothing reads it for authority.
pub(crate) fn replace_stop(
    path: &Path,
    runtime_id: &str,
    state: StopState,
    reason: &str,
    now_unix_millis: u64,
) -> Result<DeploymentStop> {
    let store = SingleFileMessagePackBackingStore::new(path);
    let before = match store.pull_all_read_only_snapshot() {
        Ok(entries) => entries,
        Err(undecodable) => {
            check_file(path, process_uid()).with_context(|| {
                format!("deployment stop store is undecodable ({undecodable:#}) and not the caller's to replace")
            })?;
            std::fs::remove_file(path)?;
            Vec::new()
        }
    };
    let sequence = match before.as_slice() {
        [entry] => match DeploymentStop::decode(entry) {
            Ok(previous) => previous.sequence.saturating_add(1),
            Err(_) => 1,
        },
        _ => 1,
    };
    let record = DeploymentStop {
        schema_version: DEPLOYMENT_STOP_SCHEMA.into(),
        runtime_id: runtime_id.into(),
        state,
        by: login_name(process_uid())?,
        reason: reason.into(),
        at_unix_millis: now_unix_millis,
        sequence,
    };
    let next = record.envelope()?;
    ensure!(
        store.compare_exchange_snapshot(&before, &[next])?,
        "deployment stop changed during transition"
    );
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engage(path: &Path, reason: &str, at: u64) -> Result<DeploymentStop> {
        replace_stop(path, "runtime", StopState::Engaged, reason, at)
    }

    #[test]
    fn absent_and_empty_stores_admit_a_clear_one_admits_an_engaged_one_does_not() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stop.cc");
        let me = process_uid();
        assert_eq!(read_stop(&path, "runtime", me), Stop::Absent);
        engage(&path, "freeze for the audit", 100)?;
        let Stop::Engaged(held) = read_stop(&path, "runtime", me) else {
            bail!("an engaged stop did not read as engaged")
        };
        assert_eq!(held.reason, "freeze for the audit");
        assert_eq!(held.by, login_name(me)?);
        assert!(!Stop::Engaged(held).admits());
        replace_stop(&path, "runtime", StopState::Clear, "audit done", 200)?;
        let cleared = read_stop(&path, "runtime", me);
        assert!(matches!(&cleared, Stop::Clear(record) if record.sequence == 2));
        assert!(cleared.admits());
        // Clear is not consumed: the same record admits again.
        assert!(read_stop(&path, "runtime", me).admits());
        Ok(())
    }

    #[test]
    fn corrupt_foreign_other_runtime_symlinked_or_foreign_owned_stores_stop() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let me = process_uid();
        let path = dir.path().join("stop.cc");
        engage(&path, "held", 100)?;
        // Another runtime's record.
        assert!(!read_stop(&path, "other", me).admits());
        // Owned by someone other than the expected uid: even a CLEAR record
        // from a foreign owner must not admit.
        let cleared = dir.path().join("cleared.cc");
        replace_stop(&cleared, "runtime", StopState::Clear, "thaw", 100)?;
        assert!(read_stop(&cleared, "runtime", me).admits());
        assert!(matches!(read_stop(&cleared, "runtime", me + 1), Stop::Unreadable(_)));
        // A symlink to a valid, clear store.
        let real = dir.path().join("real.cc");
        replace_stop(&real, "runtime", StopState::Clear, "clear", 100)?;
        let link = dir.path().join("link.cc");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link)?;
        #[cfg(unix)]
        assert!(matches!(read_stop(&link, "runtime", me), Stop::Unreadable(_)));
        // Bytes that are no store at all.
        let junk = dir.path().join("junk.cc");
        std::fs::write(&junk, b"not msgpack at all")?;
        assert!(matches!(read_stop(&junk, "runtime", me), Stop::Unreadable(_)));
        // A record of another type under another key (the retired v1 brake).
        let foreign = dir.path().join("foreign.cc");
        let store = SingleFileMessagePackBackingStore::new(&foreign);
        let envelope = CultCacheEnvelope {
            key: "deployment".into(),
            r#type: "idunn.deployment_brake".into(),
            payload: vec![0x90],
            stored_at: "1970-01-01T00:00:00+00:00".into(),
            schema_id: Some("idunn.deployment_brake.v1".into()),
        };
        assert!(store.compare_exchange_snapshot(&[], &[envelope])?);
        assert!(matches!(read_stop(&foreign, "runtime", me), Stop::Unreadable(_)));
        // A noncanonical payload under the right identity.
        let mut stop = DeploymentStop::decode(&store_entry(&path)?)?;
        stop.sequence = 9;
        let mut loose = stop.envelope()?;
        loose.payload.push(0xc0);
        let noncanonical = dir.path().join("noncanonical.cc");
        assert!(SingleFileMessagePackBackingStore::new(&noncanonical)
            .compare_exchange_snapshot(&[], &[loose])?);
        assert!(matches!(read_stop(&noncanonical, "runtime", me), Stop::Unreadable(_)));
        Ok(())
    }

    fn store_entry(path: &Path) -> Result<CultCacheEnvelope> {
        let mut entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
        entries.pop().context("no entry")
    }

    #[test]
    fn a_verb_replaces_a_retired_brake_record_and_the_sequence_advances_by_one() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stop.cc");
        let store = SingleFileMessagePackBackingStore::new(&path);
        let retired = CultCacheEnvelope {
            key: "deployment".into(),
            r#type: "idunn.deployment_brake".into(),
            payload: vec![0x90],
            stored_at: "1970-01-01T00:00:00+00:00".into(),
            schema_id: Some("idunn.deployment_brake.v1".into()),
        };
        assert!(store.compare_exchange_snapshot(&[], &[retired])?);
        assert_eq!(engage(&path, "first", 10)?.sequence, 1);
        assert_eq!(store.pull_all_read_only_snapshot()?.len(), 1);
        assert_eq!(engage(&path, "second", 11)?.sequence, 2);
        assert_eq!(
            replace_stop(&path, "runtime", StopState::Clear, "third", 12)?.sequence,
            3
        );
        assert_eq!(DeploymentStop::decode(&store_entry(&path)?)?.reason, "third");
        Ok(())
    }

    #[test]
    fn a_stale_exchange_is_refused_and_a_record_without_a_reason_is_not_written() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stop.cc");
        engage(&path, "held", 10)?;
        let store = SingleFileMessagePackBackingStore::new(&path);
        let stale = store.pull_all_read_only_snapshot()?;
        engage(&path, "moved", 11)?;
        let next = DeploymentStop {
            schema_version: DEPLOYMENT_STOP_SCHEMA.into(),
            runtime_id: "runtime".into(),
            state: StopState::Clear,
            by: "someone".into(),
            reason: "late".into(),
            at_unix_millis: 12,
            sequence: 2,
        }
        .envelope()?;
        assert!(!store.compare_exchange_snapshot(&stale, &[next])?);
        assert!(replace_stop(&path, "runtime", StopState::Clear, "  ", 13).is_err());
        assert_eq!(DeploymentStop::decode(&store_entry(&path)?)?.reason, "moved");
        Ok(())
    }

    #[test]
    fn only_a_missing_file_in_an_existing_directory_is_absent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let me = process_uid();
        // Parent is a regular file: ENOTDIR, not absence.
        let file = dir.path().join("plain");
        std::fs::write(&file, b"x")?;
        assert!(matches!(read_stop(&file.join("stop.cc"), "runtime", me), Stop::Unreadable(_)));
        // Parent does not exist.
        assert!(matches!(
            read_stop(&dir.path().join("nope/stop.cc"), "runtime", me),
            Stop::Unreadable(_)
        ));
        // Zero-length file.
        let empty = dir.path().join("empty.cc");
        std::fs::write(&empty, b"")?;
        assert!(matches!(read_stop(&empty, "runtime", me), Stop::Unreadable(_)));
        // An existing directory with no store is absent.
        assert_eq!(read_stop(&dir.path().join("stop.cc"), "runtime", me), Stop::Absent);
        // A parent that cannot be searched (skipped as root, which can).
        #[cfg(unix)]
        if me != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked)?;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;
            let reading = read_stop(&locked.join("stop.cc"), "runtime", me);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))?;
            assert!(matches!(reading, Stop::Unreadable(_)), "{reading:?}");
        }
        Ok(())
    }

    #[test]
    fn the_author_is_the_uid_login_never_the_environment() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("stop.cc");
        // SAFETY: no other test reads USER, LOGNAME or LNAME.
        unsafe {
            std::env::set_var("USER", "spoof");
            std::env::set_var("LOGNAME", "spoof");
        }
        let written = engage(&path, "held", 10)?;
        assert_eq!(written.by, login_name(process_uid())?);
        assert_ne!(written.by, "spoof");
        Ok(())
    }

    #[test]
    fn unreadable_names_the_command_and_a_break_glass_clear_replaces_junk_and_v1() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let me = process_uid();
        let path = dir.path().join("stop.cc");
        std::fs::write(&path, b"junk that is no store")?;
        let Stop::Unreadable(why) = read_stop(&path, "runtime", me) else {
            bail!("junk admitted")
        };
        assert!(why.contains("idunn-provision deployment-brake-clear --store"), "{why}");
        assert!(why.contains("--runtime-id runtime"), "{why}");
        let cleared = replace_stop(&path, "runtime", StopState::Clear, "operator break-glass", 50)?;
        assert_eq!(cleared.sequence, 1);
        assert!(read_stop(&path, "runtime", me).admits());
        // A symlink is not the caller's to replace.
        let real = dir.path().join("real.cc");
        std::fs::write(&real, b"junk")?;
        let link = dir.path().join("link.cc");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link)?;
            assert!(replace_stop(&link, "runtime", StopState::Clear, "x", 60).is_err());
            assert_eq!(std::fs::read(&real)?, b"junk");
        }
        Ok(())
    }
}
