//! idunn-launch: the unit's process for an upstream daemon that cannot speak
//! Idunn's runtime contract itself (Synapse, MAS).
//!
//! Idunn observes the unit; the launcher, inside it and supervising the child,
//! signs the presence statement with the provider identity and the activation
//! key. The launcher binds the candidate endpoint Idunn challenges, because the
//! child cannot answer a CultNet frame, and answers presence there and nowhere
//! else: members reach the child on the member bind Idunn routes them to. It
//! starts a stateful child only after it holds the process write lease, renders
//! the child's config from a closed set of placeholders, and ends with the
//! child.
//!
//! Errors, logs and refusals name a field and a code. They never echo an
//! argument, a template line, a path or a credential.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{DatabaseEntry, SingleFileMessagePackBackingStore};
use cultnet_rs::{
    CultNetMessage, CultNetRawDocumentRecord, CultNetRawPayloadEncoding,
    CultNetRudpReliableSendStatus, CultNetRudpSocketTransportConnection,
    CultNetRudpSocketTransportOptions, CultNetWireContract,
    GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA, GameCultProviderHealthIdentity,
    GameCultRuntimeCapability, GameCultRuntimePresenceHealthPurpose,
    GameCultRuntimePresenceHealthRecord, IDUNN_EXPECTED_INCARNATION_SCHEMA,
    IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME, IDUNN_RUNTIME_ACTIVATION_SCHEMA,
    IdunnExpectedIncarnationRecord, IdunnProcessWriteLeaseRecord, IdunnRuntimeActivationRecord,
    IdunnRuntimeActivationSigner, ServiceIdentitySigner, decode_cultnet_message_from_slice,
    encode_cultnet_message_to_vec, encode_frame, open_service_identity_credential_reader,
};

use crate::deployment::{RUNTIME_PRESENCE_IDENTITY_FD_NAME, SYSTEMD_CREDENTIALS_DIRECTORY_ENVIRONMENT};
use crate::drivers::{CultCacheWriteLeaseDriver, authority_lock_path, lease_binds_candidate};

/// The route challenge is the only thing that begins with this byte: a
/// CultNet frame's 4-byte big-endian length is bounded at 1 MiB, and an HTTP
/// request starts with a method letter.
const CHALLENGE_FIRST_BYTE: u8 = 0x00;
const CHALLENGE_MAX_BYTES: usize = 1024 * 1024;
/// One whole presence exchange on the candidate endpoint, from accept to
/// answer, however the peer paces it. It is shorter than the 3 s Idunn waits
/// for a challenge answer, so one stalled peer cannot cost the next challenge
/// its reply.
const PRESENCE_DEADLINE: Duration = Duration::from_secs(1);
/// How long the presence acceptor waits after an accept error before trying
/// again. No accept error ends the acceptor.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How often the lifecycle probes the child once it is Active.
const HEALTH_REFRESH: Duration = Duration::from_millis(500);
/// A probe result older than this is not evidence of health: the lifecycle
/// that refreshes it is itself stuck.
const HEALTH_STALE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);
const MAX_RECENT_WARMING_PROOFS: usize = 64;
const TERMINATE_GRACE: Duration = Duration::from_secs(20);
const ODIN_CULTMESH_CATALOG_CONNECTION_ID: u32 = 0x0d1d_0002;
const ODIN_CAPABILITY: &str = "odin.verse-rendezvous";

// ---------------------------------------------------------------- contract

/// Everything the recipe declares about the child, parsed from the launcher's
/// argv (`--flag value` pairs; `--child-arg` repeats). The release root is the
/// sealed release directory that holds the launcher itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchContract {
    pub release_root: PathBuf,
    pub child: PathBuf,
    pub child_args: Vec<String>,
    /// The flag that carries the rendered config path to the child, when the
    /// child takes one.
    pub config_flag: Option<String>,
    pub template: PathBuf,
    pub health_path: String,
    pub health_contract: String,
    pub state_root: Option<PathBuf>,
    pub lease_wait: Duration,
    pub startup: Duration,
    pub heartbeat: Duration,
}

impl LaunchContract {
    pub fn parse(release_root: PathBuf, args: impl IntoIterator<Item = String>) -> Result<Self> {
        ensure!(release_root.is_absolute(), "the release root is not absolute");
        let mut child = None;
        let mut child_args = Vec::new();
        let mut config_flag = None;
        let mut template = None;
        let mut health_path = None;
        let mut health_contract = None;
        let mut state_root = None;
        let (mut lease_wait, mut startup, mut heartbeat) = (300_u64, 120_u64, 5_u64);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args.next().context("launcher argument has no value")?;
            let seconds = |field: &str| -> Result<u64> {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .with_context(|| format!("launcher argument {field} is not a positive integer"))
            };
            match flag.as_str() {
                "--child" => child = Some(release_relative(&value, "--child")?),
                "--child-arg" => child_args.push(value),
                "--config-flag" => config_flag = Some(value),
                "--template" => template = Some(release_relative(&value, "--template")?),
                "--health-path" => {
                    ensure!(
                        value.starts_with('/'),
                        "launcher argument --health-path is not absolute"
                    );
                    health_path = Some(value)
                }
                "--health-contract" => health_contract = Some(value),
                "--state-root" => state_root = Some(PathBuf::from(value)),
                "--lease-wait-secs" => lease_wait = seconds("--lease-wait-secs")?,
                "--startup-secs" => startup = seconds("--startup-secs")?,
                "--heartbeat-secs" => heartbeat = seconds("--heartbeat-secs")?,
                _ => bail!("unknown launcher argument"),
            }
        }
        Ok(Self {
            release_root,
            child: child.context("launcher argument --child is required")?,
            child_args,
            config_flag,
            template: template.context("launcher argument --template is required")?,
            health_path: health_path.context("launcher argument --health-path is required")?,
            health_contract: health_contract
                .context("launcher argument --health-contract is required")?,
            state_root,
            lease_wait: Duration::from_secs(lease_wait),
            startup: Duration::from_secs(startup),
            heartbeat: Duration::from_secs(heartbeat),
        })
    }
}

fn release_relative(value: &str, field: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    ensure!(
        !value.is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "launcher argument {field} is not release-relative"
    );
    Ok(path)
}

// ---------------------------------------------------------------- authority

/// The bundle, activation credential and provider identity Idunn hands the
/// unit, and the two signatures every presence statement needs.
pub struct RuntimeAuthority {
    expected: IdunnExpectedIncarnationRecord,
    expected_sha256: String,
    activation: IdunnRuntimeActivationRecord,
    activation_sha256: String,
    activation_signer: IdunnRuntimeActivationSigner,
    provider_signer: ServiceIdentitySigner<GameCultProviderHealthIdentity>,
    sequence: u64,
}

impl RuntimeAuthority {
    pub fn open(
        bundle: &Path,
        activation_credential: impl Read,
        provider_credential: impl Read,
    ) -> Result<Self> {
        ensure!(bundle.is_absolute(), "Idunn runtime bundle is not absolute");
        let (key, payload) = read_single_record(
            &bundle.join("expected.cc"),
            IdunnExpectedIncarnationRecord::TYPE,
            IDUNN_EXPECTED_INCARNATION_SCHEMA,
        )?;
        let expected = IdunnExpectedIncarnationRecord::decode_canonical(&payload)?;
        expected.validate()?;
        ensure!(key == expected.target, "Expected key is substituted");
        let (key, payload) = read_single_record(
            &bundle.join("activation.cc"),
            IdunnRuntimeActivationRecord::TYPE,
            IDUNN_RUNTIME_ACTIVATION_SCHEMA,
        )?;
        let activation = IdunnRuntimeActivationRecord::decode_canonical(&payload)?;
        activation.validate()?;
        ensure!(key == expected.target, "activation key is substituted");
        let expected_sha256 = expected.canonical_sha256()?;
        ensure!(
            activation.expected_projection_sha256 == expected_sha256
                && activation.runtime_id == expected.runtime_id,
            "activation does not bind this Expected projection"
        );
        let activation_signer = IdunnRuntimeActivationSigner::from_credential_reader(
            activation_credential,
        )?;
        ensure!(
            activation_signer.identity_id() == activation.activation_signer_identity_id
                && activation_signer.public_key() == activation.activation_signer_public_key,
            "activation credential does not belong to this activation"
        );
        let provider_signer =
            open_service_identity_credential_reader::<GameCultProviderHealthIdentity>(
                provider_credential,
            )?;
        ensure!(
            provider_signer.entry().identity_id == expected.expected_signer_identity_id,
            "provider identity is not the signer selected by Expected"
        );
        Ok(Self {
            activation_sha256: activation.canonical_sha256()?,
            expected,
            expected_sha256,
            activation,
            activation_signer,
            provider_signer,
            sequence: 0,
        })
    }

    pub fn expected(&self) -> &IdunnExpectedIncarnationRecord {
        &self.expected
    }

    pub fn activation(&self) -> &IdunnRuntimeActivationRecord {
        &self.activation
    }

    /// The candidate endpoint Idunn will challenge, which the launcher binds.
    pub fn require_candidate_bind(&self, bind: &str) -> Result<()> {
        let route = self.expected.route.as_ref().context("Expected has no route")?;
        ensure!(
            route.transport == "tcp" && route.candidate_endpoint == format!("tcp://{bind}"),
            "candidate bind differs from the Expected route"
        );
        Ok(())
    }

    /// The next presence record, signed by the provider identity and the
    /// activation. The sequence is the clock: strictly increasing here and
    /// above anything an earlier incarnation of this signer published.
    pub fn sign(
        &mut self,
        state: &str,
        detail: &str,
        write_lease_sha256: Option<String>,
        now_millis: u64,
    ) -> Result<GameCultRuntimePresenceHealthRecord> {
        self.sequence = now_millis.max(
            self.sequence
                .checked_add(1)
                .ok_or_else(|| anyhow!("runtime presence sequence exhausted"))?,
        );
        let expected = &self.expected;
        let mut record = GameCultRuntimePresenceHealthRecord {
            schema_version: GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into(),
            target: expected.target.clone(),
            expected_projection_sha256: self.expected_sha256.clone(),
            plan_id: expected.plan_id.clone(),
            incarnation_id: expected.incarnation_id.clone(),
            sealed_release_id: expected.sealed_release_id.clone(),
            activation_witness_sha256: self.activation_sha256.clone(),
            state_schema_generation: expected.state_schema_generation.clone(),
            state_contract_sha256: expected.state_contract_sha256.clone(),
            runtime_id: expected.runtime_id.clone(),
            runtime_instance_id: self.activation.runtime_instance_id.clone(),
            bound_endpoint: expected
                .route
                .as_ref()
                .map(|route| route.candidate_endpoint.clone()),
            capabilities: expected
                .capabilities
                .iter()
                .map(|capability| GameCultRuntimeCapability {
                    capability: capability.capability.clone(),
                    schema: capability.schema.clone(),
                    compatibility: capability.compatibility.clone(),
                    capacity: capability.minimum_capacity,
                })
                .collect(),
            health_contract: expected.health_contract.clone(),
            state: state.into(),
            detail: detail.into(),
            write_lease_sha256,
            signer_identity_id: self.provider_signer.entry().identity_id.clone(),
            publisher_sequence: self.sequence,
            observed_at_unix_millis: now_millis,
            signature_algorithm: "ed25519".into(),
            signature: Vec::new(),
            activation_signer_identity_id: self.activation.activation_signer_identity_id.clone(),
            activation_signature: Vec::new(),
        };
        let payload = record.canonical_proof_payload()?;
        record.signature = self
            .provider_signer
            .sign::<GameCultRuntimePresenceHealthPurpose>(&payload)
            .signature;
        record.activation_signature = self.activation_signer.sign_presence_proof(&record)?;
        record.validate()?;
        Ok(record)
    }
}

fn read_single_record(path: &Path, record_type: &str, schema: &str) -> Result<(String, Vec<u8>)> {
    let entries = SingleFileMessagePackBackingStore::new(path)
        .pull_all_read_only_snapshot()
        .context("reading a runtime bundle record")?;
    let [envelope] = entries.as_slice() else {
        bail!("runtime bundle record does not hold exactly one entry");
    };
    ensure!(
        envelope.r#type == record_type && envelope.schema_id.as_deref() == Some(schema),
        "runtime bundle record has the wrong type or schema"
    );
    Ok((envelope.key.clone(), envelope.payload.clone()))
}

