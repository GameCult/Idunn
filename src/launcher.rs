//! idunn-launch: the unit's process for an upstream daemon that cannot speak
//! Idunn's runtime contract itself (Synapse, MAS).
//!
//! Idunn observes the unit; the launcher, inside it and supervising the child,
//! signs the presence statement with the provider identity and the activation
//! key. The launcher binds the candidate endpoint Idunn challenges, because the
//! child cannot answer a CultNet frame. It starts a stateful child only after
//! it holds the process write lease, renders the child's config from a closed
//! set of placeholders, and ends with the child.
//!
//! Errors, logs and refusals name a field and a code. They never echo an
//! argument, a template line, a path or a credential.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
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

use crate::deployment::RUNTIME_PRESENCE_IDENTITY_FD_NAME;
use crate::drivers::{CultCacheWriteLeaseDriver, lease_binds_candidate};

/// The route challenge is the only thing that begins with this byte: a
/// CultNet frame's 4-byte big-endian length is bounded at 1 MiB, and an HTTP
/// request starts with a method letter.
const CHALLENGE_FIRST_BYTE: u8 = 0x00;
const CHALLENGE_MAX_BYTES: usize = 1024 * 1024;
const CHALLENGE_IO_TIMEOUT: Duration = Duration::from_secs(3);
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(100);
const MAX_RECENT_WARMING_PROOFS: usize = 64;
const TERMINATE_GRACE: Duration = Duration::from_secs(20);
const ODIN_CULTMESH_CATALOG_CONNECTION_ID: u32 = 0x0d1d_0002;
const ODIN_CAPABILITY: &str = "odin.verse-rendezvous";

// ---------------------------------------------------------------- contract

/// Where the child listens. The launcher chooses the address; the child is
/// told it through `{{child.listen}}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildListener {
    UnixSocket,
    LoopbackPort,
}

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
    pub listener: ChildListener,
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
        let mut listener = None;
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
                "--listener" => {
                    listener = Some(match value.as_str() {
                        "unix" => ChildListener::UnixSocket,
                        "loopback" => ChildListener::LoopbackPort,
                        _ => bail!("launcher argument --listener is not unix or loopback"),
                    })
                }
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
            listener: listener.context("launcher argument --listener is required")?,
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

/// Systemd credentials as Idunn binds them: the environment variable `NAME`
/// holds the path of the credential file.
pub trait CredentialSource: Send + Sync {
    fn contents(&self, name: &str) -> Result<Vec<u8>>;
    fn path(&self, name: &str) -> Result<PathBuf>;
}

pub struct EnvCredentials;

impl CredentialSource for EnvCredentials {
    fn contents(&self, name: &str) -> Result<Vec<u8>> {
        fs::read(self.path(name)?).map_err(|_| anyhow!("a credential could not be read"))
    }

    fn path(&self, name: &str) -> Result<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("a credential is not bound"))
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
    fn hold_shared(&self) -> Result<Box<dyn Send>>;
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
    fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn ChildHandle>>;
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