/// The two authority descriptors systemd opens for the unit, in the order and
/// under the names Idunn's launch contract fixes.
pub fn inherited_authority_files() -> Result<(File, File)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let fds = std::env::var("LISTEN_FDS").context("systemd did not bind authority FDs")?;
    let names = std::env::var("LISTEN_FDNAMES").context("systemd did not name authority FDs")?;
    ensure!(
        fds == "2"
            && names
                == format!(
                    "{IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME}:{RUNTIME_PRESENCE_IDENTITY_FD_NAME}"
                ),
        "systemd authority FDs differ from the launch contract"
    );
    for fd in [3, 4] {
        // SAFETY: F_GETFD observes one integer descriptor and transfers nothing.
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1,
            "an authority descriptor is not open"
        );
    }
    // SAFETY: the LISTEN_* contract above assigns these two distinct, open
    // descriptors to this process; this is their first and only consumer.
    let (activation, provider) = unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) };
    for (file, exact) in [(&activation, Some(32_u64)), (&provider, None)] {
        // OpenFile descriptors arrive without close-on-exec; the child must
        // never inherit a signer.
        // SAFETY: F_SETFD on a descriptor `file` owns.
        ensure!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != -1,
            "an authority descriptor could not be sealed"
        );
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == 0
                && metadata.permissions().mode() & 0o777 == 0o400
                && metadata.nlink() == 1
                && exact.map_or(metadata.len() > 0, |size| metadata.len() == size),
            "an authority descriptor is not one root-owned 0400 file"
        );
    }
    Ok((activation, provider))
}

// ---------------------------------------------------------------- ports

pub trait Clock: Send + Sync {
    fn now_millis(&self) -> u64;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Systemd credentials as Idunn binds them: the files `NAME` under the unit's
/// credentials directory.
pub trait CredentialSource: Send + Sync {
    fn contents(&self, name: &str) -> Result<Vec<u8>>;
    fn path(&self, name: &str) -> Result<PathBuf>;
}

/// Resolves a credential only as a file directly under the credentials
/// directory systemd gives the unit (`$CREDENTIALS_DIRECTORY`, which Idunn
/// pins to `/run/credentials/<unit>`). The process environment is never a
/// credential namespace: a name is a plain file name, never a variable.
pub struct CredentialsDirectory(Option<PathBuf>);

impl CredentialsDirectory {
    pub fn new(directory: PathBuf) -> Self {
        Self(Some(directory))
    }

    /// The directory systemd exported; no directory means no credential is bound.
    pub fn from_environment() -> Self {
        Self(std::env::var_os(SYSTEMD_CREDENTIALS_DIRECTORY_ENVIRONMENT).map(PathBuf::from))
    }
}

impl CredentialSource for CredentialsDirectory {
    fn contents(&self, name: &str) -> Result<Vec<u8>> {
        fs::read(self.path(name)?).map_err(|_| anyhow!("a credential could not be read"))
    }

    fn path(&self, name: &str) -> Result<PathBuf> {
        ensure!(valid_credential_name(name), "credential name is invalid");
        let path = self
            .0
            .as_ref()
            .ok_or_else(|| anyhow!("a credential is not bound"))?
            .join(name);
        ensure!(fs::symlink_metadata(&path).is_ok(), "a credential is not bound");
        Ok(path)
    }
}

/// Where signed presence goes (Odin).
pub trait PresenceSink: Send + Sync {
    fn put(&self, record: &GameCultRuntimePresenceHealthRecord) -> Result<()>;
}

/// The lease as the launcher sees it: the current record, and a shared lock on
/// its store that Idunn's revocation cannot take while the child lives.
pub trait LeaseReader: Send + Sync {
    fn read(&self) -> Result<Option<IdunnProcessWriteLeaseRecord>>;
    fn hold_shared(&self) -> Result<Box<dyn LeaseLock>>;
}

/// The shared lock on the lease store, held until dropped. Its descriptor is
/// handed to the child (`ChildSpawner::spawn`), so the lock lives as long as
/// the launcher or any process that inherited it.
pub trait LeaseLock: Send {
    fn descriptor(&self) -> Option<RawFd>;
}

pub trait HealthProbe: Send + Sync {
    fn healthy(&self) -> bool;
}

pub trait ChildHandle: Send {
    /// The exit status once the child has exited.
    fn exited(&mut self) -> Option<i32>;
    /// SIGTERM, then SIGKILL after the grace period; returns once it is gone.
    fn terminate(&mut self, grace: Duration);
}

pub trait ChildSpawner: Send {
    /// `inherit` is a descriptor the child keeps open across exec (the lease
    /// lock); every other descriptor of the launcher is close-on-exec.
    fn spawn(
        &self,
        program: &Path,
        args: &[String],
        inherit: Option<RawFd>,
    ) -> Result<Box<dyn ChildHandle>>;
}

// ---------------------------------------------------------------- production ports

pub struct DriverLeaseReader {
    driver: CultCacheWriteLeaseDriver,
}

impl DriverLeaseReader {
    pub fn new(target: &str, record_path: &Path) -> Self {
        Self {
            driver: CultCacheWriteLeaseDriver::new(target, record_path),
        }
    }
}

struct SharedLockFile(File);

impl LeaseLock for SharedLockFile {
    fn descriptor(&self) -> Option<RawFd> {
        Some(self.0.as_raw_fd())
    }
}

impl LeaseReader for DriverLeaseReader {
    fn read(&self) -> Result<Option<IdunnProcessWriteLeaseRecord>> {
        self.driver
            .current_for_holder()
            .map(|current| current.map(|(_, lease)| lease))
            .map_err(|_| anyhow!("write-lease store refused custody, envelope or target"))
    }

    /// LOCK_SH on `<lease>.lock`, the lock Idunn's revocation respects. The
    /// descriptor is a plain file, so flock's lock belongs to its open file
    /// description: a child that inherits it keeps the lock after the launcher
    /// dies.
    fn hold_shared(&self) -> Result<Box<dyn LeaseLock>> {
        let file = File::open(authority_lock_path(&self.driver.record_path)).map_err(|_| anyhow!("write-lease lock could not be held"))?;
        // SAFETY: flock on a descriptor this function owns.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } == 0,
            "write-lease lock could not be held"
        );
        Ok(Box::new(SharedLockFile(file)))
    }
}

/// Odin takes presence over RUDP at the endpoint Expected's Odin dependency
/// names.
pub struct OdinRudpSink {
    endpoint: SocketAddr,
}

impl OdinRudpSink {
    pub fn from_expected(expected: &IdunnExpectedIncarnationRecord) -> Result<Self> {
        let dependency = expected
            .dependencies
            .iter()
            .find(|dependency| dependency.capability == ODIN_CAPABILITY)
            .context("Expected has no Odin dependency")?;
        let endpoint = dependency
            .provider_endpoint
            .as_deref()
            .context("Expected Odin dependency has no endpoint")?;
        let endpoint = endpoint.strip_prefix("rudp://").unwrap_or(endpoint);
        Ok(Self {
            endpoint: endpoint
                .parse()
                .map_err(|_| anyhow!("Expected Odin endpoint is not a socket address"))?,
        })
    }
}

impl PresenceSink for OdinRudpSink {
    fn put(&self, record: &GameCultRuntimePresenceHealthRecord) -> Result<()> {
        let message = CultNetMessage::DocumentPutRaw {
            message_id: format!(
                "runtime-presence:{}:{}:{}",
                record.target, record.runtime_instance_id, record.publisher_sequence
            ),
            document: presence_document(
                record,
                Some(vec![
                    "cultnet.transport.rudp.v0".into(),
                    "runtime-presence".into(),
                ]),
            )?,
        };
        let bind = if self.endpoint.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        let mut transport = CultNetRudpSocketTransportConnection::new(
            CultNetRudpSocketTransportOptions::client(
                &record.runtime_id,
                socket,
                self.endpoint,
                ODIN_CULTMESH_CATALOG_CONNECTION_ID,
            ),
        )?;
        transport.connect(Vec::new())?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while !transport.connected() {
            let _ = transport.receive_once()?;
            transport.poll_resends()?;
            ensure!(Instant::now() < deadline, "Odin did not accept the connection");
        }
        let receipt = transport.send_reliable(
            "schema",
            encode_cultnet_message_to_vec(&message, CultNetWireContract::CultNetSchemaV0)?,
        )?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match transport.reliable_send_status(&receipt) {
                CultNetRudpReliableSendStatus::Acknowledged => return Ok(()),
                CultNetRudpReliableSendStatus::Invalidated => {
                    bail!("Odin invalidated the presence acknowledgement")
                }
                CultNetRudpReliableSendStatus::Pending => {}
            }
            let _ = transport.receive_once()?;
            transport.poll_resends()?;
            ensure!(Instant::now() < deadline, "Odin did not acknowledge presence");
        }
    }
}

fn presence_document(
    record: &GameCultRuntimePresenceHealthRecord,
    tags: Option<Vec<String>>,
) -> Result<CultNetRawDocumentRecord> {
    let payload = rmp_serde::to_vec(record).context("encoding runtime presence")?;
    Ok(CultNetRawDocumentRecord {
        schema_id: GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into(),
        record_key: record.target.clone(),
        stored_at: chrono::DateTime::from_timestamp_millis(
            record.observed_at_unix_millis.try_into()?,
        )
        .context("runtime presence timestamp is invalid")?
        .to_rfc3339(),
        payload_encoding: CultNetRawPayloadEncoding::Messagepack,
        payload,
        source_runtime_id: Some(record.runtime_id.clone()),
        source_agent_id: Some(record.signer_identity_id.clone()),
        source_role: Some("runtime-presence-publisher".into()),
        tags,
    })
}

/// `GET <path>` over TCP at the child's member bind; healthy on a 200. One
/// deadline covers connect, write and read, so a full backlog or a silent child
/// answers false within `HEALTH_PROBE_TIMEOUT`.
pub struct HttpHealthProbe {
    pub member: SocketAddr,
    pub path: String,
}

impl HealthProbe for HttpHealthProbe {
    fn healthy(&self) -> bool {
        let deadline = Instant::now() + HEALTH_PROBE_TIMEOUT;
        let Ok(mut stream) = TcpStream::connect_timeout(&self.member, HEALTH_PROBE_TIMEOUT) else {
            return false;
        };
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            return false;
        };
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            self.path
        );
        let mut head = [0_u8; 12];
        stream.set_write_timeout(Some(remaining)).is_ok()
            && stream.write_all(request.as_bytes()).is_ok()
            && read_by(&mut stream, &mut head, deadline).is_ok()
            && head.starts_with(b"HTTP/1.")
            && &head[8..12] == b" 200"
    }
}

pub struct ProcessSpawner;

struct ProcessChild(Child);

impl ChildSpawner for ProcessSpawner {
    fn spawn(
        &self,
        program: &Path,
        args: &[String],
        inherit: Option<RawFd>,
    ) -> Result<Box<dyn ChildHandle>> {
        let mut command = Command::new(program);
        // The child gets a bare environment: no Idunn variable, no candidate
        // bind, no credential path leaks into it.
        command.args(args).env_clear().stdin(Stdio::null());
        for name in ["PATH", "TMPDIR", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(descriptor) = inherit {
            // SAFETY: runs in the forked child before exec; fcntl is
            // async-signal-safe and the parent's descriptor stays close-on-exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(descriptor, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().map_err(|_| anyhow!("the child could not be spawned"))?;
        Ok(Box::new(ProcessChild(child)))
    }
}

impl ChildHandle for ProcessChild {
    fn exited(&mut self) -> Option<i32> {
        match self.0.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(1)),
            Ok(None) => None,
            Err(_) => Some(1),
        }
    }

    fn terminate(&mut self, grace: Duration) {
        if self.exited().is_some() {
            return;
        }
        // SAFETY: the pid is our own live child.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.exited().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------- config

/// The closed placeholder set (decision D6). Any other `{{...}}` refuses.
pub struct ConfigRenderer<'a> {
    pub credentials: &'a dyn CredentialSource,
    /// The member bind the child serves on.
    pub listen: SocketAddr,
    pub state_root: Option<&'a Path>,
}

impl ConfigRenderer<'_> {
    pub fn render(&self, template: &str) -> Result<String> {
        let mut rendered = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find("{{") {
            rendered.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            let close = after
                .find("}}")
                .context("config template has an unterminated placeholder")?;
            rendered.push_str(&self.expand(&after[..close])?);
            rest = &after[close + 2..];
        }
        rendered.push_str(rest);
        Ok(rendered)
    }

    fn expand(&self, key: &str) -> Result<String> {
        match key {
            "child.listen" => return Ok(self.listen.to_string()),
            "child.listen_host" => return Ok(self.listen.ip().to_string()),
            "child.listen_port" => return Ok(self.listen.port().to_string()),
            _ => {}
        }
        if key == "state_root" {
            return Ok(self
                .state_root
                .context("config template needs a state root the launch did not give")?
                .display()
                .to_string());
        }
        if let Some(name) = key.strip_prefix("credential:") {
            ensure!(valid_credential_name(name), "credential placeholder name is invalid");
            let bytes = self.credentials.contents(name)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| anyhow!("a credential is not UTF-8 text"))?;
            return Ok(text.strip_suffix('\n').unwrap_or(&text).to_owned());
        }
        if let Some(name) = key.strip_prefix("credential_path:") {
            ensure!(valid_credential_name(name), "credential placeholder name is invalid");
            return Ok(self.credentials.path(name)?.display().to_string());
        }
        bail!("config template uses a placeholder outside the closed set")
    }
}

fn valid_credential_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn write_private_file(directory: &Path, name: &str, contents: &str) -> Result<PathBuf> {
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|_| anyhow!("the rendered config could not be created"))?;
    file.write_all(contents.as_bytes())?;
    Ok(path)
}

// ---------------------------------------------------------------- lease

/// A write lease this launcher holds: the record binds this Expected,
/// activation and one of the launcher's own Warming proofs, and its store is
/// share-locked until the hold drops.
pub struct LeaseHold {
    reader: Arc<dyn LeaseReader>,
    lease: IdunnProcessWriteLeaseRecord,
    sha256: String,
    lock: Box<dyn LeaseLock>,
}

impl LeaseHold {
    pub fn acquire(
        reader: Arc<dyn LeaseReader>,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming_proofs: &[String],
    ) -> Result<Self> {
        let lease = reader.read()?.context("no write lease is granted")?;
        lease.validate()?;
        lease_binds_candidate(expected, activation, &lease, warming_proofs)?;
        let lock = reader.hold_shared()?;
        ensure!(
            reader.read()?.as_ref() == Some(&lease),
            "write lease changed while it was being held"
        );
        Ok(Self {
            sha256: lease.canonical_sha256()?,
            reader,
            lease,
            lock,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The locked descriptor, for the child to inherit.
    pub fn lock_descriptor(&self) -> Option<RawFd> {
        self.lock.descriptor()
    }

    pub fn still_current(&self) -> bool {
        matches!(self.reader.read(), Ok(Some(current)) if current == self.lease)
    }
}

// ---------------------------------------------------------------- route

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Warming,
    Active(Option<String>),
    Failed,
}

/// What the launcher currently is, shared with the presence acceptor.
struct Shared {
    authority: Mutex<RuntimeAuthority>,
    phase: Mutex<Phase>,
    lease: Mutex<Option<LeaseHold>>,
    lease_required: bool,
    health: Arc<dyn HealthProbe>,
    /// The last probe result and the clock time it started. Only the lifecycle
    /// probes; a challenge reads this and never waits on the child.
    probed: Mutex<Option<(bool, u64)>>,
    clock: Arc<dyn Clock>,
}

impl Shared {
    /// Probes the child now and keeps the result for presence to answer from.
    fn refresh_health(&self) -> bool {
        let started = self.clock.now_millis();
        let healthy = self.health.healthy();
        if let Ok(mut probed) = self.probed.lock() {
            *probed = Some((healthy, started));
        }
        healthy
    }

    /// Active only while the last probe found the child healthy and is recent.
    /// A child never probed, wedged, or probed too long ago reads degraded. A
    /// dead presence acceptor does not degrade presence: it fails the unit.
    fn serving_state(&self) -> &'static str {
        let now = self.clock.now_millis();
        let stale = u64::try_from(HEALTH_STALE.as_millis()).unwrap_or(u64::MAX);
        let healthy = self
            .probed
            .lock()
            .ok()
            .and_then(|probed| *probed)
            .is_some_and(|(healthy, at)| healthy && now.saturating_sub(at) <= stale);
        if healthy { "active" } else { "degraded" }
    }

    fn sign(&self, state: &str, detail: &str, lease: Option<String>) -> Result<GameCultRuntimePresenceHealthRecord> {
        self.authority
            .lock()
            .map_err(|_| anyhow!("authority lock poisoned"))?
            .sign(state, detail, lease, self.clock.now_millis())
    }

    fn lease_current(&self) -> bool {
        !self.lease_required
            || self
                .lease
                .lock()
                .is_ok_and(|hold| hold.as_ref().is_some_and(LeaseHold::still_current))
    }

    /// The answer to a route observation: active only if the child is healthy
    /// and the lease is still ours.
    fn observe(&self, request: CultNetMessage) -> Result<CultNetMessage> {
        let CultNetMessage::SnapshotRequest { message_id, .. } = request else {
            bail!("route challenge is not a snapshot request");
        };
        let detail = format!("route-observation:{message_id}");
        let phase = self.phase.lock().map_err(|_| anyhow!("phase lock poisoned"))?.clone();
        let record = match phase {
            Phase::Warming => self.sign("warming", &detail, None)?,
            Phase::Failed => self.sign("failed", &detail, None)?,
            Phase::Active(lease) => {
                let state = if self.lease_current() {
                    self.serving_state()
                } else {
                    "degraded"
                };
                self.sign(state, &detail, lease)?
            }
        };
        Ok(CultNetMessage::SnapshotResponseRaw {
            message_id,
            documents: vec![presence_document(&record, None)?],
        })
    }
}

type Responder = Arc<dyn Fn(CultNetMessage) -> Result<CultNetMessage> + Send + Sync>;

/// Owns the candidate endpoint and answers presence there and nowhere else.
/// Members never reach it: Idunn routes them to the child's member bind.
///
/// The endpoint is public, so it spends bounded resources on a peer: one
/// connection at a time, so a connection holds at most one descriptor; a total
/// deadline for the whole exchange; and any first byte but a challenge's closes
/// the connection unanswered. No accept error ends it: it backs off and accepts
/// again, and only `halt` stops it.
pub struct PresenceAcceptor {
    listener: TcpListener,
    responder: Responder,
}

impl PresenceAcceptor {
    pub fn new(listener: TcpListener, responder: Responder) -> Self {
        Self { listener, responder }
    }

    pub fn serve(self, halt: Arc<AtomicBool>) {
        self.serve_accepting(halt, |listener| listener.accept());
    }

    fn serve_accepting(
        self,
        halt: Arc<AtomicBool>,
        mut accept: impl FnMut(&TcpListener) -> io::Result<(TcpStream, SocketAddr)>,
    ) {
        // A listener that cannot be made non-blocking could never observe
        // `halt`; ending here is a dead acceptor, which fails the unit.
        if self.listener.set_nonblocking(true).is_err() {
            return;
        }
        while !halt.load(Ordering::SeqCst) {
            match accept(&self.listener) {
                Ok((connection, _)) => {
                    let _ = connection.set_nonblocking(false);
                    let _ = self.answer(connection);
                }
                // An idle non-blocking accept is an error too; every one waits
                // and tries again.
                Err(_) => thread::sleep(ACCEPT_BACKOFF),
            }
        }
    }

    fn answer(&self, mut connection: TcpStream) -> Result<()> {
        let deadline = Instant::now() + PRESENCE_DEADLINE;
        let mut first = [0_u8; 1];
        read_by(&mut connection, &mut first, deadline)?;
        ensure!(first[0] == CHALLENGE_FIRST_BYTE, "not a challenge frame");
        let mut rest = [0_u8; 3];
        read_by(&mut connection, &mut rest, deadline)?;
        let length = u32::from_be_bytes([CHALLENGE_FIRST_BYTE, rest[0], rest[1], rest[2]]) as usize;
        ensure!(
            (1..=CHALLENGE_MAX_BYTES).contains(&length),
            "challenge frame is outside its bound"
        );
        let mut payload = vec![0_u8; length];
        read_by(&mut connection, &mut payload, deadline)?;
        let request =
            decode_cultnet_message_from_slice(&payload, CultNetWireContract::CultNetSchemaV0)?;
        let response = (self.responder)(request)?;
        let frame = encode_frame(&encode_cultnet_message_to_vec(
            &response,
            CultNetWireContract::CultNetSchemaV0,
        )?)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .context("the answer was not ready in time")?;
        connection.set_write_timeout(Some(remaining))?;
        connection.write_all(&frame)?;
        connection.flush()?;
        Ok(())
    }
}

/// Fills `buffer` or fails when `deadline` passes, however the peer paces it.
fn read_by(connection: &mut TcpStream, buffer: &mut [u8], deadline: Instant) -> Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .context("the peer did not send in time")?;
        connection.set_read_timeout(Some(remaining))?;
        match connection.read(&mut buffer[filled..]) {
            Ok(0) => bail!("the peer closed early"),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => bail!("the peer did not send in time"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- lifecycle

pub struct Ports {
    pub credentials: Arc<dyn CredentialSource>,
    pub clock: Arc<dyn Clock>,
    pub sink: Arc<dyn PresenceSink>,
    pub lease: Arc<dyn LeaseReader>,
    pub spawner: Box<dyn ChildSpawner>,
    /// Built from the member bind the child serves on.
    pub health: Box<dyn Fn(SocketAddr) -> Arc<dyn HealthProbe> + Send>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// SIGTERM: the child was stopped and waited for.
    Stopped,
    /// The unit must die so continuity sees it; the code names why.
    Failed(&'static str),
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Stopped => 0,
            Self::Failed(_) => 1,
        }
    }
}

/// The member bind Idunn hands the unit: a socket address on a loopback IP,
/// other than the candidate bind. Errors name the variable, never its value.
pub fn parse_member_bind(raw: &str, candidate_bind: &str) -> Result<SocketAddr> {
    let member: SocketAddr = raw
        .parse()
        .map_err(|_| anyhow!("the Idunn member bind is not a socket address"))?;
    ensure!(
        member.ip().is_loopback(),
        "the Idunn member bind is not on a loopback address"
    );
    let candidate: SocketAddr = candidate_bind
        .parse()
        .map_err(|_| anyhow!("the Idunn candidate bind is not a socket address"))?;
    ensure!(member != candidate, "the Idunn member bind equals the candidate bind");
    Ok(member)
}

/// The unit's whole life: Warming, lease, child, Active, until the child or the
/// lease ends or SIGTERM arrives. `route` is the already-bound candidate
/// listener, answered for presence only; `member` is the address the child
/// serves members on; `tmp` is the unit's private temporary directory.
pub fn run(
    contract: &LaunchContract,
    authority: RuntimeAuthority,
    route: TcpListener,
    member: SocketAddr,
    tmp: &Path,
    ports: Ports,
    stop: &AtomicBool,
) -> Outcome {
    let expected = authority.expected().clone();
    let activation = authority.activation().clone();
    if expected.health_contract != contract.health_contract {
        return Outcome::Failed("health-contract-mismatch");
    }
    let health = (ports.health)(member);
    let shared = Arc::new(Shared {
        authority: Mutex::new(authority),
        phase: Mutex::new(Phase::Warming),
        lease: Mutex::new(None),
        lease_required: expected.write_lease_required,
        health: Arc::clone(&health),
        probed: Mutex::new(None),
        clock: Arc::clone(&ports.clock),
    });
    let halt = Arc::new(AtomicBool::new(false));
    let responder: Responder = {
        let shared = Arc::clone(&shared);
        Arc::new(move |request| shared.observe(request))
    };
    let acceptor = PresenceAcceptor::new(route, responder);
    let serving = {
        let halt = Arc::clone(&halt);
        thread::Builder::new()
            .name("presence-acceptor".into())
            .spawn(move || acceptor.serve(halt))
    };
    let Ok(serving) = serving else {
        return Outcome::Failed("presence-acceptor");
    };
    let outcome = lifecycle(contract, &expected, &activation, member, tmp, &ports, &shared, &serving, stop);
    halt.store(true, Ordering::SeqCst);
    let _ = serving.join();
    outcome
}

#[allow(clippy::too_many_arguments)]
fn lifecycle(
    contract: &LaunchContract,
    expected: &IdunnExpectedIncarnationRecord,
    activation: &IdunnRuntimeActivationRecord,
    member: SocketAddr,
    tmp: &Path,
    ports: &Ports,
    shared: &Shared,
    presence: &thread::JoinHandle<()>,
    stop: &AtomicBool,
) -> Outcome {
    let clock = &*ports.clock;
    let publish = |state: &str, detail: &str, lease: Option<String>| -> Option<String> {
        let record = shared.sign(state, detail, lease).ok()?;
        let sha = record.canonical_sha256().ok()?;
        // A refused or lost publish is retried by the heartbeat; Odin may
        // refuse Warming for a while after Idunn starts the unit.
        let _ = ports.sink.put(&record);
        Some(sha)
    };
    let set_phase = |phase: Phase| {
        if let Ok(mut current) = shared.phase.lock() {
            *current = phase;
        }
    };
    let fail = |child: Option<&mut Box<dyn ChildHandle>>, code: &'static str| {
        if let Some(child) = child {
            child.terminate(TERMINATE_GRACE);
        }
        set_phase(Phase::Failed);
        publish("failed", code, None);
        Outcome::Failed(code)
    };

    // 1. Warming reaches Odin before any child exists.
    let mut proofs: VecDeque<String> = VecDeque::new();
    let remember = |sha: Option<String>, proofs: &mut VecDeque<String>| {
        if let Some(sha) = sha {
            proofs.push_back(sha);
            while proofs.len() > MAX_RECENT_WARMING_PROOFS {
                proofs.pop_front();
            }
        }
    };
    remember(publish("warming", "launcher-started", None), &mut proofs);
    let heartbeat = u64::try_from(contract.heartbeat.as_millis()).unwrap_or(u64::MAX);
    let mut next_heartbeat = clock.now_millis() + heartbeat;

    // 2. A stateful child waits for the lease; a stateless one does not.
    let mut lease_sha = None;
    let mut lease_fd = None;
    if expected.write_lease_required {
        let deadline = clock.now_millis() + contract.lease_wait.as_millis() as u64;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Outcome::Stopped;
            }
            if presence.is_finished() {
                return fail(None, "presence-acceptor");
            }
            let held = LeaseHold::acquire(
                Arc::clone(&ports.lease),
                expected,
                activation,
                proofs.make_contiguous(),
            );
            if let Ok(hold) = held {
                lease_sha = Some(hold.sha256().to_owned());
                lease_fd = hold.lock_descriptor();
                if let Ok(mut slot) = shared.lease.lock() {
                    *slot = Some(hold);
                }
                break;
            }
            let now = clock.now_millis();
            if now >= deadline {
                return fail(None, "lease-timeout");
            }
            if now >= next_heartbeat {
                remember(publish("warming", "write-lease-pending", None), &mut proofs);
                next_heartbeat = now + heartbeat;
            }
            clock.sleep(POLL);
        }
    }

    // 3. Render, spawn, and wait for health.
    let Some(mut child) = start_child(contract, member, tmp, ports, lease_fd) else {
        return fail(None, "child-start-refused");
    };
    let startup_deadline = clock.now_millis() + contract.startup.as_millis() as u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            child.terminate(TERMINATE_GRACE);
            return Outcome::Stopped;
        }
        if child.exited().is_some() {
            return fail(None, "child-exited");
        }
        if presence.is_finished() {
            return fail(Some(&mut child), "presence-acceptor");
        }
        if shared.refresh_health() {
            break;
        }
        if clock.now_millis() >= startup_deadline {
            return fail(Some(&mut child), "startup-timeout");
        }
        clock.sleep(POLL);
    }

    // 4. Active, and heartbeats while it holds.
    set_phase(Phase::Active(lease_sha.clone()));
    publish(shared.serving_state(), "child-healthy", lease_sha.clone());
    let refresh = u64::try_from(HEALTH_REFRESH.as_millis()).unwrap_or(u64::MAX);
    let mut next_probe = clock.now_millis() + refresh;
    next_heartbeat = clock.now_millis() + heartbeat;
    loop {
        if stop.load(Ordering::SeqCst) {
            child.terminate(TERMINATE_GRACE);
            return Outcome::Stopped;
        }
        if child.exited().is_some() {
            return fail(None, "child-exited");
        }
        if presence.is_finished() {
            return fail(Some(&mut child), "presence-acceptor");
        }
        if !shared.lease_current() {
            return fail(Some(&mut child), "lease-lost");
        }
        let now = clock.now_millis();
        if now >= next_probe {
            shared.refresh_health();
            next_probe = now + refresh;
        }
        if now >= next_heartbeat {
            publish(shared.serving_state(), "heartbeat", lease_sha.clone());
            next_heartbeat = now + heartbeat;
        }
        clock.sleep(POLL);
    }
}