struct SharedLockGuard {
    release: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for SharedLockGuard {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl LeaseReader for DriverLeaseReader {
    fn read(&self) -> Result<Option<IdunnProcessWriteLeaseRecord>> {
        self.driver
            .current_for_holder()
            .map(|current| current.map(|(_, lease)| lease))
            .map_err(|_| anyhow!("write-lease store refused custody, envelope or target"))
    }

    fn hold_shared(&self) -> Result<Box<dyn Send>> {
        let path = self.driver.record_path.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker = thread::spawn(move || {
            let held = SingleFileMessagePackBackingStore::new(&path).with_read_only_shared_snapshot(
                |_| {
                    let _ = ready_tx.send(true);
                    // Returns when the guard drops its sender.
                    let _ = release_rx.recv();
                    Ok(())
                },
            );
            if held.is_err() {
                let _ = ready_tx.send(false);
            }
        });
        if ready_rx.recv() != Ok(true) {
            let _ = worker.join();
            bail!("write-lease lock could not be held");
        }
        Ok(Box::new(SharedLockGuard {
            release: Some(release_tx),
            worker: Some(worker),
        }))
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

/// The child's private listener.
#[derive(Clone, Debug)]
pub enum ChildEndpoint {
    Unix(PathBuf),
    Loopback(SocketAddr),
}

pub enum ChildStream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl ChildEndpoint {
    pub fn connect(&self) -> io::Result<ChildStream> {
        match self {
            Self::Unix(path) => UnixStream::connect(path).map(ChildStream::Unix),
            Self::Loopback(address) => {
                TcpStream::connect_timeout(address, HEALTH_PROBE_TIMEOUT).map(ChildStream::Tcp)
            }
        }
    }

    /// What `{{child.listen}}` renders to.
    fn listen_value(&self) -> String {
        match self {
            Self::Unix(path) => path.display().to_string(),
            Self::Loopback(address) => address.to_string(),
        }
    }
}

impl ChildStream {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(Self::Unix),
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
        }
    }

    fn shutdown_write(&self) {
        let _ = match self {
            Self::Unix(stream) => stream.shutdown(Shutdown::Write),
            Self::Tcp(stream) => stream.shutdown(Shutdown::Write),
        };
    }

    fn set_timeout(&self, timeout: Duration) {
        let _ = match self {
            Self::Unix(stream) => stream
                .set_read_timeout(Some(timeout))
                .and_then(|()| stream.set_write_timeout(Some(timeout))),
            Self::Tcp(stream) => stream
                .set_read_timeout(Some(timeout))
                .and_then(|()| stream.set_write_timeout(Some(timeout))),
        };
    }
}

impl Read for ChildStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for ChildStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// `GET <path>` over the child's private listener; healthy on a 200.
pub struct HttpHealthProbe {
    pub endpoint: ChildEndpoint,
    pub path: String,
}

impl HealthProbe for HttpHealthProbe {
    fn healthy(&self) -> bool {
        let Ok(mut stream) = self.endpoint.connect() else {
            return false;
        };
        stream.set_timeout(HEALTH_PROBE_TIMEOUT);
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            self.path
        );
        let mut head = [0_u8; 12];
        stream.write_all(request.as_bytes()).is_ok()
            && stream.read_exact(&mut head).is_ok()
            && head.starts_with(b"HTTP/1.")
            && &head[8..12] == b" 200"
    }
}

pub struct ProcessSpawner;

struct ProcessChild(Child);

impl ChildSpawner for ProcessSpawner {
    fn spawn(&self, program: &Path, args: &[String]) -> Result<Box<dyn ChildHandle>> {
        let mut command = Command::new(program);
        // The child gets a bare environment: no Idunn variable, no candidate
        // bind, no credential path leaks into it.
        command.args(args).env_clear().stdin(Stdio::null());
        for name in ["PATH", "TMPDIR", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
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
    pub listen: &'a str,
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
        if key == "child.listen" {
            return Ok(self.listen.to_owned());
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
    _lock: Box<dyn Send>,
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
            _lock: lock,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
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

/// What the launcher currently is, shared with the route acceptor.
struct Shared {
    authority: Mutex<RuntimeAuthority>,
    phase: Mutex<Phase>,
    lease: Mutex<Option<LeaseHold>>,
    lease_required: bool,
    health: Arc<dyn HealthProbe>,
    clock: Arc<dyn Clock>,
}

impl Shared {
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
                let state = if self.health.healthy() && self.lease_current() {
                    "active"
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

/// Owns the candidate endpoint. A leading 0x00 is an Idunn challenge frame and
/// is answered here; every other byte is copied untouched to the child.
pub struct RouteAcceptor {
    listener: TcpListener,
    child: ChildEndpoint,
    responder: Responder,
}

impl RouteAcceptor {
    pub fn new(listener: TcpListener, child: ChildEndpoint, responder: Responder) -> Self {
        Self {
            listener,
            child,
            responder,
        }
    }

    pub fn serve(self, halt: Arc<AtomicBool>) -> Result<()> {
        self.listener.set_nonblocking(true)?;
        let acceptor = Arc::new(self);
        while !halt.load(Ordering::SeqCst) {
            match acceptor.listener.accept() {
                Ok((connection, _)) => {
                    let acceptor = Arc::clone(&acceptor);
                    thread::spawn(move || {
                        let _ = connection.set_nonblocking(false);
                        let _ = acceptor.handle(connection);
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        Ok(())
    }

    fn handle(&self, mut connection: TcpStream) -> Result<()> {
        let mut first = [0_u8; 1];
        if connection.read(&mut first)? == 0 {
            return Ok(());
        }
        if first[0] == CHALLENGE_FIRST_BYTE {
            return self.answer_challenge(&mut connection);
        }
        let mut upstream = self.child.connect()?;
        upstream.write_all(&first)?;
        let mut upstream_reader = upstream.try_clone()?;
        let mut client_reader = connection.try_clone()?;
        let forward = thread::spawn(move || {
            let _ = io::copy(&mut client_reader, &mut upstream);
            upstream.shutdown_write();
        });
        let _ = io::copy(&mut upstream_reader, &mut connection);
        let _ = connection.shutdown(Shutdown::Write);
        let _ = forward.join();
        Ok(())
    }

    fn answer_challenge(&self, connection: &mut TcpStream) -> Result<()> {
        connection.set_read_timeout(Some(CHALLENGE_IO_TIMEOUT))?;
        connection.set_write_timeout(Some(CHALLENGE_IO_TIMEOUT))?;
        let mut rest = [0_u8; 3];
        connection.read_exact(&mut rest)?;
        let length = u32::from_be_bytes([CHALLENGE_FIRST_BYTE, rest[0], rest[1], rest[2]]) as usize;
        ensure!(
            (1..=CHALLENGE_MAX_BYTES).contains(&length),
            "challenge frame is outside its bound"
        );
        let mut payload = vec![0_u8; length];
        connection.read_exact(&mut payload)?;
        let request =
            decode_cultnet_message_from_slice(&payload, CultNetWireContract::CultNetSchemaV0)?;
        let response = (self.responder)(request)?;
        let frame = encode_frame(&encode_cultnet_message_to_vec(
            &response,
            CultNetWireContract::CultNetSchemaV0,
        )?)?;
        connection.write_all(&frame)?;
        connection.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------- lifecycle

pub struct Ports {
    pub credentials: Arc<dyn CredentialSource>,
    pub clock: Arc<dyn Clock>,
    pub sink: Arc<dyn PresenceSink>,
    pub lease: Arc<dyn LeaseReader>,
    pub spawner: Box<dyn ChildSpawner>,
    /// Built from the endpoint `run` chooses.
    pub health: Box<dyn Fn(&ChildEndpoint) -> Arc<dyn HealthProbe> + Send>,
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

fn child_endpoint(listener: ChildListener, tmp: &Path) -> Result<ChildEndpoint> {
    Ok(match listener {
        ChildListener::UnixSocket => ChildEndpoint::Unix(tmp.join("child.sock")),
        ChildListener::LoopbackPort => {
            let probe = TcpListener::bind("127.0.0.1:0")?;
            ChildEndpoint::Loopback(probe.local_addr()?)
        }
    })
}

/// The unit's whole life: Warming, lease, child, Active, until the child or the
/// lease ends or SIGTERM arrives. `route` is the already-bound candidate
/// listener; `tmp` is the unit's private temporary directory.
pub fn run(
    contract: &LaunchContract,
    authority: RuntimeAuthority,
    route: TcpListener,
    tmp: &Path,
    ports: Ports,
    stop: &AtomicBool,
) -> Outcome {
    let expected = authority.expected().clone();
    let activation = authority.activation().clone();
    if expected.health_contract != contract.health_contract {
        return Outcome::Failed("health-contract-mismatch");
    }
    let Ok(endpoint) = child_endpoint(contract.listener, tmp) else {
        return Outcome::Failed("child-endpoint");
    };
    let health = (ports.health)(&endpoint);
    let shared = Arc::new(Shared {
        authority: Mutex::new(authority),
        phase: Mutex::new(Phase::Warming),
        lease: Mutex::new(None),
        lease_required: expected.write_lease_required,
        health: Arc::clone(&health),
        clock: Arc::clone(&ports.clock),
    });
    let halt = Arc::new(AtomicBool::new(false));
    let responder: Responder = {
        let shared = Arc::clone(&shared);
        Arc::new(move |request| shared.observe(request))
    };
    let serving = {
        let acceptor = RouteAcceptor::new(route, endpoint.clone(), responder);
        let halt = Arc::clone(&halt);
        thread::spawn(move || acceptor.serve(halt))
    };
    let outcome = lifecycle(contract, &expected, &activation, &endpoint, tmp, &ports, &shared, stop);
    halt.store(true, Ordering::SeqCst);
    let _ = serving.join();
    outcome
}

#[allow(clippy::too_many_arguments)]
fn lifecycle(
    contract: &LaunchContract,
    expected: &IdunnExpectedIncarnationRecord,
    activation: &IdunnRuntimeActivationRecord,
    endpoint: &ChildEndpoint,
    tmp: &Path,
    ports: &Ports,
    shared: &Shared,
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
    if expected.write_lease_required {
        let deadline = clock.now_millis() + contract.lease_wait.as_millis() as u64;
        loop {
            if stop.load(Ordering::SeqCst) {
                return Outcome::Stopped;
            }
            let held = LeaseHold::acquire(
                Arc::clone(&ports.lease),
                expected,
                activation,
                proofs.make_contiguous(),
            );
            if let Ok(hold) = held {
                lease_sha = Some(hold.sha256().to_owned());
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
    let Some(mut child) = start_child(contract, endpoint, tmp, ports) else {
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
        if shared.health.healthy() {
            break;
        }
        if clock.now_millis() >= startup_deadline {
            return fail(Some(&mut child), "startup-timeout");
        }
        clock.sleep(POLL);
    }

    // 4. Active, and heartbeats while it holds.
    set_phase(Phase::Active(lease_sha.clone()));
    publish("active", "child-healthy", lease_sha.clone());
    next_heartbeat = clock.now_millis() + heartbeat;
    loop {
        if stop.load(Ordering::SeqCst) {
            child.terminate(TERMINATE_GRACE);
            return Outcome::Stopped;
        }
        if child.exited().is_some() {
            return fail(None, "child-exited");
        }
        if !shared.lease_current() {
            return fail(Some(&mut child), "lease-lost");
        }
        let now = clock.now_millis();
        if now >= next_heartbeat {
            let state = if shared.health.healthy() { "active" } else { "degraded" };
            publish(state, "heartbeat", lease_sha.clone());
            next_heartbeat = now + heartbeat;
        }
        clock.sleep(POLL);
    }
}

fn start_child(
    contract: &LaunchContract,
    endpoint: &ChildEndpoint,
    tmp: &Path,
    ports: &Ports,
) -> Option<Box<dyn ChildHandle>> {
    let template = fs::read_to_string(contract.release_root.join(&contract.template)).ok()?;
    let listen = endpoint.listen_value();
    let rendered = ConfigRenderer {
        credentials: &*ports.credentials,
        listen: &listen,
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
        .spawn(&contract.release_root.join(&contract.child), &args)
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
                listener: ChildListener::UnixSocket,
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

        fn hold_shared(&self) -> Result<Box<dyn Send>> {
            Ok(Box::new(()))
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
        fn spawn(&self, _program: &Path, args: &[String]) -> Result<Box<dyn ChildHandle>> {
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
        let outcome = thread::spawn(move || run(&contract, authority, route, &tmp, ports, &flag));
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
        let template = "listen={{child.listen}}\nroot={{state_root}}\nkey={{credential:SYNAPSE_KEY}}\npath={{credential_path:SYNAPSE_KEY}}\n";
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
        let socket = fx.dir.path().join("tmp/child.sock");
        assert_eq!(
            rendered,
            format!(
                "listen={}\nroot=/var/lib/synapse\nkey=s3cret-value\npath=/run/credentials/unit/SYNAPSE_KEY\n",
                socket.display()
            )
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
            listen: "L",
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
        Arc::new(Shared {
            authority: Mutex::new(fx.authority()),
            phase: Mutex::new(Phase::Active(None)),
            lease: Mutex::new(None),
            lease_required: false,
            health,
            clock: Arc::new(SystemClock),
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
        assert_eq!(state_of(&shared), "active");
        health.0.store(false, Ordering::SeqCst);
        assert_eq!(state_of(&shared), "degraded");
    }

    #[test]
    fn route_challenge_is_answered_with_both_signatures_through_idunns_client() {
        let fx = fixture(false);
        let shared = shared(&fx, Arc::new(Health(AtomicBool::new(true))));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responder: Responder = Arc::new(move |request| shared.observe(request));
        let halt = Arc::new(AtomicBool::new(false));
        let acceptor = RouteAcceptor::new(
            listener,
            ChildEndpoint::Unix("/nonexistent".into()),
            responder,
        );
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
        serving.join().unwrap().unwrap();
    }

    #[test]
    fn member_bytes_reach_the_child_verbatim_and_only_challenge_frames_are_answered() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("child.sock");
        let child = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let request: Vec<u8> =
            b"POST /_matrix/x HTTP/1.1\r\nContent-Length: 6\r\n\r\n\x00\x01\x00\x02\x00\x03".to_vec();
        let response: Vec<u8> =
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n\x00\xff\x00\x10".to_vec();
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let fake = {
            let (received, connections, response, want) = (
                received.clone(),
                connections.clone(),
                response.clone(),
                request.len(),
            );
            thread::spawn(move || {
                for stream in child.incoming() {
                    let mut stream = stream.unwrap();
                    connections.fetch_add(1, Ordering::SeqCst);
                    let mut buffer = vec![0_u8; want];
                    if stream.read_exact(&mut buffer).is_err() {
                        continue;
                    }
                    *received.lock().unwrap() = buffer;
                    stream.write_all(&response).unwrap();
                    return;
                }
            })
        };
        let challenges = Arc::new(AtomicUsize::new(0));
        let responder: Responder = {
            let challenges = challenges.clone();
            Arc::new(move |_| {
                challenges.fetch_add(1, Ordering::SeqCst);
                bail!("not under test")
            })
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let halt = Arc::new(AtomicBool::new(false));
        let acceptor = RouteAcceptor::new(listener, ChildEndpoint::Unix(socket), responder);
        let serving = {
            let halt = halt.clone();
            thread::spawn(move || acceptor.serve(halt))
        };

        // A leading 0x00 that is not a challenge frame never reaches the child.
        let mut probe = TcpStream::connect(address).unwrap();
        probe.write_all(&[0x00, 0xff, 0xff, 0xff, 1, 2, 3]).unwrap();
        let mut answered = Vec::new();
        let _ = probe.read_to_end(&mut answered);
        assert!(answered.is_empty());
        assert_eq!(connections.load(Ordering::SeqCst), 0);

        // Everything else is copied untouched, both ways, 0x00 bytes included.
        let mut member = TcpStream::connect(address).unwrap();
        member.write_all(&request).unwrap();
        let mut answer = Vec::new();
        member.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, response);
        fake.join().unwrap();
        assert_eq!(*received.lock().unwrap(), request);
        assert_eq!(
            challenges.load(Ordering::SeqCst),
            0,
            "a malformed frame fails before the responder; member traffic never reaches it"
        );
        halt.store(true, Ordering::SeqCst);
        serving.join().unwrap().unwrap();
    }

    #[test]
    fn contract_parsing_refuses_escapes_and_echoes_nothing() {
        let base = |extra: &[&str]| -> Result<LaunchContract> {
            let mut args: Vec<String> = [
                "--child",
                "bin/c",
                "--template",
                "t",
                "--listener",
                "unix",
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
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("child.sock");
        let probe = HttpHealthProbe {
            endpoint: ChildEndpoint::Unix(socket.clone()),
            path: "/health".into(),
        };
        assert!(!probe.healthy(), "no listener is not healthy");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 64];
                let read = stream.read(&mut request).unwrap();
                assert!(request[..read].starts_with(b"GET /health HTTP/1.1\r\n"));
                write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\nOK").unwrap();
            }
        });
        assert!(probe.healthy());
        assert!(!probe.healthy());
        server.join().unwrap();
    }

    #[test]
    fn process_spawner_reports_exit_codes_and_terminates_a_running_child() {
        let mut gone = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "exit 3".into()])
            .unwrap();
        wait_until("child exit", || gone.exited().is_some());
        assert_eq!(gone.exited(), Some(3));
        let mut running = ProcessSpawner
            .spawn(Path::new("/bin/sh"), &["-c".into(), "sleep 30".into()])
            .unwrap();
        assert_eq!(running.exited(), None);
        running.terminate(Duration::from_secs(5));
        assert!(running.exited().is_some());
    }
}