fn start_child(
    contract: &LaunchContract,
    member: SocketAddr,
    tmp: &Path,
    ports: &Ports,
    lease_fd: Option<RawFd>,
) -> Option<Box<dyn ChildHandle>> {
    let template = fs::read_to_string(contract.release_root.join(&contract.template)).ok()?;
    let rendered = ConfigRenderer {
        credentials: &*ports.credentials,
        listen: member,
        state_root: contract.state_root.as_deref(),
    }
    .render(&template)
    .ok()?;
    let config = write_private_file(tmp, "child-config", &rendered).ok()?;
    let mut args = contract.child_args.clone();
    if let Some(flag) = &contract.config_flag {
        args.push(flag.clone());
        args.push(config.display().to_string());
    }
    ports
        .spawner
        .spawn(&contract.release_root.join(&contract.child), &args, lease_fd)
        .ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use cultcache_rs::{CultCacheEnvelope, TryCompareExchangeSnapshotOutcome};
    use cultnet_rs::{
        IdunnExpectedCapability, IdunnExpectedRoute, IdunnRuntimeActivationLaunch,
        IdunnServiceIdentity, RuntimePresenceAuthenticationContext, VerifiedRuntimeAuthority,
        authenticate_runtime_presence_claim, enroll_service_identity_at, verify_runtime_authority,
    };

    use super::*;
    use crate::drivers::{WriteLeasePort, exact_route_snapshot_response, request_tcp_snapshot};

    /// The member bind Idunn would hand these fixtures' unit.
    fn member() -> SocketAddr {
        "127.0.0.1:18845".parse().unwrap()
    }

    fn digest(byte: char) -> String {
        format!("sha256-{}", byte.to_string().repeat(64))
    }

    struct Fixture {
        dir: tempfile::TempDir,
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
        activation_credential: Vec<u8>,
        provider_credential: Vec<u8>,
        verified: VerifiedRuntimeAuthority,
    }

    fn write_record(path: &Path, key: &str, kind: &str, schema: &str, payload: Vec<u8>) {
        let envelope = CultCacheEnvelope {
            key: key.into(),
            r#type: kind.into(),
            payload,
            stored_at: chrono::Utc::now().to_rfc3339(),
            schema_id: Some(schema.into()),
        };
        let outcome = SingleFileMessagePackBackingStore::new(path)
            .try_compare_exchange_snapshot(&[], &[envelope])
            .unwrap();
        assert!(matches!(
            outcome,
            TryCompareExchangeSnapshotOutcome::Exchanged
        ));
    }

    fn fixture(lease_required: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let provider_path = root.join("provider.cc");
        let provider =
            enroll_service_identity_at::<GameCultProviderHealthIdentity>(&provider_path).unwrap();
        let idunn =
            enroll_service_identity_at::<IdunnServiceIdentity>(&root.join("idunn.cc")).unwrap();
        let expected = IdunnExpectedIncarnationRecord {
            schema_version: IDUNN_EXPECTED_INCARNATION_SCHEMA.into(),
            target: "synapse".into(),
            plan_id: digest('1'),
            incarnation_id: "incarnation-1".into(),
            sealed_release_id: digest('2'),
            source_repository: "github.com/GameCult/gamecult-chat".into(),
            source_revision: "3".repeat(40),
            recipe_sha256: digest('4'),
            runtime_id: "synapse-yggdrasil".into(),
            expected_signer_identity_id: provider.entry().identity_id.clone(),
            health_contract: "gamecult.chat.synapse.presence-launcher.v1".into(),
            artifact_sha256: digest('5'),
            state_schema_generation: lease_required.then(|| "state-v1".into()),
            state_contract_sha256: lease_required.then(|| digest('6')),
            write_lease_required: lease_required,
            route: Some(IdunnExpectedRoute {
                route_id: "synapse-tcp".into(),
                transport: "tcp".into(),
                stable_endpoint: "tcp://127.0.0.1:18834".into(),
                candidate_endpoint: "tcp://127.0.0.1:18835".into(),
            }),
            capabilities: vec![IdunnExpectedCapability {
                capability: "gamecult.chat.homeserver".into(),
                schema: "matrix.client.v3".into(),
                compatibility: "v3".into(),
                minimum_capacity: 1,
            }],
            dependencies: Vec::new(),
        };
        let launch =
            IdunnRuntimeActivationLaunch::issue(&expected, digest('7'), 100, &idunn).unwrap();
        let activation = launch.activation().clone();
        let mut activation_credential = Vec::new();
        launch.write_credential(&mut activation_credential).unwrap();
        let verified = verify_runtime_authority(
            &expected,
            &activation,
            &idunn.trust_anchor().unwrap(),
            &provider.entry().public_key,
        )
        .unwrap();
        let bundle = root.join("bundle");
        fs::create_dir(&bundle).unwrap();
        write_record(
            &bundle.join("expected.cc"),
            &expected.target,
            IdunnExpectedIncarnationRecord::TYPE,
            IDUNN_EXPECTED_INCARNATION_SCHEMA,
            expected.canonical_bytes().unwrap(),
        );
        write_record(
            &bundle.join("activation.cc"),
            &expected.target,
            IdunnRuntimeActivationRecord::TYPE,
            IDUNN_RUNTIME_ACTIVATION_SCHEMA,
            activation.canonical_bytes().unwrap(),
        );
        let provider_credential = fs::read(&provider_path).unwrap();
        for directory in ["release/bin", "tmp"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        Fixture {
            dir,
            expected,
            activation,
            activation_credential,
            provider_credential,
            verified,
        }
    }

    impl Fixture {
        fn authority(&self) -> RuntimeAuthority {
            RuntimeAuthority::open(
                &self.dir.path().join("bundle"),
                &self.activation_credential[..],
                &self.provider_credential[..],
            )
            .unwrap()
        }

        fn lease(&self, warming_sha256: &str) -> IdunnProcessWriteLeaseRecord {
            IdunnProcessWriteLeaseRecord {
                schema_version: cultnet_rs::IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into(),
                target: self.expected.target.clone(),
                expected_projection_sha256: self.expected.canonical_sha256().unwrap(),
                plan_id: self.expected.plan_id.clone(),
                incarnation_id: self.expected.incarnation_id.clone(),
                sealed_release_id: self.expected.sealed_release_id.clone(),
                activation_witness_sha256: self.activation.canonical_sha256().unwrap(),
                state_schema_generation: self.expected.state_schema_generation.clone().unwrap(),
                state_contract_sha256: self.expected.state_contract_sha256.clone().unwrap(),
                runtime_id: self.expected.runtime_id.clone(),
                runtime_instance_id: self.activation.runtime_instance_id.clone(),
                warming_presence_sha256: warming_sha256.into(),
                lease_epoch: 1,
                issued_at_unix_millis: 200,
            }
        }

        fn contract(&self, template: &str) -> LaunchContract {
            fs::write(self.dir.path().join("release/config.tmpl"), template).unwrap();
            LaunchContract {
                release_root: self.dir.path().join("release"),
                child: PathBuf::from("bin/child"),
                child_args: vec!["--serve".into()],
                config_flag: Some("--config".into()),
                template: PathBuf::from("config.tmpl"),
                health_path: "/health".into(),
                health_contract: self.expected.health_contract.clone(),
                state_root: Some(PathBuf::from("/var/lib/synapse")),
                lease_wait: Duration::from_secs(3600),
                startup: Duration::from_secs(3600),
                heartbeat: Duration::from_secs(5),
            }
        }
    }

    /// Virtual time runs a hundred times faster than the wall clock.
    struct FastClock(AtomicU64);

    impl Clock for FastClock {
        fn now_millis(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }

        fn sleep(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[derive(Default)]
    struct Sink(Mutex<Vec<GameCultRuntimePresenceHealthRecord>>);

    impl PresenceSink for Sink {
        fn put(&self, record: &GameCultRuntimePresenceHealthRecord) -> Result<()> {
            self.0.lock().unwrap().push(record.clone());
            Ok(())
        }
    }

    impl Sink {
        fn states(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|record| record.state.clone())
                .collect()
        }

        fn first(&self, state: &str) -> Option<GameCultRuntimePresenceHealthRecord> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|record| record.state == state)
                .cloned()
        }
    }

    #[derive(Default)]
    struct FakeLease(Mutex<Option<IdunnProcessWriteLeaseRecord>>);

    impl LeaseReader for FakeLease {
        fn read(&self) -> Result<Option<IdunnProcessWriteLeaseRecord>> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn hold_shared(&self) -> Result<Box<dyn LeaseLock>> {
            Ok(Box::new(FakeLock))
        }
    }

    /// The descriptor number a fake lock claims; the launcher must hand it on.
    const FAKE_LOCK_FD: RawFd = 1234;

    struct FakeLock;

    impl LeaseLock for FakeLock {
        fn descriptor(&self) -> Option<RawFd> {
            Some(FAKE_LOCK_FD)
        }
    }

    struct Health(AtomicBool);

    impl HealthProbe for Health {
        fn healthy(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct Child {
        spawns: AtomicUsize,
        exit: Mutex<Option<i32>>,
        terminated: AtomicBool,
        args: Mutex<Vec<Vec<String>>>,
        inherited: Mutex<Vec<Option<RawFd>>>,
    }

    struct FakeChild(Arc<Child>);

    impl ChildHandle for FakeChild {
        fn exited(&mut self) -> Option<i32> {
            *self.0.exit.lock().unwrap()
        }

        fn terminate(&mut self, _grace: Duration) {
            self.0.terminated.store(true, Ordering::SeqCst);
            *self.0.exit.lock().unwrap() = Some(143);
        }
    }

    struct FakeSpawner(Arc<Child>);

    impl ChildSpawner for FakeSpawner {
        fn spawn(
            &self,
            _program: &Path,
            args: &[String],
            inherit: Option<RawFd>,
        ) -> Result<Box<dyn ChildHandle>> {
            self.0.inherited.lock().unwrap().push(inherit);
            self.0.spawns.fetch_add(1, Ordering::SeqCst);
            self.0.args.lock().unwrap().push(args.to_vec());
            Ok(Box::new(FakeChild(Arc::clone(&self.0))))
        }
    }

    struct Credentials(BTreeMap<String, Vec<u8>>);

    impl CredentialSource for Credentials {
        fn contents(&self, name: &str) -> Result<Vec<u8>> {
            self.0.get(name).cloned().ok_or_else(|| anyhow!("unbound"))
        }

        fn path(&self, name: &str) -> Result<PathBuf> {
            self.0
                .contains_key(name)
                .then(|| PathBuf::from(format!("/run/credentials/unit/{name}")))
                .ok_or_else(|| anyhow!("unbound"))
        }
    }

    struct Run {
        sink: Arc<Sink>,
        lease: Arc<FakeLease>,
        child: Arc<Child>,
        stop: Arc<AtomicBool>,
        outcome: Option<thread::JoinHandle<Outcome>>,
    }

    fn start(fx: &Fixture, contract: LaunchContract, healthy: bool) -> Run {
        let sink = Arc::new(Sink::default());
        let lease = Arc::new(FakeLease::default());
        let health = Arc::new(Health(AtomicBool::new(healthy)));
        let child = Arc::new(Child::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut secrets = BTreeMap::new();
        secrets.insert("SYNAPSE_KEY".to_owned(), b"s3cret-value\n".to_vec());
        let ports = Ports {
            credentials: Arc::new(Credentials(secrets)),
            clock: Arc::new(FastClock(AtomicU64::new(1_000_000))),
            sink: sink.clone(),
            lease: lease.clone(),
            spawner: Box::new(FakeSpawner(child.clone())),
            health: Box::new(move |_| health.clone()),
        };
        let authority = fx.authority();
        let route = TcpListener::bind("127.0.0.1:0").unwrap();
        let tmp = fx.dir.path().join("tmp");
        let flag = stop.clone();
        let outcome =
            thread::spawn(move || run(&contract, authority, route, member(), &tmp, ports, &flag));
        Run {
            sink,
            lease,
            child,
            stop,
            outcome: Some(outcome),
        }
    }

    impl Run {
        fn finish(&mut self) -> Outcome {
            self.outcome.take().unwrap().join().unwrap()
        }

        fn stop(&mut self) -> Outcome {
            self.stop.store(true, Ordering::SeqCst);
            self.finish()
        }
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..4000 {
            if condition() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn stateful_child_never_starts_before_its_lease() {
        let fx = fixture(true);
        let mut launch = start(&fx, fx.contract("listen {{child.listen}}"), true);
        wait_until("two warming heartbeats", || {
            launch.sink.states().iter().filter(|s| *s == "warming").count() >= 2
        });
        assert_eq!(launch.child.spawns.load(Ordering::SeqCst), 0);
        let warming = launch.sink.first("warming").unwrap();
        assert!(warming.write_lease_sha256.is_none());

        // The lease binds the launcher's own Warming proof; only then it starts.
        let lease = fx.lease(&warming.canonical_sha256().unwrap());
        *launch.lease.0.lock().unwrap() = Some(lease.clone());
        wait_until("child spawn", || {
            launch.child.spawns.load(Ordering::SeqCst) == 1
        });
        wait_until("active", || launch.sink.first("active").is_some());
        let active = launch.sink.first("active").unwrap();
        assert_eq!(
            active.write_lease_sha256.as_deref(),
            Some(lease.canonical_sha256().unwrap().as_str())
        );
        assert_eq!(launch.stop(), Outcome::Stopped);
        assert!(launch.child.terminated.load(Ordering::SeqCst));
    }

    #[test]
    fn a_lease_for_another_activation_or_expected_is_not_accepted() {
        let fx = fixture(true);
        let foreign: [(&str, fn(&mut IdunnProcessWriteLeaseRecord)); 3] = [
            ("another runtime instance", |lease| {
                lease.runtime_instance_id = "other".into()
            }),
            ("another Expected", |lease| {
                lease.expected_projection_sha256 = digest('9')
            }),
            ("another warming proof", |lease| {
                lease.warming_presence_sha256 = digest('9')
            }),
        ];
        for (name, edit) in foreign {
            let mut contract = fx.contract("x");
            contract.lease_wait = Duration::from_secs(5);
            let mut launch = start(&fx, contract, true);
            // Installed at once; the launcher's own proofs are never digest('8').
            let mut lease = fx.lease(&digest('8'));
            edit(&mut lease);
            *launch.lease.0.lock().unwrap() = Some(lease);
            assert_eq!(launch.finish(), Outcome::Failed("lease-timeout"), "{name}");
            assert_eq!(launch.child.spawns.load(Ordering::SeqCst), 0, "{name}");
            assert_eq!(launch.sink.states().last().unwrap(), "failed", "{name}");
        }
    }

    #[test]
    fn lease_loss_stops_the_child() {
        let fx = fixture(true);
        let mut launch = start(&fx, fx.contract("x"), true);
        wait_until("warming", || launch.sink.first("warming").is_some());
        let warming = launch.sink.first("warming").unwrap();
        *launch.lease.0.lock().unwrap() = Some(fx.lease(&warming.canonical_sha256().unwrap()));
        wait_until("active", || launch.sink.first("active").is_some());
        *launch.lease.0.lock().unwrap() = None;
        let outcome = launch.finish();
        assert_eq!(outcome, Outcome::Failed("lease-lost"));
        assert_ne!(outcome.exit_code(), 0);
        assert!(launch.child.terminated.load(Ordering::SeqCst));
        assert_eq!(launch.sink.states().last().unwrap(), "failed");
    }

    #[test]
    fn stateless_child_starts_without_a_lease() {
        let fx = fixture(false);
        let mut launch = start(&fx, fx.contract("x"), true);
        wait_until("active", || launch.sink.first("active").is_some());
        assert_eq!(launch.child.spawns.load(Ordering::SeqCst), 1);
        assert_eq!(launch.sink.states().first().unwrap(), "warming");
        assert!(
            launch
                .sink
                .first("active")
                .unwrap()
                .write_lease_sha256
                .is_none()
        );
        launch.stop();
    }

    #[test]
    fn child_exit_ends_the_launcher() {
        for code in [0, 3] {
            let fx = fixture(false);
            let mut launch = start(&fx, fx.contract("x"), true);
            wait_until("active", || launch.sink.first("active").is_some());
            *launch.child.exit.lock().unwrap() = Some(code);
            let outcome = launch.finish();
            assert_eq!(outcome, Outcome::Failed("child-exited"), "exit {code}");
            assert_eq!(outcome.exit_code(), 1);
            assert_eq!(launch.sink.states().last().unwrap(), "failed");
        }
    }

    #[test]
    fn closed_placeholder_set_renders_and_refuses() {
        let fx = fixture(false);
        let template = "listen={{child.listen}}\nhost={{child.listen_host}}\nport={{child.listen_port}}\nroot={{state_root}}\nkey={{credential:SYNAPSE_KEY}}\npath={{credential_path:SYNAPSE_KEY}}\n";
        let mut launch = start(&fx, fx.contract(template), true);
        wait_until("spawn", || launch.child.spawns.load(Ordering::SeqCst) == 1);
        let args = launch.child.args.lock().unwrap()[0].clone();
        assert_eq!(args[..2], ["--serve", "--config"]);
        let config = PathBuf::from(&args[2]);
        assert_eq!(config.parent().unwrap(), fx.dir.path().join("tmp"));
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let rendered = fs::read_to_string(&config).unwrap();
        assert_eq!(
            rendered,
            "listen=127.0.0.1:18845\nhost=127.0.0.1\nport=18845\nroot=/var/lib/synapse\nkey=s3cret-value\npath=/run/credentials/unit/SYNAPSE_KEY\n"
        );
        assert!(
            !rendered.contains("18835"),
            "the candidate bind never reaches the child"
        );
        launch.stop();

        // Any other placeholder refuses to start, and the refusal names no input.
        let fx = fixture(false);
        let mut launch = start(&fx, fx.contract("a={{canary_bind}}"), true);
        assert_eq!(launch.finish(), Outcome::Failed("child-start-refused"));
        assert_eq!(launch.child.spawns.load(Ordering::SeqCst), 0);
        let credentials = Credentials(BTreeMap::new());
        let renderer = ConfigRenderer {
            credentials: &credentials,
            listen: member(),
            state_root: None,
        };
        for template in [
            "{{canary_bind}}",
            "{{credential:CANARY-MISSING}}",
            "{{credential:canary b}}",
            "{{child.listen}} {{canary",
            "{{state_root}}",
        ] {
            let error = format!("{:#}", renderer.render(template).unwrap_err());
            assert!(!error.to_lowercase().contains("canary"), "{error}");
        }
    }

    fn shared(fx: &Fixture, health: Arc<Health>) -> Arc<Shared> {
        shared_on(fx, health, Arc::new(SystemClock))
    }

    fn shared_on(fx: &Fixture, health: Arc<Health>, clock: Arc<dyn Clock>) -> Arc<Shared> {
        Arc::new(Shared {
            authority: Mutex::new(fx.authority()),
            phase: Mutex::new(Phase::Active(None)),
            lease: Mutex::new(None),
            lease_required: false,
            health,
            probed: Mutex::new(None),
            clock,
        })
    }

    #[test]
    fn route_answer_degrades_with_the_child() {
        let fx = fixture(false);
        let health = Arc::new(Health(AtomicBool::new(true)));
        let shared = shared(&fx, health.clone());
        let state_of = |shared: &Shared| {
            let request = CultNetMessage::SnapshotRequest {
                message_id: "challenge-1".into(),
                schema_ids: None,
                record_keys: None,
            };
            let CultNetMessage::SnapshotResponseRaw { documents, .. } =
                shared.observe(request).unwrap()
            else {
                panic!("not a raw snapshot response")
            };
            let record: GameCultRuntimePresenceHealthRecord =
                rmp_serde::from_slice(&documents[0].payload).unwrap();
            assert_eq!(record.detail, "route-observation:challenge-1");
            record.state
        };
        // Never probed: nothing says the child is healthy.
        assert_eq!(state_of(&shared), "degraded");
        shared.refresh_health();
        assert_eq!(state_of(&shared), "active");
        // A challenge answers from the last probe and never runs one.
        health.0.store(false, Ordering::SeqCst);
        assert_eq!(state_of(&shared), "active");
        shared.refresh_health();
        assert_eq!(state_of(&shared), "degraded");
    }

    #[test]
    fn route_challenge_is_answered_with_both_signatures_through_idunns_client() {
        let fx = fixture(false);
        let shared = shared(&fx, Arc::new(Health(AtomicBool::new(true))));
        shared.refresh_health();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responder: Responder = Arc::new(move |request| shared.observe(request));
        let halt = Arc::new(AtomicBool::new(false));
        let acceptor = PresenceAcceptor::new(listener, responder);
        let serving = {
            let halt = halt.clone();
            thread::spawn(move || acceptor.serve(halt))
        };
        let request = CultNetMessage::SnapshotRequest {
            message_id: "challenge-7".into(),
            schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
            record_keys: Some(vec![fx.expected.target.clone()]),
        };
        let payload =
            encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0).unwrap();
        let response = request_tcp_snapshot(address, &payload).ok().unwrap();
        let message =
            decode_cultnet_message_from_slice(&response, CultNetWireContract::CultNetSchemaV0)
                .unwrap();
        let answer =
            exact_route_snapshot_response(message, "challenge-7", &fx.expected.target).unwrap();
        let record: GameCultRuntimePresenceHealthRecord =
            rmp_serde::from_slice(&answer.canonical_presence).unwrap();
        let claim = authenticate_runtime_presence_claim(
            &answer.canonical_presence,
            &fx.verified,
            RuntimePresenceAuthenticationContext {
                trusted_received_at_unix_millis: record.observed_at_unix_millis,
                maximum_age_millis: 10_000,
                maximum_future_skew_millis: 10,
            },
        )
        .unwrap();
        assert_eq!(claim.record().state, "active");
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    #[test]
    fn contract_parsing_refuses_escapes_and_echoes_nothing() {
        let base = |extra: &[&str]| -> Result<LaunchContract> {
            let mut args: Vec<String> = [
                "--child",
                "bin/c",
                "--template",
                "t",
                "--health-path",
                "/health",
                "--health-contract",
                "h",
            ]
            .map(String::from)
            .to_vec();
            args.extend(extra.iter().map(|value| value.to_string()));
            LaunchContract::parse(PathBuf::from("/r"), args)
        };
        assert!(base(&["--child-arg", "--flag-looking-value"]).is_ok());
        assert!(base(&["--child", "../escape"]).is_err());
        // The child serves members on Idunn's member bind: there is no
        // listener to choose.
        assert!(base(&["--listener", "loopback"]).is_err());
        assert!(base(&["--template", "/etc/passwd"]).is_err());
        let error = base(&["--canary-flag", "canary-value"])
            .unwrap_err()
            .to_string();
        assert!(!error.contains("canary"));
    }

    #[test]
    fn real_lease_reader_holds_the_shared_lock_idunn_must_respect() {
        let fx = fixture(true);
        let path = fx.dir.path().join("lease.cc");
        let warming = digest('a');
        let lease = fx.lease(&warming);
        write_record(
            &path,
            &lease.target,
            IdunnProcessWriteLeaseRecord::TYPE,
            cultnet_rs::IDUNN_PROCESS_WRITE_LEASE_SCHEMA,
            lease.canonical_bytes().unwrap(),
        );
        let reader: Arc<dyn LeaseReader> = Arc::new(DriverLeaseReader::new(&lease.target, &path));
        let hold = LeaseHold::acquire(reader, &fx.expected, &fx.activation, &[warming]).unwrap();
        assert!(hold.still_current());
        let idunn = CultCacheWriteLeaseDriver::new(&lease.target, &path);
        let denial = idunn.revoke_exact(Some(&lease)).unwrap_err();
        assert!(denial.to_string().contains("shared-lock consumer"));
        drop(hold);
        idunn.revoke_exact(Some(&lease)).unwrap();
    }

    #[test]
    fn authority_binds_the_candidate_and_signs_strictly_increasing_sequences() {
        let fx = fixture(false);
        let mut authority = fx.authority();
        assert!(authority.require_candidate_bind("127.0.0.1:18835").is_ok());
        assert!(authority.require_candidate_bind("127.0.0.1:18836").is_err());
        let first = authority.sign("warming", "a", None, 5_000).unwrap();
        let second = authority.sign("warming", "b", None, 5_000).unwrap();
        assert_eq!(first.publisher_sequence, 5_000);
        assert_eq!(second.publisher_sequence, 5_001);
        assert_eq!(
            first.bound_endpoint.as_deref(),
            Some("tcp://127.0.0.1:18835")
        );
    }

    #[test]
    fn health_probe_is_healthy_only_on_a_200_from_the_child() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let member = listener.local_addr().unwrap();
        let probe = HttpHealthProbe { member, path: "/health".into() };
        let server = thread::spawn(move || {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                io::BufRead::read_line(&mut io::BufReader::new(stream.try_clone().unwrap()), &mut line)
                    .unwrap();
                assert_eq!(line, "GET /health HTTP/1.1\r\n");
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\nOK").unwrap();
            }
        });
        assert!(probe.healthy());
        assert!(!probe.healthy());
        server.join().unwrap();
        assert!(!probe.healthy(), "no listener is not healthy");
    }

    #[test]
    fn contract_parsing_reads_every_declared_field() {
        let args: Vec<String> = [
            "--child", "bin/c", "--child-arg", "-x", "--child-arg", "y", "--config-flag", "-c",
            "--template", "t", "--health-path", "/h",
            "--health-contract", "hc", "--state-root", "/s", "--lease-wait-secs", "11",
            "--startup-secs", "22", "--heartbeat-secs", "33",
        ]
        .map(String::from)
        .to_vec();
        let contract = LaunchContract::parse(PathBuf::from("/r"), args.clone()).unwrap();
        assert_eq!(contract.child_args, ["-x", "y"]);
        assert_eq!(contract.config_flag.as_deref(), Some("-c"));
        assert_eq!(contract.state_root, Some(PathBuf::from("/s")));
        assert_eq!(
            (contract.lease_wait, contract.startup, contract.heartbeat),
            (Duration::from_secs(11), Duration::from_secs(22), Duration::from_secs(33))
        );
        for zero in ["--lease-wait-secs", "--startup-secs", "--heartbeat-secs"] {
            let mut bad = args.clone();
            bad.extend([zero.to_owned(), "0".to_owned()]);
            assert!(LaunchContract::parse(PathBuf::from("/r"), bad).is_err(), "{zero}");
        }
    }

    #[test]
    fn credential_names_cannot_escape_or_be_empty() {
        struct Any;
        impl CredentialSource for Any {
            fn contents(&self, _: &str) -> Result<Vec<u8>> {
                Ok(b"v".to_vec())
            }
            fn path(&self, name: &str) -> Result<PathBuf> {
                Ok(PathBuf::from(name))
            }
        }
        let renderer = ConfigRenderer { credentials: &Any, listen: member(), state_root: None };
        for ok in ["A", "a-b_9", "Z0"] {
            assert_eq!(renderer.render(&format!("{{{{credential:{ok}}}}}")).unwrap(), "v");
        }
        for bad in ["", "../x", "a/b", "a b", "a.b", "a:b", "é"] {
            assert!(renderer.render(&format!("{{{{credential:{bad}}}}}")).is_err(), "{bad}");
            assert!(renderer.render(&format!("{{{{credential_path:{bad}}}}}")).is_err(), "{bad}");
        }
        assert_eq!(renderer.render("a{{child.listen}}b{{child.listen}}c").unwrap(), "a127.0.0.1:18845b127.0.0.1:18845c");
    }

    #[test]
    fn credentials_resolve_only_as_files_under_the_credentials_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cred"), b"abc\n").unwrap();
        let credentials = CredentialsDirectory::new(dir.path().to_owned());
        assert_eq!(credentials.path("cred").unwrap(), dir.path().join("cred"));
        assert_eq!(credentials.contents("cred").unwrap(), b"abc\n");
        assert!(credentials.path("unbound").is_err());
        for bad in ["", "..", "../cred", "a/b", ".", "a.b"] {
            assert!(credentials.path(bad).is_err(), "{bad}");
            assert!(credentials.contents(bad).is_err(), "{bad}");
        }
        assert!(CredentialsDirectory(None).path("cred").is_err());
    }

    #[test]
    fn credential_placeholders_never_read_the_process_environment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("elsewhere");
        fs::write(&file, b"canary-file\n").unwrap();
        // SAFETY: variables no other test reads.
        unsafe {
            std::env::set_var("GAMECULT_IDUNN_CANDIDATE_BIND", "127.0.0.1:18835");
            std::env::set_var("IDUNN_LAUNCH_TEST_ENV_CREDENTIAL", &file);
        }
        let credentials = CredentialsDirectory::new(dir.path().to_owned());
        let renderer = ConfigRenderer { credentials: &credentials, listen: member(), state_root: None };
        for name in [
            "GAMECULT_IDUNN_CANDIDATE_BIND",
            "IDUNN_LAUNCH_TEST_ENV_CREDENTIAL",
            "../elsewhere",
            "a/b",
            "..",
        ] {
            for kind in ["credential", "credential_path"] {
                let error = renderer.render(&format!("x={{{{{kind}:{name}}}}}"));
                assert!(error.is_err(), "{kind}:{name}");
                assert!(!format!("{:#}", error.unwrap_err()).contains("18835"));
            }
        }
        // The same name resolves when it is a credential file, never otherwise.
        fs::write(dir.path().join("GAMECULT_IDUNN_CANDIDATE_BIND"), b"file-value\n").unwrap();
        assert_eq!(
            renderer.render("{{credential:GAMECULT_IDUNN_CANDIDATE_BIND}}").unwrap(),
            "file-value"
        );
    }

    #[test]
    fn odin_sink_takes_its_endpoint_only_from_the_odin_dependency() {
        let fx = fixture(false);
        let dependency = |capability: &str, endpoint: &str| cultnet_rs::IdunnExpectedDependency {
            kind: "shared-infrastructure".into(),
            capability: capability.into(),
            schema: "s".into(),
            compatibility: "v1".into(),
            minimum_capacity: 1,
            startup: "before-promotion".into(),
            provider_id: None,
            provider_authority: None,
            provider_expected_projection_sha256: None,
            provider_endpoint: Some(endpoint.into()),
        };
        let mut expected = fx.expected.clone();
        assert!(OdinRudpSink::from_expected(&expected).is_err());
        expected.dependencies = vec![dependency("other.thing", "rudp://127.0.0.1:1")];
        assert!(OdinRudpSink::from_expected(&expected).is_err());
        expected.dependencies = vec![
            dependency("other.thing", "rudp://127.0.0.1:1"),
            dependency(ODIN_CAPABILITY, "rudp://127.0.0.1:4100"),
        ];
        assert_eq!(
            OdinRudpSink::from_expected(&expected).unwrap().endpoint,
            "127.0.0.1:4100".parse().unwrap()
        );
        expected.dependencies = vec![dependency(ODIN_CAPABILITY, "rudp://not-an-address")];
        assert!(OdinRudpSink::from_expected(&expected).is_err());
    }


    #[test]
    fn a_probe_result_is_evidence_only_while_it_is_recent() {
        let fx = fixture(false);
        let clock = Arc::new(FastClock(AtomicU64::new(1_000_000)));
        let shared = shared_on(&fx, Arc::new(Health(AtomicBool::new(true))), clock.clone());
        shared.refresh_health();
        let stale = HEALTH_STALE.as_millis() as u64;
        clock.0.fetch_add(stale, Ordering::SeqCst);
        assert_eq!(shared.serving_state(), "active", "a result exactly at the bound is still recent");
        clock.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(shared.serving_state(), "degraded", "a result past the bound is not evidence");
        shared.refresh_health();
        assert_eq!(shared.serving_state(), "active");
    }

    /// Runs `work` and returns its result, failing the test when it has not
    /// finished within `limit`, so a mutant that makes it wait forever fails
    /// instead of hanging the suite.
    fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
        join_within(limit, thread::spawn(work))
    }

    fn join_within<T>(limit: Duration, handle: thread::JoinHandle<T>) -> T {
        let started = Instant::now();
        while !handle.is_finished() {
            assert!(started.elapsed() < limit, "still running after {limit:?}");
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().unwrap()
    }

    /// The state the presence endpoint signs in answer to a challenge, or None
    /// when it is not answered promptly.
    fn challenge_state(address: SocketAddr, fx: &Fixture) -> Option<String> {
        let request = CultNetMessage::SnapshotRequest {
            message_id: "wedge".into(),
            schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
            record_keys: Some(vec![fx.expected.target.clone()]),
        };
        let payload =
            encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0).unwrap();
        let started = Instant::now();
        let answer = request_tcp_snapshot(address, &payload).ok()?;
        if started.elapsed() > Duration::from_millis(750) {
            return None;
        }
        let CultNetMessage::SnapshotResponseRaw { documents, .. } =
            decode_cultnet_message_from_slice(&answer, CultNetWireContract::CultNetSchemaV0).ok()?
        else {
            return None;
        };
        let record: GameCultRuntimePresenceHealthRecord =
            rmp_serde::from_slice(&documents.first()?.payload).ok()?;
        Some(record.state)
    }

    #[derive(Clone, Copy)]
    enum Wedge {
        /// Accepts and never answers.
        Silent,
        /// Never accepts; the kernel backlog fills.
        FullBacklog,
        /// Accepts and answers 200 after 1.1 s, past the probe's deadline.
        Slow,
    }

    type WedgeSwitch = Arc<Mutex<Option<Wedge>>>;

    /// A child that answers 200 until wedged. Returns the member address, the
    /// switch, and the thread to join after `halt`.
    fn serving_child(halt: Arc<AtomicBool>) -> (SocketAddr, WedgeSwitch, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let wedge: WedgeSwitch = Arc::default();
        let mode = wedge.clone();
        let serving = thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let current = *mode.lock().unwrap();
                if matches!(current, Some(Wedge::FullBacklog)) {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                match current {
                    None => {
                        let mut request = [0_u8; 256];
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                    }
                    Some(Wedge::Slow) => {
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(1100));
                            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                        });
                    }
                    _ => {
                        thread::spawn(move || {
                            thread::sleep(Duration::from_secs(8));
                            drop(stream);
                        });
                    }
                }
            }
        });
        (address, wedge, serving)
    }

    /// Whatever wedges the child, every challenge is still answered promptly,
    /// and the answer turns to a signed degraded within a few probes.
    fn a_wedged_child_is_answered_degraded(wedge: Wedge) {
        let fx = fixture(false);
        let sink = Arc::new(Sink::default());
        let halt = Arc::new(AtomicBool::new(false));
        let (member, mode, child) = serving_child(halt.clone());
        let ports = Ports {
            credentials: Arc::new(Credentials(BTreeMap::new())),
            clock: Arc::new(SystemClock),
            sink: sink.clone(),
            lease: Arc::new(FakeLease::default()),
            spawner: Box::new(FakeSpawner(Arc::new(Child::default()))),
            health: Box::new(|member| Arc::new(HttpHealthProbe { member, path: "/health".into() })),
        };
        let contract = fx.contract("x");
        let authority = fx.authority();
        let route = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = route.local_addr().unwrap();
        let tmp = fx.dir.path().join("tmp");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let unit = thread::spawn(move || run(&contract, authority, route, member, &tmp, ports, &flag));
        wait_until("active", || sink.first("active").is_some());
        assert_eq!(challenge_state(address, &fx).as_deref(), Some("active"));

        *mode.lock().unwrap() = Some(wedge);
        // A backlog fills only once clients have connected without being
        // accepted.
        let mut queued = Vec::new();
        if matches!(wedge, Wedge::FullBacklog) {
            for _ in 0..4096 {
                match TcpStream::connect_timeout(&member, Duration::from_millis(20)) {
                    Ok(stream) => queued.push(stream),
                    Err(_) => break,
                }
            }
        }
        let started = Instant::now();
        let mut degraded = false;
        while started.elapsed() < Duration::from_secs(6) {
            let state = challenge_state(address, &fx);
            assert!(state.is_some(), "a challenge went unanswered after {:?}", started.elapsed());
            if state.as_deref() == Some("degraded") {
                degraded = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(degraded, "the wedged child was never reported degraded");
        stop.store(true, Ordering::SeqCst);
        assert_eq!(join_within(Duration::from_secs(10), unit), Outcome::Stopped);
        halt.store(true, Ordering::SeqCst);
        join_within(Duration::from_secs(10), child);
        drop(queued);
    }

    #[test]
    fn a_silent_child_is_answered_degraded_never_silent() {
        a_wedged_child_is_answered_degraded(Wedge::Silent);
    }

    #[test]
    fn a_child_with_a_full_backlog_is_answered_degraded_never_silent() {
        a_wedged_child_is_answered_degraded(Wedge::FullBacklog);
    }

    #[test]
    fn a_child_answering_after_the_probe_deadline_is_answered_degraded_never_silent() {
        a_wedged_child_is_answered_degraded(Wedge::Slow);
    }

    /// The virtual time (milliseconds) of each record that has the given
    /// detail.
    fn times_of(sink: &Sink, detail: &str) -> Vec<u64> {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.detail == detail)
            .map(|record| record.observed_at_unix_millis)
            .collect()
    }

    #[test]
    fn a_child_that_never_becomes_healthy_fails_the_unit_at_the_startup_timeout() {
        let fx = fixture(false);
        let mut contract = fx.contract("x");
        contract.startup = Duration::from_secs(5);
        let mut launch = start(&fx, contract, false);
        let sink = launch.sink.clone();
        let child = launch.child.clone();
        let outcome = within(Duration::from_secs(20), move || launch.finish());
        assert_eq!(outcome, Outcome::Failed("startup-timeout"));
        assert!(child.terminated.load(Ordering::SeqCst), "the child outlived the unit");
        let started = times_of(&sink, "launcher-started")[0];
        let failed = times_of(&sink, "startup-timeout")[0];
        // Virtual time: the deadline, found within one poll of it.
        let waited = failed - started;
        assert!(
            (5_000..=5_000 + POLL.as_millis() as u64).contains(&waited),
            "failed after {waited} ms of virtual time"
        );
    }

    #[test]
    fn heartbeats_follow_the_contracts_cadence_while_waiting_for_the_lease_and_while_active() {
        let fx = fixture(true);
        let mut contract = fx.contract("x");
        contract.heartbeat = Duration::from_secs(1);
        let mut launch = start(&fx, contract, true);
        let cadence = 1_000..=1_000 + POLL.as_millis() as u64;
        wait_until("lease-wait heartbeats", || {
            times_of(&launch.sink, "write-lease-pending").len() >= 4
        });
        let warming = launch.sink.first("warming").unwrap();
        let mut beats = vec![warming.observed_at_unix_millis];
        beats.extend(times_of(&launch.sink, "write-lease-pending").into_iter().take(4));
        for pair in beats.windows(2) {
            assert!(cadence.contains(&(pair[1] - pair[0])), "lease-wait beats {beats:?}");
        }
        *launch.lease.0.lock().unwrap() = Some(fx.lease(&warming.canonical_sha256().unwrap()));
        wait_until("active heartbeats", || times_of(&launch.sink, "heartbeat").len() >= 4);
        let mut beats = vec![times_of(&launch.sink, "child-healthy")[0]];
        beats.extend(times_of(&launch.sink, "heartbeat").into_iter().take(4));
        for pair in beats.windows(2) {
            assert!(cadence.contains(&(pair[1] - pair[0])), "active beats {beats:?}");
        }
        assert_eq!(launch.stop().exit_code(), 0);
    }

    #[test]
    fn process_spawner_escalates_to_kill_when_the_child_ignores_sigterm() {
        let mut stubborn = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "trap '' TERM; while :; do :; done".into()], None)
            .unwrap();
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        stubborn.terminate(Duration::from_millis(400));
        assert!(started.elapsed() >= Duration::from_millis(400), "SIGTERM gets its grace period");
        assert!(stubborn.exited().is_some(), "then SIGKILL ends it");
    }

    #[test]
    fn process_spawner_reports_exit_codes_and_terminates_a_running_child() {
        let mut gone = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "exit 3".into()], None)
            .unwrap();
        wait_until("child exit", || gone.exited().is_some());
        assert_eq!(gone.exited(), Some(3));
        let mut running = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "sleep 30".into()], None)
            .unwrap();
        assert_eq!(running.exited(), None);
        running.terminate(Duration::from_secs(5));
        assert!(running.exited().is_some());
    }

    // ------------------------------------------------------------ presence acceptor

    type Accepted = io::Result<(TcpStream, SocketAddr)>;

    fn serve_presence_accepting(
        fx: &Fixture,
        responder: Responder,
        accept: impl FnMut(&TcpListener) -> Accepted + Send + 'static,
    ) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let _ = fx;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = PresenceAcceptor::new(listener, responder);
        let halt = Arc::new(AtomicBool::new(false));
        let flag = halt.clone();
        (address, halt, thread::spawn(move || acceptor.serve_accepting(flag, accept)))
    }

    fn observing(fx: &Fixture) -> Responder {
        let shared = shared(fx, Arc::new(Health(AtomicBool::new(true))));
        shared.refresh_health();
        Arc::new(move |request| shared.observe(request))
    }

    fn serve_presence(fx: &Fixture) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        serve_presence_accepting(fx, observing(fx), |listener| listener.accept())
    }

    /// True when the peer closed (EOF or reset) within `within`, having sent
    /// nothing.
    fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
        stream.set_read_timeout(Some(within)).unwrap();
        let mut byte = [0_u8; 64];
        match stream.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => !matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
        }
    }

    fn challenge_answered(address: SocketAddr, fx: &Fixture) -> bool {
        let request = CultNetMessage::SnapshotRequest {
            message_id: "bounded".into(),
            schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
            record_keys: Some(vec![fx.expected.target.clone()]),
        };
        let payload =
            encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0).unwrap();
        request_tcp_snapshot(address, &payload).is_ok()
    }

    #[test]
    fn only_challenge_frames_are_answered_and_anything_else_is_closed_unanswered() {
        let fx = fixture(false);
        let reached = Arc::new(AtomicUsize::new(0));
        let responder: Responder = {
            let (inner, reached) = (observing(&fx), reached.clone());
            Arc::new(move |request| {
                reached.fetch_add(1, Ordering::SeqCst);
                inner(request)
            })
        };
        let (address, halt, serving) =
            serve_presence_accepting(&fx, responder, |listener| listener.accept());
        // Member traffic is not presence: an HTTP request gets no byte back.
        let mut member = TcpStream::connect(address).unwrap();
        member
            .write_all(b"POST /_matrix/x HTTP/1.1\r\nContent-Length: 2\r\n\r\n\x00\x01")
            .unwrap();
        assert!(closed_within(&mut member, Duration::from_secs(3)), "member bytes were answered");
        // A leading 0x00 with a frame length outside its bound never reaches
        // the responder.
        let mut malformed = TcpStream::connect(address).unwrap();
        malformed.write_all(&[0x00, 0xff, 0xff, 0xff]).unwrap();
        assert!(closed_within(&mut malformed, Duration::from_secs(3)), "a malformed frame was answered");
        assert_eq!(reached.load(Ordering::SeqCst), 0);
        // A real challenge is answered, and is the only thing that reaches it.
        assert!(challenge_answered(address, &fx));
        assert_eq!(reached.load(Ordering::SeqCst), 1);
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    #[test]
    fn a_challenge_up_to_the_frame_bound_is_answered_and_one_past_it_is_not() {
        let fx = fixture(false);
        let (address, halt, serving) = serve_presence(&fx);
        // A request far larger than a few KiB, well inside the 1 MiB bound.
        let request = CultNetMessage::SnapshotRequest {
            message_id: "large".into(),
            schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
            record_keys: Some((0..300).map(|index| format!("{index:0>40}")).collect()),
        };
        let payload =
            encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0).unwrap();
        assert!(payload.len() > 10_000 && payload.len() < CHALLENGE_MAX_BYTES);
        assert!(request_tcp_snapshot(address, &payload).is_ok());
        // One byte past the bound is closed unanswered.
        let past = (CHALLENGE_MAX_BYTES as u32 + 1).to_be_bytes();
        assert_eq!(past[0], CHALLENGE_FIRST_BYTE);
        let mut oversized = TcpStream::connect(address).unwrap();
        oversized.write_all(&past).unwrap();
        assert!(closed_within(&mut oversized, Duration::from_secs(3)), "an oversized frame was read");
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    #[test]
    fn an_idle_peer_holds_the_acceptor_for_the_presence_deadline_and_no_longer() {
        let fx = fixture(false);
        let (address, halt, serving) = serve_presence(&fx);
        let mut idle = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        // Idunn waits 3 s for a challenge answer; one stalled peer ahead of it
        // must not cost it that.
        assert!(challenge_answered(address, &fx));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(800), "the idle peer was cut early: {waited:?}");
        assert!(waited < Duration::from_millis(2200), "the idle peer held the acceptor: {waited:?}");
        assert!(closed_within(&mut idle, Duration::from_secs(1)), "the idle peer stayed connected");
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    #[test]
    fn a_challenge_trickled_a_byte_at_a_time_is_cut_off_at_the_presence_deadline() {
        let fx = fixture(false);
        let (address, halt, serving) = serve_presence(&fx);
        let mut slow = TcpStream::connect(address).unwrap();
        slow.write_all(&[0x00, 0x00, 0x01, 0x00]).unwrap();
        let started = Instant::now();
        let mut closed = false;
        while started.elapsed() < Duration::from_secs(4) {
            if closed_within(&mut slow, Duration::from_millis(150)) || slow.write_all(&[0x01]).is_err() {
                closed = true;
                break;
            }
        }
        assert!(closed, "the trickle was never cut off");
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(800), "cut before its deadline: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(2200), "cut after its deadline: {elapsed:?}");
        assert!(challenge_answered(address, &fx));
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    #[test]
    fn accept_errors_never_end_the_presence_acceptor() {
        let fx = fixture(false);
        // Descriptor exhaustion: the first accepts fail, then descriptors free.
        let failures = Arc::new(AtomicUsize::new(0));
        let (address, halt, serving) = {
            let failures = failures.clone();
            serve_presence_accepting(&fx, observing(&fx), move |listener| {
                if failures.fetch_add(1, Ordering::SeqCst) < 3 {
                    Err(io::Error::from_raw_os_error(libc::EMFILE))
                } else {
                    listener.accept()
                }
            })
        };
        assert!(challenge_answered(address, &fx), "the acceptor did not recover");
        assert!(failures.load(Ordering::SeqCst) >= 4);
        assert!(!serving.is_finished());
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    /// A listener whose accept always fails: a datagram socket's descriptor.
    fn failing_listener() -> TcpListener {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let datagram = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: the descriptor is owned by the new listener alone.
        unsafe { TcpListener::from_raw_fd(datagram.into_raw_fd()) }
    }

    #[test]
    fn an_acceptor_whose_every_accept_fails_runs_until_halted() {
        let fx = fixture(false);
        let acceptor = PresenceAcceptor::new(failing_listener(), observing(&fx));
        let halt = Arc::new(AtomicBool::new(false));
        let flag = halt.clone();
        let serving = thread::spawn(move || acceptor.serve(flag));
        thread::sleep(Duration::from_millis(400));
        assert!(!serving.is_finished(), "the acceptor ended on an accept error");
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap();
    }

    /// A clock that kills the presence thread when it reads the time: the
    /// stand-in for a presence acceptor that dies while the child is fine.
    /// Answering a challenge signs, and signing reads the clock.
    struct PresenceTripwire(FastClock);

    impl Clock for PresenceTripwire {
        fn now_millis(&self) -> u64 {
            assert_ne!(thread::current().name(), Some("presence-acceptor"), "presence thread killed");
            self.0.now_millis()
        }

        fn sleep(&self, duration: Duration) {
            self.0.sleep(duration);
        }
    }

    #[test]
    fn a_dead_presence_acceptor_fails_the_unit() {
        let fx = fixture(false);
        let sink = Arc::new(Sink::default());
        let child = Arc::new(Child::default());
        let ports = Ports {
            credentials: Arc::new(Credentials(BTreeMap::new())),
            clock: Arc::new(PresenceTripwire(FastClock(AtomicU64::new(1_000_000)))),
            sink: sink.clone(),
            lease: Arc::new(FakeLease::default()),
            spawner: Box::new(FakeSpawner(child.clone())),
            health: Box::new(|_| Arc::new(Health(AtomicBool::new(true)))),
        };
        let contract = fx.contract("x");
        let authority = fx.authority();
        let route = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = route.local_addr().unwrap();
        let tmp = fx.dir.path().join("tmp");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle =
            thread::spawn(move || run(&contract, authority, route, member(), &tmp, ports, &flag));
        wait_until("active", || sink.first("active").is_some());
        // The first challenge reaches the tripwire on the presence thread.
        assert!(!challenge_answered(address, &fx));
        assert_eq!(join_within(Duration::from_secs(20), handle), Outcome::Failed("presence-acceptor"));
        assert_eq!(sink.first("failed").unwrap().detail, "presence-acceptor");
        assert!(child.terminated.load(Ordering::SeqCst), "the child outlived the unit");
    }

    #[test]
    fn a_live_acceptor_keeps_presence_active() {
        let fx = fixture(false);
        let mut launch = start(&fx, fx.contract("x"), true);
        wait_until("two active heartbeats", || {
            launch.sink.states().iter().filter(|state| *state == "active").count() >= 2
        });
        assert!(launch.sink.first("degraded").is_none());
        launch.stop();
    }

    /// A store with a lease for `fx`, and Idunn's own driver over it.
    fn lease_store(
        fx: &Fixture,
        name: &str,
    ) -> (DriverLeaseReader, CultCacheWriteLeaseDriver, IdunnProcessWriteLeaseRecord) {
        let dir = fx.dir.path().join(name);
        fs::create_dir(&dir).unwrap();
        let path = dir.join("lease.cc");
        let lease = fx.lease(&digest('a'));
        write_record(
            &path,
            &lease.target,
            IdunnProcessWriteLeaseRecord::TYPE,
            cultnet_rs::IDUNN_PROCESS_WRITE_LEASE_SCHEMA,
            lease.canonical_bytes().unwrap(),
        );
        fs::write(dir.join("lease.cc.lock"), b"").unwrap();
        (
            DriverLeaseReader::new(&lease.target, &path),
            CultCacheWriteLeaseDriver::new(&lease.target, &path),
            lease,
        )
    }

    #[test]
    fn a_child_that_inherits_the_lease_lock_holds_it_after_the_launcher_is_gone() {
        let fx = fixture(true);
        let (reader, idunn, lease) = lease_store(&fx, "inherited");
        let guard = reader.hold_shared().unwrap();
        let mut child = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "exec sleep 30".into()], guard.descriptor())
            .unwrap();
        // The launcher dies: its own descriptor goes with it.
        drop(guard);
        // Held for a full second: a fork of some other test could pin the lock
        // for an instant, only the child under test keeps it that long.
        for _ in 0..10 {
            let denial = idunn.revoke_exact(Some(&lease)).unwrap_err();
            assert!(denial.to_string().contains("shared-lock consumer"), "the child does not hold the lock");
            thread::sleep(Duration::from_millis(100));
        }
        child.terminate(Duration::from_secs(5));
        wait_until("child gone", || child.exited().is_some());
        idunn.revoke_exact(Some(&lease)).unwrap();
    }

    #[test]
    fn a_child_spawned_without_the_descriptor_leaves_the_lock_with_the_launcher() {
        let fx = fixture(true);
        let (reader, idunn, lease) = lease_store(&fx, "uninherited");
        let guard = reader.hold_shared().unwrap();
        let mut child = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "exec sleep 30".into()], None)
            .unwrap();
        drop(guard);
        // Another test's fork may hold a copy for the instant before its exec;
        // the child under test would hold it for thirty seconds.
        wait_until("lock released", || idunn.revoke_exact(Some(&lease)).is_ok());
        child.terminate(Duration::from_secs(5));
    }

    #[test]
    fn the_launcher_hands_its_lease_descriptor_to_the_child_it_spawns() {
        let fx = fixture(true);
        let mut launch = start(&fx, fx.contract("x"), true);
        wait_until("warming", || launch.sink.first("warming").is_some());
        let warming = launch.sink.first("warming").unwrap();
        *launch.lease.0.lock().unwrap() = Some(fx.lease(&warming.canonical_sha256().unwrap()));
        wait_until("active", || launch.sink.first("active").is_some());
        assert_eq!(*launch.child.inherited.lock().unwrap(), [Some(FAKE_LOCK_FD)]);
        launch.stop();

        let fx = fixture(false);
        let mut launch = start(&fx, fx.contract("x"), true);
        wait_until("active", || launch.sink.first("active").is_some());
        assert_eq!(*launch.child.inherited.lock().unwrap(), [None]);
        launch.stop();
    }

    /// Unhealthy for its first `probes` calls, healthy from then on.
    struct LateHealth {
        calls: AtomicUsize,
        probes: usize,
    }

    impl HealthProbe for LateHealth {
        fn healthy(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst) >= self.probes
        }
    }

    #[test]
    fn a_child_that_becomes_healthy_late_is_not_killed_at_its_first_probe() {
        let fx = fixture(false);
        let mut contract = fx.contract("x");
        contract.startup = Duration::from_secs(60);
        let sink = Arc::new(Sink::default());
        let child = Arc::new(Child::default());
        let health = Arc::new(LateHealth { calls: AtomicUsize::new(0), probes: 5 });
        let ports = Ports {
            credentials: Arc::new(Credentials(BTreeMap::new())),
            clock: Arc::new(FastClock(AtomicU64::new(1_000_000))),
            sink: sink.clone(),
            lease: Arc::new(FakeLease::default()),
            spawner: Box::new(FakeSpawner(child.clone())),
            health: Box::new(move |_| health.clone()),
        };
        let authority = fx.authority();
        let route = TcpListener::bind("127.0.0.1:0").unwrap();
        let tmp = fx.dir.path().join("tmp");
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle =
            thread::spawn(move || run(&contract, authority, route, member(), &tmp, ports, &flag));
        wait_until("active", || sink.first("active").is_some());
        assert!(!child.terminated.load(Ordering::SeqCst), "killed while it was still starting");
        assert_eq!(child.spawns.load(Ordering::SeqCst), 1);
        assert!(sink.first("failed").is_none());
        stop.store(true, Ordering::SeqCst);
        assert_eq!(handle.join().unwrap(), Outcome::Stopped);
    }

    #[test]
    fn a_silent_child_fails_the_probe_within_its_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let member = listener.local_addr().unwrap();
        // Accepts and never answers.
        let held = thread::spawn(move || {
            let _connection = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(4));
        });
        let probe = HttpHealthProbe { member, path: "/health".into() };
        let started = Instant::now();
        assert!(!within(Duration::from_secs(5), move || probe.healthy()));
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "gave up before its timeout: {elapsed:?}");
        assert!(elapsed < HEALTH_PROBE_TIMEOUT + Duration::from_millis(200), "outlived its timeout: {elapsed:?}");
        drop(held);
    }

    #[test]
    fn the_probe_deadline_covers_the_whole_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let member = listener.local_addr().unwrap();
        // Every byte arrives inside any per-read timeout, but the head takes
        // three seconds in all.
        let dribbler = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for byte in b"HTTP/1.1 200 OK" {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
        let probe = HttpHealthProbe { member, path: "/health".into() };
        let started = Instant::now();
        assert!(!within(Duration::from_secs(5), move || probe.healthy()));
        let elapsed = started.elapsed();
        assert!(elapsed < HEALTH_PROBE_TIMEOUT + Duration::from_millis(200), "outlived its deadline: {elapsed:?}");
        join_within(Duration::from_secs(10), dribbler);
    }

    #[test]
    fn a_process_spawner_refuses_an_invalid_lease_descriptor_and_runs_no_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = format!("touch '{}'", marker.display());
        for invalid in [-1, 9999] {
            let refused = ProcessSpawner.spawn(
                Path::new("/bin/sh"),
                &["-c".to_owned(), script.clone()],
                Some(invalid),
            );
            assert!(refused.is_err(), "descriptor {invalid} was accepted");
        }
        thread::sleep(Duration::from_millis(300));
        assert!(!marker.exists(), "a child ran past an invalid descriptor");
        // Control: the same spawn without a descriptor runs the child.
        let mut child = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".to_owned(), script], None)
            .unwrap();
        wait_until("the control child to exit", || child.exited().is_some());
        assert!(marker.exists());
    }

    #[test]
    fn the_member_bind_is_a_loopback_address_other_than_the_candidate() {
        let candidate = "127.0.0.1:18835";
        assert_eq!(
            parse_member_bind("127.0.0.1:18845", candidate).unwrap(),
            "127.0.0.1:18845".parse().unwrap()
        );
        for bad in [
            "127.0.0.1:18835",
            "10.0.0.5:18845",
            "0.0.0.0:18845",
            "canary-not-an-address",
            "",
        ] {
            let error = parse_member_bind(bad, candidate).unwrap_err().to_string();
            assert!(!error.contains("canary") && !error.contains("10.0.0.5"), "{error}");
        }
        assert!(parse_member_bind("127.0.0.1:18845", "not-an-address").is_err());
    }
}
