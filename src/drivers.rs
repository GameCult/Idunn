use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{
    CacheBackingStore, CultCacheEnvelope, CultCacheExpectedEnvelope, DatabaseEntry,
    SingleFileMessagePackBackingStore, TryCompareExchangeSnapshotOutcome,
};
use cultmesh_rs::{CultMeshRudpSnapshotOptions, request_raw_snapshot_from_rudp_catalog};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::control_plane::SequenceAdmittedWarming;
use crate::deployment::{
    ArtifactOutput, ArtifactSource, DockerRunnerBinding, GitlinkBinding,
    IDUNN_PROCESS_WRITE_LEASE_ENVIRONMENT, IDUNN_RUNTIME_BUNDLE_ENVIRONMENT,
    IDUNN_RUNTIME_CANDIDATE_BIND_ENVIRONMENT, LaunchArgument, OperatorBinding,
    RUNTIME_PRESENCE_IDENTITY_BINDING, RUNTIME_PRESENCE_IDENTITY_FD_NAME, RouteBinding,
    RouteDriver, SourceSelectionPolicy, TargetDeclaration, WorkloadNetwork,
};
use crate::deployment_plan::{
    ArtifactReceipt, CompiledDeploymentPlan, ExternalInputMaterializationReceipt, GitlinkTreeFact,
    SOURCE_SELECTION_FACTS_SCHEMA, SealedRelease, SourceSelection, SourceSelectionFacts,
};
use cultnet_rs::{
    CultNetMessage, CultNetRawPayloadEncoding, CultNetWireContract,
    GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA, GAMECULT_RUNTIME_PRESENCE_HEALTH_SIGNING_PURPOSE,
    GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA, GameCultProviderHealthIdentity,
    GameCultServiceTrustAnchorRecord, IDUNN_EXPECTED_INCARNATION_SCHEMA,
    IDUNN_PROCESS_WRITE_LEASE_SCHEMA, IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME,
    IDUNN_RUNTIME_ACTIVATION_SCHEMA, IdunnExpectedIncarnationRecord, IdunnProcessWriteLeaseRecord,
    IdunnRuntimeActivationLaunch, IdunnRuntimeActivationRecord, IdunnRuntimeActivationSigner,
    ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA, OdinRuntimeTopologyCorrelationRecord,
    ServiceIdentityProfile, ServiceIdentityTrustAnchor, decode_cultnet_message_from_slice,
    derive_service_identity_id, encode_cultnet_message_to_vec, encode_frame,
    open_service_identity_credential_reader,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitTreeEntry {
    mode: String,
    kind: String,
    object: String,
    path: PathBuf,
}

/// F2/F3 layer (b): the only way `materialize_tree_raw` is allowed to obtain
/// a directory to write into. `path` must already be a directory this
/// function created earlier in the same freeze (tracked in `created_dirs`,
/// seeded with `root` by the caller), or must not exist yet at all. Anything
/// else already at `path` — a symlink, a file, or a directory this freeze did
/// not itself create — is refused outright rather than traversed, which is
/// what a plain `fs::create_dir_all` would silently do through a symlink.
fn ensure_frozen_directory(
    root: &Path,
    created_dirs: &mut std::collections::HashSet<PathBuf>,
    path: &Path,
) -> Result<()> {
    if created_dirs.contains(path) {
        return Ok(());
    }
    ensure!(
        path.starts_with(root),
        "frozen source path {} escapes its root",
        path.display()
    );
    let parent = path
        .parent()
        .context("frozen source path has no parent")?;
    ensure_frozen_directory(root, created_dirs, parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => bail!(
            "frozen source writer refuses to write through an existing {} at {}",
            if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                "directory this freeze did not create"
            } else {
                "file"
            },
            path.display()
        ),
        Err(ref error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| format!("creating {}", path.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
    created_dirs.insert(path.to_path_buf());
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSource {
    pub facts: SourceSelectionFacts,
    pub recipe_bytes: Vec<u8>,
}

impl ResolvedSource {
    pub fn validate_against(&self, binding: &OperatorBinding) -> Result<()> {
        self.facts.validate_against(binding)?;
        ensure!(!self.recipe_bytes.is_empty(), "resolved recipe is empty");
        ensure!(
            sha256_id(&self.recipe_bytes) == self.facts.recipe_blob_sha256,
            "resolved recipe bytes differ from the selected Git object"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenSource {
    receipt: FrozenSourceReceipt,
    facts: SourceSelectionFacts,
    recipe_bytes: Vec<u8>,
    root: PathBuf,
}

impl FrozenSource {
    pub fn receipt(&self) -> &FrozenSourceReceipt {
        &self.receipt
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenSourceReceipt {
    pub transaction_id: String,
    pub plan_id: String,
    pub snapshot_sha256: String,
}

impl FrozenSourceReceipt {
    pub fn validate_against(&self, plan: &CompiledDeploymentPlan) -> Result<()> {
        plan.validate()?;
        require_driver_id(&self.transaction_id, "source transaction")?;
        ensure!(self.plan_id == plan.plan_id, "frozen source plan differs");
        require_sha256_id(&self.snapshot_sha256, "frozen source snapshot")
    }

    fn snapshot_component(&self) -> &str {
        self.snapshot_sha256
            .strip_prefix("sha256-")
            .expect("validated frozen source digest")
    }
}

pub trait SourcePort {
    fn resolve(
        &self,
        binding: &OperatorBinding,
        resolution_id: &str,
        selected_at_unix_millis: u64,
    ) -> Result<ResolvedSource>;

    fn freeze(
        &self,
        transaction_id: &str,
        plan: &CompiledDeploymentPlan,
    ) -> Result<FrozenSourceReceipt>;

    fn observe_frozen(
        &self,
        plan: &CompiledDeploymentPlan,
        receipt: &FrozenSourceReceipt,
    ) -> Result<FrozenSource>;

    fn cleanup(&self, transaction_id: &str, receipt: Option<&FrozenSourceReceipt>) -> Result<()>;
}

pub trait RunnerPort {
    fn materialize(
        &self,
        source: &FrozenSource,
        plan: &CompiledDeploymentPlan,
        staging_root: &Path,
        sealed_at_unix_millis: u64,
    ) -> Result<MaterializedRelease>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedRelease {
    pub release: SealedRelease,
    pub root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledReleaseObservation {
    pub sealed_release_id: String,
    pub root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceCredentialObservation {
    pub environment_name: String,
    pub delivered_path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub size: u64,
    pub sha256: String,
}

/// Root-owned source metadata for one descriptor that PID1 opens and passes
/// only to the service's initial process. The process must consume and close
/// descriptors 3 and 4 before spawning any child; no filesystem path is
/// projected into the workload environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentOnlyFileDescriptorObservation {
    pub fd_number: u32,
    pub fd_name: String,
    pub source_path: PathBuf,
    pub access: String,
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub links: u64,
    pub size: u64,
    pub sha256: String,
}

/// What a workload driver proved about the native process it started. One
/// variant per kind of host: a systemd unit on the Idunn host, or a process
/// on a managed host reported by that host's actuator. The wire form is
/// untagged so a transaction persisted by the previous Idunn, which knew only
/// the systemd shape, decodes unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkloadObservation {
    Systemd(SystemdWorkloadObservation),
    Host(HostWorkloadObservation),
}

impl WorkloadObservation {
    pub fn systemd(&self) -> Result<&SystemdWorkloadObservation> {
        match self {
            Self::Systemd(observation) => Ok(observation),
            Self::Host(_) => bail!("workload observation is a host process, not a systemd unit"),
        }
    }

    pub fn host(&self) -> Result<&HostWorkloadObservation> {
        match self {
            Self::Host(observation) => Ok(observation),
            Self::Systemd(_) => bail!("workload observation is a systemd unit, not a host process"),
        }
    }

    pub fn runtime_instance_id(&self) -> &str {
        match self {
            Self::Systemd(observation) => &observation.runtime_instance_id,
            Self::Host(observation) => &observation.runtime_instance_id,
        }
    }

    pub fn executable_sha256(&self) -> &str {
        match self {
            Self::Systemd(observation) => &observation.executable_sha256,
            Self::Host(observation) => &observation.executable_sha256,
        }
    }

    /// One line for a status listing: where the process is and how it is
    /// kept, in the vocabulary of its own driver.
    pub fn describe(&self) -> String {
        match self {
            Self::Systemd(observation) => format!(
                "unit={} restart={}",
                observation.unit, observation.restart_policy
            ),
            Self::Host(observation) => format!(
                "host={} pid={} created={}",
                observation.host, observation.process_id, observation.process_creation_time
            ),
        }
    }

    /// Two processes running at once must be distinct at the boundary their
    /// host actually enforces. On the Idunn host that is UID, PID namespace
    /// and mount namespace. On a managed host it is the process itself:
    /// pid and creation time, which the kernel will not hand to two live
    /// processes. An incumbent of the other kind cannot be compared and is
    /// refused rather than assumed isolated.
    pub fn prove_isolation(
        candidate: &Self,
        incumbent: Option<&Self>,
    ) -> Result<IsolationEvidence> {
        match (candidate, incumbent) {
            (Self::Systemd(candidate), None) => {
                candidate.require_private_identity("candidate")?;
                Ok(IsolationEvidence::Linux(LinuxIsolationEvidence {
                    candidate_uid: candidate.process_uids[0],
                    candidate_pid_namespace_id: candidate.pid_namespace_id,
                    candidate_mount_namespace_id: candidate.mount_namespace_id,
                    incumbent_uid: None,
                    incumbent_pid_namespace_id: None,
                    incumbent_mount_namespace_id: None,
                }))
            }
            (Self::Systemd(candidate), Some(Self::Systemd(incumbent))) => {
                candidate.require_private_identity("candidate")?;
                incumbent.require_private_identity("incumbent")?;
                ensure!(
                    candidate.process_uids[0] != incumbent.process_uids[0]
                        && candidate.pid_namespace_id != incumbent.pid_namespace_id
                        && candidate.mount_namespace_id != incumbent.mount_namespace_id,
                    "candidate and incumbent are not distinct by UID, PID namespace, and mount namespace"
                );
                Ok(IsolationEvidence::Linux(LinuxIsolationEvidence {
                    candidate_uid: candidate.process_uids[0],
                    candidate_pid_namespace_id: candidate.pid_namespace_id,
                    candidate_mount_namespace_id: candidate.mount_namespace_id,
                    incumbent_uid: Some(incumbent.process_uids[0]),
                    incumbent_pid_namespace_id: Some(incumbent.pid_namespace_id),
                    incumbent_mount_namespace_id: Some(incumbent.mount_namespace_id),
                }))
            }
            (Self::Host(candidate), None) => {
                candidate.require_live("candidate")?;
                Ok(IsolationEvidence::Host(HostIsolationEvidence {
                    candidate_process_id: candidate.process_id,
                    candidate_process_creation_time: candidate.process_creation_time,
                    incumbent_process_id: None,
                    incumbent_process_creation_time: None,
                }))
            }
            (Self::Host(candidate), Some(Self::Host(incumbent))) => {
                candidate.require_live("candidate")?;
                incumbent.require_live("incumbent")?;
                ensure!(
                    candidate.host == incumbent.host,
                    "candidate and incumbent are on different hosts"
                );
                ensure!(
                    (candidate.process_id, candidate.process_creation_time)
                        != (incumbent.process_id, incumbent.process_creation_time),
                    "candidate and incumbent are the same host process"
                );
                Ok(IsolationEvidence::Host(HostIsolationEvidence {
                    candidate_process_id: candidate.process_id,
                    candidate_process_creation_time: candidate.process_creation_time,
                    incumbent_process_id: Some(incumbent.process_id),
                    incumbent_process_creation_time: Some(incumbent.process_creation_time),
                }))
            }
            _ => {
                bail!("candidate and incumbent were observed by different kinds of workload driver")
            }
        }
    }
}

/// Isolation proof, one shape per kind of host. Untagged for the same reason
/// as the observation: persisted Linux evidence decodes unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IsolationEvidence {
    Linux(LinuxIsolationEvidence),
    Host(HostIsolationEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxIsolationEvidence {
    pub candidate_uid: u32,
    pub candidate_pid_namespace_id: u64,
    pub candidate_mount_namespace_id: u64,
    pub incumbent_uid: Option<u32>,
    pub incumbent_pid_namespace_id: Option<u64>,
    pub incumbent_mount_namespace_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIsolationEvidence {
    pub candidate_process_id: u32,
    pub candidate_process_creation_time: u64,
    pub incumbent_process_id: Option<u32>,
    pub incumbent_process_creation_time: Option<u64>,
}

/// A process on a managed host as its actuator proved it. `process_creation_time`
/// is the host's own clock for process creation (Windows: FILETIME ticks); the
/// pair with `process_id` names exactly one process for the host's lifetime.
/// `exit_code` is `Some` once the process is gone; nothing restarts it, so an
/// exited observation is permanently stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostWorkloadObservation {
    pub host: String,
    pub actuator_identity_id: String,
    pub process_id: u32,
    pub process_creation_time: u64,
    pub session_id: u32,
    pub user_sid: String,
    pub executable: String,
    pub executable_sha256: String,
    pub command_line_sha256: String,
    pub environment_names: Vec<String>,
    pub environment_contract_sha256: String,
    pub runtime_bundle: String,
    pub runtime_instance_id: String,
    pub activation_signer_identity_id: String,
    pub activation_signer_public_key: Vec<u8>,
    pub exit_code: Option<u32>,
}

impl HostWorkloadObservation {
    fn require_live(&self, role: &str) -> Result<()> {
        ensure!(self.process_id > 0, "{role} has no host process id");
        ensure!(
            self.process_creation_time > 0,
            "{role} has no host process creation time"
        );
        ensure!(self.exit_code.is_none(), "{role} host process has exited");
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemdWorkloadObservation {
    pub unit: String,
    pub unit_description: String,
    pub invocation_id: String,
    pub exec_main_start_timestamp_monotonic: u64,
    pub service_type: String,
    pub restart_policy: String,
    pub kill_mode: String,
    pub dynamic_user: bool,
    pub systemd_user: String,
    pub systemd_group: String,
    pub supplementary_groups: String,
    pub capability_bounding_set: String,
    pub ambient_capabilities: String,
    pub private_mounts: bool,
    pub private_pids: bool,
    pub protect_proc: String,
    pub proc_subset: String,
    pub no_new_privileges: bool,
    pub umask: String,
    pub inaccessible_paths: String,
    pub load_credential: String,
    pub main_pid: u32,
    pub process_start_time: u64,
    pub process_uids: [u32; 4],
    pub process_gids: [u32; 4],
    pub process_groups: Vec<u32>,
    pub process_cap_inheritable: u64,
    pub process_cap_permitted: u64,
    pub process_cap_effective: u64,
    pub process_cap_bounding: u64,
    pub process_cap_ambient: u64,
    pub process_no_new_privileges: bool,
    pub process_namespace_pids: Vec<u32>,
    pub mount_namespace_id: u64,
    pub pid_namespace_id: u64,
    pub executable: PathBuf,
    pub executable_device: u64,
    pub executable_inode: u64,
    pub executable_sha256: String,
    pub runtime_instance_id: String,
    pub working_directory: PathBuf,
    pub runtime_bundle: PathBuf,
    pub command_line_sha256: String,
    pub environment_names: Vec<String>,
    pub environment_contract_sha256: String,
    pub control_group: String,
    pub credentials_directory: Option<PathBuf>,
    pub parent_only_file_descriptors: Vec<ParentOnlyFileDescriptorObservation>,
    pub activation_signer_identity_id: String,
    pub activation_signer_public_key: Vec<u8>,
    pub service_credentials: Vec<ServiceCredentialObservation>,
}

impl SystemdWorkloadObservation {
    fn require_private_identity(&self, role: &str) -> Result<()> {
        ensure!(
            self.dynamic_user
                && self.private_pids
                && self.private_mounts
                && self.process_uids[0] > 0
                && self.pid_namespace_id > 0
                && self.mount_namespace_id > 0,
            "{role} lacks dynamic identity or private namespaces"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LinuxProcessSecurityObservation {
    uids: [u32; 4],
    gids: [u32; 4],
    groups: Vec<u32>,
    cap_inheritable: u64,
    cap_permitted: u64,
    cap_effective: u64,
    cap_bounding: u64,
    cap_ambient: u64,
    no_new_privileges: bool,
    namespace_pids: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SystemdUnitObservation {
    properties: BTreeMap<String, String>,
    open_files: Vec<String>,
}

pub trait WorkloadPort {
    fn install(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &MaterializedRelease,
    ) -> Result<InstalledReleaseObservation>;

    fn prepare_activation(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        launch: IdunnRuntimeActivationLaunch,
    ) -> Result<IdunnRuntimeActivationRecord>;

    fn start_prepared(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &SealedRelease,
        installed: &InstalledReleaseObservation,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<WorkloadObservation>;

    fn discard_prepared(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()>;

    fn observe(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        prior: &WorkloadObservation,
    ) -> Result<WorkloadObservation>;

    fn stop(&self, observation: &WorkloadObservation) -> Result<()>;

    /// True when this candidate can never run again without a new transaction.
    ///
    /// Idunn launches candidates as transient units with `Restart=no`, so a
    /// unit that has failed, or that systemd no longer knows, will not come
    /// back on its own. Past the fence every error is otherwise treated as
    /// resumable and retried forever, which is right while a candidate can
    /// still recover and a trap once it cannot.
    fn is_permanently_stopped(&self, observation: &WorkloadObservation) -> Result<bool>;
}

pub trait TopologyPort {
    fn publish_expected(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<String>;
    /// Remove every record of one incarnation. Other incarnations of the same
    /// target, and the target's anchor while any remain, are untouched.
    fn withdraw_incarnation(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
        activation: Option<&IdunnRuntimeActivationRecord>,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<()>;
    fn publish_observed_activation(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        observation: &WorkloadObservation,
    ) -> Result<String>;
    fn publish_process_write_lease(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<String>;
    fn withdraw_process_write_lease(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<()>;
    /// Odin's correlation for one exact incarnation. A correlation about another
    /// incarnation of the same target is not this one's evidence and is never
    /// returned for it.
    fn receive(
        &self,
        target: &str,
        expected_sha256: &str,
    ) -> Result<Option<ReceivedOdinTopologyCorrelation>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedOdinTopologyCorrelation {
    pub target: String,
    pub expected_sha256: String,
    pub canonical_bytes: Vec<u8>,
}

pub trait WriteLeasePort {
    fn revoke_exact(&self, incumbent: Option<&IdunnProcessWriteLeaseRecord>) -> Result<()>;
    fn observe_empty(&self) -> Result<bool>;
    fn observe_exact(&self, lease: &IdunnProcessWriteLeaseRecord) -> Result<bool>;
    fn grant(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<String>;
    fn observe(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<bool>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteObservation {
    pub route_id: String,
    pub runtime_instance_id: String,
    pub membership_sha256: String,
    pub signed_presence_sha256: String,
    pub observed_at_unix_millis: u64,
}

impl RouteObservation {
    pub fn validate(&self) -> Result<()> {
        require_driver_id(&self.route_id, "route observation")?;
        require_sha256_id(&self.runtime_instance_id, "route runtime instance")?;
        require_sha256_id(&self.membership_sha256, "route membership")?;
        require_sha256_id(&self.signed_presence_sha256, "route signed presence")?;
        ensure!(
            self.observed_at_unix_millis > 0,
            "route observation has no trusted receipt time"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutePreflightReceipt {
    pub route_id: String,
    pub candidate_runtime_instance_id: String,
    pub candidate_membership_sha256: String,
    pub incumbent_runtime_instance_id: Option<String>,
    pub incumbent_membership_sha256: Option<String>,
    pub incumbent_configuration: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSnapshotResponse {
    pub message_id: String,
    pub canonical_presence: Vec<u8>,
}

const ROUTE_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(3);
const ROUTE_SNAPSHOT_MAX_BYTES: usize = 1024 * 1024;
const ROUTE_HTTP_MAX_HEADER_BYTES: usize = 32 * 1024;
const ROUTE_HTTP_SNAPSHOT_PATH: &str = "/cultnet/snapshot";

impl RoutePreflightReceipt {
    pub fn validate(&self) -> Result<()> {
        require_driver_id(&self.route_id, "route preflight")?;
        require_sha256_id(
            &self.candidate_runtime_instance_id,
            "candidate route runtime instance",
        )?;
        require_sha256_id(
            &self.candidate_membership_sha256,
            "candidate route membership",
        )?;
        match (
            &self.incumbent_runtime_instance_id,
            &self.incumbent_membership_sha256,
            &self.incumbent_configuration,
        ) {
            (None, None, None) => {}
            (Some(runtime), Some(membership), Some(configuration)) => {
                require_sha256_id(runtime, "incumbent route runtime instance")?;
                require_sha256_id(membership, "incumbent route membership")?;
                ensure!(
                    !configuration.is_empty(),
                    "incumbent route configuration is empty"
                );
                ensure!(
                    sha256_id(configuration) == *membership,
                    "incumbent route configuration differs from its membership digest"
                );
            }
            _ => bail!("incumbent route preflight evidence is partial"),
        }
        Ok(())
    }
}

/// The narrow shape `freeze_exact` needs to fetch and archive a repository at
/// an exact revision. It carries no operator authority (no minimum revision,
/// selection policy, or runners): callers that hold an `OperatorBinding`
/// derive it with `exact_source_from`, and a future verify binding lowers to
/// it directly. Cut 1 shares this type between deploy's freeze and (later)
/// verify; it must not grow a second shape for the same fetch-and-archive
/// core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactSource {
    pub origin: String,
    pub checkout: PathBuf,
    pub gitlinks: BTreeMap<PathBuf, GitlinkBinding>,
    pub recipe_path: PathBuf,
}

fn exact_source_from(binding: &OperatorBinding) -> ExactSource {
    ExactSource {
        origin: binding.repository.origin.clone(),
        checkout: binding.repository.checkout.clone(),
        gitlinks: binding.repository.gitlinks.clone(),
        recipe_path: binding.repository.recipe_path.clone(),
    }
}

/// Fixed-argv Git source driver. It never interprets recipe text as a command
/// and never derives source policy from the target repository. The configured
/// identity performs every Git/network read; root Idunn writes only exact Git
/// blob content into a separate transaction-owned immutable store — no
/// `git archive`, so no attribute-driven transform runs on the way out.
pub struct GitSourceDriver {
    pub source_cache_root: PathBuf,
    pub frozen_source_root: PathBuf,
    pub identity: Option<ProcessIdentity>,
    pub git_program: PathBuf,
}

impl GitSourceDriver {
    pub fn new(
        source_cache_root: impl Into<PathBuf>,
        frozen_source_root: impl Into<PathBuf>,
        identity: Option<ProcessIdentity>,
    ) -> Self {
        Self {
            source_cache_root: source_cache_root.into(),
            frozen_source_root: frozen_source_root.into(),
            identity,
            git_program: PathBuf::from("/usr/bin/git"),
        }
    }

    fn git_command<I, S>(&self, args: I) -> Result<Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure!(
            self.git_program.is_absolute(),
            "Git source driver program is not absolute"
        );
        let mut command = Command::new(&self.git_program);
        command
            .args(args)
            .stdin(Stdio::null())
            .env_clear()
            .env("HOME", self.source_cache_root.join(".home"))
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LANG", "C.UTF-8");
        apply_identity(&mut command, self.identity)?;
        Ok(command)
    }

    fn git<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git_command(args)?.output().with_context(|| {
            format!(
                "starting fixed-argv Git driver {}",
                self.git_program.display()
            )
        })?;
        if !output.status.success() {
            bail!(
                "Git source driver exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    fn git_text<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git(args)?;
        let value = String::from_utf8(output.stdout).context("Git emitted non-UTF-8 text")?;
        Ok(value.trim().to_owned())
    }

    fn ensure_checkout(&self, source: &ExactSource) -> Result<()> {
        let checkout = &source.checkout;
        if !checkout.exists() {
            self.git([
                OsString::from("-c"),
                OsString::from("transfer.fsckObjects=true"),
                OsString::from("clone"),
                OsString::from("--filter=blob:none"),
                OsString::from("--no-checkout"),
                OsString::from("--origin"),
                OsString::from("origin"),
                source.origin.clone().into(),
                checkout.as_os_str().to_owned(),
            ])?;
        }
        let checkout_metadata = fs::symlink_metadata(checkout)
            .with_context(|| format!("inspecting source checkout {}", checkout.display()))?;
        ensure!(
            checkout_metadata.is_dir() && !checkout_metadata.file_type().is_symlink(),
            "source checkout is not a native directory"
        );
        let canonical_checkout = checkout.canonicalize()?;
        ensure!(
            canonical_checkout == *checkout,
            "source checkout traverses a symlink or noncanonical path"
        );
        let git_metadata = fs::symlink_metadata(checkout.join(".git"))?;
        ensure!(
            git_metadata.is_dir() && !git_metadata.file_type().is_symlink(),
            "source checkout has no native Git object directory"
        );
        let actual_origin = self.git_text([
            OsString::from("-C"),
            checkout.as_os_str().to_owned(),
            OsString::from("remote"),
            OsString::from("get-url"),
            OsString::from("origin"),
        ])?;
        ensure!(
            actual_origin == source.origin,
            "source checkout origin differs from the operator binding"
        );
        Ok(())
    }

    fn admitted_ref_name(binding: &OperatorBinding, resolution_id: &str) -> Result<String> {
        require_driver_id(resolution_id, "source resolution")?;
        Ok(format!(
            "refs/idunn/resolutions/{}/{}",
            binding.target, resolution_id
        ))
    }

    fn resolve_selected_revision(
        &self,
        binding: &OperatorBinding,
        admitted_ref_revision: &str,
    ) -> Result<(String, SourceSelection)> {
        let checkout = &binding.repository.checkout;
        let (revision, selection) = match binding.repository.selection {
            SourceSelectionPolicy::PinnedObject => (
                binding
                    .repository
                    .pinned_revision
                    .clone()
                    .context("pinned source binding lost its exact revision")?,
                SourceSelection::PinnedObject,
            ),
            SourceSelectionPolicy::RefHead => {
                (admitted_ref_revision.to_owned(), SourceSelection::RefHead)
            }
            SourceSelectionPolicy::SignedRelease => {
                bail!("signed-release selection requires a release-authority source port")
            }
        };
        require_git_sha(&revision, "selected source revision")?;
        self.git([
            OsString::from("-C"),
            checkout.as_os_str().to_owned(),
            OsString::from("merge-base"),
            OsString::from("--is-ancestor"),
            binding.repository.minimum_revision.clone().into(),
            revision.clone().into(),
        ])
        .context("selected source is below the operator minimum revision")?;
        self.git([
            OsString::from("-C"),
            checkout.as_os_str().to_owned(),
            OsString::from("merge-base"),
            OsString::from("--is-ancestor"),
            revision.clone().into(),
            admitted_ref_revision.into(),
        ])
        .context("selected source is outside the fetched admitted ref")?;
        Ok((revision, selection))
    }

    fn git_tree_entries(&self, repository: &Path, revision: &str) -> Result<Vec<GitTreeEntry>> {
        let output = self
            .git([
                OsString::from("-C"),
                repository.as_os_str().to_owned(),
                OsString::from("ls-tree"),
                OsString::from("-r"),
                OsString::from("-z"),
                revision.into(),
            ])?
            .stdout;
        let mut entries = Vec::new();
        for record in output
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("Git tree record has no path delimiter")?;
            let header = std::str::from_utf8(&record[..tab])
                .context("Git tree record header is not UTF-8")?;
            let path =
                std::str::from_utf8(&record[tab + 1..]).context("Git tree path is not UTF-8")?;
            let mut fields = header.split(' ');
            let mode = fields.next().context("Git tree record has no mode")?;
            let kind = fields.next().context("Git tree record has no kind")?;
            let object = fields.next().context("Git tree record has no object")?;
            ensure!(fields.next().is_none(), "Git tree record has extra fields");
            require_git_sha(object, "Git tree object")?;
            let path = PathBuf::from(path);
            let normalized = normalized_relative(&path)?;
            ensure!(
                normalized == path.to_string_lossy(),
                "Git tree path is not normalized"
            );
            ensure!(
                path.components().all(|component| !matches!(
                    component,
                    std::path::Component::Normal(value) if value == ".git"
                )),
                "Git tree contains forbidden .git metadata"
            );
            entries.push(GitTreeEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                object: object.to_owned(),
                path,
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        ensure!(
            entries.windows(2).all(|pair| pair[0].path != pair[1].path),
            "Git tree emits a path twice"
        );
        // S7 (Self's ruling, third Cut 1 fix batch): the case-insensitive
        // collision/alias refusal that used to run here is deleted. The host
        // is case-sensitive ext4, so a case-fold collision was never a real
        // write hazard, and the check was inconsistent (leaf paths only, a
        // plain `to_lowercase` rather than Unicode case folding or
        // normalization). `.git` look-alikes are now refused by `git fsck`
        // itself (S1, `resolve()` and `freeze_exact`), independent of case.
        // An exact-name alias (a leaf `a` sharing a root with a tree that
        // recurses to `a/pwn`) is still refused, at write time, by
        // `ensure_frozen_directory` below: it never writes through an
        // existing entry, symlink or otherwise.
        Ok(entries)
    }

    fn exact_recipe_and_gitlinks(
        &self,
        source: &ExactSource,
        revision: &str,
    ) -> Result<(Vec<u8>, BTreeMap<PathBuf, GitlinkTreeFact>)> {
        let entries = self.git_tree_entries(&source.checkout, revision)?;
        let recipe = entries
            .iter()
            .find(|entry| entry.path == source.recipe_path)
            .context("selected tree has no deployment recipe")?;
        ensure!(
            matches!(recipe.mode.as_str(), "100644" | "100755") && recipe.kind == "blob",
            "deployment recipe is not a regular Git blob"
        );
        let recipe_bytes = self
            .git([
                OsString::from("-C"),
                source.checkout.as_os_str().to_owned(),
                OsString::from("cat-file"),
                OsString::from("blob"),
                recipe.object.clone().into(),
            ])?
            .stdout;
        ensure!(
            !recipe_bytes.is_empty(),
            "selected deployment recipe is empty"
        );

        let observed_gitlinks = entries
            .iter()
            .filter(|entry| entry.mode == "160000")
            .map(|entry| {
                ensure!(entry.kind == "commit", "Gitlink has the wrong object kind");
                Ok((entry.path.clone(), entry.object.clone()))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let expected_paths = source
            .gitlinks
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let observed_paths = observed_gitlinks
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(
            observed_paths == expected_paths,
            "selected tree Gitlinks do not exactly match operator bindings"
        );
        let gitlinks = observed_gitlinks
            .into_iter()
            .map(|(path, revision)| {
                let origin = source.gitlinks[&path].origin.clone();
                (path, GitlinkTreeFact { origin, revision })
            })
            .collect();
        Ok((recipe_bytes, gitlinks))
    }

    fn prepare_source_root(&self, source: &ExactSource) -> Result<()> {
        #[cfg(unix)]
        ensure!(
            unsafe { libc::geteuid() } != 0 || self.identity.is_some(),
            "root Idunn must configure an unprivileged source identity"
        );
        ensure!(
            source.checkout.starts_with(&self.source_cache_root)
                && source.checkout != self.source_cache_root,
            "repository checkout is outside Idunn's source authority root"
        );
        ensure_source_directory(&self.source_cache_root, self.identity)?;
        ensure!(
            self.source_cache_root.canonicalize()? == self.source_cache_root,
            "source cache root traverses a symlink or noncanonical path"
        );
        ensure_source_directory(&self.source_cache_root.join(".home"), self.identity)?;
        ensure_source_directory_tree(
            &self.source_cache_root,
            source
                .checkout
                .parent()
                .context("repository checkout has no parent directory")?,
            self.identity,
        )?;
        self.ensure_checkout(source)
    }

    fn verify_exact_source(&self, source: &ExactSource, resolved: &ResolvedSource) -> Result<()> {
        let actual_tree = self.git_text([
            OsString::from("-C"),
            source.checkout.as_os_str().to_owned(),
            OsString::from("rev-parse"),
            OsString::from(format!("{}^{{tree}}", resolved.facts.revision)),
        ])?;
        ensure!(
            actual_tree == resolved.facts.source_tree,
            "selected revision no longer resolves to the frozen source tree"
        );
        let (recipe_bytes, gitlinks) =
            self.exact_recipe_and_gitlinks(source, &resolved.facts.revision)?;
        ensure!(
            recipe_bytes == resolved.recipe_bytes,
            "selected recipe object differs from the durable source resolution"
        );
        ensure!(
            gitlinks == resolved.facts.gitlinks,
            "Gitlinks differ from the durable source resolution"
        );
        Ok(())
    }

    /// F1: fetches every object in `objects` in one bulk round trip, instead
    /// of letting a partial (`blob:none`) clone lazily fetch each missing
    /// blob one at a time when `stream_blobs` later reads it. `--stdin`
    /// (Git 2.36+) takes the object list off argv, so this holds for
    /// thousands of objects without an argv-length limit. Objects the
    /// checkout already has are asked for again; Git answers from the local
    /// pack without a network round trip for those, which is cheap next to
    /// the round trips this replaces. `transfer.fsckObjects=true` is defense
    /// in depth: Git itself refuses a fetched tree with duplicate names
    /// before any of it reaches disk, independent of the explicit `fsck`
    /// call S1 added around the revision this bulk fetch serves.
    fn bulk_fetch_objects(&self, repository: &Path, objects: &[String]) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        // A repository with no `origin` remote is necessarily self-contained
        // (every real checkout this driver creates has one); skip the fetch
        // rather than fail on a repository that already holds everything.
        let has_origin = self
            .git([
                OsString::from("-C"),
                repository.as_os_str().to_owned(),
                OsString::from("remote"),
                OsString::from("get-url"),
                OsString::from("origin"),
            ])
            .is_ok();
        if !has_origin {
            return Ok(());
        }
        let mut command = self.git_command([
            OsString::from("-c"),
            OsString::from("transfer.fsckObjects=true"),
            OsString::from("-C"),
            repository.as_os_str().to_owned(),
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from("--no-write-fetch-head"),
            OsString::from("--stdin"),
            OsString::from("origin"),
        ])?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("starting bulk Git fetch")?;
        let mut stdin = child.stdin.take().context("bulk Git fetch has no stdin")?;
        let request: Vec<u8> = objects
            .iter()
            .flat_map(|object| {
                let mut line = object.as_bytes().to_vec();
                line.push(b'\n');
                line
            })
            .collect();
        let writer = std::thread::spawn(move || -> std::io::Result<()> { stdin.write_all(&request) });
        let output = child
            .wait_with_output()
            .context("waiting for bulk Git fetch to exit")?;
        let write_result = writer
            .join()
            .map_err(|_| anyhow!("bulk Git fetch stdin writer panicked"))?;
        write_result.context("writing bulk Git fetch requests")?;
        ensure!(
            output.status.success(),
            "bulk Git fetch exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }

    /// Streams every named blob object's raw bytes from `repository` through
    /// one `git cat-file --batch` process, with no attribute-driven
    /// transform of any kind, calling `on_object(object, reader, size)` once
    /// per object **in request order** with a reader positioned at exactly
    /// `size` bytes of that object's content; `on_object` must consume all
    /// of it. F9: stderr is drained on a dedicated thread and the child is
    /// waited on unconditionally, on every return path including an error
    /// from `on_object`, so a failure here never leaves a zombie process or
    /// a stalled pipe.
    fn stream_blobs(
        &self,
        repository: &Path,
        objects: &[String],
        mut on_object: impl FnMut(&str, &mut dyn Read, usize) -> Result<()>,
    ) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        let mut command = self.git_command([
            OsString::from("-C"),
            repository.as_os_str().to_owned(),
            OsString::from("cat-file"),
            OsString::from("--batch"),
        ])?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("starting Git cat-file --batch")?;
        let mut stdin = child
            .stdin
            .take()
            .context("Git cat-file --batch has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Git cat-file --batch has no stdout")?;
        let mut stderr = child
            .stderr
            .take()
            .context("Git cat-file --batch has no stderr")?;
        let request: Vec<u8> = objects
            .iter()
            .flat_map(|object| {
                let mut line = object.as_bytes().to_vec();
                line.push(b'\n');
                line
            })
            .collect();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            stdin.write_all(&request)
            // `stdin` drops here, closing the pipe so Git sees end of input.
        });
        let stderr_reader = std::thread::spawn(move || -> Vec<u8> {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            buffer
        });
        let body: Result<()> = (|| {
            let mut reader = std::io::BufReader::new(stdout);
            for object in objects {
                let mut header = Vec::new();
                reader
                    .read_until(b'\n', &mut header)
                    .context("reading a Git cat-file --batch header")?;
                ensure!(
                    header.last() == Some(&b'\n'),
                    "Git cat-file --batch closed before answering every object"
                );
                header.pop();
                let header = String::from_utf8(header)
                    .context("Git cat-file --batch header is not UTF-8")?;
                let mut fields = header.split(' ');
                let returned_object = fields
                    .next()
                    .context("Git cat-file --batch header names no object")?;
                ensure!(
                    returned_object == object,
                    "Git cat-file --batch answered objects out of the requested order"
                );
                let kind_or_missing = fields
                    .next()
                    .context("Git cat-file --batch header is malformed")?;
                ensure!(
                    kind_or_missing != "missing",
                    "Git object {object} is missing from the repository"
                );
                let size: usize = fields
                    .next()
                    .context("Git cat-file --batch header has no size")?
                    .parse()
                    .context("Git cat-file --batch size is not a number")?;
                ensure!(
                    fields.next().is_none(),
                    "Git cat-file --batch header has extra fields"
                );
                on_object(object, &mut reader, size)?;
                let mut trailer = [0u8; 1];
                reader
                    .read_exact(&mut trailer)
                    .context("reading the newline after a Git cat-file --batch object")?;
                ensure!(
                    trailer[0] == b'\n',
                    "Git cat-file --batch object content ran past its declared size"
                );
            }
            Ok(())
        })();
        let stderr_bytes = stderr_reader.join().unwrap_or_default();
        let write_result = writer.join().map_err(|_| anyhow!("Git cat-file --batch stdin writer panicked"));
        let status = child
            .wait()
            .context("waiting for Git cat-file --batch to exit")?;
        body?;
        write_result?.context("writing Git cat-file --batch requests")?;
        ensure!(
            status.success(),
            "Git cat-file --batch exited with {status}: {}",
            String::from_utf8_lossy(&stderr_bytes).trim()
        );
        Ok(())
    }

    /// Writes every entry of `repository`'s tree at `revision` into
    /// `destination`, byte for byte from the object store: no `git archive`,
    /// no attribute-driven transform, no smudge or clean filter. Modes and
    /// symlinks are preserved exactly as the tree records them. Returns the
    /// paths, relative to `destination`, whose content is a Git LFS pointer
    /// (LFS content itself is never fetched).
    ///
    /// F1: blobs are bulk-fetched once (`bulk_fetch_objects`) and then
    /// streamed straight to their destination file (`stream_blobs`), never
    /// held whole in memory. F2/F3 layer (b): every directory this writes
    /// into is created by `ensure_frozen_directory`, which never treats an
    /// existing symlink or foreign directory as traversable, and every
    /// symlink entry is written only in the final pass, after every
    /// directory and regular file this tree needs already exists — so
    /// nothing written earlier can ever be reached back out through a
    /// symlink this call creates.
    fn materialize_tree_raw(
        &self,
        repository: &Path,
        revision: &str,
        destination: &Path,
        frozen_root: &Path,
        created_dirs: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        ensure_frozen_directory(frozen_root, created_dirs, destination)?;
        let entries = self.git_tree_entries(repository, revision)?;

        // Pass 1: every directory this tree needs, before any file or
        // symlink content is written. No symlink exists yet at this point,
        // so nothing here can traverse one.
        for entry in &entries {
            let target = destination.join(&entry.path);
            match entry.mode.as_str() {
                "100644" | "100755" | "120000" => {
                    let parent = target
                        .parent()
                        .context("frozen source tree entry has no parent")?;
                    ensure_frozen_directory(frozen_root, created_dirs, parent)?;
                }
                "160000" => {
                    // Gitlinks are materialized by the caller, which knows
                    // each one's admitted origin; reserve the directory now
                    // so the caller's own writer has somewhere safe to land.
                    ensure_frozen_directory(frozen_root, created_dirs, &target)?;
                }
                other => bail!(
                    "frozen source tree entry {} has an unsupported mode {other}",
                    entry.path.display()
                ),
            }
        }

        // Pass 2: bulk-fetch every blob this tree needs in one round trip
        // (F1), then stream each one to disk. Regular files are written
        // directly; symlink targets are tiny path strings, buffered here and
        // written in Pass 3, last.
        let mut object_order: Vec<String> = Vec::new();
        let mut entries_by_object: std::collections::HashMap<&str, Vec<&GitTreeEntry>> =
            std::collections::HashMap::new();
        for entry in entries.iter().filter(|entry| entry.kind == "blob") {
            entries_by_object
                .entry(entry.object.as_str())
                .or_insert_with(|| {
                    object_order.push(entry.object.clone());
                    Vec::new()
                })
                .push(entry);
        }
        self.bulk_fetch_objects(repository, &object_order)?;

        let mut lfs_pointer_paths = Vec::new();
        let mut symlink_targets: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        self.stream_blobs(repository, &object_order, |object, reader, size| {
            let group = entries_by_object
                .get(object)
                .context("Git cat-file --batch answered an object nobody requested")?;
            let (first, rest) = group
                .split_first()
                .expect("every requested object groups at least one entry");
            let write_file = |entry: &GitTreeEntry, content: &[u8]| -> Result<()> {
                let target = destination.join(&entry.path);
                fs::write(&target, content).with_context(|| format!("writing {}", target.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
                    fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
                }
                Ok(())
            };
            match first.mode.as_str() {
                "100644" | "100755" => {
                    let target = destination.join(&first.path);
                    let mut file = fs::File::create(&target)
                        .with_context(|| format!("writing {}", target.display()))?;
                    let mut remaining = size;
                    let mut buffer = [0u8; 65536];
                    let mut peek: Vec<u8> = Vec::new();
                    while remaining > 0 {
                        let want = buffer.len().min(remaining);
                        reader
                            .read_exact(&mut buffer[..want])
                            .context("reading Git cat-file --batch object content")?;
                        if peek.len() < 128 {
                            let take = (128 - peek.len()).min(want);
                            peek.extend_from_slice(&buffer[..take]);
                        }
                        file.write_all(&buffer[..want])
                            .with_context(|| format!("writing {}", target.display()))?;
                        remaining -= want;
                    }
                    drop(file);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = if first.mode == "100755" { 0o755 } else { 0o644 };
                        fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
                    }
                    let is_lfs = is_lfs_pointer(&peek);
                    if is_lfs {
                        lfs_pointer_paths.push(first.path.clone());
                    }
                    for extra in rest {
                        match extra.mode.as_str() {
                            "100644" | "100755" => {
                                let extra_target = destination.join(&extra.path);
                                fs::copy(&target, &extra_target).with_context(|| {
                                    format!("writing {}", extra_target.display())
                                })?;
                                #[cfg(unix)]
                                {
                                    use std::os::unix::fs::PermissionsExt;
                                    let mode = if extra.mode == "100755" { 0o755 } else { 0o644 };
                                    fs::set_permissions(&extra_target, fs::Permissions::from_mode(mode))?;
                                }
                                if is_lfs {
                                    lfs_pointer_paths.push(extra.path.clone());
                                }
                            }
                            "120000" => {
                                symlink_targets.insert(extra.path.clone(), fs::read(&target)?);
                            }
                            other => bail!(
                                "frozen source tree entry {} has an unsupported mode {other}",
                                extra.path.display()
                            ),
                        }
                    }
                }
                "120000" => {
                    let mut content = vec![0u8; size];
                    reader
                        .read_exact(&mut content)
                        .context("reading Git cat-file --batch object content")?;
                    symlink_targets.insert(first.path.clone(), content.clone());
                    for extra in rest {
                        match extra.mode.as_str() {
                            "120000" => {
                                symlink_targets.insert(extra.path.clone(), content.clone());
                            }
                            "100644" | "100755" => write_file(extra, &content)?,
                            other => bail!(
                                "frozen source tree entry {} has an unsupported mode {other}",
                                extra.path.display()
                            ),
                        }
                    }
                }
                other => bail!(
                    "frozen source tree entry {} has an unsupported mode {other}",
                    first.path.display()
                ),
            }
            Ok(())
        })?;

        // Pass 3: symlinks, last. Every directory this tree needs already
        // exists from Pass 1, and no symlink this call creates existed
        // before this point.
        for (relative_path, content) in symlink_targets {
            let target = destination.join(&relative_path);
            let link_target = std::str::from_utf8(&content)
                .context("frozen source symlink target is not UTF-8")?;
            ensure!(
                fs::symlink_metadata(&target).is_err(),
                "frozen source writer refuses to overwrite an existing entry at {}",
                target.display()
            );
            #[cfg(unix)]
            std::os::unix::fs::symlink(link_target, &target)
                .with_context(|| format!("creating symlink {}", target.display()))?;
        }
        Ok(lfs_pointer_paths)
    }

    /// Clones a Gitlink's admitted origin at its exact recorded revision and
    /// writes its tree raw under `destination_root.join(path)`, the same
    /// byte-exact primitive as the superproject, sharing the superproject's
    /// `created_dirs` bookkeeping so a Gitlink can never alias a path the
    /// superproject (or an earlier Gitlink) already wrote. Returns the
    /// materialized LFS-pointer paths, relative to `destination_root`.
    fn materialize_gitlink_raw(
        &self,
        source: &ExactSource,
        path: &Path,
        fact: &GitlinkTreeFact,
        destination_root: &Path,
        frozen_root: &Path,
        created_dirs: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        let checkout_text = source
            .checkout
            .to_str()
            .context("repository checkout path is not UTF-8")?;
        let checkout_key = sha256_id(checkout_text.as_bytes());
        let gitlink_root = self.source_cache_root.join(".gitlinks").join(
            checkout_key
                .strip_prefix("sha256-")
                .unwrap_or(&checkout_key),
        );
        ensure_source_directory_tree(&self.source_cache_root, &gitlink_root, self.identity)?;
        let checkout = gitlink_root.join(format!("{}-{}", fact.revision, Uuid::new_v4()));
        let result = (|| {
            self.git([
                OsString::from("-c"),
                OsString::from("transfer.fsckObjects=true"),
                OsString::from("clone"),
                OsString::from("--filter=blob:none"),
                OsString::from("--no-checkout"),
                OsString::from("--origin"),
                OsString::from("origin"),
                fact.origin.clone().into(),
                checkout.as_os_str().to_owned(),
            ])?;
            self.git([
                OsString::from("-c"),
                OsString::from("transfer.fsckObjects=true"),
                OsString::from("-C"),
                checkout.as_os_str().to_owned(),
                OsString::from("fetch"),
                OsString::from("--no-tags"),
                OsString::from("origin"),
                fact.revision.clone().into(),
            ])?;
            let actual = self.git_text([
                OsString::from("-C"),
                checkout.as_os_str().to_owned(),
                OsString::from("rev-parse"),
                OsString::from(format!("{}^{{commit}}", fact.revision)),
            ])?;
            ensure!(actual == fact.revision, "Gitlink exact revision is absent");
            ensure!(
                !self
                    .git_tree_entries(&checkout, &fact.revision)?
                    .iter()
                    .any(|entry| entry.mode == "160000"),
                "nested Gitlinks are not admitted in Idunn v1"
            );
            self.materialize_tree_raw(
                &checkout,
                &fact.revision,
                &destination_root.join(path),
                frozen_root,
                created_dirs,
            )
        })();
        let cleanup = if checkout.exists() {
            remove_tree_inside(&gitlink_root, &checkout)
        } else {
            Ok(())
        };
        match (result, cleanup) {
            (Ok(lfs_pointer_paths), Ok(())) => Ok(lfs_pointer_paths
                .into_iter()
                .map(|relative| path.join(relative))
                .collect()),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error.context("cleaning exact Gitlink checkout")),
        }
    }

    /// Fetches `source` at the exact `revision` and writes it and its
    /// Gitlinks, byte for byte from the Git object store, into a fresh
    /// root-owned immutable tree under `root/<transaction_id>`. Returns that
    /// tree's root, its content digest, the recipe blob bytes the tree's own
    /// Git objects name, and whether any materialized file is a Git LFS
    /// pointer (LFS content itself is never fetched). It derives the recipe
    /// and Gitlink facts itself from the tree rather than trusting a caller's
    /// prior resolution, so it needs no `OperatorBinding` or compiled plan and
    /// is safe to call again later for a verify transaction over a different
    /// revision of a different repository. It shares its fetch core with
    /// `freeze`, which additionally checks the frozen recipe against a
    /// durable resolution made at admission time.
    pub(crate) fn freeze_exact(
        &self,
        source: &ExactSource,
        revision: &str,
        transaction_id: &str,
        root: &Path,
    ) -> Result<(PathBuf, String, Vec<u8>, bool)> {
        require_driver_id(transaction_id, "source transaction")?;
        require_git_sha(revision, "frozen source revision")?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::geteuid() } == 0 && self.identity.is_some(),
            "freezing source requires root Idunn with an unprivileged Git identity"
        );
        self.prepare_source_root(source)?;
        self.git([
            OsString::from("-c"),
            OsString::from("transfer.fsckObjects=true"),
            OsString::from("-C"),
            source.checkout.as_os_str().to_owned(),
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from("origin"),
            OsString::from(revision),
        ])
        .context("fetching the durable exact source revision")?;
        // S1: `transfer.fsckObjects=true` above only inspects objects this
        // fetch actually transfers. An object the checkout already holds --
        // brought in by an earlier `resolve()` on this same checkout, or by
        // any other path into the local store -- is never re-transferred and
        // so is never re-checked by that flag. Fsck the selected revision
        // explicitly and independently here, so objects that were already
        // local get checked too, not only freshly fetched ones.
        self.git([
            OsString::from("-C"),
            source.checkout.as_os_str().to_owned(),
            OsString::from("fsck"),
            OsString::from("--strict"),
            OsString::from("--no-dangling"),
            OsString::from(revision),
        ])
        .with_context(|| format!("fsck refused the selected revision {revision}"))?;
        let (recipe_bytes, gitlinks) = self.exact_recipe_and_gitlinks(source, revision)?;
        let transaction_root = prepare_frozen_transaction_root(root, transaction_id)?;
        let partial = transaction_root.join(".partial");
        prepare_frozen_source_destination(&partial)?;
        let materialization = (|| {
            let mut created_dirs = std::collections::HashSet::new();
            created_dirs.insert(partial.clone());
            let mut lfs_pointer_paths =
                self.materialize_tree_raw(&source.checkout, revision, &partial, &partial, &mut created_dirs)?;
            for (path, fact) in &gitlinks {
                lfs_pointer_paths.extend(self.materialize_gitlink_raw(
                    source,
                    path,
                    fact,
                    &partial,
                    &partial,
                    &mut created_dirs,
                )?);
            }
            let recipe_file = partial.join(&source.recipe_path);
            let recipe_metadata = fs::symlink_metadata(&recipe_file)?;
            ensure!(
                recipe_metadata.is_file() && !recipe_metadata.file_type().is_symlink(),
                "frozen deployment recipe is not a regular file"
            );
            let materialized_recipe =
                fs::read(&recipe_file).context("reading frozen deployment recipe")?;
            ensure!(
                materialized_recipe == recipe_bytes,
                "frozen recipe differs from the selected tree's recipe blob"
            );
            // F4: one hardening pass, last, sets and validates the frozen
            // tree's ownership, modes and symlinks in the same walk. A
            // separate, later `validate_frozen_source` re-walk was a
            // redundant duplicate of exactly this check; it stays only as
            // the read-only re-check `observe_frozen` uses on a tree it did
            // not just write.
            harden_frozen_source(&partial)?;
            let snapshot_sha256 = frozen_source_sha256(&partial)?;
            Ok::<_, anyhow::Error>((snapshot_sha256, !lfs_pointer_paths.is_empty()))
        })();
        let (snapshot_sha256, contains_lfs_pointers) = match materialization {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = remove_frozen_transaction_root(root, transaction_id);
                return Err(error);
            }
        };
        let snapshot_component = snapshot_sha256
            .strip_prefix("sha256-")
            .expect("generated frozen source digest");
        let final_root = transaction_root.join(snapshot_component);
        fs::rename(&partial, &final_root).context("publishing immutable frozen source")?;
        // S6: symlinks are judged against the tree's real, final published
        // path, not `.partial`. This must run after the rename and before
        // anything trusts the tree.
        if let Err(error) = validate_frozen_source_symlinks(&final_root) {
            let _ = remove_frozen_transaction_root(root, transaction_id);
            return Err(error);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&transaction_root, fs::Permissions::from_mode(0o500))?;
        }
        Ok((final_root, snapshot_sha256, recipe_bytes, contains_lfs_pointers))
    }
}

/// A Git LFS pointer file's exact, well-known first line (the LFS pointer
/// spec fixes this text). Detecting it needs no LFS tooling: the pointer is
/// itself the blob content when LFS smudging never runs, which
/// `stream_blobs` guarantees by construction.
fn is_lfs_pointer(content: &[u8]) -> bool {
    content.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
}

impl SourcePort for GitSourceDriver {
    fn resolve(
        &self,
        binding: &OperatorBinding,
        resolution_id: &str,
        selected_at_unix_millis: u64,
    ) -> Result<ResolvedSource> {
        binding.validate()?;
        ensure!(
            selected_at_unix_millis > 0,
            "source selection has no timestamp"
        );
        let source = exact_source_from(binding);
        self.prepare_source_root(&source)?;
        let fetched_ref = Self::admitted_ref_name(binding, resolution_id)?;
        // S1: this fetch brings the superproject's commit and tree objects
        // into the local store first, before any other guard ever sees them.
        // `freeze_exact`'s own fetch is fsck-guarded, but a fetch by exact
        // revision never re-transfers (and so never re-checks) an object
        // that is already local -- and this call is what makes it local.
        // Without `transfer.fsckObjects=true` here, a hostile `.git`
        // look-alike or a tree with duplicate entries lands unchecked.
        self.git([
            OsString::from("-c"),
            OsString::from("transfer.fsckObjects=true"),
            OsString::from("-C"),
            binding.repository.checkout.as_os_str().to_owned(),
            OsString::from("fetch"),
            OsString::from("--force"),
            OsString::from("--no-tags"),
            OsString::from("origin"),
            OsString::from(format!(
                "+{}:{fetched_ref}",
                binding.repository.admitted_ref
            )),
        ])
        .context("fetching the admitted ref")?;
        let admitted_ref_revision = self.git_text([
            OsString::from("-C"),
            binding.repository.checkout.as_os_str().to_owned(),
            OsString::from("rev-parse"),
            OsString::from(format!("{fetched_ref}^{{commit}}")),
        ])?;
        require_git_sha(&admitted_ref_revision, "fetched admitted-ref revision")?;
        let (revision, selection) =
            self.resolve_selected_revision(binding, &admitted_ref_revision)?;
        let source_tree = self.git_text([
            OsString::from("-C"),
            binding.repository.checkout.as_os_str().to_owned(),
            OsString::from("rev-parse"),
            OsString::from(format!("{revision}^{{tree}}")),
        ])?;
        require_git_sha(&source_tree, "selected source tree")?;
        let (recipe_bytes, gitlinks) = self.exact_recipe_and_gitlinks(&source, &revision)?;
        let facts = SourceSelectionFacts {
            schema: SOURCE_SELECTION_FACTS_SCHEMA.into(),
            origin: binding.repository.origin.clone(),
            admitted_ref: binding.repository.admitted_ref.clone(),
            admitted_ref_revision,
            revision,
            source_tree,
            recipe_path: binding.repository.recipe_path.clone(),
            recipe_blob_sha256: sha256_id(&recipe_bytes),
            gitlinks,
            selection,
            selected_at_unix_millis,
        };
        facts.validate_against(binding)?;
        let resolved = ResolvedSource {
            facts,
            recipe_bytes,
        };
        resolved.validate_against(binding)?;
        Ok(resolved)
    }

    fn freeze(
        &self,
        transaction_id: &str,
        plan: &CompiledDeploymentPlan,
    ) -> Result<FrozenSourceReceipt> {
        plan.validate()?;
        require_driver_id(transaction_id, "source transaction")?;
        let (_, binding) = plan.parsed_inputs()?;
        let resolved = ResolvedSource {
            facts: plan.source.clone(),
            recipe_bytes: plan.recipe_blob.clone(),
        };
        resolved.validate_against(&binding)?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::geteuid() } == 0 && self.identity.is_some(),
            "freezing source requires root Idunn with an unprivileged Git identity"
        );
        let source = exact_source_from(&binding);
        self.prepare_source_root(&source)?;
        // `freeze_exact` fetches the exact revision itself; fetching here too
        // would fetch twice for one freeze. `verify_exact_source` below needs
        // no fetch of its own: it only reads objects already local, and
        // `freeze_exact` has just made the selected revision local.
        // `contains_lfs_pointers` is the frozen result's own record that LFS
        // content was not fetched (F2); `FrozenSourceReceipt` has no field for
        // it yet, so it is not yet carried past this call. Promoting it to a
        // persisted, Verse-visible fact is follow-up scope, not this fix.
        let (_tree_root, snapshot_sha256, recipe_bytes, _contains_lfs_pointers) = self
            .freeze_exact(
                &source,
                &resolved.facts.revision,
                transaction_id,
                &self.frozen_source_root,
            )?;
        self.verify_exact_source(&source, &resolved)?;
        ensure!(
            recipe_bytes == resolved.recipe_bytes,
            "frozen recipe differs from the durable source resolution"
        );
        let receipt = FrozenSourceReceipt {
            transaction_id: transaction_id.to_owned(),
            plan_id: plan.plan_id.clone(),
            snapshot_sha256,
        };
        receipt.validate_against(plan)?;
        Ok(receipt)
    }

    fn observe_frozen(
        &self,
        plan: &CompiledDeploymentPlan,
        receipt: &FrozenSourceReceipt,
    ) -> Result<FrozenSource> {
        receipt.validate_against(plan)?;
        validate_frozen_source_store(&self.frozen_source_root)?;
        let root = self
            .frozen_source_root
            .join(&receipt.transaction_id)
            .join(receipt.snapshot_component());
        let canonical_store = self.frozen_source_root.canonicalize()?;
        let canonical_root = root.canonicalize()?;
        ensure!(
            canonical_store == self.frozen_source_root
                && canonical_root == root
                && canonical_root.starts_with(&canonical_store),
            "frozen source receipt resolves outside its authority root"
        );
        validate_frozen_source(&root)?;
        ensure!(
            frozen_source_sha256(&root)? == receipt.snapshot_sha256,
            "frozen source snapshot differs from its receipt"
        );
        let recipe_path = root.join(&plan.source.recipe_path);
        let recipe_metadata = fs::symlink_metadata(&recipe_path)?;
        ensure!(
            recipe_metadata.is_file() && !recipe_metadata.file_type().is_symlink(),
            "observed deployment recipe is not a regular file"
        );
        ensure!(
            fs::read(&recipe_path)? == plan.recipe_blob,
            "observed deployment recipe differs from the persisted plan"
        );
        Ok(FrozenSource {
            receipt: receipt.clone(),
            facts: plan.source.clone(),
            recipe_bytes: plan.recipe_blob.clone(),
            root,
        })
    }

    fn cleanup(&self, transaction_id: &str, receipt: Option<&FrozenSourceReceipt>) -> Result<()> {
        require_driver_id(transaction_id, "source transaction")?;
        if let Some(receipt) = receipt {
            ensure!(
                receipt.transaction_id == transaction_id,
                "frozen source cleanup receipt belongs to another transaction"
            );
            require_sha256_id(&receipt.snapshot_sha256, "frozen source snapshot")?;
        }
        let transaction_root = self.frozen_source_root.join(transaction_id);
        if !transaction_root.exists() {
            return Ok(());
        }
        remove_frozen_transaction_root(&self.frozen_source_root, transaction_id)
    }
}

/// The network a container step is admitted to. `None` and `Bridge` are today's
/// only lowered profiles; `Named` carries an operator-supplied Docker network
/// name forward unchanged, matching the binding's free-form
/// `network_profile` before this cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContainerNetwork {
    None,
    Bridge,
    Named(String),
}

impl ContainerNetwork {
    fn docker_value(&self) -> &str {
        match self {
            ContainerNetwork::None => "none",
            ContainerNetwork::Bridge => "bridge",
            ContainerNetwork::Named(name) => name,
        }
    }
}

/// Cut 2 adds `Unconfined`. This cut declares only the variant every runner
/// gets today, so a seccomp override is not yet reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContainerSeccomp {
    Default,
}

/// A secret bound into the container's environment from a root-owned,
/// exact-group-bound file, resolved and validated by `ContainerSpec::for_step`
/// before the container ever runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SecretMount {
    pub env_name: String,
    pub host_path: PathBuf,
    pub container_path: String,
}

/// A Docker container's full admission shape: the binding's affordances, plus
/// the step's resolved environment. It is deliberately a plain struct with no
/// binding or recipe types in it, so `docker_run_args` can lower it with no
/// filesystem effects and no knowledge of `DockerRunnerBinding`, and so a
/// future verify runner binding can build one directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerSpec {
    pub image: String,
    pub user: String,
    pub network: ContainerNetwork,
    pub seccomp: ContainerSeccomp,
    pub memory_mebibytes: u32,
    pub cpu_quota_percent: u32,
    pub pids_limit: u32,
    pub tmpfs_mebibytes: u32,
    pub cache_root: Option<PathBuf>,
    pub secret_mounts: Vec<SecretMount>,
    pub environment: Vec<(String, String)>,
}

impl ContainerSpec {
    /// The one constructor: a runner binding, the step's required environment
    /// names, and the source stamp pair. Cache-root and secret validation
    /// happen here, against the filesystem, before `docker_run_args` ever
    /// runs; that function stays pure.
    pub(crate) fn for_step(
        runner: &DockerRunnerBinding,
        required_environment: &std::collections::BTreeSet<String>,
        source_stamp: (&str, &str),
    ) -> Result<Self> {
        let identity = container_identity(&runner.user)?;
        if let Some(cache_root) = &runner.cache_root {
            ensure_runner_cache_root(cache_root, identity)?;
        }
        let mut environment = vec![(source_stamp.0.to_owned(), source_stamp.1.to_owned())];
        let mut secret_mounts = Vec::new();
        for name in required_environment {
            ensure!(
                name != source_stamp.0,
                "runner cannot bind step environment {name}, which collides with the Idunn source stamp"
            );
            if let Some(value) = runner.environment.get(name) {
                environment.push((name.clone(), value.clone()));
            } else if let Some(path) = runner.secret_files.get(name) {
                validate_runner_secret(path, identity)?;
                secret_mounts.push(SecretMount {
                    env_name: name.clone(),
                    host_path: path.clone(),
                    container_path: format!("/run/idunn/secrets/{name}"),
                });
            } else {
                bail!("runner lacks declared step environment {name}")
            }
        }
        let network = match runner.network_profile.as_deref() {
            None | Some("none") => ContainerNetwork::None,
            Some("bridge") => ContainerNetwork::Bridge,
            Some(other) => ContainerNetwork::Named(other.to_owned()),
        };
        Ok(ContainerSpec {
            image: runner.image.clone(),
            user: runner.user.clone(),
            network,
            seccomp: ContainerSeccomp::Default,
            memory_mebibytes: runner.memory_mebibytes,
            cpu_quota_percent: runner.cpu_quota_percent,
            pids_limit: runner.pids_limit,
            tmpfs_mebibytes: runner.tmpfs_mebibytes,
            cache_root: runner.cache_root.clone(),
            secret_mounts,
            environment,
        })
    }
}

/// Lowers `spec` into the exact `docker run` argv for `argv` running in
/// `workspace` at `working_directory`. Pure: it performs no filesystem
/// effects and validates nothing that requires the filesystem (the caller
/// validates the cache root and every secret through `ContainerSpec::for_step`
/// before calling this). It still needs the per-workspace machine-id file's
/// path, which `build_machine_id` computes without touching the filesystem;
/// the caller is responsible for having materialized that file with
/// `build_machine_id_file` first.
pub(crate) fn docker_run_args(
    spec: &ContainerSpec,
    workspace: &Path,
    working_directory: &Path,
    argv: &[String],
) -> Result<Vec<OsString>> {
    ensure!(!argv.is_empty(), "runner command is empty");
    let mut args = vec![
        OsString::from("run"),
        OsString::from("--rm"),
        OsString::from("--network"),
        OsString::from(spec.network.docker_value()),
        OsString::from("--memory"),
        OsString::from(format!("{}m", spec.memory_mebibytes)),
        OsString::from("--cpus"),
        OsString::from(format!(
            "{:.2}",
            f64::from(spec.cpu_quota_percent) / 100.0
        )),
        OsString::from("--mount"),
        bind_mount(workspace, "/workspace", false)?,
        OsString::from("--user"),
        OsString::from(&spec.user),
        OsString::from("--cap-drop"),
        OsString::from("ALL"),
        OsString::from("--security-opt"),
        OsString::from("no-new-privileges"),
    ];
    match spec.seccomp {
        ContainerSeccomp::Default => {}
    }
    args.extend([
        OsString::from("--read-only"),
        OsString::from("--pids-limit"),
        OsString::from(spec.pids_limit.to_string()),
        OsString::from("--tmpfs"),
        OsString::from(format!(
            "/tmp:rw,nosuid,nodev,noexec,size={}m",
            spec.tmpfs_mebibytes
        )),
        OsString::from("--mount"),
        bind_mount(
            &PathBuf::from("/run/idunn/build-machine-ids").join(build_machine_id(workspace)?),
            "/etc/machine-id",
            true,
        )?,
    ]);
    if let Some(cache_root) = &spec.cache_root {
        args.push(OsString::from("--mount"));
        args.push(bind_mount(cache_root, "/cache", false)?);
    }
    args.push(OsString::from("--workdir"));
    args.push(OsString::from(format!(
        "/workspace/{}",
        normalized_relative(working_directory)?
    )));
    for (name, value) in &spec.environment {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!("{name}={value}")));
    }
    for mount in &spec.secret_mounts {
        args.push(OsString::from("--env"));
        args.push(OsString::from(format!(
            "{}={}",
            mount.env_name, mount.container_path
        )));
        args.push(OsString::from("--mount"));
        args.push(bind_mount(&mount.host_path, &mount.container_path, true)?);
    }
    args.push(spec.image.clone().into());
    args.extend(argv.iter().map(OsString::from));
    Ok(args)
}

/// Docker is only a runner substrate. Recipe argv stays an argv vector, the
/// operator binding selects the exact image/network/mount affordances, and the
/// driver returns complete materialization receipts rather than build truth by
/// convention.
#[derive(Clone)]
pub struct DockerRunnerDriver {
    pub docker_program: PathBuf,
}

impl Default for DockerRunnerDriver {
    fn default() -> Self {
        Self {
            docker_program: PathBuf::from("/usr/bin/docker"),
        }
    }
}

impl DockerRunnerDriver {
    fn docker<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure!(
            self.docker_program.is_absolute(),
            "Docker runner program is not absolute"
        );
        let output = Command::new(&self.docker_program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("starting Docker runner {}", self.docker_program.display()))?;
        if !output.status.success() {
            bail!(
                "Docker runner exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    fn run_in_workspace(
        &self,
        runner: &DockerRunnerBinding,
        workspace: &Path,
        working_directory: &Path,
        argv: &[String],
        required_environment: &std::collections::BTreeSet<String>,
        source_stamp: (&str, &str),
    ) -> Result<()> {
        ensure!(!argv.is_empty(), "runner command is empty");
        ensure!(
            runner.allowed_programs.contains(&argv[0]),
            "runner program {} is not operator-bound",
            argv[0]
        );
        build_machine_id_file(workspace)?;
        let spec = ContainerSpec::for_step(runner, required_environment, source_stamp)?;
        let args = docker_run_args(&spec, workspace, working_directory, argv)?;
        self.docker(args)?;
        Ok(())
    }

    fn materialize_external_input(
        &self,
        declaration: &TargetDeclaration,
        binding: &OperatorBinding,
        workspaces: &BTreeMap<String, PathBuf>,
        input_id: &str,
    ) -> Result<ExternalInputMaterializationReceipt> {
        let input = declaration
            .external_inputs
            .iter()
            .find(|candidate| candidate.id == input_id)
            .context("external input declaration disappeared")?;
        let runner = binding.runners[&input.runner].docker()?;
        let workspace = &workspaces[&input.runner];
        let destination = workspace.join(&input.destination);
        ensure!(
            destination.starts_with(workspace),
            "external input escaped its runner workspace"
        );
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        build_machine_id_file(workspace)?;
        let spec = ContainerSpec::for_step(
            runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_EXTERNAL_INPUT_ID", input.id.as_str()),
        )?;
        let curl_argv = vec![
            "curl".to_owned(),
            "--fail".to_owned(),
            "--location".to_owned(),
            "--proto".to_owned(),
            "=https".to_owned(),
            "--output".to_owned(),
            format!("/workspace/{}", normalized_relative(&input.destination)?),
            input.url.clone(),
        ];
        let args = docker_run_args(&spec, workspace, Path::new("."), &curl_argv)?;
        self.docker(args)?;
        let bytes = fs::read(&destination).with_context(|| {
            format!(
                "reading materialized external input {}",
                destination.display()
            )
        })?;
        ensure!(!bytes.is_empty(), "materialized external input is empty");
        let digest = raw_sha256(&bytes);
        ensure!(
            digest == input.sha256,
            "materialized external input digest differs from its recipe pin"
        );
        Ok(ExternalInputMaterializationReceipt {
            input_id: input.id.clone(),
            url: input.url.clone(),
            sha256: format!("sha256-{digest}"),
            runner: input.runner.clone(),
            destination: input.destination.clone(),
            size_bytes: bytes.len().try_into()?,
        })
    }

    fn collect_artifact(
        &self,
        declaration: &TargetDeclaration,
        source: &FrozenSource,
        workspaces: &BTreeMap<String, PathBuf>,
        staging_root: &Path,
        artifact_id: &str,
    ) -> Result<ArtifactReceipt> {
        let artifact = declaration
            .artifacts
            .iter()
            .find(|candidate| candidate.id == artifact_id)
            .context("artifact declaration disappeared")?;
        let (containment_root, source_path) = match artifact.source_kind {
            ArtifactSource::RunnerOutput => {
                let workspace = workspaces
                    .get(
                        artifact
                            .runner
                            .as_deref()
                            .context("runner artifact lost its runner")?,
                    )
                    .context("runner workspace is absent")?;
                (workspace.clone(), workspace.join(&artifact.source))
            }
            ArtifactSource::WorktreeTree => (source.root.clone(), source.root.join(&artifact.source)),
        };
        ensure!(source_path.exists(), "declared artifact output is absent");
        let destination = staging_root.join(&artifact.destination);
        ensure!(
            destination.starts_with(staging_root),
            "artifact destination escaped its release staging root"
        );
        copy_artifact(&containment_root, &source_path, &destination)?;
        let (sha256, size_bytes) = digest_artifact(&destination)?;
        if let Some(expected) = &artifact.expected_sha256 {
            ensure!(
                &sha256 == expected,
                "artifact output differs from its recipe-pinned digest"
            );
        }
        Ok(ArtifactReceipt {
            artifact_id: artifact.id.clone(),
            destination: artifact.destination.clone(),
            sha256: format!("sha256-{sha256}"),
            size_bytes,
            executable: artifact.executable,
        })
    }
}

impl RunnerPort for DockerRunnerDriver {
    fn materialize(
        &self,
        source: &FrozenSource,
        plan: &CompiledDeploymentPlan,
        staging_parent: &Path,
        sealed_at_unix_millis: u64,
    ) -> Result<MaterializedRelease> {
        plan.validate()?;
        source.receipt.validate_against(plan)?;
        ensure!(
            source.facts == plan.source && source.recipe_bytes == plan.recipe_blob,
            "runner source differs from the compiled plan"
        );
        let (declaration, binding) = plan.parsed_inputs()?;
        fs::create_dir_all(staging_parent)?;
        let staging_root = staging_parent.join(&source.receipt.transaction_id);
        if staging_root.exists() {
            remove_tree_inside(staging_parent, &staging_root)?;
        }
        fs::create_dir_all(&staging_root)?;

        let mut workspaces = BTreeMap::new();
        for runner_id in binding.runners.keys() {
            let workspace = staging_root.join(format!(".runner-{runner_id}"));
            copy_tree(&source.root, &workspace)?;
            workspaces.insert(runner_id.clone(), workspace);
        }

        for input in &declaration.external_inputs {
            let workspace = &workspaces[&input.runner];
            let destination = workspace.join(&input.destination);
            ensure!(
                destination.starts_with(workspace),
                "external input escaped its runner workspace"
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
        }
        for (runner_id, workspace) in &workspaces {
            assign_runner_tree(
                workspace,
                container_identity(&binding.runners[runner_id].docker()?.user)?,
            )?;
        }

        let mut external_inputs = Vec::new();
        for input in &declaration.external_inputs {
            external_inputs.push(self.materialize_external_input(
                &declaration,
                &binding,
                &workspaces,
                &input.id,
            )?);
        }
        for step in &declaration.steps {
            let runner = binding.runners[&step.runner].docker()?;
            let workspace = &workspaces[&step.runner];
            for required in &step.required_environment {
                ensure!(
                    runner.environment.contains_key(required)
                        || runner.secret_files.contains_key(required),
                    "step {} lacks operator-bound environment {required}",
                    step.id
                );
            }
            self.run_in_workspace(
                runner,
                workspace,
                &step.working_directory,
                &step.argv,
                &step.required_environment,
                (
                    &declaration.source_stamp_environment,
                    &source.facts.revision,
                ),
            )?;
        }

        let mut artifacts = Vec::new();
        for artifact in &declaration.artifacts {
            artifacts.push(self.collect_artifact(
                &declaration,
                source,
                &workspaces,
                &staging_root,
                &artifact.id,
            )?);
        }
        for workspace in workspaces.values() {
            remove_tree_inside(&staging_root, workspace)?;
        }
        let release = SealedRelease::new(
            plan,
            artifacts.clone(),
            external_inputs.clone(),
            sealed_at_unix_millis,
        )?;
        Ok(MaterializedRelease {
            release,
            root: staging_root,
        })
    }
}

/// systemd owns process execution. This driver lowers one validated launch
/// contract into a transient unit and then proves the native process and
/// executable that systemd actually started. It does not decide admission,
/// readiness, write authority, or route membership.
#[derive(Clone)]
pub struct SystemdTransientWorkloadDriver {
    pub systemd_run_program: PathBuf,
    pub systemctl_program: PathBuf,
    pub proc_root: PathBuf,
    pub credential_root: PathBuf,
}

impl Default for SystemdTransientWorkloadDriver {
    fn default() -> Self {
        Self {
            systemd_run_program: PathBuf::from("/usr/bin/systemd-run"),
            systemctl_program: PathBuf::from("/usr/bin/systemctl"),
            proc_root: PathBuf::from("/proc"),
            credential_root: PathBuf::from("/run/idunn/activation-credentials"),
        }
    }
}

impl SystemdTransientWorkloadDriver {
    fn command<I, S>(&self, program: &Path, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure!(
            program.is_absolute(),
            "workload actuator program is not absolute"
        );
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .env_clear()
            .env("LANG", "C.UTF-8")
            .output()
            .with_context(|| format!("starting workload actuator {}", program.display()))?;
        if !output.status.success() {
            bail!(
                "workload actuator {} exited with {}: {}",
                program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    fn unit_name(&self, prefix: &str, runtime_instance_id: &str) -> Result<String> {
        let suffix = runtime_instance_id
            .strip_prefix("sha256-")
            .context("runtime instance id has no sha256 prefix")?;
        ensure!(
            suffix.len() == 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "runtime instance id is not a SHA-256 digest"
        );
        Ok(format!("{prefix}-{suffix}.service"))
    }

    fn activation_credential_source(
        &self,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<PathBuf> {
        activation.validate()?;
        ensure!(
            self.credential_root.is_absolute()
                && !self
                    .credential_root
                    .as_os_str()
                    .to_string_lossy()
                    .chars()
                    .any(char::is_whitespace),
            "activation credential root must be an absolute path without whitespace"
        );
        Ok(self
            .credential_root
            .join(format!("{}.credential", activation.runtime_instance_id)))
    }

    fn write_activation_credential(
        &self,
        launch: IdunnRuntimeActivationLaunch,
    ) -> Result<(IdunnRuntimeActivationRecord, PathBuf)> {
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        ensure_activation_credential_root(&self.credential_root)?;
        let expected_activation = launch.activation().clone();
        let source = self.activation_credential_source(&expected_activation)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o400);
        let mut file = options.open(&source).with_context(|| {
            format!(
                "creating one-shot activation credential {}",
                source.display()
            )
        })?;
        let written = (|| -> Result<IdunnRuntimeActivationRecord> {
            #[cfg(unix)]
            file.set_permissions(fs::Permissions::from_mode(0o400))?;
            let activation = launch
                .write_credential(&mut file)
                .context("writing one-shot activation credential")?;
            file.flush()?;
            file.sync_all()?;
            Ok(activation)
        })();
        drop(file);
        let activation = match written {
            Ok(activation) => activation,
            Err(error) => {
                return match remove_activation_credential_source(&source) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!(
                        "deleting incomplete activation credential also failed: {cleanup:#}"
                    ))),
                };
            }
        };
        let validation = (|| -> Result<()> {
            ensure!(
                activation == expected_activation,
                "activation launch changed while writing its credential"
            );
            validate_activation_credential_source(&source)?;
            sync_parent_directory(&source)
        })();
        if let Err(error) = validation {
            return match remove_activation_credential_source(&source) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "deleting invalid activation credential also failed: {cleanup:#}"
                ))),
            };
        }
        Ok((activation, source))
    }

    fn validate_prepared_activation_credential(
        &self,
        activation: &IdunnRuntimeActivationRecord,
        source: &Path,
    ) -> Result<()> {
        validate_activation_credential_source(source)?;
        let signer = IdunnRuntimeActivationSigner::from_credential_reader(
            open_native_read_only(source).with_context(|| {
                format!(
                    "opening prepared activation credential {}",
                    source.display()
                )
            })?,
        )?;
        ensure!(
            signer.identity_id() == activation.activation_signer_identity_id
                && signer.public_key() == activation.activation_signer_public_key,
            "prepared activation credential differs from the persisted activation"
        );
        Ok(())
    }

    fn parent_only_file_descriptors(
        &self,
        binding: &OperatorBinding,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<Vec<ParentOnlyFileDescriptorObservation>> {
        let activation_source = self.activation_credential_source(activation)?;
        self.validate_prepared_activation_credential(activation, &activation_source)?;
        let presence_source = binding
            .workload
            .systemd()?
            .secret_files
            .get(RUNTIME_PRESENCE_IDENTITY_BINDING)
            .context("workload has no parent-only runtime presence identity source")?;
        let presence_signer =
            open_service_identity_credential_reader::<GameCultProviderHealthIdentity>(
                open_native_read_only(presence_source).with_context(|| {
                    format!(
                        "opening runtime presence identity source {}",
                        presence_source.display()
                    )
                })?,
            )?;
        ensure!(
            presence_signer.entry().identity_id == expected.expected_signer_identity_id,
            "runtime presence identity source differs from Expected"
        );
        let descriptors = vec![
            observe_parent_only_file_descriptor(
                3,
                IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME,
                &activation_source,
            )?,
            observe_parent_only_file_descriptor(
                4,
                RUNTIME_PRESENCE_IDENTITY_FD_NAME,
                presence_source,
            )?,
        ];
        parent_only_open_file_properties(&descriptors)?;
        Ok(descriptors)
    }

    fn stop_submitted_unit(&self, unit: &str, expected_description: &str) -> Result<()> {
        let Some(observation) = self.show_unit(unit)? else {
            return Ok(());
        };
        let values = &observation.properties;
        ensure!(
            values.get("Description").map(String::as_str) == Some(expected_description),
            "refusing to stop a submitted unit whose description changed"
        );
        self.command(
            &self.systemctl_program,
            [OsString::from("stop"), OsString::from(unit)],
        )?;
        if let Some(observation) = self.show_unit(unit)? {
            let values = &observation.properties;
            ensure!(
                values
                    .get("ActiveState")
                    .is_some_and(|state| { matches!(state.as_str(), "inactive" | "failed") }),
                "submitted systemd unit remained active after stop"
            );
        }
        Ok(())
    }

    fn install_release(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &MaterializedRelease,
    ) -> Result<InstalledReleaseObservation> {
        release.release.validate_against(plan)?;
        let (_, binding) = plan.parsed_inputs()?;
        let release_root = &binding.workload.systemd()?.release_root;
        fs::create_dir_all(release_root)
            .with_context(|| format!("creating release root {}", release_root.display()))?;
        let installed = release_root.join(&release.release.sealed_release_id);
        if !installed.exists() {
            let temporary = release_root.join(format!(
                ".install-{}-{}",
                release.release.sealed_release_id,
                Uuid::new_v4()
            ));
            copy_tree(&release.root, &temporary)?;
            if let Err(error) = fs::rename(&temporary, &installed) {
                let cleanup = remove_tree_inside(release_root, &temporary);
                if installed.exists() && error.kind() == ErrorKind::AlreadyExists {
                    cleanup?;
                } else {
                    cleanup?;
                    return Err(error).with_context(|| {
                        format!("installing sealed release {}", installed.display())
                    });
                }
            }
        }
        for artifact in &release.release.artifacts {
            let path = installed.join(&artifact.destination);
            let (sha256, size_bytes) = digest_artifact(&path)?;
            ensure!(
                format!("sha256-{sha256}") == artifact.sha256 && size_bytes == artifact.size_bytes,
                "installed artifact {} differs from its sealed receipt",
                artifact.artifact_id
            );
        }
        harden_installed_release(&installed, &release.release.artifacts)?;
        Ok(InstalledReleaseObservation {
            sealed_release_id: release.release.sealed_release_id.clone(),
            root: installed,
        })
    }

    fn validate_installed_release(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &SealedRelease,
        installed: &InstalledReleaseObservation,
    ) -> Result<()> {
        release.validate_against(plan)?;
        let (_, binding) = plan.parsed_inputs()?;
        ensure!(
            installed.sealed_release_id == release.sealed_release_id
                && installed.root
                    == binding
                        .workload
                        .systemd()?
                        .release_root
                        .join(&release.sealed_release_id),
            "installed release observation belongs to another sealed release"
        );
        for artifact in &release.artifacts {
            let path = installed.root.join(&artifact.destination);
            let (sha256, size_bytes) = digest_artifact(&path)?;
            ensure!(
                format!("sha256-{sha256}") == artifact.sha256 && size_bytes == artifact.size_bytes,
                "installed artifact {} differs from its sealed receipt",
                artifact.artifact_id
            );
        }
        harden_installed_release(&installed.root, &release.artifacts)
    }

    fn prepare_runtime_bundle(
        &self,
        binding: &OperatorBinding,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<PathBuf> {
        let bundle = binding
            .workload
            .systemd()?
            .runtime_root
            .join(&activation.runtime_instance_id);
        write_runtime_bundle_records(&bundle, expected, activation)?;
        harden_runtime_bundle(&bundle)?;
        let state_group_id = binding
            .workload
            .systemd()?
            .state_group
            .as_deref()
            .map(resolve_group_id)
            .transpose()?;
        ensure_bundle_is_reachable_by_workload(&bundle, state_group_id)?;
        Ok(bundle)
    }

    fn show_unit(&self, unit: &str) -> Result<Option<SystemdUnitObservation>> {
        ensure!(
            self.systemctl_program.is_absolute(),
            "systemctl program is not absolute"
        );
        let output = Command::new(&self.systemctl_program)
            .args([
                OsString::from("show"),
                OsString::from(unit),
                OsString::from("--no-pager"),
                OsString::from("--property=LoadState"),
                OsString::from("--property=ActiveState"),
                OsString::from("--property=SubState"),
                OsString::from("--property=Description"),
                OsString::from("--property=InvocationID"),
                OsString::from("--property=MainPID"),
                OsString::from("--property=ExecMainStartTimestampMonotonic"),
                OsString::from("--property=Type"),
                OsString::from("--property=Restart"),
                OsString::from("--property=KillMode"),
                OsString::from("--property=DynamicUser"),
                OsString::from("--property=User"),
                OsString::from("--property=Group"),
                OsString::from("--property=SupplementaryGroups"),
                OsString::from("--property=CapabilityBoundingSet"),
                OsString::from("--property=AmbientCapabilities"),
                OsString::from("--property=PrivateMounts"),
                OsString::from("--property=PrivatePIDs"),
                OsString::from("--property=ProtectProc"),
                OsString::from("--property=ProcSubset"),
                OsString::from("--property=NoNewPrivileges"),
                OsString::from("--property=UMask"),
                OsString::from("--property=InaccessiblePaths"),
                OsString::from("--property=LoadCredential"),
                OsString::from("--property=OpenFile"),
                OsString::from("--property=WorkingDirectory"),
                OsString::from("--property=ControlGroup"),
            ])
            .stdin(Stdio::null())
            .env_clear()
            .env("LANG", "C.UTF-8")
            .output()
            .with_context(|| format!("observing systemd unit {unit}"))?;
        let text =
            std::str::from_utf8(&output.stdout).context("systemd show output is not UTF-8")?;
        let mut values = BTreeMap::new();
        let mut open_files = Vec::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let (name, value) = line
                .split_once('=')
                .context("systemd show output is malformed")?;
            if name == "OpenFile" {
                open_files.push(value.to_owned());
                continue;
            }
            ensure!(
                values.insert(name.to_owned(), value.to_owned()).is_none(),
                "systemd show property is duplicated"
            );
        }
        if values
            .get("LoadState")
            .is_some_and(|value| value == "not-found")
        {
            return Ok(None);
        }
        ensure!(
            output.status.success(),
            "systemd show failed for {unit}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Some(SystemdUnitObservation {
            properties: values,
            open_files,
        }))
    }

    fn observe_unit(
        &self,
        unit: &str,
        expected_executable: &Path,
        runtime_instance_id: &str,
        activation_signer_identity_id: &str,
        activation_signer_public_key: &[u8],
        environment_names: &[String],
        service_credential_names: &[String],
        parent_only_file_descriptors: &[ParentOnlyFileDescriptorObservation],
    ) -> Result<SystemdWorkloadObservation> {
        ensure!(
            service_credential_names
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "service credential names are not unique and sorted"
        );
        let unit_observation = self
            .show_unit(unit)?
            .with_context(|| format!("systemd unit {unit} is absent"))?;
        let values = &unit_observation.properties;
        ensure!(
            unit_observation.open_files
                == parent_only_open_file_properties(parent_only_file_descriptors)?,
            "systemd parent-only descriptor contract differs from the Idunn launch"
        );
        ensure!(
            values
                .get("ActiveState")
                .is_some_and(|value| value == "active")
                && values
                    .get("SubState")
                    .is_some_and(|value| value == "running"),
            "systemd unit {unit} is not running"
        );
        let invocation_id = values
            .get("InvocationID")
            .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .context("systemd unit has no canonical invocation id")?
            .clone();
        let exec_main_start_timestamp_monotonic =
            required_systemd_property(&values, "ExecMainStartTimestampMonotonic")?
                .parse::<u64>()
                .context("systemd unit has no numeric main-process start timestamp")?;
        ensure!(
            exec_main_start_timestamp_monotonic > 0,
            "systemd unit has no main-process start timestamp"
        );
        let unit_description = required_systemd_property(&values, "Description")?.to_owned();
        let service_type = required_systemd_property(&values, "Type")?.to_owned();
        let restart_policy = required_systemd_property(&values, "Restart")?.to_owned();
        let kill_mode = required_systemd_property(&values, "KillMode")?.to_owned();
        let dynamic_user = parse_systemd_boolean(&values, "DynamicUser")?;
        // Recorded as evidence of which identity actually ran, not asserted on.
        let systemd_user = systemd_property(&values, "User")?.to_owned();
        let systemd_group = systemd_property(&values, "Group")?.to_owned();
        let supplementary_groups = systemd_property(&values, "SupplementaryGroups")?.to_owned();
        let capability_bounding_set =
            systemd_property(&values, "CapabilityBoundingSet")?.to_owned();
        let ambient_capabilities = systemd_property(&values, "AmbientCapabilities")?.to_owned();
        let private_mounts = parse_systemd_boolean(&values, "PrivateMounts")?;
        let private_pids = parse_systemd_boolean(&values, "PrivatePIDs")?;
        let protect_proc = required_systemd_property(&values, "ProtectProc")?.to_owned();
        let proc_subset = required_systemd_property(&values, "ProcSubset")?.to_owned();
        let no_new_privileges = parse_systemd_boolean(&values, "NoNewPrivileges")?;
        let umask = required_systemd_property(&values, "UMask")?.to_owned();
        let inaccessible_paths =
            required_systemd_property(&values, "InaccessiblePaths")?.to_owned();
        let load_credential = required_systemd_property(&values, "LoadCredential")?.to_owned();
        ensure!(
            service_type == "exec"
                && restart_policy == "no"
                && kill_mode == "mixed"
                // `User` is deliberately not asserted here. Idunn never emits a
                // User property, and systemd reports the identity it *allocated*
                // for a DynamicUser unit -- the unit name, or a generated `_du…`
                // when that is too long -- so the property is never empty for a
                // running workload and cannot tell an allocated identity from a
                // pinned one. The invariant it was reaching for, that the workload
                // does not run as a persistent named account, is carried by
                // `dynamic_user` below and by Idunn never passing User at all.
                && dynamic_user
                && supplementary_groups.is_empty()
                && capability_bounding_set.is_empty()
                && ambient_capabilities.is_empty()
                && private_mounts
                && private_pids
                && protect_proc == "invisible"
                && proc_subset == "all"
                && no_new_privileges
                && umask == "0007"
                && inaccessible_paths == self.credential_root.display().to_string()
                && load_credential
                    == if service_credential_names.is_empty() {
                        ""
                    } else {
                        "[unprintable]"
                    },
            "systemd workload isolation properties differ from the Idunn contract"
        );
        let working_directory =
            PathBuf::from(required_systemd_property(&values, "WorkingDirectory")?);
        ensure!(
            working_directory.is_absolute(),
            "systemd unit working directory is not absolute"
        );
        let control_group = required_systemd_property(&values, "ControlGroup")?.to_owned();
        ensure!(
            control_group.starts_with('/') && !control_group.contains(['\n', '\r', '\0']),
            "systemd unit control group is invalid"
        );
        let main_pid: u32 = values
            .get("MainPID")
            .context("systemd unit has no MainPID")?
            .parse()
            .context("systemd MainPID is not a u32")?;
        ensure!(main_pid > 0, "systemd unit has no live main process");
        let process_root = self.proc_root.join(main_pid.to_string());
        let process_executable = process_root.join("exe");
        let executable = fs::read_link(&process_executable)
            .with_context(|| format!("observing executable for process {main_pid}"))?;
        let mut executable_file = open_proc_magic_link(&process_executable)?;
        let executable_metadata = executable_file.metadata()?;
        let expected_executable_metadata = fs::metadata(expected_executable)?;
        #[cfg(unix)]
        let (executable_device, executable_inode) = {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                executable_metadata.is_file()
                    && executable_metadata.dev() == expected_executable_metadata.dev()
                    && executable_metadata.ino() == expected_executable_metadata.ino(),
                "systemd started an executable inode outside the sealed release"
            );
            (executable_metadata.dev(), executable_metadata.ino())
        };
        #[cfg(not(unix))]
        let (executable_device, executable_inode) = (0, 0);
        ensure!(
            fs::canonicalize(&executable)? == fs::canonicalize(expected_executable)?,
            "systemd started an executable outside the sealed release"
        );
        let executable_sha256 = sha256_reader(&mut executable_file)?;
        let process_start_time = linux_process_start_time(&process_root.join("stat"))?;
        let process_security = linux_process_security(&process_root.join("status"), main_pid)?;
        ensure!(
            process_security
                .uids
                .iter()
                .all(|uid| *uid == process_security.uids[0])
                && (61_184..=65_519).contains(&process_security.uids[0]),
            "systemd workload does not have one unprivileged process uid"
        );
        ensure!(
            process_security
                .gids
                .iter()
                .all(|gid| *gid == process_security.gids[0])
                && process_security.gids[0] > 0
                && process_security
                    .groups
                    .iter()
                    .all(|gid| *gid == process_security.gids[0]),
            "systemd workload has foreign supplementary groups"
        );
        ensure!(
            [
                process_security.cap_inheritable,
                process_security.cap_permitted,
                process_security.cap_effective,
                process_security.cap_bounding,
                process_security.cap_ambient,
            ]
            .iter()
            .all(|capabilities| *capabilities == 0),
            "systemd workload retained Linux capabilities"
        );
        ensure!(
            process_security.no_new_privileges,
            "systemd workload lacks kernel no-new-privileges enforcement"
        );
        ensure!(
            process_security.namespace_pids.len() >= 2
                && process_security.namespace_pids[0] == main_pid
                && process_security.namespace_pids.last() == Some(&1)
                && process_security.namespace_pids.iter().all(|pid| *pid > 0),
            "systemd workload is not observed in a private pid namespace"
        );
        let mount_namespace_id = linux_namespace_id(&process_root.join("ns/mnt"), "mnt")?;
        let pid_namespace_id = linux_namespace_id(&process_root.join("ns/pid"), "pid")?;
        let observer_mount_namespace_id =
            linux_namespace_id(&self.proc_root.join("self/ns/mnt"), "mnt")?;
        let observer_pid_namespace_id =
            linux_namespace_id(&self.proc_root.join("self/ns/pid"), "pid")?;
        ensure!(
            mount_namespace_id != observer_mount_namespace_id
                && pid_namespace_id != observer_pid_namespace_id,
            "systemd workload did not receive private mount and pid namespaces"
        );
        let process_control_groups = fs::read_to_string(process_root.join("cgroup"))?;
        let process_control_groups = process_control_groups.lines().collect::<Vec<_>>();
        ensure!(
            process_control_groups.len() == 1
                && process_control_groups[0] == format!("0::{control_group}"),
            "systemd MainPID is outside the unit control group"
        );
        let command_line = fs::read(process_root.join("cmdline"))?;
        ensure!(
            !command_line.is_empty(),
            "systemd MainPID has no command line"
        );
        let process_environment = read_process_environment(&process_root.join("environ"))?;
        let selected_environment =
            select_process_environment(&process_environment, environment_names)?;
        let runtime_bundle = PathBuf::from(
            selected_environment
                .get(IDUNN_RUNTIME_BUNDLE_ENVIRONMENT)
                .context("systemd MainPID lacks the Idunn runtime bundle environment")?,
        );
        ensure!(
            runtime_bundle.is_absolute(),
            "runtime bundle path is not absolute"
        );
        let credentials_directory = if service_credential_names.is_empty() {
            ensure!(
                !process_environment
                    .iter()
                    .any(|entry| entry.starts_with(b"CREDENTIALS_DIRECTORY=")),
                "systemd MainPID exposed an unowned credential directory"
            );
            None
        } else {
            let path = PathBuf::from(required_process_environment_value(
                &process_environment,
                "CREDENTIALS_DIRECTORY",
            )?);
            ensure!(
                path == Path::new("/run/credentials").join(unit),
                "systemd MainPID exposed an unexpected credential directory"
            );
            Some(path)
        };
        let activation_descriptor = &parent_only_file_descriptors[0];
        ensure!(
            activation_descriptor.fd_name == IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME
                && activation_descriptor.size == 32
                && activation_descriptor.uid == 0
                && activation_descriptor.gid == 0
                && activation_descriptor.mode == 0o400
                && activation_descriptor.links == 1,
            "activation signing descriptor is not exact root-only source material"
        );
        let mut service_credentials = Vec::new();
        for name in service_credential_names {
            let delivered_path = credentials_directory
                .as_ref()
                .context("service credential has no systemd credential directory")?
                .join(name);
            let delivered_value = delivered_path.display().to_string();
            ensure!(
                selected_environment.get(name) == Some(&delivered_value),
                "workload secret environment does not name its delivered systemd credential"
            );
            let observer_path =
                path_inside_process_root(&process_root.join("root"), &delivered_path)?;
            let mut file = open_native_read_only(&observer_path).with_context(|| {
                format!(
                    "opening delivered service credential {}",
                    delivered_path.display()
                )
            })?;
            let metadata = file.metadata()?;
            #[cfg(unix)]
            let (device, inode, uid, gid, mode, links) = {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.uid(),
                    metadata.gid(),
                    metadata.permissions().mode() & 0o777,
                    metadata.nlink(),
                )
            };
            #[cfg(not(unix))]
            let (device, inode, uid, gid, mode, links) = (0, 0, 0, 0, 0, 0);
            // systemd owns credential delivery and does not hand the file to the
            // workload by ownership: it writes it root:root with no group or
            // world access and grants the workload's (dynamic) uid read through
            // a POSIX ACL. Observed on systemd 257:
            //
            //     -r--r-----+ 1 0 0   user::r--  user:<workload uid>:r--
            //                         group::---  mask::r--  other::---
            //
            // The previous assertion required uid/gid to equal the workload's
            // and mode to be exactly 0400, a shape systemd never produces, so
            // it failed for every delivered credential. What is verified here is
            // what the file mode can carry: root-owned, unaliased, and closed to
            // group and world. The workload-only grant itself is the ACL's, and
            // is systemd's to enforce rather than ours to re-derive.
            ensure!(
                metadata.is_file()
                    && metadata.len() > 0
                    && links == 1
                    && uid == 0
                    && mode & 0o007 == 0,
                "delivered service credential is not root-owned and closed to group and world"
            );
            service_credentials.push(ServiceCredentialObservation {
                environment_name: name.clone(),
                delivered_path,
                device,
                inode,
                uid,
                gid,
                mode,
                size: metadata.len(),
                sha256: sha256_reader(&mut file)?,
            });
        }
        let final_values = self
            .show_unit(unit)?
            .with_context(|| format!("systemd unit {unit} vanished during observation"))?;
        ensure!(
            final_values == unit_observation
                && linux_process_start_time(&process_root.join("stat"))? == process_start_time,
            "systemd workload identity changed during native observation"
        );
        Ok(SystemdWorkloadObservation {
            unit: unit.to_owned(),
            unit_description,
            invocation_id,
            exec_main_start_timestamp_monotonic,
            service_type,
            restart_policy,
            kill_mode,
            dynamic_user,
            systemd_user,
            systemd_group,
            supplementary_groups,
            capability_bounding_set,
            ambient_capabilities,
            private_mounts,
            private_pids,
            protect_proc,
            proc_subset,
            no_new_privileges,
            umask,
            inaccessible_paths,
            load_credential,
            main_pid,
            process_start_time,
            process_uids: process_security.uids,
            process_gids: process_security.gids,
            process_groups: process_security.groups,
            process_cap_inheritable: process_security.cap_inheritable,
            process_cap_permitted: process_security.cap_permitted,
            process_cap_effective: process_security.cap_effective,
            process_cap_bounding: process_security.cap_bounding,
            process_cap_ambient: process_security.cap_ambient,
            process_no_new_privileges: process_security.no_new_privileges,
            process_namespace_pids: process_security.namespace_pids,
            mount_namespace_id,
            pid_namespace_id,
            executable,
            executable_device,
            executable_inode,
            executable_sha256,
            runtime_instance_id: runtime_instance_id.to_owned(),
            working_directory,
            runtime_bundle,
            command_line_sha256: sha256_id(&command_line),
            environment_names: environment_names.to_vec(),
            environment_contract_sha256: sha256_id(&rmp_serde::to_vec(&selected_environment)?),
            control_group,
            credentials_directory,
            parent_only_file_descriptors: parent_only_file_descriptors.to_vec(),
            activation_signer_identity_id: activation_signer_identity_id.to_owned(),
            activation_signer_public_key: activation_signer_public_key.to_vec(),
            service_credentials,
        })
    }

    fn launch_command(
        &self,
        declaration: &TargetDeclaration,
        binding: &OperatorBinding,
        installed: &Path,
    ) -> Result<Vec<OsString>> {
        let executable_artifact =
            release_artifact(declaration, &declaration.service.executable_artifact)?;
        let executable = installed.join(&executable_artifact.destination);
        ensure!(executable.is_file(), "sealed service executable is absent");
        let mut command = vec![executable.into_os_string()];
        for argument in &declaration.service.arguments {
            command.push(match argument {
                LaunchArgument::Literal { value } => value.into(),
                LaunchArgument::Binding { name } => binding.workload.systemd()?.argument_bindings
                    [name]
                    .clone()
                    .into(),
            });
        }
        Ok(command)
    }

    fn launch_environment(
        &self,
        binding: &OperatorBinding,
        bundle: &Path,
        expected: &IdunnExpectedIncarnationRecord,
        unit: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut environment = binding.workload.systemd()?.environment.clone();
        let credentials_directory = Path::new("/run/credentials").join(unit);
        for name in binding
            .workload
            .systemd()?
            .secret_files
            .keys()
            .filter(|name| name.as_str() != RUNTIME_PRESENCE_IDENTITY_BINDING)
        {
            ensure!(
                environment
                    .insert(
                        name.clone(),
                        credentials_directory.join(name).display().to_string(),
                    )
                    .is_none(),
                "workload environment and secret bindings collide"
            );
        }
        ensure!(
            environment
                .insert(
                    IDUNN_RUNTIME_BUNDLE_ENVIRONMENT.into(),
                    bundle.display().to_string(),
                )
                .is_none(),
            "operator binding attempts to replace the Idunn runtime bundle"
        );
        match &expected.route {
            Some(route) => {
                let (host, port) = endpoint_host_port(
                    &route.candidate_endpoint,
                    &format!("{}://", route.transport),
                )?;
                ensure!(
                    environment
                        .insert(
                            IDUNN_RUNTIME_CANDIDATE_BIND_ENVIRONMENT.into(),
                            format!("{host}:{port}"),
                        )
                        .is_none(),
                    "operator binding attempts to replace the Idunn candidate bind"
                );
            }
            None => ensure!(
                !environment.contains_key(IDUNN_RUNTIME_CANDIDATE_BIND_ENVIRONMENT),
                "unrouted workload carries an Idunn candidate bind"
            ),
        }
        match &binding.process_write_lease {
            Some(write_lease) => ensure!(
                environment
                    .insert(
                        IDUNN_PROCESS_WRITE_LEASE_ENVIRONMENT.into(),
                        write_lease.record_path.display().to_string(),
                    )
                    .is_none(),
                "operator binding attempts to replace the Idunn process write lease"
            ),
            None => ensure!(
                !environment.contains_key(IDUNN_PROCESS_WRITE_LEASE_ENVIRONMENT),
                "stateless workload carries an Idunn process write lease"
            ),
        }
        Ok(environment)
    }

    fn validate_launch_observation(
        &self,
        observation: &SystemdWorkloadObservation,
        declaration: &TargetDeclaration,
        binding: &OperatorBinding,
        installed: &Path,
        bundle: &Path,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        let command = self.launch_command(declaration, binding, installed)?;
        let environment = self.launch_environment(binding, bundle, expected, &observation.unit)?;
        let environment_names = environment.keys().cloned().collect::<Vec<_>>();
        let expected_description = format!(
            "Idunn {} {}",
            binding.target,
            bundle
                .file_name()
                .context("runtime bundle has no instance id")?
                .to_string_lossy()
        );
        ensure!(
            observation.unit.ends_with(".service"),
            "workload unit has no service suffix"
        );
        let expected_group = binding.workload.systemd()?.state_group.as_deref();
        let expected_group_id = expected_group.map(resolve_group_id).transpose()?;
        let regular_credential_names = binding
            .workload
            .systemd()?
            .secret_files
            .keys()
            .filter(|name| name.as_str() != RUNTIME_PRESENCE_IDENTITY_BINDING)
            .cloned()
            .collect::<Vec<_>>();
        let expected_credentials_directory = (!regular_credential_names.is_empty())
            .then(|| Path::new("/run/credentials").join(&observation.unit));
        let descriptor_properties =
            parent_only_open_file_properties(&observation.parent_only_file_descriptors)?;
        let activation_source = self.activation_credential_source(activation)?;
        let presence_source = binding
            .workload
            .systemd()?
            .secret_files
            .get(RUNTIME_PRESENCE_IDENTITY_BINDING)
            .context("workload has no parent-only runtime presence identity source")?;
        let observed_credential_names = observation
            .service_credentials
            .iter()
            .map(|credential| credential.environment_name.clone())
            .collect::<Vec<_>>();
        // Checked one clause at a time so a mismatch names itself. As a single
        // conjunction this reported only "the launch contract differs", leaving
        // an operator to bisect thirty conditions by hand against a workload
        // that has already exited.
        //
        // `systemd_user` is deliberately absent: systemd reports the identity it
        // allocated for a DynamicUser unit, so it is never empty for a running
        // workload. See the same note in `observe_unit`.
        let expected_load_credential = if regular_credential_names.is_empty() {
            ""
        } else {
            "[unprintable]"
        };
        let launch_contract: [(bool, &str); 28] = [
            (
                observation.unit_description == expected_description,
                "unit description",
            ),
            (observation.service_type == "exec", "service type"),
            (observation.restart_policy == "no", "restart policy"),
            (observation.kill_mode == "mixed", "kill mode"),
            (observation.dynamic_user, "dynamic user"),
            (
                match expected_group {
                    Some(group) => observation.systemd_group == group,
                    None => observation.systemd_group.is_empty(),
                },
                "systemd group",
            ),
            (
                observation.supplementary_groups.is_empty(),
                "supplementary groups",
            ),
            (
                observation.capability_bounding_set.is_empty(),
                "capability bounding set",
            ),
            (
                observation.ambient_capabilities.is_empty(),
                "ambient capabilities",
            ),
            (observation.private_mounts, "private mounts"),
            (observation.private_pids, "private PIDs"),
            (observation.protect_proc == "invisible", "protect proc"),
            (observation.proc_subset == "all", "proc subset"),
            (observation.no_new_privileges, "no new privileges"),
            (observation.umask == "0007", "umask"),
            (
                observation.inaccessible_paths == self.credential_root.display().to_string(),
                "inaccessible paths",
            ),
            (
                observation.load_credential == expected_load_credential,
                "load credential",
            ),
            (
                observation.working_directory == installed,
                "working directory",
            ),
            (observation.runtime_bundle == bundle, "runtime bundle"),
            (
                observation.credentials_directory == expected_credentials_directory,
                "credentials directory",
            ),
            (
                observation.command_line_sha256 == sha256_id(&proc_command_line(&command)),
                "command line",
            ),
            (
                observation.environment_names == environment_names,
                "environment names",
            ),
            (
                observation.environment_contract_sha256
                    == sha256_id(&rmp_serde::to_vec(&environment)?),
                "environment contract",
            ),
            (descriptor_properties.len() == 2, "descriptor count"),
            (
                observation.parent_only_file_descriptors[0].source_path == activation_source
                    && observation.parent_only_file_descriptors[0].size == 32
                    && observation.parent_only_file_descriptors[1].source_path == *presence_source,
                "parent-only descriptors",
            ),
            (
                observation.activation_signer_identity_id
                    == activation.activation_signer_identity_id
                    && observation.activation_signer_public_key
                        == activation.activation_signer_public_key,
                "activation signer",
            ),
            (
                observed_credential_names == regular_credential_names,
                "credential names",
            ),
            (
                match expected_group_id {
                    Some(group_id) => observation.process_gids[0] == group_id,
                    None => observation.process_gids[0] == observation.process_uids[0],
                },
                "process group id",
            ),
        ];
        for (holds, clause) in launch_contract {
            ensure!(
                holds,
                "running workload launch contract differs from the admitted launch: {clause}"
            );
        }
        ensure!(
            Path::new(&observation.control_group).file_name()
                == Some(OsStr::new(&observation.unit)),
            "running workload control group belongs to another unit"
        );
        Ok(())
    }

    fn validate_writable_bindings(&self, binding: &OperatorBinding) -> Result<()> {
        let Some(state_group) = binding.workload.systemd()?.state_group.as_deref() else {
            ensure!(
                binding.workload.systemd()?.state_root.is_none()
                    && binding.workload.systemd()?.read_write_paths.is_empty(),
                "dynamic workload writable paths have no fixed state group"
            );
            return Ok(());
        };
        let state_group_id = resolve_group_id(state_group)?;
        if let Some(state_root) = &binding.workload.systemd()?.state_root {
            validate_workload_writable_path(state_root, state_group_id, true)?;
        }
        for path in &binding.workload.systemd()?.read_write_paths {
            validate_workload_writable_path(path, state_group_id, false)?;
        }
        Ok(())
    }

    fn start_transient(
        &self,
        declaration: &TargetDeclaration,
        binding: &OperatorBinding,
        installed: &Path,
        bundle: &Path,
        expected: &IdunnExpectedIncarnationRecord,
        parent_only_file_descriptors: &[ParentOnlyFileDescriptorObservation],
        unit: &str,
    ) -> Result<()> {
        ensure!(
            declaration
                .service
                .required_environment
                .contains(IDUNN_RUNTIME_BUNDLE_ENVIRONMENT),
            "target does not declare the standard Idunn runtime bundle"
        );
        ensure!(
            !binding
                .workload
                .systemd()?
                .environment
                .contains_key(IDUNN_RUNTIME_BUNDLE_ENVIRONMENT)
                && !binding
                    .workload
                    .systemd()?
                    .secret_files
                    .contains_key(IDUNN_RUNTIME_BUNDLE_ENVIRONMENT),
            "operator binding attempts to replace the Idunn runtime bundle"
        );
        validate_service_credential_sources(&binding.workload.systemd()?.secret_files)?;
        let parent_only_open_files =
            parent_only_open_file_properties(parent_only_file_descriptors)?;
        let environment = self.launch_environment(binding, bundle, expected, unit)?;
        let executable_artifact =
            release_artifact(declaration, &declaration.service.executable_artifact)?;
        let executable = installed.join(&executable_artifact.destination);
        ensure!(executable.is_file(), "sealed service executable is absent");
        let unit_description = format!(
            "Idunn {} {}",
            binding.target,
            bundle
                .file_name()
                .context("runtime bundle has no instance id")?
                .to_string_lossy()
        );
        let mut args = vec![
            OsString::from("--no-block"),
            OsString::from("--no-ask-password"),
            OsString::from("--expand-environment=no"),
            OsString::from(format!("--unit={unit}")),
            OsString::from("--property=Type=exec"),
            OsString::from("--property=Restart=no"),
            OsString::from("--property=KillMode=mixed"),
            OsString::from("--property=DynamicUser=yes"),
            OsString::from("--property=SupplementaryGroups="),
            OsString::from("--property=CapabilityBoundingSet="),
            OsString::from("--property=AmbientCapabilities="),
            OsString::from("--property=NoNewPrivileges=yes"),
            OsString::from("--property=PrivateMounts=yes"),
            OsString::from("--property=PrivatePIDs=yes"),
            OsString::from("--property=ProtectProc=invisible"),
            OsString::from("--property=ProcSubset=all"),
            OsString::from("--property=PrivateTmp=yes"),
            OsString::from("--property=ProtectSystem=strict"),
            OsString::from("--property=ProtectHome=yes"),
            OsString::from("--property=ProtectControlGroups=yes"),
            OsString::from("--property=ProtectKernelModules=yes"),
            OsString::from("--property=ProtectKernelTunables=yes"),
            OsString::from("--property=RestrictSUIDSGID=yes"),
            OsString::from("--property=LockPersonality=yes"),
            OsString::from("--property=UMask=0007"),
            OsString::from(format!("--property=Description={unit_description}")),
            OsString::from(format!(
                "--property=InaccessiblePaths={}",
                self.credential_root.display()
            )),
            OsString::from(format!(
                "--property=MemoryMax={}M",
                binding.workload.systemd()?.memory_mebibytes
            )),
            OsString::from(format!(
                "--property=CPUQuota={}%",
                binding.workload.systemd()?.cpu_quota_percent
            )),
            OsString::from(format!("--working-directory={}", installed.display())),
            OsString::from(format!("--property=ReadOnlyPaths={}", installed.display())),
            OsString::from(format!("--property=ReadOnlyPaths={}", bundle.display())),
        ];
        for open_file in parent_only_open_files {
            args.push(OsString::from(format!("--property=OpenFile={open_file}")));
        }
        if let Some(state_group) = &binding.workload.systemd()?.state_group {
            args.push(OsString::from(format!("--property=Group={state_group}")));
        }
        if binding.workload.systemd()?.network == WorkloadNetwork::None {
            args.push(OsString::from("--property=PrivateNetwork=yes"));
        }
        if let Some(state_root) = &binding.workload.systemd()?.state_root {
            args.push(OsString::from(format!(
                "--property=ReadWritePaths={}",
                state_root.display()
            )));
        }
        if let Some(write_lease) = &binding.process_write_lease {
            for path in [write_lease.record_path.clone(), write_lease.lock_path()] {
                args.push(OsString::from(format!(
                    "--property=ReadOnlyPaths=-{}",
                    path.display()
                )));
            }
        }
        for path in &binding.workload.systemd()?.read_only_paths {
            args.push(OsString::from(format!(
                "--property=ReadOnlyPaths={}",
                path.display()
            )));
        }
        for path in &binding.workload.systemd()?.read_write_paths {
            args.push(OsString::from(format!(
                "--property=ReadWritePaths={}",
                path.display()
            )));
        }
        if binding.workload.systemd()?.devices.is_empty() {
            args.push(OsString::from("--property=PrivateDevices=yes"));
        } else {
            args.push(OsString::from("--property=DevicePolicy=closed"));
            for device in &binding.workload.systemd()?.devices {
                args.push(OsString::from(format!(
                    "--property=DeviceAllow={} rw",
                    device.display()
                )));
            }
        }
        for (name, value) in environment {
            args.push(OsString::from(format!("--setenv={name}={value}")));
        }
        for (name, path) in binding
            .workload
            .systemd()?
            .secret_files
            .iter()
            .filter(|(name, _)| name.as_str() != RUNTIME_PRESENCE_IDENTITY_BINDING)
        {
            args.push(OsString::from(format!(
                "--property=LoadCredential={name}:{}",
                path.display()
            )));
        }
        args.extend(self.launch_command(declaration, binding, installed)?);
        self.command(&self.systemd_run_program, args)?;
        Ok(())
    }
}

impl WorkloadPort for SystemdTransientWorkloadDriver {
    fn install(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &MaterializedRelease,
    ) -> Result<InstalledReleaseObservation> {
        self.install_release(plan, release)
    }

    fn prepare_activation(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        launch: IdunnRuntimeActivationLaunch,
    ) -> Result<IdunnRuntimeActivationRecord> {
        plan.validate()?;
        expected.validate()?;
        let proposed_activation = launch.activation();
        proposed_activation.validate()?;
        ensure!(
            expected.plan_id == plan.plan_id
                && proposed_activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "prepared activation does not belong to the deployment plan"
        );
        let (_, binding) = plan.parsed_inputs()?;
        ensure!(
            expected.target == binding.target,
            "prepared activation target differs from the operator binding"
        );
        let unit = self.unit_name(
            &binding.workload.systemd()?.unit_prefix,
            &proposed_activation.runtime_instance_id,
        )?;
        ensure!(
            self.show_unit(&unit)?.is_none(),
            "refusing to replace prepared activation material while its deterministic unit exists"
        );
        ensure_activation_credential_root(&self.credential_root)?;
        let source = self.activation_credential_source(proposed_activation)?;
        match fs::symlink_metadata(&source) {
            Ok(_) => remove_activation_credential_source(&source)
                .context("retiring an orphaned pre-persistence activation credential")?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspecting pre-persistence activation credential {}",
                        source.display()
                    )
                });
            }
        }
        let (activation, written_source) = self.write_activation_credential(launch)?;
        if written_source != source {
            let error =
                anyhow::anyhow!("prepared activation used a nondeterministic credential path");
            return match remove_activation_credential_source(&written_source) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "deleting nondeterministic activation credential also failed: {cleanup:#}"
                ))),
            };
        }
        Ok(activation)
    }

    fn start_prepared(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &SealedRelease,
        installed: &InstalledReleaseObservation,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<WorkloadObservation> {
        plan.validate()?;
        release.validate_against(plan)?;
        self.validate_installed_release(plan, release, installed)?;
        expected.validate()?;
        activation.validate()?;
        ensure!(
            expected.plan_id == plan.plan_id
                && expected.sealed_release_id == release.sealed_release_id
                && activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "workload inputs do not describe one sealed incarnation"
        );
        let (declaration, binding) = plan.parsed_inputs()?;
        self.validate_writable_bindings(&binding)?;
        let executable_artifact =
            release_artifact(&declaration, &declaration.service.executable_artifact)?;
        let executable = installed.root.join(&executable_artifact.destination);
        let unit = self.unit_name(
            &binding.workload.systemd()?.unit_prefix,
            &activation.runtime_instance_id,
        )?;
        let bundle = self.prepare_runtime_bundle(&binding, expected, activation)?;
        let service_credential_names = binding
            .workload
            .systemd()?
            .secret_files
            .keys()
            .filter(|name| name.as_str() != RUNTIME_PRESENCE_IDENTITY_BINDING)
            .cloned()
            .collect::<Vec<_>>();
        let environment_names = self
            .launch_environment(&binding, &bundle, expected, &unit)?
            .into_keys()
            .collect::<Vec<_>>();
        let parent_only_file_descriptors =
            self.parent_only_file_descriptors(&binding, expected, activation)?;
        let unit_exists = self.show_unit(&unit)?.is_some();
        if !unit_exists {
            self.start_transient(
                &declaration,
                &binding,
                &installed.root,
                &bundle,
                expected,
                &parent_only_file_descriptors,
                &unit,
            )?;
        }
        let mut last_error = None;
        for _ in 0..100 {
            match self.observe_unit(
                &unit,
                &executable,
                &activation.runtime_instance_id,
                &activation.activation_signer_identity_id,
                &activation.activation_signer_public_key,
                &environment_names,
                &service_credential_names,
                &parent_only_file_descriptors,
            ) {
                Ok(observation) => {
                    let validation = self.validate_launch_observation(
                        &observation,
                        &declaration,
                        &binding,
                        &installed.root,
                        &bundle,
                        expected,
                        &activation,
                    );
                    match validation.and_then(|()| {
                        ensure!(
                            observation.executable_sha256 == expected.artifact_sha256,
                            "started workload executable differs from Expected"
                        );
                        Ok(())
                    }) {
                        Ok(()) => return Ok(WorkloadObservation::Systemd(observation)),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(error) => last_error = Some(error),
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("systemd did not expose the candidate")))
    }

    fn discard_prepared(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        plan.validate()?;
        expected.validate()?;
        activation.validate()?;
        ensure!(
            expected.plan_id == plan.plan_id
                && activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "discarded activation does not belong to the prepared deployment"
        );
        let (_, binding) = plan.parsed_inputs()?;
        ensure!(
            binding.target == expected.target,
            "discarded activation target differs from its binding"
        );
        let unit = self.unit_name(
            &binding.workload.systemd()?.unit_prefix,
            &activation.runtime_instance_id,
        )?;
        let unit_description = format!(
            "Idunn {} {}",
            binding.target, activation.runtime_instance_id
        );
        self.stop_submitted_unit(&unit, &unit_description)
            .context("stopping prepared candidate without a durable workload observation")?;
        remove_activation_credential_source(&self.activation_credential_source(activation)?)
            .context("deleting discarded activation credential source")
    }

    fn observe(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        prior: &WorkloadObservation,
    ) -> Result<WorkloadObservation> {
        expected.validate()?;
        activation.validate()?;
        let prior = prior.systemd()?;
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?
                && activation.runtime_instance_id == prior.runtime_instance_id,
            "workload observation belongs to another activation"
        );
        let observed = self.observe_unit(
            &prior.unit,
            &prior.executable,
            &activation.runtime_instance_id,
            &activation.activation_signer_identity_id,
            &activation.activation_signer_public_key,
            &prior.environment_names,
            &prior
                .service_credentials
                .iter()
                .map(|credential| credential.environment_name.clone())
                .collect::<Vec<_>>(),
            &prior.parent_only_file_descriptors,
        )?;
        ensure!(
            observed == *prior && observed.executable_sha256 == expected.artifact_sha256,
            "native workload identity changed after observation"
        );
        let credential_source = self.activation_credential_source(activation)?;
        remove_activation_credential_source(&credential_source)
            .context("retiring a recovered activation credential source")?;
        let after_unlink = self.observe_unit(
            &prior.unit,
            &prior.executable,
            &activation.runtime_instance_id,
            &activation.activation_signer_identity_id,
            &activation.activation_signer_public_key,
            &prior.environment_names,
            &prior
                .service_credentials
                .iter()
                .map(|credential| credential.environment_name.clone())
                .collect::<Vec<_>>(),
            &prior.parent_only_file_descriptors,
        )?;
        ensure!(
            after_unlink == observed,
            "native workload identity changed after recovered credential cleanup"
        );
        Ok(WorkloadObservation::Systemd(after_unlink))
    }

    fn is_permanently_stopped(&self, observation: &WorkloadObservation) -> Result<bool> {
        let observation = observation.systemd()?;
        ensure!(
            observation.restart_policy == "no",
            "workload unit does not carry the admitted Restart=no policy"
        );
        let Some(unit) = self.show_unit(&observation.unit)? else {
            // systemd has forgotten the unit entirely, so nothing will start it.
            return Ok(true);
        };
        Ok(unit.properties.get("ActiveState").map(String::as_str) == Some("failed"))
    }

    fn stop(&self, observation: &WorkloadObservation) -> Result<()> {
        let observation = observation.systemd()?;
        let Some(unit_observation) = self.show_unit(&observation.unit)? else {
            return Ok(());
        };
        let values = &unit_observation.properties;
        let active = required_systemd_property(&values, "ActiveState")?;
        let sub = required_systemd_property(&values, "SubState")?;
        if active == "active" && sub == "running" {
            let current = self.observe_unit(
                &observation.unit,
                &observation.executable,
                &observation.runtime_instance_id,
                &observation.activation_signer_identity_id,
                &observation.activation_signer_public_key,
                &observation.environment_names,
                &observation
                    .service_credentials
                    .iter()
                    .map(|credential| credential.environment_name.clone())
                    .collect::<Vec<_>>(),
                &observation.parent_only_file_descriptors,
            )?;
            ensure!(
                current == *observation,
                "refusing to stop a workload whose native identity changed"
            );
        } else {
            ensure!(
                values.get("InvocationID") == Some(&observation.invocation_id)
                    && values.get("Description") == Some(&observation.unit_description)
                    && matches!(active, "inactive" | "failed"),
                "refusing to stop a foreign or transitional systemd unit"
            );
        }
        self.command(
            &self.systemctl_program,
            [OsString::from("stop"), OsString::from(&observation.unit)],
        )?;
        if let Some(unit_observation) = self.show_unit(&observation.unit)? {
            let values = &unit_observation.properties;
            ensure!(
                values
                    .get("ActiveState")
                    .is_some_and(|state| state == "inactive" || state == "failed"),
                "systemd unit remained active after stop"
            );
        }
        Ok(())
    }
}

/// One dedicated CultCache file is the write-lease authority for one target.
/// The admitted target holds its sibling lock shared for that process lifetime.
/// Grant and revocation therefore make one nonblocking CAS attempt; revocation
/// follows an exact incumbent stop. A foreign or stuck holder cannot freeze
/// Idunn's control loop. Route membership never enters this store.
pub struct CultCacheWriteLeaseDriver {
    pub target: String,
    pub record_path: PathBuf,
}

impl CultCacheWriteLeaseDriver {
    pub fn new(target: impl Into<String>, record_path: impl Into<PathBuf>) -> Self {
        Self {
            target: target.into(),
            record_path: record_path.into(),
        }
    }

    fn current(&self) -> Result<Option<(CultCacheEnvelope, IdunnProcessWriteLeaseRecord)>> {
        validate_root_authority_path(&self.record_path)?;
        if !self.record_path.exists() {
            return Ok(None);
        }
        let entries = SingleFileMessagePackBackingStore::new(&self.record_path)
            .pull_all_read_only_snapshot()?;
        match entries.as_slice() {
            [] => Ok(None),
            [envelope]
                if envelope.r#type == IdunnProcessWriteLeaseRecord::TYPE
                    && envelope.schema_id.as_deref() == Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA) =>
            {
                let lease = IdunnProcessWriteLeaseRecord::decode_canonical(&envelope.payload)?;
                ensure!(
                    envelope.key == lease.target,
                    "write-lease key is not its target"
                );
                ensure!(
                    lease.target == self.target,
                    "write-lease store belongs to another target"
                );
                Ok(Some((envelope.clone(), lease)))
            }
            _ => bail!("process write-lease store is foreign or ambiguous"),
        }
    }

    fn envelope(&self, lease: &IdunnProcessWriteLeaseRecord) -> Result<CultCacheEnvelope> {
        Ok(CultCacheEnvelope {
            key: lease.target.clone(),
            r#type: IdunnProcessWriteLeaseRecord::TYPE.into(),
            payload: lease.canonical_bytes()?,
            stored_at: rfc3339_millis(lease.issued_at_unix_millis)?,
            schema_id: Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into()),
        })
    }

    fn validate_grant(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<()> {
        expected.validate()?;
        activation.validate()?;
        lease.validate()?;
        let expected_sha256 = expected.canonical_sha256()?;
        let activation_sha256 = activation.canonical_sha256()?;
        ensure!(
            self.target == expected.target
                && lease.target == expected.target
                && lease.expected_projection_sha256 == expected_sha256
                && lease.plan_id == expected.plan_id
                && lease.incarnation_id == expected.incarnation_id
                && lease.sealed_release_id == expected.sealed_release_id
                && lease.activation_witness_sha256 == activation_sha256
                && lease.runtime_id == expected.runtime_id
                && lease.runtime_instance_id == activation.runtime_instance_id
                && lease.state_schema_generation
                    == expected
                        .state_schema_generation
                        .as_deref()
                        .context("write-lease Expected has no state generation")?
                && lease.state_contract_sha256
                    == expected
                        .state_contract_sha256
                        .as_deref()
                        .context("write-lease Expected has no state contract")?
                && lease.warming_presence_sha256 == warming.signed_presence_sha256()
                && warming.runtime_instance_id() == activation.runtime_instance_id.as_str(),
            "process write lease does not bind the exact warming candidate"
        );
        Ok(())
    }
}

impl WriteLeasePort for CultCacheWriteLeaseDriver {
    fn revoke_exact(&self, incumbent: Option<&IdunnProcessWriteLeaseRecord>) -> Result<()> {
        if let Some(incumbent) = incumbent {
            incumbent.validate()?;
            ensure!(
                incumbent.target == self.target,
                "incumbent write lease belongs to another target"
            );
        }
        let current = self.current()?;
        let Some((envelope, current_lease)) = current else {
            return Ok(());
        };
        ensure!(
            incumbent == Some(&current_lease),
            "refusing to revoke an unexpected process write lease"
        );
        let store = SingleFileMessagePackBackingStore::new(&self.record_path);
        match store.try_compare_exchange_snapshot(std::slice::from_ref(&envelope), &[])? {
            TryCompareExchangeSnapshotOutcome::Exchanged => Ok(()),
            TryCompareExchangeSnapshotOutcome::Mismatch => {
                bail!("process write lease changed while fencing the incumbent")
            }
            TryCompareExchangeSnapshotOutcome::LockContended => {
                bail!("process write lease is held by a shared-lock consumer")
            }
        }
    }

    fn observe_empty(&self) -> Result<bool> {
        Ok(self.current()?.is_none())
    }

    fn observe_exact(&self, lease: &IdunnProcessWriteLeaseRecord) -> Result<bool> {
        lease.validate()?;
        ensure!(
            lease.target == self.target,
            "observed process write lease belongs to another target"
        );
        Ok(self
            .current()?
            .is_some_and(|(_, current)| current == *lease))
    }

    fn grant(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<String> {
        self.validate_grant(expected, activation, warming, lease)?;
        if let Some((_, current)) = self.current()? {
            ensure!(
                current == *lease,
                "another process already owns the write lease"
            );
            harden_root_authority_file(&self.record_path)?;
            harden_root_authority_file(&authority_lock_path(&self.record_path))?;
            return lease.canonical_sha256();
        }
        let envelope = self.envelope(lease)?;
        let store = SingleFileMessagePackBackingStore::new(&self.record_path);
        match store.try_compare_exchange_snapshot(&[], &[envelope])? {
            TryCompareExchangeSnapshotOutcome::Exchanged => {}
            TryCompareExchangeSnapshotOutcome::Mismatch => {
                bail!("process write-lease grant lost its empty-store compare-exchange")
            }
            TryCompareExchangeSnapshotOutcome::LockContended => {
                bail!("process write lease is held by a shared-lock consumer")
            }
        }
        harden_root_authority_file(&self.record_path)?;
        harden_root_authority_file(&authority_lock_path(&self.record_path))?;
        lease.canonical_sha256()
    }

    fn observe(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<bool> {
        self.validate_grant(expected, activation, warming, lease)?;
        Ok(self
            .current()?
            .is_some_and(|(_, current)| current == *lease))
    }
}

/// Idunn projects desired identity plus its own activation and current-lease
/// facts here. Service presence is absent by construction; only Odin may
/// correlate these records with signed runtime observation into Present/Ready.
///
/// Every record is keyed by the incarnation it describes, never by the target
/// alone. A target being replaced has two incarnations at once -- the admitted
/// incumbent and the sealed candidate -- and both are projected side by side.
/// Publishing the candidate therefore cannot touch what the incumbent is; the
/// one target-keyed record is the runtime presence trust anchor, which is
/// provider lookup material and the same for every incarnation.
pub struct CultCacheTopologyDriver {
    pub projection_store: PathBuf,
    pub correlation_store: PathBuf,
}

/// The key one incarnation's projection records live under.
///
/// This is a contract with Odin's reader (`IncarnationRef::key` in
/// `odin-daemon`): `{target}@{expected projection sha256}`.
pub fn incarnation_key(expected: &IdunnExpectedIncarnationRecord) -> Result<String> {
    Ok(incarnation_key_of(
        &expected.target,
        &expected.canonical_sha256()?,
    ))
}

pub fn incarnation_key_of(target: &str, expected_sha256: &str) -> String {
    format!("{target}@{expected_sha256}")
}

/// The three record types that belong to one incarnation.
fn is_incarnation_record_type(record_type: &str) -> bool {
    matches!(
        record_type,
        IdunnExpectedIncarnationRecord::TYPE
            | IdunnRuntimeActivationRecord::TYPE
            | IdunnProcessWriteLeaseRecord::TYPE
    )
}

/// One incarnation's records as currently projected, each checked to be the
/// exact document the caller names. A record present under the key but
/// differing from the caller's is a substitution and is refused.
struct ProjectedIncarnation {
    expected: Option<CultCacheEnvelope>,
    activation: Option<CultCacheEnvelope>,
    lease: Option<CultCacheEnvelope>,
}

impl ProjectedIncarnation {
    fn read(
        entries: &[CultCacheEnvelope],
        expected: &IdunnExpectedIncarnationRecord,
        activation: Option<&IdunnRuntimeActivationRecord>,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<Self> {
        let key = incarnation_key(expected)?;
        let projected_expected =
            projection_entry(entries, IdunnExpectedIncarnationRecord::TYPE, &key)?;
        if let Some(envelope) = projected_expected {
            ensure!(
                envelope.schema_id.as_deref() == Some(IDUNN_EXPECTED_INCARNATION_SCHEMA)
                    && IdunnExpectedIncarnationRecord::decode_canonical(&envelope.payload)?
                        == *expected,
                "projected Expected under this incarnation key is substituted"
            );
        }
        let projected_activation =
            projection_entry(entries, IdunnRuntimeActivationRecord::TYPE, &key)?;
        if let Some(envelope) = projected_activation {
            let current = IdunnRuntimeActivationRecord::decode_canonical(&envelope.payload)?;
            ensure!(
                envelope.schema_id.as_deref() == Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA)
                    && activation == Some(&current),
                "projected activation under this incarnation key is substituted"
            );
        }
        let projected_lease = projection_entry(entries, IdunnProcessWriteLeaseRecord::TYPE, &key)?;
        if let Some(envelope) = projected_lease {
            let current = IdunnProcessWriteLeaseRecord::decode_canonical(&envelope.payload)?;
            ensure!(
                envelope.schema_id.as_deref() == Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA)
                    && lease == Some(&current),
                "projected write lease under this incarnation key is substituted"
            );
        }
        Ok(Self {
            expected: projected_expected.cloned(),
            activation: projected_activation.cloned(),
            lease: projected_lease.cloned(),
        })
    }
}

fn expected_envelope(expected: &IdunnExpectedIncarnationRecord) -> Result<CultCacheEnvelope> {
    Ok(CultCacheEnvelope {
        key: incarnation_key(expected)?,
        r#type: IdunnExpectedIncarnationRecord::TYPE.into(),
        payload: expected.canonical_bytes()?,
        stored_at: chrono::Utc::now().to_rfc3339(),
        schema_id: Some(IDUNN_EXPECTED_INCARNATION_SCHEMA.into()),
    })
}

fn anchor_envelope(anchor: &GameCultServiceTrustAnchorRecord) -> Result<CultCacheEnvelope> {
    Ok(CultCacheEnvelope {
        key: anchor.trust_anchor_id.clone(),
        r#type: GameCultServiceTrustAnchorRecord::TYPE.into(),
        payload: rmp_serde::to_vec(anchor)?,
        stored_at: rfc3339_millis(anchor.bound_at_unix_millis)?,
        schema_id: Some(GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA.into()),
    })
}

fn activation_envelope(
    key: &str,
    activation: &IdunnRuntimeActivationRecord,
) -> Result<CultCacheEnvelope> {
    Ok(CultCacheEnvelope {
        key: key.to_owned(),
        r#type: IdunnRuntimeActivationRecord::TYPE.into(),
        payload: activation.canonical_bytes()?,
        stored_at: rfc3339_millis(activation.issued_at_unix_millis)?,
        schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
    })
}

/// Same-content envelopes compare equal regardless of `stored_at`, which is
/// a publication timestamp and not part of the record's identity.
fn same_record(current: &CultCacheEnvelope, replacement: &CultCacheEnvelope) -> bool {
    current.key == replacement.key
        && current.r#type == replacement.r#type
        && current.schema_id == replacement.schema_id
        && current.payload == replacement.payload
}

impl CultCacheTopologyDriver {
    fn snapshot(&self) -> Result<Vec<CultCacheEnvelope>> {
        if !self.projection_store.exists() {
            return Ok(Vec::new());
        }
        SingleFileMessagePackBackingStore::new(&self.projection_store).pull_all_read_only_snapshot()
    }

    /// Apply one whole-snapshot mutation with compare-and-swap. `mutate`
    /// returns `None` when the snapshot already has the shape it wants, and
    /// the replacement set otherwise; every writer below is one of these.
    fn mutate<F>(&self, mutate: F) -> Result<()>
    where
        F: Fn(&[CultCacheEnvelope]) -> Result<Option<Vec<CultCacheEnvelope>>>,
    {
        if let Some(parent) = self.projection_store.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = SingleFileMessagePackBackingStore::new(&self.projection_store);
        for _ in 0..8 {
            let entries = self.snapshot()?;
            let Some(replacement) = mutate(&entries)? else {
                return Ok(());
            };
            if store.compare_exchange_snapshot(&entries, &replacement)? {
                publish_projection_mode(&self.projection_store)?;
                return Ok(());
            }
        }
        bail!("CultCache projection changed repeatedly during publication")
    }

    fn other_incarnations_of(entries: &[CultCacheEnvelope], target: &str, key: &str) -> bool {
        entries.iter().any(|envelope| {
            envelope.r#type == IdunnExpectedIncarnationRecord::TYPE
                && envelope.key != key
                && envelope
                    .key
                    .split_once('@')
                    .is_some_and(|(owner, _)| owner == target)
        })
    }

    /// Records this contract does not own: the three incarnation types keyed by
    /// the bare target, as the previous single-slot projection wrote them. A
    /// publish for that target retires them; nothing reads them.
    fn is_legacy_slot_record(envelope: &CultCacheEnvelope, target: &str) -> bool {
        is_incarnation_record_type(&envelope.r#type) && envelope.key == target
    }

    /// Demote one admitted incarnation to Expected-only. Its activation and
    /// write lease are withdrawn; its Expected and anchor are ensured present.
    /// This is for an incarnation whose process is gone -- continuity about
    /// to restart it, or an abort that already fenced and stopped it -- and
    /// says nothing about any other incarnation of the target.
    pub fn demote_to_expected_only(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
        activation: &IdunnRuntimeActivationRecord,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<String> {
        expected.validate()?;
        activation.validate()?;
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "admitted activation does not bind the admitted Expected projection"
        );
        if let Some(lease) = lease {
            validate_topology_lease(expected, activation, lease)?;
        }
        let anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        let key = incarnation_key(expected)?;
        self.mutate(|entries| {
            let projected = ProjectedIncarnation::read(entries, expected, Some(activation), lease)?;
            let current_anchor = projection_entry(
                entries,
                GameCultServiceTrustAnchorRecord::TYPE,
                &anchor.trust_anchor_id,
            )?;
            if let Some(envelope) = current_anchor {
                ensure!(
                    service_trust_anchor_from_envelope(envelope)? == anchor,
                    "refusing to replace an unknown runtime presence trust anchor"
                );
            }
            if projected.expected.is_some()
                && current_anchor.is_some()
                && projected.activation.is_none()
                && projected.lease.is_none()
            {
                return Ok(None);
            }
            let mut replacement = entries
                .iter()
                .filter(|envelope| {
                    !(envelope.key == key
                        || (envelope.r#type == GameCultServiceTrustAnchorRecord::TYPE
                            && envelope.key == anchor.trust_anchor_id))
                })
                .cloned()
                .collect::<Vec<_>>();
            replacement.push(match projected.expected {
                Some(envelope) => envelope,
                None => expected_envelope(expected)?,
            });
            replacement.push(match current_anchor {
                Some(envelope) => envelope.clone(),
                None => anchor_envelope(&anchor)?,
            });
            Ok(Some(replacement))
        })?;
        expected.canonical_sha256()
    }

    /// Whether the projection currently names an activation for this
    /// incarnation. Continuity asks so it can tell a projection that still
    /// describes a dead incarnation from one already demoted to Expected-only.
    pub fn projected_activation_is_present(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
    ) -> Result<bool> {
        let key = incarnation_key(expected)?;
        Ok(
            projection_entry(&self.snapshot()?, IdunnRuntimeActivationRecord::TYPE, &key)?
                .is_some(),
        )
    }

    /// Every incarnation currently projected for one target, by its Expected.
    pub fn projected_incarnations(
        &self,
        target: &str,
    ) -> Result<Vec<IdunnExpectedIncarnationRecord>> {
        let mut incarnations = Vec::new();
        for envelope in self.snapshot()? {
            if envelope.r#type != IdunnExpectedIncarnationRecord::TYPE {
                continue;
            }
            let Some((owner, _)) = envelope.key.split_once('@') else {
                continue;
            };
            if owner != target {
                continue;
            }
            if envelope.schema_id.as_deref() != Some(IDUNN_EXPECTED_INCARNATION_SCHEMA) {
                continue;
            }
            let expected = IdunnExpectedIncarnationRecord::decode_canonical(&envelope.payload)?;
            if incarnation_key(&expected)? == envelope.key {
                incarnations.push(expected);
            }
        }
        Ok(incarnations)
    }

    /// Remove everything projected under one incarnation that nothing owns any
    /// more: not the admitted generation, not a live transaction. The caller
    /// has established that; no record under the key is compared, because a
    /// stale incarnation is stale whatever it carries. The target's anchor
    /// stays while any other incarnation of the target remains.
    pub fn withdraw_stale_incarnation(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<()> {
        expected.validate()?;
        let anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        let key = incarnation_key(expected)?;
        self.mutate(|entries| {
            if !entries.iter().any(|envelope| envelope.key == key) {
                return Ok(None);
            }
            let anchor_stays = Self::other_incarnations_of(entries, &expected.target, &key);
            Ok(Some(
                entries
                    .iter()
                    .filter(|envelope| {
                        !(envelope.key == key
                            || (!anchor_stays
                                && envelope.r#type == GameCultServiceTrustAnchorRecord::TYPE
                                && envelope.key == anchor.trust_anchor_id))
                    })
                    .cloned()
                    .collect(),
            ))
        })
    }

    /// Whether the projection already carries this Expected and its anchor.
    /// Deliberately says nothing about the activation.
    pub fn admitted_expected_projection_is_exact(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<bool> {
        expected.validate()?;
        let entries = self.snapshot()?;
        let anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        let key = incarnation_key(expected)?;
        let expected_is_exact =
            match projection_entry(&entries, IdunnExpectedIncarnationRecord::TYPE, &key)? {
                Some(envelope) => {
                    envelope.schema_id.as_deref() == Some(IDUNN_EXPECTED_INCARNATION_SCHEMA)
                        && IdunnExpectedIncarnationRecord::decode_canonical(&envelope.payload)?
                            == *expected
                }
                None => false,
            };
        let anchor_is_exact = match projection_entry(
            &entries,
            GameCultServiceTrustAnchorRecord::TYPE,
            &anchor.trust_anchor_id,
        )? {
            Some(envelope) => service_trust_anchor_from_envelope(envelope)? == anchor,
            None => false,
        };
        Ok(expected_is_exact && anchor_is_exact)
    }

    pub fn admitted_runtime_projection_is_exact(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
        activation: &IdunnRuntimeActivationRecord,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<bool> {
        expected.validate()?;
        activation.validate()?;
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "admitted activation does not bind its Expected projection"
        );
        if let Some(lease) = lease {
            validate_topology_lease(expected, activation, lease)?;
        }
        let entries = self.snapshot()?;
        let anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        let projected = ProjectedIncarnation::read(&entries, expected, Some(activation), lease)
            .context("admitted runtime projection was replaced")?;
        let anchor_is_exact = match projection_entry(
            &entries,
            GameCultServiceTrustAnchorRecord::TYPE,
            &anchor.trust_anchor_id,
        )? {
            Some(envelope) => {
                ensure!(
                    service_trust_anchor_from_envelope(envelope)? == anchor,
                    "admitted runtime presence trust anchor was replaced"
                );
                true
            }
            None => false,
        };
        Ok(projected.expected.is_some()
            && anchor_is_exact
            && projected.activation.is_some()
            && projected.lease.is_some() == lease.is_some())
    }
}

fn runtime_presence_trust_anchor(
    expected: &IdunnExpectedIncarnationRecord,
    provider_anchor: &ServiceIdentityTrustAnchor,
) -> Result<GameCultServiceTrustAnchorRecord> {
    expected.validate()?;
    ensure!(
        provider_anchor.schema_version
            == <GameCultProviderHealthIdentity as ServiceIdentityProfile>::TRUST_ANCHOR_SCHEMA,
        "runtime presence trust anchor schema is unsupported"
    );
    ensure!(
        derive_service_identity_id::<GameCultProviderHealthIdentity>(&provider_anchor.public_key)?
            == provider_anchor.identity_id
            && provider_anchor.identity_id == expected.expected_signer_identity_id,
        "runtime presence trust anchor does not bind the Expected signer"
    );
    let bound_at_unix_millis: u64 =
        chrono::DateTime::parse_from_rfc3339(&provider_anchor.identity_created_at)?
            .timestamp_millis()
            .try_into()
            .context("runtime presence trust anchor creation time predates Unix epoch")?;
    let anchor = GameCultServiceTrustAnchorRecord {
        schema_version: GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA.into(),
        trust_anchor_id: runtime_presence_trust_anchor_id(&expected.target),
        service_id: expected.target.clone(),
        runtime_id: expected.runtime_id.clone(),
        signer_identity_id: provider_anchor.identity_id.clone(),
        signer_public_key: provider_anchor.public_key.clone(),
        signature_algorithm: "ed25519".into(),
        signing_purpose: GAMECULT_RUNTIME_PRESENCE_HEALTH_SIGNING_PURPOSE.into(),
        signed_schema: GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into(),
        binding_authority: "root".into(),
        bound_at_unix_millis,
        expires_at_unix_millis: None,
        private_state_exposed: false,
    };
    anchor.validate()?;
    Ok(anchor)
}

fn runtime_presence_trust_anchor_id(target: &str) -> String {
    format!("root/{target}/runtime-presence")
}

fn validate_topology_lease(
    expected: &IdunnExpectedIncarnationRecord,
    activation: &IdunnRuntimeActivationRecord,
    lease: &IdunnProcessWriteLeaseRecord,
) -> Result<()> {
    expected.validate()?;
    activation.validate()?;
    lease.validate()?;
    ensure!(
        expected.write_lease_required
            && lease.target == expected.target
            && lease.expected_projection_sha256 == expected.canonical_sha256()?
            && lease.plan_id == expected.plan_id
            && lease.incarnation_id == expected.incarnation_id
            && lease.sealed_release_id == expected.sealed_release_id
            && lease.activation_witness_sha256 == activation.canonical_sha256()?
            && lease.state_schema_generation
                == expected
                    .state_schema_generation
                    .as_deref()
                    .context("write-lease Expected has no state generation")?
            && lease.state_contract_sha256
                == expected
                    .state_contract_sha256
                    .as_deref()
                    .context("write-lease Expected has no state contract")?
            && lease.runtime_id == expected.runtime_id
            && lease.runtime_instance_id == activation.runtime_instance_id,
        "topology write lease does not bind the exact Expected activation"
    );
    Ok(())
}

fn service_trust_anchor_from_envelope(
    envelope: &CultCacheEnvelope,
) -> Result<GameCultServiceTrustAnchorRecord> {
    ensure!(
        envelope.schema_id.as_deref() == Some(GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA),
        "service trust-anchor projection schema is foreign"
    );
    let anchor: GameCultServiceTrustAnchorRecord = rmp_serde::from_slice(&envelope.payload)?;
    ensure!(
        rmp_serde::to_vec(&anchor)? == envelope.payload,
        "service trust-anchor projection is noncanonical"
    );
    anchor.validate()?;
    Ok(anchor)
}

fn projection_entry<'a>(
    entries: &'a [CultCacheEnvelope],
    r#type: &str,
    key: &str,
) -> Result<Option<&'a CultCacheEnvelope>> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.r#type == r#type && entry.key == key);
    let current = matches.next();
    ensure!(
        matches.next().is_none(),
        "CultCache projection identity is ambiguous"
    );
    Ok(current)
}

impl TopologyPort for CultCacheTopologyDriver {
    fn publish_expected(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<String> {
        expected.validate()?;
        let anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        let key = incarnation_key(expected)?;
        let target = expected.target.clone();
        self.mutate(|entries| {
            let expected_record = expected_envelope(expected)?;
            let anchor_record = anchor_envelope(&anchor)?;
            let current_expected =
                projection_entry(entries, IdunnExpectedIncarnationRecord::TYPE, &key)?;
            let current_anchor = projection_entry(
                entries,
                GameCultServiceTrustAnchorRecord::TYPE,
                &anchor.trust_anchor_id,
            )?;
            let legacy = entries
                .iter()
                .any(|envelope| Self::is_legacy_slot_record(envelope, &target));
            if !legacy
                && current_expected.is_some_and(|current| same_record(current, &expected_record))
                && current_anchor.is_some_and(|current| same_record(current, &anchor_record))
            {
                return Ok(None);
            }
            // Only the Expected and anchor records are replaced. An activation
            // or lease already projected under this key is this incarnation's
            // own runtime fact and is left exactly as published.
            let mut replacement = entries
                .iter()
                .filter(|envelope| {
                    !((envelope.key == key
                        && envelope.r#type == IdunnExpectedIncarnationRecord::TYPE)
                        || (envelope.r#type == GameCultServiceTrustAnchorRecord::TYPE
                            && envelope.key == anchor.trust_anchor_id)
                        || Self::is_legacy_slot_record(envelope, &target))
                })
                .cloned()
                .collect::<Vec<_>>();
            replacement.push(match current_expected {
                Some(current) if same_record(current, &expected_record) => current.clone(),
                _ => expected_record,
            });
            replacement.push(anchor_record);
            Ok(Some(replacement))
        })?;
        expected.canonical_sha256()
    }

    fn withdraw_incarnation(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        provider_anchor: &ServiceIdentityTrustAnchor,
        activation: Option<&IdunnRuntimeActivationRecord>,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<()> {
        expected.validate()?;
        let exact_anchor = runtime_presence_trust_anchor(expected, provider_anchor)?;
        if let Some(activation) = activation {
            activation.validate()?;
            ensure!(
                activation.expected_projection_sha256 == expected.canonical_sha256()?
                    && activation.runtime_id == expected.runtime_id,
                "withdrawn activation does not bind the exact Expected projection"
            );
        }
        if let Some(lease) = lease {
            validate_topology_lease(
                expected,
                activation.context("withdrawn lease has no exact activation")?,
                lease,
            )?;
        }
        let key = incarnation_key(expected)?;
        self.mutate(|entries| {
            let projected = ProjectedIncarnation::read(entries, expected, activation, lease)?;
            let anchor_stays = Self::other_incarnations_of(entries, &expected.target, &key);
            let current_anchor = projection_entry(
                entries,
                GameCultServiceTrustAnchorRecord::TYPE,
                &exact_anchor.trust_anchor_id,
            )?;
            if let Some(envelope) = current_anchor {
                ensure!(
                    service_trust_anchor_from_envelope(envelope)? == exact_anchor,
                    "refusing to withdraw a substituted runtime presence trust anchor"
                );
            }
            let nothing_to_remove = projected.expected.is_none()
                && projected.activation.is_none()
                && projected.lease.is_none()
                && (anchor_stays || current_anchor.is_none());
            if nothing_to_remove {
                return Ok(None);
            }
            Ok(Some(
                entries
                    .iter()
                    .filter(|envelope| {
                        !(envelope.key == key
                            || (!anchor_stays
                                && envelope.r#type == GameCultServiceTrustAnchorRecord::TYPE
                                && envelope.key == exact_anchor.trust_anchor_id))
                    })
                    .cloned()
                    .collect(),
            ))
        })
    }

    fn publish_observed_activation(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        observation: &WorkloadObservation,
    ) -> Result<String> {
        expected.validate()?;
        activation.validate()?;
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?
                && activation.runtime_instance_id == observation.runtime_instance_id()
                && observation.executable_sha256() == expected.artifact_sha256,
            "observed activation does not name the Expected native process"
        );
        let key = incarnation_key(expected)?;
        self.mutate(|entries| {
            // The Expected must be this one. A prior activation under the key
            // is replaced without comparison: every launch is issued a fresh
            // one, and the observation just made is the authority on which is
            // current.
            let projected_expected =
                projection_entry(entries, IdunnExpectedIncarnationRecord::TYPE, &key)?
                    .context("observed activation has no current Expected projection")?;
            ensure!(
                projected_expected.schema_id.as_deref() == Some(IDUNN_EXPECTED_INCARNATION_SCHEMA)
                    && IdunnExpectedIncarnationRecord::decode_canonical(
                        &projected_expected.payload
                    )? == *expected,
                "observed activation's Expected projection is substituted"
            );
            let record = activation_envelope(&key, activation)?;
            if projection_entry(entries, IdunnRuntimeActivationRecord::TYPE, &key)?
                .is_some_and(|current| same_record(current, &record))
            {
                return Ok(None);
            }
            let mut replacement = entries
                .iter()
                .filter(|envelope| {
                    !(envelope.key == key && envelope.r#type == IdunnRuntimeActivationRecord::TYPE)
                })
                .cloned()
                .collect::<Vec<_>>();
            replacement.push(record);
            Ok(Some(replacement))
        })?;
        activation.canonical_sha256()
    }

    fn publish_process_write_lease(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        lease: &IdunnProcessWriteLeaseRecord,
    ) -> Result<String> {
        validate_topology_lease(expected, activation, lease)?;
        let key = incarnation_key(expected)?;
        let anchor_id = runtime_presence_trust_anchor_id(&expected.target);
        self.mutate(|entries| {
            let projected =
                ProjectedIncarnation::read(entries, expected, Some(activation), Some(lease))?;
            ensure!(
                projected.expected.is_some(),
                "process write lease has no current Expected projection"
            );
            ensure!(
                projected.activation.is_some(),
                "process write lease has no observed activation projection"
            );
            let projected_anchor = service_trust_anchor_from_envelope(
                projection_entry(entries, GameCultServiceTrustAnchorRecord::TYPE, &anchor_id)?
                    .context("process write lease has no runtime presence trust anchor")?,
            )?;
            ensure!(
                projected_anchor.service_id == expected.target
                    && projected_anchor.runtime_id == expected.runtime_id
                    && projected_anchor.signer_identity_id == expected.expected_signer_identity_id
                    && projected_anchor.signing_purpose
                        == GAMECULT_RUNTIME_PRESENCE_HEALTH_SIGNING_PURPOSE
                    && projected_anchor.signed_schema == GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA,
                "process write lease runtime presence trust anchor is substituted"
            );
            if projected.lease.is_some() {
                return Ok(None);
            }
            let mut replacement = entries.to_vec();
            replacement.push(CultCacheEnvelope {
                key: key.clone(),
                r#type: IdunnProcessWriteLeaseRecord::TYPE.into(),
                payload: lease.canonical_bytes()?,
                stored_at: rfc3339_millis(lease.issued_at_unix_millis)?,
                schema_id: Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into()),
            });
            Ok(Some(replacement))
        })?;
        lease.canonical_sha256()
    }

    fn withdraw_process_write_lease(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        lease: Option<&IdunnProcessWriteLeaseRecord>,
    ) -> Result<()> {
        expected.validate()?;
        activation.validate()?;
        if let Some(lease) = lease {
            validate_topology_lease(expected, activation, lease)?;
        }
        let key = incarnation_key(expected)?;
        self.mutate(|entries| {
            let current = projection_entry(entries, IdunnProcessWriteLeaseRecord::TYPE, &key)?;
            let Some(envelope) = current else {
                return Ok(None);
            };
            let projected = IdunnProcessWriteLeaseRecord::decode_canonical(&envelope.payload)?;
            ensure!(
                envelope.schema_id.as_deref() == Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA)
                    && lease == Some(&projected),
                "refusing to withdraw an unexpected process write-lease projection"
            );
            Ok(Some(
                entries
                    .iter()
                    .filter(|candidate| *candidate != envelope)
                    .cloned()
                    .collect(),
            ))
        })
    }

    fn receive(
        &self,
        target: &str,
        expected_sha256: &str,
    ) -> Result<Option<ReceivedOdinTopologyCorrelation>> {
        require_driver_id(target, "topology target")?;
        if !self.correlation_store.exists() {
            return Ok(None);
        }
        let key = incarnation_key_of(target, expected_sha256);
        let entries = SingleFileMessagePackBackingStore::new(&self.correlation_store)
            .pull_all_read_only_snapshot()?;
        let mut matches = entries.iter().filter(|envelope| {
            envelope.r#type == OdinRuntimeTopologyCorrelationRecord::TYPE && envelope.key == key
        });
        let Some(envelope) = matches.next() else {
            return Ok(None);
        };
        ensure!(
            matches.next().is_none(),
            "Odin topology correlation is ambiguous"
        );
        ensure!(
            envelope.schema_id.as_deref() == Some(ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA),
            "Odin topology correlation schema is foreign"
        );
        Ok(Some(ReceivedOdinTopologyCorrelation {
            target: target.to_owned(),
            expected_sha256: expected_sha256.to_owned(),
            canonical_bytes: envelope.payload.clone(),
        }))
    }
}

/// nginx owns proxy mechanics. Idunn supplies one exact backend membership,
/// validates the complete nginx configuration, and reloads it. Configuration
/// bytes are actuator state, not proof that the selected runtime answered on
/// the stable route; the control plane admits that proof separately.
///
/// The host firewall allow for the stable endpoint is the same authority as
/// the fragment: a route that is admitted is reachable, a route that is
/// withdrawn is not, and neither is something an operator opens by hand. The
/// driver owns exactly the one rule it writes, tagged with its route id, and
/// touches no other.
pub struct NginxRouteDriver {
    pub binding: RouteBinding,
    pub nginx_program: PathBuf,
    pub systemd_run_program: PathBuf,
    pub systemctl_program: PathBuf,
    pub ufw_program: PathBuf,
    pub preflight_root: PathBuf,
}

impl NginxRouteDriver {
    pub fn new(binding: RouteBinding) -> Self {
        Self {
            binding,
            nginx_program: PathBuf::from("/usr/sbin/nginx"),
            systemd_run_program: PathBuf::from("/usr/bin/systemd-run"),
            systemctl_program: PathBuf::from("/usr/bin/systemctl"),
            ufw_program: PathBuf::from("/usr/sbin/ufw"),
            preflight_root: PathBuf::from("/run/idunn/route-preflight"),
        }
    }

    /// The one firewall rule this route owns: inbound to the stable endpoint's
    /// address and port, its transport's protocol, tagged with the route id.
    fn endpoint_rule(&self) -> Result<Vec<OsString>> {
        let (host, port) = self.binding.stable_socket()?;
        let protocol = match self.binding.driver {
            RouteDriver::NginxStreamTcp => "tcp",
            RouteDriver::NginxStreamUdp => "udp",
        };
        Ok(vec![
            OsString::from("allow"),
            OsString::from("in"),
            OsString::from("to"),
            OsString::from(host.to_string()),
            OsString::from("port"),
            OsString::from(port.to_string()),
            OsString::from("proto"),
            OsString::from(protocol),
        ])
    }

    fn admit_endpoint(&self) -> Result<()> {
        let mut args = self.endpoint_rule()?;
        args.push(OsString::from("comment"));
        args.push(OsString::from(format!(
            "Idunn route {}",
            self.binding.route_id
        )));
        self.command(&self.ufw_program, args)
            .context("admitting the stable endpoint on the host firewall")?;
        Ok(())
    }

    fn withdraw_endpoint(&self) -> Result<()> {
        let mut args = vec![OsString::from("delete")];
        args.extend(self.endpoint_rule()?);
        match self.command(&self.ufw_program, args) {
            Ok(_) => Ok(()),
            // ufw reports a rule that is already gone as a failure; for a
            // withdrawal that is the state being asked for.
            Err(error) if format!("{error:#}").contains("non-existent") => Ok(()),
            Err(error) => {
                Err(error).context("withdrawing the stable endpoint from the host firewall")
            }
        }
    }

    fn command<I, S>(&self, program: &Path, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure!(
            program.is_absolute(),
            "route actuator program is not absolute"
        );
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .env_clear()
            .env("LANG", "C.UTF-8")
            .output()
            .with_context(|| format!("starting route actuator {}", program.display()))?;
        if !output.status.success() {
            bail!(
                "route actuator {} exited with {}: {}",
                program.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }

    fn render(&self, expected: &IdunnExpectedIncarnationRecord) -> Result<Vec<u8>> {
        expected.validate()?;
        let expected_projection_sha256 = expected.canonical_sha256()?;
        let route = expected
            .route
            .as_ref()
            .context("expected incarnation has no route")?;
        ensure!(
            route.route_id == self.binding.route_id
                && route.stable_endpoint == self.binding.stable_endpoint,
            "route driver binding differs from Expected"
        );
        let endpoint_prefix = match (self.binding.driver, route.transport.as_str()) {
            (RouteDriver::NginxStreamTcp, "http") => "http://",
            (RouteDriver::NginxStreamTcp, "tcp") => "tcp://",
            (RouteDriver::NginxStreamUdp, "rudp") => "rudp://",
            _ => bail!("route driver cannot carry the Expected transport"),
        };
        let (candidate_host, candidate_port) =
            endpoint_host_port(&route.candidate_endpoint, endpoint_prefix)?;
        ensure!(
            candidate_host == self.binding.private_host
                && (self.binding.private_port_start..=self.binding.private_port_end)
                    .contains(&candidate_port),
            "Expected candidate endpoint is outside the route binding"
        );
        let upstream = nginx_identifier(&self.binding.route_id)?;
        let rendered = match self.binding.driver {
            RouteDriver::NginxStreamTcp => {
                let (stable_host, stable_port) =
                    endpoint_host_port(&route.stable_endpoint, endpoint_prefix)?;
                format!(
                    "# Idunn Expected {expected_projection_sha256}\nupstream {upstream} {{\n    server {candidate_host}:{candidate_port};\n}}\nserver {{\n    listen {stable_host}:{stable_port};\n    proxy_pass {upstream};\n}}\n"
                )
            }
            RouteDriver::NginxStreamUdp => {
                let (stable_host, stable_port) =
                    endpoint_host_port(&route.stable_endpoint, "rudp://")?;
                format!(
                    "# Idunn Expected {expected_projection_sha256}\nupstream {upstream} {{\n    server {candidate_host}:{candidate_port};\n}}\nserver {{\n    listen {stable_host}:{stable_port} udp reuseport;\n    proxy_pass {upstream};\n}}\n"
                )
            }
        };
        Ok(rendered.into_bytes())
    }

    fn current_configuration(&self) -> Result<Option<Vec<u8>>> {
        Ok(match fs::read(&self.binding.config_path) {
            // A fragment with no bytes grants no route, so it is absence, not a
            // membership of zero servers. The distinction matters on the first
            // deployment of a routed target: the preflight bind-mounts its
            // candidate over config_path, and systemd materializes that mount
            // point, leaving an empty file behind on the host. Read as content,
            // it made the baseline "change" during validation, and then made
            // every retry report an unadmitted incumbent -- the target could
            // never be deployed a first time, and the wedge was permanent.
            Ok(bytes) if bytes.is_empty() => None,
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading route fragment"),
        })
    }

    fn write_fragment(&self, content: Option<&[u8]>) -> Result<()> {
        match content {
            Some(bytes) => atomic_replace(&self.binding.config_path, bytes),
            None => match fs::remove_file(&self.binding.config_path) {
                Ok(()) => sync_parent_directory(&self.binding.config_path),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("removing route fragment"),
            },
        }
    }

    fn reload(&self) -> Result<()> {
        self.command(&self.nginx_program, [OsString::from("-t")])?;
        self.command(
            &self.systemctl_program,
            [
                OsString::from("reload"),
                OsString::from(&self.binding.reload_unit),
            ],
        )?;
        Ok(())
    }

    fn validate_candidate_in_private_mount(&self, rendered: &[u8]) -> Result<()> {
        ensure_route_preflight_root(&self.preflight_root)?;
        let route = nginx_identifier(&self.binding.route_id)?;
        let nonce = Uuid::new_v4();
        let candidate = self
            .preflight_root
            .join(format!("{route}-{nonce}.candidate"));
        write_root_owned_file(&candidate, rendered, 0o400)?;
        let unit = format!("idunn-nginx-preflight-{nonce}");
        let validation = self.command(
            &self.systemd_run_program,
            [
                OsString::from("--wait"),
                OsString::from("--collect"),
                OsString::from("--quiet"),
                OsString::from(format!("--unit={unit}")),
                OsString::from("--property=Type=exec"),
                OsString::from("--property=PrivateMounts=yes"),
                systemd_read_only_bind_property(&candidate, &self.binding.config_path)?,
                OsString::from("--"),
                self.nginx_program.clone().into_os_string(),
                OsString::from("-t"),
            ],
        );
        let cleanup = remove_exact_root_owned_file(&candidate, 0o400);
        match (validation, cleanup) {
            (Ok(_), Ok(())) => Ok(()),
            (Err(validation), Ok(())) => {
                Err(validation).context("candidate route is invalid in a private mount namespace")
            }
            (Ok(_), Err(cleanup)) => {
                Err(cleanup).context("deleting route preflight material")
            }
            (Err(validation), Err(cleanup)) => Err(validation).context(format!(
                "candidate route is invalid; deleting its private preflight material also failed: {cleanup:#}"
            )),
        }
    }

    fn restore(&self, prior: Option<&[u8]>) -> Result<()> {
        self.write_fragment(prior)?;
        self.reload()?;
        // No prior membership means the stable endpoint no longer routes to
        // anything; its firewall allow goes with the fragment.
        if prior.is_none() {
            self.withdraw_endpoint()?;
        }
        Ok(())
    }

    fn fail_after_rollback<T>(
        &self,
        prior: Option<&[u8]>,
        failure: anyhow::Error,
        context: &str,
    ) -> Result<T> {
        match self.restore(prior) {
            Ok(()) => Err(failure).context(context.to_owned()),
            Err(rollback) => Err(failure).context(format!(
                "{context}; route rollback also failed: {rollback:#}"
            )),
        }
    }

    pub fn preflight(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        runtime_instance_id: &str,
        incumbent: Option<&RouteObservation>,
    ) -> Result<RoutePreflightReceipt> {
        let rendered = self.render(expected)?;
        ensure!(!rendered.is_empty(), "candidate route rendered empty");
        let current = self.current_configuration()?;
        let incumbent_membership_sha256 = current.as_ref().map(|bytes| sha256_id(bytes));
        match incumbent {
            Some(incumbent) => ensure!(
                incumbent.route_id == self.binding.route_id
                    && Some(incumbent.membership_sha256.as_str())
                        == incumbent_membership_sha256.as_deref(),
                "route preflight baseline differs from the admitted incumbent"
            ),
            None => ensure!(
                incumbent_membership_sha256.is_none(),
                "route preflight found an unadmitted incumbent"
            ),
        }
        self.validate_candidate_in_private_mount(&rendered)?;
        let after = self.current_configuration()?;
        ensure!(
            after == current,
            "nginx route baseline changed during candidate validation"
        );
        let receipt = RoutePreflightReceipt {
            route_id: self.binding.route_id.clone(),
            candidate_runtime_instance_id: runtime_instance_id.to_owned(),
            candidate_membership_sha256: sha256_id(&rendered),
            incumbent_runtime_instance_id: incumbent
                .map(|observation| observation.runtime_instance_id.clone()),
            incumbent_membership_sha256,
            incumbent_configuration: current,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    /// Install the candidate route membership and require nginx to accept the
    /// complete configuration. Success is not route admission: callers must
    /// still obtain a fresh signed runtime observation through the stable
    /// listener.
    pub fn install(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        runtime_instance_id: &str,
        preflight: &RoutePreflightReceipt,
        rollback_allowed: bool,
    ) -> Result<String> {
        preflight.validate()?;
        let rendered = self.render(expected)?;
        ensure!(
            preflight.route_id == self.binding.route_id
                && preflight.candidate_runtime_instance_id == runtime_instance_id
                && preflight.candidate_membership_sha256 == sha256_id(&rendered),
            "route preflight does not authorize this candidate membership"
        );
        let prior = self.current_configuration()?;
        let prior_sha256 = prior.as_ref().map(|bytes| sha256_id(bytes));
        let candidate_already_written = prior.as_deref() == Some(rendered.as_slice());
        ensure!(
            candidate_already_written
                || (prior == preflight.incumbent_configuration
                    && prior_sha256 == preflight.incumbent_membership_sha256),
            "route baseline changed after preflight"
        );
        if !candidate_already_written {
            atomic_replace(&self.binding.config_path, &rendered)?;
        }
        if let Err(error) = self.admit_endpoint().and_then(|()| self.reload()) {
            if rollback_allowed {
                return self.fail_after_rollback(
                    preflight.incumbent_configuration.as_deref(),
                    error,
                    "candidate route validation or reload failed",
                );
            }
            return Err(error).context(
                "candidate route reload failed after incumbent route authority was fenced; candidate fragment retained for retry",
            );
        }
        ensure!(
            self.current_configuration()?.as_deref() == Some(rendered.as_slice()),
            "candidate route fragment changed during reload"
        );
        Ok(sha256_id(&rendered))
    }

    pub fn observe_membership(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        membership_sha256: &str,
    ) -> Result<bool> {
        let rendered = self.render(expected)?;
        ensure!(
            membership_sha256 == sha256_id(&rendered),
            "route observation does not describe the expected membership"
        );
        Ok(self.current_configuration()?.as_deref() == Some(rendered.as_slice()))
    }

    /// Restore the exact membership owned by an admitted generation. Any
    /// different fragment is drift, not a rollback baseline, so it is never
    /// restored after a failed reload. A failed reload removes the newly
    /// written fragment so the next continuity pass cannot mistake disk bytes
    /// for an adopted route. The caller must still challenge the stable
    /// listener to prove that nginx workers adopted this membership.
    /// Put the route back exactly as the candidate found it.
    ///
    /// The preflight receipt captured the incumbent's configuration before this
    /// candidate's membership was installed, so restoring it is precise whether
    /// there was an incumbent (its bytes) or none (removal). Used when a
    /// transaction is abandoned after the fence.
    pub fn withdraw_candidate_membership(&self, preflight: &RoutePreflightReceipt) -> Result<()> {
        preflight.validate()?;
        ensure!(
            preflight.route_id == self.binding.route_id,
            "route preflight receipt describes another route"
        );
        self.restore(preflight.incumbent_configuration.as_deref())
    }

    pub fn restore_admitted_membership(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        membership_sha256: &str,
    ) -> Result<()> {
        let rendered = self.render(expected)?;
        ensure!(
            membership_sha256 == sha256_id(&rendered),
            "admitted route receipt does not describe the expected membership"
        );
        self.validate_candidate_in_private_mount(&rendered)?;
        if self.current_configuration()?.as_deref() != Some(rendered.as_slice()) {
            atomic_replace(&self.binding.config_path, &rendered)?;
        }
        if let Err(reload) = self.admit_endpoint().and_then(|()| self.reload()) {
            return match self.write_fragment(None) {
                Ok(()) => Err(reload).context("reloading the exact admitted route membership"),
                Err(cleanup) => Err(reload).context(format!(
                    "reloading the exact admitted route membership; removing its unproved fragment also failed: {cleanup:#}"
                )),
            };
        }
        ensure!(
            self.current_configuration()?.as_deref() == Some(rendered.as_slice()),
            "admitted route fragment changed during restoration"
        );
        Ok(())
    }

    /// Ask the stable listener for one freshly minted provider-owned presence
    /// statement. This method validates the CultNet wire exchange and exact
    /// document selection only; Idunn's control plane authenticates and
    /// correlates the signed payload against current authority.
    pub fn request_runtime_presence(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        message_id: &str,
    ) -> Result<RouteSnapshotResponse> {
        let route = expected
            .route
            .as_ref()
            .context("route challenge has no Expected route")?;
        ensure!(
            route.route_id == self.binding.route_id
                && route.stable_endpoint == self.binding.stable_endpoint,
            "route challenge binding differs from Expected"
        );
        self.request_runtime_presence_at(expected, message_id, &route.stable_endpoint)
    }

    /// The bootstrap exception for the first managed Odin observes Warming on
    /// the exact candidate endpoint. It does not install or admit a route.
    pub fn request_candidate_runtime_presence(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        message_id: &str,
    ) -> Result<RouteSnapshotResponse> {
        let route = expected
            .route
            .as_ref()
            .context("candidate challenge has no Expected route")?;
        ensure!(
            route.route_id == self.binding.route_id,
            "candidate challenge binding differs from Expected"
        );
        self.render(expected)?;
        self.request_runtime_presence_at(expected, message_id, &route.candidate_endpoint)
    }

    fn request_runtime_presence_at(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        message_id: &str,
        endpoint: &str,
    ) -> Result<RouteSnapshotResponse> {
        expected.validate()?;
        require_driver_id(message_id, "route challenge message")?;
        let route = expected
            .route
            .as_ref()
            .context("route challenge has no Expected route")?;
        let request = CultNetMessage::SnapshotRequest {
            message_id: message_id.to_owned(),
            schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
            record_keys: Some(vec![expected.target.clone()]),
        };
        let endpoint_prefix = match route.transport.as_str() {
            "http" => "http://",
            "tcp" => "tcp://",
            "rudp" => "rudp://",
            _ => bail!("route challenge transport is unsupported"),
        };
        let (host, port) = endpoint_host_port(endpoint, endpoint_prefix)?;
        let target = SocketAddr::new(
            host.parse()
                .context("route challenge endpoint host is not an IP address")?,
            port,
        );
        let response = match route.transport.as_str() {
            "http" => {
                let payload =
                    encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0)?;
                let response = request_http_snapshot(target, &payload)?;
                decode_cultnet_message_from_slice(&response, CultNetWireContract::CultNetSchemaV0)?
            }
            "tcp" => {
                let payload =
                    encode_cultnet_message_to_vec(&request, CultNetWireContract::CultNetSchemaV0)?;
                let response = request_tcp_snapshot(target, &payload)?;
                decode_cultnet_message_from_slice(&response, CultNetWireContract::CultNetSchemaV0)?
            }
            "rudp" => request_raw_snapshot_from_rudp_catalog(CultMeshRudpSnapshotOptions {
                target,
                runtime_id: "idunn-route-observer".into(),
                message_id: message_id.to_owned(),
                schema_ids: Some(vec![GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into()]),
                record_keys: Some(vec![expected.target.clone()]),
                ..CultMeshRudpSnapshotOptions::default()
            })?,
            _ => bail!("route challenge transport is unsupported"),
        };
        exact_route_snapshot_response(response, message_id, &expected.target)
    }

    /// Restore the exact preflight baseline. This may only replace the exact
    /// candidate authorized by the receipt; an unrelated writer is never
    /// overwritten as a side effect of rollback.
    pub fn rollback(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        runtime_instance_id: &str,
        preflight: &RoutePreflightReceipt,
    ) -> Result<()> {
        preflight.validate()?;
        let rendered = self.render(expected)?;
        ensure!(
            preflight.route_id == self.binding.route_id
                && preflight.candidate_runtime_instance_id == runtime_instance_id
                && preflight.candidate_membership_sha256 == sha256_id(&rendered),
            "route preflight does not authorize rollback of this candidate membership"
        );
        ensure!(
            self.current_configuration()?.as_deref() == Some(rendered.as_slice()),
            "candidate route changed before rollback"
        );
        self.restore(preflight.incumbent_configuration.as_deref())
    }
}

fn request_tcp_snapshot(target: SocketAddr, payload: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&target, ROUTE_SNAPSHOT_TIMEOUT)
        .with_context(|| format!("connecting stable CultNet TCP route {target}"))?;
    stream.set_read_timeout(Some(ROUTE_SNAPSHOT_TIMEOUT))?;
    stream.set_write_timeout(Some(ROUTE_SNAPSHOT_TIMEOUT))?;
    stream.write_all(&encode_frame(payload)?)?;
    stream.flush()?;

    let mut header = [0_u8; 4];
    stream
        .read_exact(&mut header)
        .context("reading stable CultNet TCP response frame")?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=ROUTE_SNAPSHOT_MAX_BYTES).contains(&length),
        "stable CultNet TCP response exceeds the route observation bound"
    );
    let mut response = vec![0_u8; length];
    stream
        .read_exact(&mut response)
        .context("reading stable CultNet TCP response payload")?;
    Ok(response)
}

fn request_http_snapshot(target: SocketAddr, payload: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&target, ROUTE_SNAPSHOT_TIMEOUT)
        .with_context(|| format!("connecting stable CultNet HTTP route {target}"))?;
    stream.set_read_timeout(Some(ROUTE_SNAPSHOT_TIMEOUT))?;
    stream.set_write_timeout(Some(ROUTE_SNAPSHOT_TIMEOUT))?;
    let head = format!(
        "POST {ROUTE_HTTP_SNAPSHOT_PATH} HTTP/1.1\r\nHost: {target}\r\nContent-Type: application/msgpack\r\nAccept: application/msgpack\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;

    let maximum = ROUTE_HTTP_MAX_HEADER_BYTES
        .checked_add(ROUTE_SNAPSHOT_MAX_BYTES)
        .context("route HTTP response bound overflow")?;
    let mut response = Vec::new();
    (&mut stream)
        .take((maximum + 1) as u64)
        .read_to_end(&mut response)
        .context("reading stable CultNet HTTP response")?;
    ensure!(
        response.len() <= maximum,
        "stable CultNet HTTP response exceeds the route observation bound"
    );
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .context("stable CultNet HTTP response has no header boundary")?;
    ensure!(
        header_end <= ROUTE_HTTP_MAX_HEADER_BYTES,
        "stable CultNet HTTP response headers exceed their bound"
    );
    let headers = std::str::from_utf8(&response[..header_end])
        .context("stable CultNet HTTP response headers are not UTF-8")?;
    let mut lines = headers.split("\r\n");
    let status = lines
        .next()
        .context("stable CultNet HTTP response has no status")?;
    ensure!(
        matches!(status, "HTTP/1.1 200 OK" | "HTTP/1.0 200 OK"),
        "stable CultNet HTTP route rejected the snapshot challenge: {status}"
    );
    let mut content_length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("stable CultNet HTTP response contains a malformed header")?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            bail!("stable CultNet HTTP response uses unsupported transfer encoding");
        }
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(
                content_length.is_none(),
                "stable CultNet HTTP response repeats Content-Length"
            );
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("stable CultNet HTTP Content-Length is invalid")?,
            );
        }
    }
    let content_length =
        content_length.context("stable CultNet HTTP response has no Content-Length")?;
    ensure!(
        content_length <= ROUTE_SNAPSHOT_MAX_BYTES
            && response.len().saturating_sub(header_end) == content_length,
        "stable CultNet HTTP response body differs from its bounded Content-Length"
    );
    Ok(response[header_end..].to_vec())
}

fn exact_route_snapshot_response(
    response: CultNetMessage,
    message_id: &str,
    target: &str,
) -> Result<RouteSnapshotResponse> {
    let CultNetMessage::SnapshotResponseRaw {
        message_id: response_id,
        documents,
    } = response
    else {
        bail!("stable route returned a non-raw snapshot response")
    };
    ensure!(
        response_id == message_id,
        "stable route snapshot response belongs to another challenge"
    );
    ensure!(
        documents.len() == 1,
        "stable route snapshot response is not the exact singleton presence document"
    );
    let document = documents
        .into_iter()
        .next()
        .expect("singleton response length was checked");
    ensure!(
        document.schema_id == GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA
            && document.record_key == target
            && document.payload_encoding == CultNetRawPayloadEncoding::Messagepack,
        "stable route substituted the runtime presence document"
    );
    ensure!(
        !document.payload.is_empty() && document.payload.len() <= ROUTE_SNAPSHOT_MAX_BYTES,
        "stable route presence payload is empty or exceeds its bound"
    );
    Ok(RouteSnapshotResponse {
        message_id: response_id,
        canonical_presence: document.payload,
    })
}

pub(crate) fn release_artifact<'a>(
    declaration: &'a TargetDeclaration,
    artifact_id: &str,
) -> Result<&'a ArtifactOutput> {
    declaration
        .artifacts
        .iter()
        .find(|artifact| artifact.id == artifact_id)
        .with_context(|| format!("target declares no artifact {artifact_id}"))
}

/// The two records every Idunn-launched workload reads at start: its Expected
/// and its activation, each immutable once written. Shared by every workload
/// driver; the systemd driver hardens the directory afterwards, the host
/// actuator writes it inside the host user's own profile.
pub(crate) fn write_runtime_bundle_records(
    bundle: &Path,
    expected: &IdunnExpectedIncarnationRecord,
    activation: &IdunnRuntimeActivationRecord,
) -> Result<()> {
    fs::create_dir_all(bundle)
        .with_context(|| format!("creating runtime bundle {}", bundle.display()))?;
    write_immutable_record(
        &bundle.join("expected.cc"),
        CultCacheEnvelope {
            key: expected.target.clone(),
            r#type: IdunnExpectedIncarnationRecord::TYPE.into(),
            payload: expected.canonical_bytes()?,
            stored_at: rfc3339_millis(activation.issued_at_unix_millis)?,
            schema_id: Some(IDUNN_EXPECTED_INCARNATION_SCHEMA.into()),
        },
    )?;
    write_immutable_record(
        &bundle.join("activation.cc"),
        CultCacheEnvelope {
            key: expected.target.clone(),
            r#type: IdunnRuntimeActivationRecord::TYPE.into(),
            payload: activation.canonical_bytes()?,
            stored_at: rfc3339_millis(activation.issued_at_unix_millis)?,
            schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
        },
    )
}

pub(crate) fn write_immutable_record(path: &Path, envelope: CultCacheEnvelope) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut store = SingleFileMessagePackBackingStore::new(path);
    match store.pull_all_read_only_snapshot()?.as_slice() {
        [] => store.push(&envelope),
        [current] if current == &envelope => Ok(()),
        _ => bail!(
            "immutable runtime document {} already differs",
            path.display()
        ),
    }
}

fn upsert_record(path: &Path, replacement: CultCacheEnvelope) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let store = SingleFileMessagePackBackingStore::new(path);
    for _ in 0..8 {
        let entries = store.pull_all_read_only_snapshot()?;
        let mut matches = entries
            .iter()
            .filter(|entry| entry.r#type == replacement.r#type && entry.key == replacement.key);
        let current = matches.next().cloned();
        ensure!(
            matches.next().is_none(),
            "CultCache projection identity is ambiguous"
        );
        if current.as_ref() == Some(&replacement) {
            return Ok(());
        }
        if store.compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: replacement.r#type.clone(),
                key: replacement.key.clone(),
                current,
            }],
            std::slice::from_ref(&replacement),
        )? {
            return Ok(());
        }
    }
    bail!("CultCache projection changed too often to publish")
}

pub(crate) fn rfc3339_millis(millis: u64) -> Result<String> {
    Ok(
        chrono::DateTime::from_timestamp_millis(i64::try_from(millis)?)
            .context("runtime timestamp is out of range")?
            .to_rfc3339(),
    )
}

#[cfg(unix)]
fn validate_frozen_source_store(store: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(store.is_absolute(), "frozen source store is not absolute");
    let canonical_store = store
        .canonicalize()
        .with_context(|| format!("resolving frozen source store {}", store.display()))?;
    ensure!(
        canonical_store == store,
        "frozen source store traverses a symlink or noncanonical path"
    );
    let metadata = fs::symlink_metadata(store)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0,
        "frozen source store is not root-owned and nonwritable"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_frozen_source_store(_store: &Path) -> Result<()> {
    bail!("frozen source materialization requires a Unix actuator")
}

#[cfg(unix)]
fn prepare_frozen_transaction_root(store: &Path, transaction_id: &str) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    require_driver_id(transaction_id, "source transaction")?;
    validate_frozen_source_store(store)?;
    let transaction_root = store.join(transaction_id);
    if transaction_root.exists() {
        remove_frozen_transaction_root(store, transaction_id)?;
    }
    fs::create_dir(&transaction_root)?;
    fs::set_permissions(&transaction_root, fs::Permissions::from_mode(0o700))?;
    Ok(transaction_root)
}

#[cfg(not(unix))]
fn prepare_frozen_transaction_root(_store: &Path, _transaction_id: &str) -> Result<PathBuf> {
    bail!("frozen source materialization requires a Unix actuator")
}

#[cfg(unix)]
fn remove_frozen_transaction_root(store: &Path, transaction_id: &str) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    require_driver_id(transaction_id, "source transaction")?;
    validate_frozen_source_store(store)?;
    let transaction_root = store.join(transaction_id);
    let metadata = fs::symlink_metadata(&transaction_root)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0,
        "frozen source transaction root is not Idunn-owned"
    );
    fs::set_permissions(&transaction_root, fs::Permissions::from_mode(0o700))?;
    fs::remove_dir_all(&transaction_root).with_context(|| {
        format!(
            "removing exact frozen source transaction {}",
            transaction_root.display()
        )
    })
}

#[cfg(not(unix))]
fn remove_frozen_transaction_root(_store: &Path, _transaction_id: &str) -> Result<()> {
    bail!("frozen source cleanup requires a Unix actuator")
}

#[cfg(unix)]
fn prepare_frozen_source_destination(destination: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "frozen source materialization requires root Idunn"
    );
    ensure!(
        destination.is_absolute(),
        "frozen source destination is not absolute"
    );
    let parent = destination
        .parent()
        .context("frozen source destination has no parent")?;
    ensure!(destination != parent, "frozen source destination is broad");
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("resolving frozen source parent {}", parent.display()))?;
    ensure!(
        canonical_parent == parent,
        "frozen source parent traverses a symlink or noncanonical path"
    );
    let parent_metadata = fs::symlink_metadata(parent)?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "frozen source parent is not root-owned and nonwritable"
    );
    if destination.exists() {
        remove_frozen_source_destination(destination)?;
    }
    fs::create_dir(destination)
        .with_context(|| format!("creating frozen source {}", destination.display()))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn prepare_frozen_source_destination(_destination: &Path) -> Result<()> {
    bail!("frozen source materialization requires a Unix actuator")
}

#[cfg(unix)]
fn remove_frozen_source_destination(destination: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        destination.is_absolute(),
        "frozen source destination is not absolute"
    );
    let parent = destination
        .parent()
        .context("frozen source destination has no parent")?;
    ensure!(destination != parent, "frozen source destination is broad");
    let parent_metadata = fs::symlink_metadata(parent)?;
    let metadata = fs::symlink_metadata(destination)?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "frozen source parent is not root-owned and nonwritable"
    );
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0,
        "existing frozen source is not an Idunn-owned directory"
    );
    fs::remove_dir_all(destination)
        .with_context(|| format!("removing exact frozen source {}", destination.display()))
}

#[cfg(not(unix))]
fn remove_frozen_source_destination(_destination: &Path) -> Result<()> {
    bail!("frozen source materialization requires a Unix actuator")
}

#[cfg(unix)]
fn harden_frozen_source(root: &Path) -> Result<()> {
    harden_frozen_source_tree(root, root)
}

#[cfg(unix)]
fn harden_frozen_source_tree(root: &Path, current: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(current)?;
    ensure!(metadata.uid() == 0, "frozen source entry is not root-owned");
    ensure!(
        current == root || current.file_name() != Some(OsStr::new(".git")),
        "frozen source contains forbidden .git metadata"
    );
    if metadata.is_dir() {
        let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            harden_frozen_source_tree(root, &entry.path())?;
        }
        fs::set_permissions(current, fs::Permissions::from_mode(0o555))?;
    } else if metadata.is_file() {
        let mode = if metadata.permissions().mode() & 0o111 == 0 {
            0o444
        } else {
            0o555
        };
        fs::set_permissions(current, fs::Permissions::from_mode(mode))?;
    } else if metadata.file_type().is_symlink() {
        // S6: only the target's shape is checked here, against `.partial`,
        // the temporary name the tree is built under. Whether the chain
        // actually resolves inside the root -- and whether a dangling
        // target still counts as inside it -- is decided later, by
        // `validate_frozen_source_symlinks`, against the tree's real, final
        // published path. A link built to look correct under `.partial` and
        // dangling or escaping once renamed (or the reverse) must be judged
        // by what it resolves to at the name every later reader actually
        // opens, not by this transient one.
        let target = fs::read_link(current)?;
        ensure!(target.is_relative(), "frozen source symlink is absolute");
    } else {
        bail!("frozen source contains a special filesystem entry")
    }
    Ok(())
}

/// S6 (Self's ruling, third Cut 1 fix batch): resolves a frozen source
/// symlink's full chain against the tree's real, final published root. Every
/// component the chain crosses that exists on disk is resolved for real
/// (`fs::canonicalize`, recursing through any symlink it meets), so a
/// self-referential directory cannot lexically undercount how far the chain
/// travels -- the same defense F5 relied on. Only the walk's trailing,
/// still-nonexistent components may be missing: once resolution reaches a
/// component that does not exist, nothing on disk remains to be tricked by,
/// so the rest of the target is appended lexically and containment keeps
/// being checked at every step. A dangling link is accepted exactly when
/// every component that exists stays inside the root; one whose real or
/// lexical remainder ever leaves the root is refused, dangling or not.
#[cfg(unix)]
fn resolve_frozen_source_symlink(
    canonical_root: &Path,
    path: &Path,
    hops: u32,
) -> Result<PathBuf> {
    const MAX_HOPS: u32 = 40;
    ensure!(
        hops < MAX_HOPS,
        "frozen source symlink chain exceeds the hop limit"
    );
    let target = fs::read_link(path)?;
    ensure!(target.is_relative(), "frozen source symlink is absolute");
    let parent = path
        .parent()
        .context("frozen source symlink has no parent")?;
    let mut resolved = parent.canonicalize().with_context(|| {
        format!("resolving frozen source symlink chain at {}", path.display())
    })?;
    ensure!(
        resolved.starts_with(canonical_root),
        "frozen source symlink escapes its root"
    );
    for component in target.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                ensure!(
                    resolved.pop() && resolved.starts_with(canonical_root),
                    "frozen source symlink escapes its root"
                );
            }
            std::path::Component::Normal(part) => {
                resolved.push(part);
                ensure!(
                    resolved.starts_with(canonical_root),
                    "frozen source symlink escapes its root"
                );
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        resolved = resolve_frozen_source_symlink(canonical_root, &resolved, hops + 1)?;
                    }
                    Ok(_) => {
                        resolved = resolved.canonicalize().with_context(|| {
                            format!("resolving frozen source symlink chain at {}", resolved.display())
                        })?;
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // Nothing exists here yet: the remaining components,
                        // including this one, are appended lexically below
                        // (no real symlink can hide along a path nothing on
                        // disk has reached), and containment is still
                        // enforced on every step that follows.
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspecting {}", resolved.display()));
                    }
                }
            }
            _ => bail!("frozen source symlink target has a non-normal path component"),
        }
        ensure!(
            resolved.starts_with(canonical_root),
            "frozen source symlink escapes its root"
        );
    }
    Ok(resolved)
}

#[cfg(unix)]
fn validate_frozen_source_symlink(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = root.canonicalize().context("resolving frozen source root")?;
    resolve_frozen_source_symlink(&canonical_root, path, 0).map(|_| ())
}

/// S6: run once, against the tree's real, final published path -- after the
/// atomic rename off `.partial`, in `freeze_exact`, and again (read-only)
/// whenever `observe_frozen` re-checks a tree it did not just write. Separate
/// from `validate_frozen_source_tree`'s ownership and mode checks (already
/// covered by `harden_frozen_source` at write time) so this stays one cheap
/// extra walk, not a second full re-hardening pass.
#[cfg(unix)]
fn validate_frozen_source_symlinks(root: &Path) -> Result<()> {
    validate_frozen_source_symlinks_tree(root, root)
}

#[cfg(unix)]
fn validate_frozen_source_symlinks_tree(root: &Path, current: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(current)?;
    if metadata.is_dir() {
        let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            validate_frozen_source_symlinks_tree(root, &entry.path())?;
        }
    } else if metadata.file_type().is_symlink() {
        validate_frozen_source_symlink(root, current)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn harden_frozen_source(_root: &Path) -> Result<()> {
    bail!("frozen source materialization requires Unix permissions")
}

#[cfg(unix)]
fn validate_frozen_source(root: &Path) -> Result<()> {
    validate_frozen_source_tree(root, root)
}

#[cfg(unix)]
fn validate_frozen_source_tree(root: &Path, current: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(current)?;
    ensure!(metadata.uid() == 0, "frozen source entry is not root-owned");
    ensure!(
        current == root || current.file_name() != Some(OsStr::new(".git")),
        "frozen source contains forbidden .git metadata"
    );
    if metadata.is_dir() {
        ensure!(
            metadata.permissions().mode() & 0o777 == 0o555,
            "frozen source directory is not 0555"
        );
        let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            validate_frozen_source_tree(root, &entry.path())?;
        }
    } else if metadata.is_file() {
        ensure!(
            matches!(metadata.permissions().mode() & 0o777, 0o444 | 0o555),
            "frozen source file has a noncanonical mode"
        );
    } else if metadata.file_type().is_symlink() {
        validate_frozen_source_symlink(root, current)?;
    } else {
        bail!("frozen source contains a special filesystem entry")
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_frozen_source(_root: &Path) -> Result<()> {
    bail!("frozen source observation requires Unix permissions")
}

#[cfg(not(unix))]
fn validate_frozen_source_symlinks(_root: &Path) -> Result<()> {
    bail!("frozen source observation requires Unix permissions")
}

#[cfg(unix)]
fn frozen_source_sha256(root: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_frozen_source_tree(root, root, &mut hasher)?;
    Ok(format!("sha256-{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn hash_frozen_source_tree(root: &Path, current: &Path, hasher: &mut Sha256) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = normalized_relative(path.strip_prefix(root)?)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            hasher.update(b"dir\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hash_frozen_source_tree(root, &path, hasher)?;
        } else if metadata.is_file() {
            hasher.update(b"file\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update(
                (metadata.permissions().mode() & 0o111 != 0)
                    .to_string()
                    .as_bytes(),
            );
            hasher.update(b"\0");
            hasher.update(metadata.len().to_le_bytes());
            // S9: streamed in fixed-size chunks rather than `fs::read`ing the
            // whole file, so peak RSS is bounded by the buffer, not by the
            // largest file in the tree.
            let mut file = fs::File::open(&path)
                .with_context(|| format!("opening {} to hash it", path.display()))?;
            let mut buffer = [0u8; 1 << 16];
            loop {
                let read = file
                    .read(&mut buffer)
                    .with_context(|| format!("reading {} to hash it", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            hasher.update(b"link\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update(target.as_os_str().as_encoded_bytes());
            hasher.update(b"\0");
        } else {
            bail!("frozen source contains a special filesystem entry")
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn frozen_source_sha256(_root: &Path) -> Result<String> {
    bail!("frozen source observation requires Unix permissions")
}

#[cfg(unix)]
fn harden_installed_release(root: &Path, artifacts: &[ArtifactReceipt]) -> Result<()> {
    let executable_paths = artifacts
        .iter()
        .filter(|artifact| artifact.executable)
        .map(|artifact| root.join(&artifact.destination))
        .collect::<std::collections::BTreeSet<_>>();
    harden_root_tree(root, root, &executable_paths)
}

#[cfg(unix)]
fn harden_root_tree(
    root: &Path,
    current: &Path,
    executable_paths: &std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(current)?;
    ensure!(
        metadata.uid() == 0,
        "sealed release entry is not root-owned"
    );
    if metadata.is_dir() {
        let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            harden_root_tree(root, &entry.path(), executable_paths)?;
        }
        fs::set_permissions(current, fs::Permissions::from_mode(0o555))?;
    } else if metadata.is_file() {
        fs::set_permissions(
            current,
            fs::Permissions::from_mode(if executable_paths.contains(current) {
                0o555
            } else {
                0o444
            }),
        )?;
    } else if metadata.file_type().is_symlink() {
        validate_release_symlink(root, current)?;
    } else {
        bail!("sealed release contains a special filesystem entry")
    }
    Ok(())
}

#[cfg(unix)]
fn validate_release_symlink(root: &Path, path: &Path) -> Result<()> {
    let target = fs::read_link(path)?;
    ensure!(target.is_relative(), "sealed release symlink is absolute");
    ensure!(
        target.components().all(|component| matches!(
            component,
            std::path::Component::CurDir | std::path::Component::Normal(_)
        )),
        "sealed release symlink has a parent traversal"
    );
    let destination = path
        .parent()
        .context("sealed release symlink has no parent")?
        .join(target);
    ensure!(
        destination.starts_with(root),
        "sealed release symlink escaped its release"
    );
    Ok(())
}

#[cfg(not(unix))]
fn harden_installed_release(_root: &Path, _artifacts: &[ArtifactReceipt]) -> Result<()> {
    bail!("systemd workload installation requires Unix permissions")
}

#[cfg(unix)]
fn harden_runtime_bundle(bundle: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(bundle)?;
    let parent = bundle.parent().context("runtime bundle has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == 0,
        "runtime bundle is not a root-owned directory"
    );
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "runtime bundle root is not root-owned and service-nonwritable"
    );
    for name in [
        "expected.cc",
        "expected.cc.lock",
        "activation.cc",
        "activation.cc.lock",
    ] {
        let path = bundle.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.uid() == 0,
            "runtime bundle document is not root-owned"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    }
    fs::set_permissions(bundle, fs::Permissions::from_mode(0o555))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_runtime_bundle(_bundle: &Path) -> Result<()> {
    bail!("systemd runtime bundles require Unix permissions")
}

fn observe_parent_only_file_descriptor(
    fd_number: u32,
    fd_name: &str,
    source_path: &Path,
) -> Result<ParentOnlyFileDescriptorObservation> {
    validate_open_file_component(fd_name, "parent-only descriptor name")?;
    ensure!(
        source_path.is_absolute(),
        "parent-only descriptor source is not absolute"
    );
    validate_open_file_component(
        &source_path.as_os_str().to_string_lossy(),
        "parent-only descriptor source",
    )?;
    let mut file = open_native_read_only(source_path).with_context(|| {
        format!(
            "opening parent-only descriptor source {}",
            source_path.display()
        )
    })?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "parent-only descriptor source is not one nonempty native file"
    );
    #[cfg(unix)]
    let (device, inode, uid, gid, mode, links) = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.gid(),
            metadata.permissions().mode() & 0o777,
            metadata.nlink(),
        )
    };
    #[cfg(not(unix))]
    let (device, inode, uid, gid, mode, links) = (0, 0, 0, 0, 0, 0);
    ensure!(
        uid == 0 && gid == 0 && mode == 0o400 && links == 1,
        "parent-only descriptor source is not root-owned, 0400, and singly linked"
    );
    Ok(ParentOnlyFileDescriptorObservation {
        fd_number,
        fd_name: fd_name.to_owned(),
        source_path: source_path.to_owned(),
        access: "read-only".into(),
        device,
        inode,
        uid,
        gid,
        mode,
        links,
        size: metadata.len(),
        sha256: sha256_reader(&mut file)?,
    })
}

fn parent_only_open_file_properties(
    descriptors: &[ParentOnlyFileDescriptorObservation],
) -> Result<Vec<String>> {
    ensure!(
        descriptors.len() == 2
            && descriptors[0].fd_number == 3
            && descriptors[0].fd_name == IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME
            && descriptors[1].fd_number == 4
            && descriptors[1].fd_name == RUNTIME_PRESENCE_IDENTITY_FD_NAME,
        "parent-only descriptor set is not the exact ordered activation/presence pair"
    );
    descriptors
        .iter()
        .map(|descriptor| {
            validate_open_file_component(&descriptor.fd_name, "parent-only descriptor name")?;
            validate_open_file_component(
                &descriptor.source_path.as_os_str().to_string_lossy(),
                "parent-only descriptor source",
            )?;
            ensure!(
                descriptor.source_path.is_absolute()
                    && descriptor.access == "read-only"
                    && descriptor.uid == 0
                    && descriptor.gid == 0
                    && descriptor.mode == 0o400
                    && descriptor.links == 1
                    && descriptor.size > 0,
                "parent-only descriptor metadata is outside the Idunn contract"
            );
            Ok(format!(
                "{}:{}:{}",
                descriptor.source_path.display(),
                descriptor.fd_name,
                descriptor.access
            ))
        })
        .collect()
}

fn validate_open_file_component(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.contains(':') && !value.chars().any(char::is_control),
        "{label} cannot be represented by systemd OpenFile"
    );
    Ok(())
}

#[cfg(unix)]
fn ensure_activation_credential_root(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "activation credential actuation requires root Idunn"
    );
    ensure!(
        path.is_absolute(),
        "activation credential root is not absolute"
    );
    let parent = path
        .parent()
        .context("activation credential root has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent).with_context(|| {
        format!(
            "inspecting activation credential parent {}",
            parent.display()
        )
    })?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0
            && parent.canonicalize()? == parent,
        "activation credential parent is not a canonical root-owned directory"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && metadata.gid() == 0
                && metadata.permissions().mode() & 0o777 == 0o700
                && path.canonicalize()? == path,
            "activation credential root is not an exact root-only native directory"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| {
                format!("creating activation credential root {}", path.display())
            })?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.gid() == 0
                    && metadata.permissions().mode() & 0o777 == 0o700,
                "new activation credential root has the wrong authority"
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting activation credential root {}", path.display())
            });
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_activation_credential_root(_path: &Path) -> Result<()> {
    bail!("systemd activation credentials require a Unix authority path")
}

#[cfg(unix)]
fn validate_activation_credential_source(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting activation credential source {}", path.display()))?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.permissions().mode() & 0o777 == 0o400
            && metadata.nlink() == 1
            && metadata.len() == 32,
        "activation credential source is not one exact root-owned 0400 32-byte file"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_activation_credential_source(_path: &Path) -> Result<()> {
    bail!("systemd activation credentials require Unix file authority")
}

#[cfg(unix)]
fn validate_service_credential_sources(sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for (name, path) in sources {
        ensure!(
            path.is_absolute(),
            "service credential {name} is not absolute"
        );
        let parent = path
            .parent()
            .context("service credential source has no parent")?;
        let parent_metadata = fs::symlink_metadata(parent).with_context(|| {
            format!("inspecting service credential parent {}", parent.display())
        })?;
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspecting service credential source {}", path.display()))?;
        ensure!(
            parent.canonicalize()? == parent
                && parent_metadata.is_dir()
                && !parent_metadata.file_type().is_symlink()
                && parent_metadata.uid() == 0
                && parent_metadata.permissions().mode() & 0o022 == 0
                && path.canonicalize()? == path.as_path()
                && metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == 0
                && metadata.gid() == 0
                && metadata.permissions().mode() & 0o777 == 0o400
                && metadata.nlink() == 1
                && metadata.len() > 0,
            "service credential {name} is not one canonical root-owned 0400 file"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_service_credential_sources(_sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    bail!("systemd service credentials require Unix file authority")
}

fn remove_activation_credential_source(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_activation_credential_source_for_removal(path, &metadata)?;
            fs::remove_file(path).with_context(|| {
                format!("deleting activation credential source {}", path.display())
            })?;
            sync_parent_directory(path)?;
            ensure!(
                matches!(fs::symlink_metadata(path), Err(error) if error.kind() == ErrorKind::NotFound),
                "activation credential source remained after deletion"
            );
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspecting activation credential source {}", path.display())),
    }
}

#[cfg(unix)]
fn validate_activation_credential_source_for_removal(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let parent = path
        .parent()
        .context("activation credential source has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.gid() == 0
            && parent_metadata.permissions().mode() & 0o777 == 0o700
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.nlink() == 1
            && metadata.len() <= 32,
        "refusing to delete a surprising activation credential source"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_activation_credential_source_for_removal(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> Result<()> {
    bail!("systemd activation credentials require Unix file authority")
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path.parent().context("file has no parent to synchronize")?;
    let directory = open_native_read_only(parent)?;
    directory
        .sync_all()
        .with_context(|| format!("synchronizing directory {}", parent.display()))
}

fn open_native_read_only(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}

fn open_proc_magic_link(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC);
    }
    options
        .open(path)
        .with_context(|| format!("opening procfs executable {}", path.display()))
}

fn sha256_reader(reader: &mut impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256-{:x}", digest.finalize()))
}

fn path_inside_process_root(process_root: &Path, absolute_path: &Path) -> Result<PathBuf> {
    ensure!(
        process_root.is_absolute() && absolute_path.is_absolute(),
        "process-root projection requires absolute paths"
    );
    ensure!(
        absolute_path.components().all(|component| matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )),
        "process-root projection path is not normalized"
    );
    Ok(process_root.join(absolute_path.strip_prefix(Path::new("/"))?))
}

fn linux_process_security(
    status_path: &Path,
    expected_host_pid: u32,
) -> Result<LinuxProcessSecurityObservation> {
    let status = fs::read_to_string(status_path)
        .with_context(|| format!("reading process status {}", status_path.display()))?;
    let uids = linux_status_u32_array(&status, "Uid")?;
    let gids = linux_status_u32_array(&status, "Gid")?;
    let groups = linux_status_u32_values(&status, "Groups")?;
    let cap_inheritable = linux_status_hex_u64(&status, "CapInh")?;
    let cap_permitted = linux_status_hex_u64(&status, "CapPrm")?;
    let cap_effective = linux_status_hex_u64(&status, "CapEff")?;
    let cap_bounding = linux_status_hex_u64(&status, "CapBnd")?;
    let cap_ambient = linux_status_hex_u64(&status, "CapAmb")?;
    let no_new_privileges = match linux_status_value(&status, "NoNewPrivs")? {
        "1" => true,
        "0" => false,
        _ => bail!("process NoNewPrivs value is invalid"),
    };
    let namespace_pids = linux_status_u32_values(&status, "NSpid")?;
    ensure!(
        namespace_pids.first() == Some(&expected_host_pid),
        "process status belongs to another host pid"
    );
    Ok(LinuxProcessSecurityObservation {
        uids,
        gids,
        groups,
        cap_inheritable,
        cap_permitted,
        cap_effective,
        cap_bounding,
        cap_ambient,
        no_new_privileges,
        namespace_pids,
    })
}

fn linux_status_value<'a>(status: &'a str, name: &str) -> Result<&'a str> {
    let prefix = format!("{name}:");
    let matches = status
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "process status has zero or duplicate {name} fields"
    );
    Ok(matches[0].trim())
}

fn linux_status_u32_values(status: &str, name: &str) -> Result<Vec<u32>> {
    linux_status_value(status, name)?
        .split_whitespace()
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("process status {name} value is not a u32"))
        })
        .collect()
}

fn linux_status_u32_array(status: &str, name: &str) -> Result<[u32; 4]> {
    linux_status_u32_values(status, name)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("process status {name} does not have four values"))
}

fn linux_status_hex_u64(status: &str, name: &str) -> Result<u64> {
    u64::from_str_radix(linux_status_value(status, name)?, 16)
        .with_context(|| format!("process status {name} value is not hexadecimal"))
}

fn linux_namespace_id(path: &Path, kind: &str) -> Result<u64> {
    let target = fs::read_link(path)
        .with_context(|| format!("reading Linux namespace link {}", path.display()))?;
    let target = target
        .to_str()
        .context("Linux namespace link is not UTF-8")?;
    let prefix = format!("{kind}:[");
    let id = target
        .strip_prefix(&prefix)
        .and_then(|value| value.strip_suffix(']'))
        .context("Linux namespace link has an unexpected shape")?;
    let id = id
        .parse::<u64>()
        .context("Linux namespace id is not a u64")?;
    ensure!(id > 0, "Linux namespace id is zero");
    Ok(id)
}

#[cfg(unix)]
fn resolve_group_id(group: &str) -> Result<u32> {
    use std::ffi::CString;

    if let Ok(group_id) = group.parse::<u32>() {
        ensure!(group_id > 0, "state group must be unprivileged");
        return Ok(group_id);
    }
    let group = CString::new(group).context("state group contains a NUL byte")?;
    let mut buffer_size = 16_384_usize;
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getgrnam_r(
                group.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer_size < 1_048_576 {
            buffer_size *= 2;
            continue;
        }
        ensure!(
            status == 0,
            "resolving state group failed with errno {status}"
        );
        ensure!(!result.is_null(), "configured state group does not exist");
        let entry = unsafe { entry.assume_init() };
        ensure!(entry.gr_gid > 0, "state group must be unprivileged");
        return Ok(entry.gr_gid);
    }
}

#[cfg(not(unix))]
fn resolve_group_id(_group: &str) -> Result<u32> {
    bail!("systemd workload groups require a Unix actuator")
}

#[cfg(unix)]
fn validate_workload_writable_path(
    path: &Path,
    state_group_id: u32,
    require_setgid: bool,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting workload writable path {}", path.display()))?;
    let mode = metadata.permissions().mode();
    ensure!(
        !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == state_group_id
            && mode & 0o002 == 0,
        "workload writable path {} is not preprovisioned root:state-group without world write",
        path.display()
    );
    if metadata.is_dir() {
        ensure!(
            mode & 0o070 == 0o070 && (!require_setgid || mode & 0o2000 != 0),
            "workload writable directory {} lacks group access or setgid inheritance",
            path.display()
        );
    } else {
        ensure!(
            metadata.is_file() && mode & 0o060 == 0o060,
            "workload writable file is special or lacks group access"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_workload_writable_path(
    _path: &Path,
    _state_group_id: u32,
    _require_setgid: bool,
) -> Result<()> {
    bail!("systemd workload writable paths require Unix permissions")
}

fn linux_process_start_time(stat_path: &Path) -> Result<u64> {
    let stat = fs::read_to_string(stat_path)
        .with_context(|| format!("reading process stat {}", stat_path.display()))?;
    let close = stat
        .rfind(')')
        .context("process stat has no command terminator")?;
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    fields
        .get(19)
        .context("process stat has no start-time field")?
        .parse()
        .context("process start-time field is not a u64")
}

fn required_systemd_property<'a>(
    values: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str> {
    let value = systemd_property(values, name)?;
    ensure!(!value.is_empty(), "systemd unit has no {name}");
    Ok(value)
}

fn systemd_property<'a>(values: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str> {
    values
        .get(name)
        .map(String::as_str)
        .with_context(|| format!("systemd unit did not expose {name}"))
}

fn parse_systemd_boolean(values: &BTreeMap<String, String>, name: &str) -> Result<bool> {
    match systemd_property(values, name)? {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => bail!("systemd unit exposed a non-boolean {name}"),
    }
}

fn read_process_environment(path: &Path) -> Result<Vec<Vec<u8>>> {
    let bytes = fs::read(path)
        .with_context(|| format!("reading process environment {}", path.display()))?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

fn required_process_environment_value<'a>(entries: &'a [Vec<u8>], name: &str) -> Result<&'a str> {
    let prefix = format!("{name}=").into_bytes();
    let matches = entries
        .iter()
        .filter_map(|entry| entry.strip_prefix(prefix.as_slice()))
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "process environment has zero or duplicate {name} values"
    );
    let value = std::str::from_utf8(matches[0])
        .with_context(|| format!("process environment {name} is not UTF-8"))?;
    ensure!(!value.is_empty(), "process environment {name} is empty");
    Ok(value)
}

fn select_process_environment(
    entries: &[Vec<u8>],
    names: &[String],
) -> Result<BTreeMap<String, String>> {
    ensure!(
        names.windows(2).all(|pair| pair[0] < pair[1]),
        "workload environment witness names are not unique and sorted"
    );
    let mut selected = BTreeMap::new();
    for name in names {
        let prefix = format!("{name}=").into_bytes();
        let matches = entries
            .iter()
            .filter(|entry| entry.starts_with(&prefix))
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "workload process environment has missing or duplicate {name}"
        );
        let value = std::str::from_utf8(&matches[0][prefix.len()..])
            .with_context(|| format!("workload environment {name} is not UTF-8"))?;
        selected.insert(name.clone(), value.to_owned());
    }
    Ok(selected)
}

fn proc_command_line(command: &[OsString]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for argument in command {
        encoded.extend_from_slice(argument.as_encoded_bytes());
        encoded.push(0);
    }
    encoded
}

fn endpoint_host_port(endpoint: &str, scheme: &str) -> Result<(String, u16)> {
    let authority = endpoint
        .strip_prefix(scheme)
        .with_context(|| format!("endpoint does not use {scheme}"))?;
    ensure!(
        !authority
            .chars()
            .any(|character| matches!(character, '/' | '?' | '#' | '@'))
            && !authority.starts_with('['),
        "route endpoint is not a plain host and port"
    );
    let (host, port) = authority
        .rsplit_once(':')
        .context("route endpoint has no port")?;
    ensure!(
        !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')),
        "route endpoint host is invalid"
    );
    let port: u16 = port.parse().context("route endpoint port is not a u16")?;
    ensure!(port > 0, "route endpoint port is zero");
    Ok((host.to_owned(), port))
}

fn nginx_identifier(route_id: &str) -> Result<String> {
    ensure!(!route_id.is_empty(), "nginx route id is empty");
    let mut identifier = String::from("idunn_");
    for byte in route_id.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => {
                identifier.push(char::from(byte));
            }
            b'-' | b'.' => identifier.push('_'),
            _ => bail!("route id cannot lower to an nginx identifier"),
        }
    }
    Ok(identifier)
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    ensure!(path.is_absolute(), "route path is not absolute");
    let parent = path.parent().context("route path has no parent")?;
    ensure_route_authority_parent(parent)?;
    match fs::symlink_metadata(path) {
        Ok(_) => validate_root_owned_file(path, 0o644)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspecting route fragment"),
    }
    let file_name = path
        .file_name()
        .context("route path has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.idunn-{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o644);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating route stage {}", temporary.display()))?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    validate_root_owned_file(&temporary, 0o644)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("publishing route {}", path.display()));
    }
    sync_parent_directory(path)?;
    validate_root_owned_file(path, 0o644)
}

#[cfg(unix)]
fn ensure_route_authority_parent(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "route actuation requires root Idunn"
    );
    ensure!(path.is_absolute(), "route authority parent is not absolute");
    if !path.exists() {
        let ancestor = path
            .parent()
            .context("route authority parent has no ancestor")?;
        let ancestor_metadata = fs::symlink_metadata(ancestor)?;
        ensure!(
            ancestor.canonicalize()? == ancestor
                && ancestor_metadata.is_dir()
                && !ancestor_metadata.file_type().is_symlink()
                && ancestor_metadata.uid() == 0
                && ancestor_metadata.permissions().mode() & 0o022 == 0,
            "route authority ancestor is not canonical root-owned and nonwritable"
        );
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        path.canonicalize()? == path
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.permissions().mode() & 0o022 == 0,
        "route authority parent is not canonical root-owned and nonwritable"
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_route_authority_parent(_path: &Path) -> Result<()> {
    bail!("nginx route actuation requires Unix file authority")
}

#[cfg(unix)]
fn ensure_route_preflight_root(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(path.is_absolute(), "route preflight root is not absolute");
    if !path.exists() {
        let parent = path
            .parent()
            .context("route preflight root has no parent")?;
        ensure_route_authority_parent(parent)?;
        fs::create_dir(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        unsafe { libc::geteuid() } == 0
            && path.canonicalize()? == path
            && metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.permissions().mode() & 0o777 == 0o700,
        "route preflight root is not one canonical root-only directory"
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_route_preflight_root(_path: &Path) -> Result<()> {
    bail!("nginx route preflight requires Unix file authority")
}

#[cfg(unix)]
fn write_root_owned_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(mode);
    let mut file = options.open(path)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    sync_parent_directory(path)?;
    validate_root_owned_file(path, mode)
}

#[cfg(not(unix))]
fn write_root_owned_file(_path: &Path, _bytes: &[u8], _mode: u32) -> Result<()> {
    bail!("root-owned route material requires Unix file authority")
}

#[cfg(unix)]
fn validate_root_owned_file(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        path.canonicalize()? == path
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.permissions().mode() & 0o777 == mode
            && metadata.nlink() == 1,
        "route authority material is not one canonical root-owned file"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_root_owned_file(_path: &Path, _mode: u32) -> Result<()> {
    bail!("root-owned route material requires Unix file authority")
}

fn remove_exact_root_owned_file(path: &Path, mode: u32) -> Result<()> {
    validate_root_owned_file(path, mode)?;
    fs::remove_file(path)?;
    sync_parent_directory(path)
}

/// A build container's own machine identity, mounted read-only at
/// `/etc/machine-id`.
///
/// The image carries no machine-id, and code that binds a service identity to
/// the machine -- CultLib's Linux protector does exactly this -- cannot run
/// without one, so a target whose tests enrol an identity cannot be built at
/// all. The host's machine-id is not the answer: it would let a build container
/// protect a seed that unwraps on the host, which is precisely the property the
/// binding exists to deny.
///
/// So each frozen workspace gets its own identity, derived from the workspace
/// path. It is stable for the life of a build, and an identity enrolled inside
/// one is unusable anywhere else, including on this host.
/// Every directory above the runtime bundle must be traversable by the
/// workload, which runs under `DynamicUser` and is in no group but the state
/// group.
///
/// This is checked here rather than left to the operator because the failure is
/// silent and expensive: CultLib's backing store reports an unreadable file as
/// an *empty* store, so a workload that cannot traverse to its bundle sees zero
/// records rather than a permission error. Odin reported "runtime authority
/// store must contain exactly one record" and Heimdall simply decided it held
/// no write lease and warmed forever. Neither names the actual fault.
///
/// `ReadOnlyPaths=` on the bundle does not help: it binds the leaf into the
/// unit's namespace but grants no traversal on the path above it.
#[cfg(unix)]
fn ensure_bundle_is_reachable_by_workload(
    bundle: &Path,
    state_group_id: Option<u32>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for ancestor in bundle.ancestors().skip(1) {
        if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
            break;
        }
        let metadata =
            fs::metadata(ancestor).with_context(|| format!("reading {}", ancestor.display()))?;
        let mode = metadata.permissions().mode();
        // The workload's uid is allocated by DynamicUser and owns nothing here,
        // so traversal can only come from the state group or from other.
        let by_group = state_group_id == Some(metadata.gid()) && mode & 0o010 != 0;
        ensure!(
            mode & 0o001 != 0 || by_group,
            "{} is not traversable by the workload, so the runtime bundle cannot be read;              give it o+x (0711 leaks no names) or group-own it by the state group with g+x",
            ancestor.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_bundle_is_reachable_by_workload(
    _bundle: &Path,
    _state_group_id: Option<u32>,
) -> Result<()> {
    Ok(())
}

/// The topology store is Idunn's *published* surface: every managed target
/// reads it to verify its own Expected incarnation against the Idunn anchor.
///
/// Idunn runs with `UMask=027`, which is right for its private state and wrong
/// for this one file -- it lands `0640 root:root`, and a `DynamicUser` workload
/// gets EACCES. A chmod by hand does not hold, because each publish writes a
/// new file. Integrity here comes from the signatures over the records, not
/// from the mode, so the published copy is readable.
fn publish_projection_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The lock sibling too: CultCache opens it alongside the store, so a
        // 0640 lock denies the read just as surely as a 0640 store, and it is
        // created fresh under Idunn's umask on every publish.
        for path in [path.to_path_buf(), authority_lock_path(path)] {
            if path.exists() {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                    .with_context(|| format!("publishing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn build_machine_id(workspace: &Path) -> Result<String> {
    let text = workspace
        .to_str()
        .context("frozen workspace path is not UTF-8")?;
    // systemd machine-id format: exactly 32 lowercase hex digits.
    let digest = sha256_id(text.as_bytes());
    Ok(digest
        .strip_prefix("sha256-")
        .unwrap_or(&digest)
        .chars()
        .take(32)
        .collect())
}

fn build_machine_id_file(workspace: &Path) -> Result<PathBuf> {
    let root = PathBuf::from("/run/idunn/build-machine-ids");
    fs::create_dir_all(&root).context("creating the build machine-id root")?;
    let id = build_machine_id(workspace)?;
    let path = root.join(&id);
    if !path.exists() {
        fs::write(
            &path,
            format!(
                "{id}
"
            ),
        )
        .context("writing the build machine-id")?;
        #[cfg(unix)]
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o444))
            .context("sealing the build machine-id")?;
    }
    Ok(path)
}

fn bind_mount(source: &Path, destination: &str, read_only: bool) -> Result<OsString> {
    ensure!(source.is_absolute(), "Docker bind source is not absolute");
    let text = source.to_str().context("Docker bind source is not UTF-8")?;
    ensure!(
        !text
            .chars()
            .any(|character| matches!(character, ',' | '\n' | '\r')),
        "Docker bind source contains a forbidden mount character"
    );
    let read_only = if read_only { ",readonly" } else { "" };
    Ok(OsString::from(format!(
        "type=bind,src={text},dst={destination}{read_only}"
    )))
}

fn systemd_read_only_bind_property(source: &Path, destination: &Path) -> Result<OsString> {
    ensure!(
        source.is_absolute() && destination.is_absolute(),
        "systemd bind paths are not absolute"
    );
    let source = source
        .to_str()
        .context("systemd bind source is not UTF-8")?;
    let destination = destination
        .to_str()
        .context("systemd bind destination is not UTF-8")?;
    ensure!(
        [source, destination]
            .iter()
            .all(|path| !path.contains([':', '\n', '\r', '\0'])),
        "systemd bind path contains an unescaped property delimiter"
    );
    Ok(OsString::from(format!(
        "--property=BindReadOnlyPaths={source}:{destination}"
    )))
}

fn normalized_relative(path: &Path) -> Result<String> {
    ensure!(path.is_relative(), "runner path is not relative");
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .context("runner path contains non-UTF-8")?
                    .to_owned(),
            ),
            _ => bail!("runner path escapes its workspace"),
        }
    }
    Ok(parts.join("/"))
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    ensure!(source.is_dir(), "source tree is not a directory");
    ensure!(!destination.exists(), "destination tree already exists");
    fs::create_dir_all(destination)?;
    copy_tree_contents(source, destination)
}

fn copy_tree_contents(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_tree_contents(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else if metadata.file_type().is_symlink() {
            copy_symlink(&source_path, &destination_path)?;
        } else {
            bail!("source tree contains a special filesystem entry")
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
    Ok(())
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)?;
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

/// F5: `source` may name a path reached through an intermediate symlink a
/// runner's own build output created (`fs::symlink_metadata` follows every
/// path component but the last, so a symlinked directory earlier in `source`
/// would otherwise be followed transparently). `root` is resolved and
/// checked to contain the fully resolved `source` with the same
/// whole-chain-aware resolver `validate_frozen_source_symlink` uses, before
/// anything is read from it.
pub(crate) fn copy_artifact(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let canonical_root = root.canonicalize().context("resolving artifact root")?;
    let canonical_source = source
        .canonicalize()
        .with_context(|| format!("resolving artifact source {}", source.display()))?;
    ensure!(
        canonical_source.starts_with(&canonical_root),
        "artifact source escapes its root"
    );
    let metadata = fs::symlink_metadata(&canonical_source)?;
    if metadata.is_dir() {
        copy_tree(&canonical_source, destination)
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&canonical_source, destination)?;
        Ok(())
    } else {
        bail!("artifact output is not a regular file or directory")
    }
}

pub(crate) fn digest_artifact(path: &Path) -> Result<(String, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        let bytes = fs::read(path)?;
        return Ok((raw_sha256(&bytes), bytes.len().try_into()?));
    }
    ensure!(metadata.is_dir(), "artifact is not a file or directory");
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    digest_tree(path, path, &mut hasher, &mut size)?;
    ensure!(size > 0, "artifact directory has no file content");
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn digest_tree(root: &Path, current: &Path, hasher: &mut Sha256, size: &mut u64) -> Result<()> {
    let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let relative = normalized_relative(relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            hasher.update(b"dir\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            digest_tree(root, &path, hasher, size)?;
        } else if metadata.is_file() {
            let bytes = fs::read(&path)?;
            hasher.update(b"file\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
            *size = size.saturating_add(bytes.len().try_into()?);
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            hasher.update(b"link\0");
            hasher.update(relative.as_bytes());
            hasher.update(b"\0");
            hasher.update(target.as_os_str().as_encoded_bytes());
            *size = size.saturating_add(target.as_os_str().len().try_into()?);
        } else {
            bail!("artifact tree contains a special filesystem entry")
        }
    }
    Ok(())
}

pub(crate) fn raw_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn remove_tree_inside(root: &Path, target: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let target = target.canonicalize()?;
    ensure!(
        target.starts_with(&root) && target != root,
        "refusing broad tree removal"
    );
    fs::remove_dir_all(&target)
        .with_context(|| format!("removing disposable runner workspace {}", target.display()))
}

pub(crate) fn sha256_id(bytes: &[u8]) -> String {
    format!("sha256-{:x}", Sha256::digest(bytes))
}

fn require_git_sha(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 40
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a lowercase SHA-1 object id"
    );
    Ok(())
}

fn require_sha256_id(value: &str, label: &str) -> Result<()> {
    let digest = value
        .strip_prefix("sha256-")
        .with_context(|| format!("{label} has no sha256 prefix"))?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a lowercase SHA-256 id"
    );
    Ok(())
}

fn require_driver_id(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| { byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') }),
        "{label} id is invalid"
    );
    Ok(())
}

fn ensure_source_directory_tree(
    root: &Path,
    directory: &Path,
    identity: Option<ProcessIdentity>,
) -> Result<()> {
    ensure!(
        directory.starts_with(root),
        "source directory escaped Idunn's source authority root"
    );
    ensure_source_directory(root, identity)?;
    let relative = directory.strip_prefix(root)?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("source directory is not normalized")
        };
        current.push(component);
        ensure_source_directory(&current, identity)?;
    }
    Ok(())
}

fn container_identity(value: &str) -> Result<ProcessIdentity> {
    let (uid, gid) = value
        .split_once(':')
        .context("runner user is not numeric UID:GID")?;
    let identity = ProcessIdentity {
        uid: uid.parse().context("runner UID is not a u32")?,
        gid: gid.parse().context("runner GID is not a u32")?,
    };
    ensure!(
        identity.uid > 0 && identity.gid > 0,
        "runner identity must be unprivileged"
    );
    Ok(identity)
}

#[cfg(unix)]
fn ensure_runner_cache_root(path: &Path, identity: ProcessIdentity) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "runner cache admission requires root Idunn"
    );
    let parent = path.parent().context("runner cache root has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspecting runner cache parent {}", parent.display()))?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "runner cache parent is not root-owned and nonwritable"
    );
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == identity.uid
                && metadata.gid() == identity.gid
                && metadata.permissions().mode() & 0o777 == 0o700,
            "runner cache is not a dedicated exact-identity 0700 directory"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("creating runner cache {}", path.display()))?;
            let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .context("runner cache path contains a NUL byte")?;
            if unsafe { libc::lchown(path_c.as_ptr(), identity.uid, identity.gid) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("assigning runner cache owner");
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting runner cache {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_runner_cache_root(_path: &Path, _identity: ProcessIdentity) -> Result<()> {
    bail!("runner cache admission requires a Unix actuator")
}

#[cfg(unix)]
fn validate_runner_secret(path: &Path, identity: ProcessIdentity) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "runner secret admission requires root Idunn"
    );
    let parent = path.parent().context("runner secret has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspecting runner secret parent {}", parent.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting runner secret {}", path.display()))?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "runner secret parent is not root-owned and nonwritable"
    );
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == identity.gid
            && metadata.permissions().mode() & 0o777 == 0o440,
        "runner secret is not root-owned, exact-group-bound, and 0440"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_runner_secret(_path: &Path, _identity: ProcessIdentity) -> Result<()> {
    bail!("runner secret admission requires a Unix actuator")
}

#[cfg(unix)]
fn assign_runner_tree(path: &Path, identity: ProcessIdentity) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "runner workspace ownership requires root Idunn"
    );
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            assign_runner_tree(&entry.path(), identity)?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o750))?;
    } else if metadata.is_file() {
        let executable = metadata.permissions().mode() & 0o111 != 0;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o750 } else { 0o640 }),
        )?;
    } else if !metadata.file_type().is_symlink() {
        bail!("runner workspace contains a special filesystem entry")
    }
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .context("runner workspace path contains a NUL byte")?;
    let result = unsafe { libc::lchown(path_c.as_ptr(), identity.uid, identity.gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("assigning runner workspace owner");
    }
    Ok(())
}

#[cfg(not(unix))]
fn assign_runner_tree(_path: &Path, _identity: ProcessIdentity) -> Result<()> {
    bail!("non-root runner identities require a Unix actuator")
}

fn ensure_source_directory(path: &Path, identity: Option<ProcessIdentity>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "source authority path {} is not a native directory",
                path.display()
            );
            validate_source_directory_owner(path, &metadata, identity)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("source authority directory has no parent")?;
            ensure!(
                parent.is_dir(),
                "source authority parent {} is absent",
                parent.display()
            );
            fs::create_dir(path).with_context(|| {
                format!("creating source authority directory {}", path.display())
            })?;
            assign_source_directory_owner(path, identity)?;
            let metadata = fs::symlink_metadata(path)?;
            validate_source_directory_owner(path, &metadata, identity)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspecting source authority path {}", path.display())),
    }
}

fn authority_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[cfg(unix)]
fn validate_root_authority_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ensure!(
        unsafe { libc::geteuid() } == 0,
        "process write-lease actuation requires root Idunn"
    );
    let parent = path
        .parent()
        .context("process write-lease path has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspecting write-lease parent {}", parent.display()))?;
    ensure!(
        parent_metadata.is_dir()
            && !parent_metadata.file_type().is_symlink()
            && parent_metadata.uid() == 0
            && parent_metadata.permissions().mode() & 0o022 == 0,
        "write-lease parent is not root-owned and service-nonwritable"
    );
    for authority_file in [path.to_owned(), authority_lock_path(path)] {
        match fs::symlink_metadata(&authority_file) {
            Ok(metadata) => ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.gid() == parent_metadata.gid()
                    && metadata.permissions().mode() & 0o022 == 0,
                "write-lease authority file {} is not root-owned and service-nonwritable",
                authority_file.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspecting write-lease authority file {}",
                        authority_file.display()
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_root_authority_path(_path: &Path) -> Result<()> {
    bail!("process write-lease actuation requires a Unix authority path")
}

#[cfg(unix)]
fn harden_root_authority_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let parent = path
        .parent()
        .context("process write-lease authority file has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == 0
            && metadata.gid() == parent_metadata.gid(),
        "new write-lease authority file has the wrong owner"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o640))?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_root_authority_file(_path: &Path) -> Result<()> {
    bail!("process write-lease actuation requires Unix permissions")
}

#[cfg(unix)]
fn assign_source_directory_owner(path: &Path, identity: Option<ProcessIdentity>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    if let Some(identity) = identity {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .context("source authority path contains a NUL byte")?;
        let result = unsafe { libc::chown(path.as_ptr(), identity.uid, identity.gid) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("assigning source authority directory owner");
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o750))?;
    Ok(())
}

#[cfg(not(unix))]
fn assign_source_directory_owner(_path: &Path, identity: Option<ProcessIdentity>) -> Result<()> {
    ensure!(
        identity.is_none(),
        "configured source identities require a Unix actuator"
    );
    Ok(())
}

#[cfg(unix)]
fn validate_source_directory_owner(
    path: &Path,
    metadata: &fs::Metadata,
    identity: Option<ProcessIdentity>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let expected = identity.unwrap_or(ProcessIdentity {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    });
    ensure!(
        metadata.uid() == expected.uid && metadata.gid() == expected.gid,
        "source authority directory {} has the wrong owner",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o200 != 0,
        "source authority directory {} is not owner-writable",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_source_directory_owner(
    _path: &Path,
    _metadata: &fs::Metadata,
    identity: Option<ProcessIdentity>,
) -> Result<()> {
    ensure!(
        identity.is_none(),
        "configured source identities require a Unix actuator"
    );
    Ok(())
}

fn apply_identity(command: &mut Command, identity: Option<ProcessIdentity>) -> Result<()> {
    let Some(identity) = identity else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        ensure!(
            identity.uid > 0 && identity.gid > 0,
            "configured source identity must be unprivileged"
        );
        unsafe {
            command.pre_exec(move || {
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(identity.gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(identity.uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        let _ = identity;
        bail!("configured process identities require a Unix actuator")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Admission audit, claim 3: `is_permanently_stopped` now lets the
    /// Isolation gate skip `prove_isolation` against the incumbent. Enumerate
    /// what it answers `true` for, using a fake `systemctl show` that replays
    /// whatever unit state the test writes. The states that matter are the
    /// transitional ones -- a restarting, activating, or briefly inactive unit
    /// must read as NOT permanently stopped -- and a systemctl failure must be
    /// an error, never a quiet "stopped".
    #[cfg(unix)]
    #[test]
    fn is_permanently_stopped_only_for_failed_or_forgotten_units() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let systemctl = temp.path().join("systemctl");
        let state = temp.path().join("systemctl.state");
        let fail = temp.path().join("systemctl.fail");
        std::fs::write(
            &systemctl,
            "#!/bin/sh\nif [ \"$1\" != \"show\" ]; then exit 64; fi\ncat \"$0.state\"\nif [ -e \"$0.fail\" ]; then exit 1; fi\nexit 0\n",
        )?;
        std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755))?;
        let driver = SystemdTransientWorkloadDriver {
            systemctl_program: systemctl,
            ..Default::default()
        };
        let observation = audit_workload_observation("no");

        let show = |load: &str, active: &str, sub: &str| -> String {
            format!(
                "LoadState={load}\nActiveState={active}\nSubState={sub}\nInvocationID=inv\nDescription=Idunn test\n"
            )
        };
        for (active, sub) in [
            ("active", "running"),
            ("activating", "start"),
            ("activating", "auto-restart"),
            ("deactivating", "stop-sigterm"),
            ("deactivating", "final-sigterm"),
            ("inactive", "dead"),
            ("reloading", "reload"),
        ] {
            std::fs::write(&state, show("loaded", active, sub))?;
            assert!(
                !driver.is_permanently_stopped(&observation)?,
                "ActiveState={active} SubState={sub} must not read as permanently stopped"
            );
        }

        std::fs::write(&state, show("loaded", "failed", "failed"))?;
        assert!(
            driver.is_permanently_stopped(&observation)?,
            "failed is permanent under Restart=no"
        );

        // systemd has no such unit: `systemctl show` exits 0 with not-found.
        std::fs::write(&state, show("not-found", "inactive", "dead"))?;
        assert!(
            driver.is_permanently_stopped(&observation)?,
            "a forgotten unit cannot restart"
        );

        // A systemctl failure with no properties is an error, not "stopped".
        std::fs::write(&state, "")?;
        std::fs::write(&fail, "")?;
        assert!(
            driver.is_permanently_stopped(&observation).is_err(),
            "a systemctl failure must not be read as a stopped incumbent"
        );
        std::fs::remove_file(&fail)?;

        // A failure that still printed properties (systemctl exited non-zero
        // after output) is also an error.
        std::fs::write(&state, show("loaded", "failed", "failed"))?;
        std::fs::write(&fail, "")?;
        assert!(driver.is_permanently_stopped(&observation).is_err());
        std::fs::remove_file(&fail)?;

        // The recorded policy is asked before systemd is: any incumbent whose
        // observation carries another Restart policy makes the Isolation gate
        // error out (pre-fencing abort), whatever systemd says.
        std::fs::write(&state, show("loaded", "failed", "failed"))?;
        let restarting = audit_workload_observation("always");
        let error = driver
            .is_permanently_stopped(&restarting)
            .expect_err("Restart=always incumbent must not be judged stopped");
        assert!(format!("{error:#}").contains("Restart=no"), "{error:#}");
        Ok(())
    }

    fn host_audit_observation(pid: u32, created: u64) -> HostWorkloadObservation {
        HostWorkloadObservation {
            host: "raven".into(),
            actuator_identity_id: "actuator".into(),
            process_id: pid,
            process_creation_time: created,
            session_id: 1,
            user_sid: "S-1-5-21-1-2-3-1001".into(),
            executable: "C:/GameCult/idunn/releases/muninn/x/muninn.exe".into(),
            executable_sha256: format!("sha256-{}", "1".repeat(64)),
            command_line_sha256: format!("sha256-{}", "2".repeat(64)),
            environment_names: vec!["GAMECULT_IDUNN_RUNTIME_BUNDLE".into()],
            environment_contract_sha256: format!("sha256-{}", "3".repeat(64)),
            runtime_bundle: "C:/GameCult/idunn/runtime/muninn/x".into(),
            runtime_instance_id: format!("sha256-{}", "a".repeat(64)),
            activation_signer_identity_id: "activation".into(),
            activation_signer_public_key: vec![1; 32],
            exit_code: None,
        }
    }

    /// The previous Idunn persisted the systemd observation alone, under both
    /// MessagePack encodings CultCache uses. The enum must read those bytes
    /// as the systemd variant, and a host observation must not be mistaken
    /// for one.
    #[test]
    fn persisted_systemd_observations_decode_as_the_systemd_variant() -> Result<()> {
        let systemd = systemd_audit_observation("no", 61_000);
        for bytes in [
            rmp_serde::to_vec(&systemd)?,
            rmp_serde::to_vec_named(&systemd)?,
        ] {
            let decoded: WorkloadObservation = rmp_serde::from_slice(&bytes)?;
            assert_eq!(decoded, WorkloadObservation::Systemd(systemd.clone()));
        }
        let host = host_audit_observation(4242, 133_000_000_000_000_000);
        for bytes in [rmp_serde::to_vec(&host)?, rmp_serde::to_vec_named(&host)?] {
            let decoded: WorkloadObservation = rmp_serde::from_slice(&bytes)?;
            assert_eq!(decoded, WorkloadObservation::Host(host.clone()));
        }
        let linux = IsolationEvidence::Linux(LinuxIsolationEvidence {
            candidate_uid: 1,
            candidate_pid_namespace_id: 2,
            candidate_mount_namespace_id: 3,
            incumbent_uid: None,
            incumbent_pid_namespace_id: None,
            incumbent_mount_namespace_id: None,
        });
        let host_evidence = IsolationEvidence::Host(HostIsolationEvidence {
            candidate_process_id: 1,
            candidate_process_creation_time: 2,
            incumbent_process_id: Some(3),
            incumbent_process_creation_time: Some(4),
        });
        for evidence in [linux, host_evidence] {
            for bytes in [
                rmp_serde::to_vec(&evidence)?,
                rmp_serde::to_vec_named(&evidence)?,
            ] {
                let decoded: IsolationEvidence = rmp_serde::from_slice(&bytes)?;
                assert_eq!(decoded, evidence);
            }
        }
        Ok(())
    }

    #[test]
    fn host_isolation_is_two_live_distinct_processes_on_one_host() {
        let live = |pid: u32, created: u64| {
            WorkloadObservation::Host(host_audit_observation(pid, created))
        };
        let incumbent = live(100, 5);
        assert!(WorkloadObservation::prove_isolation(&live(101, 6), Some(&incumbent)).is_ok());
        assert!(WorkloadObservation::prove_isolation(&live(100, 5), Some(&incumbent)).is_err());
        let mut exited = host_audit_observation(101, 6);
        exited.exit_code = Some(1);
        let exited = WorkloadObservation::Host(exited);
        assert!(WorkloadObservation::prove_isolation(&exited, Some(&incumbent)).is_err());
        let systemd = audit_workload_observation("no");
        assert!(WorkloadObservation::prove_isolation(&live(101, 6), Some(&systemd)).is_err());
    }

    fn audit_workload_observation(restart_policy: &str) -> WorkloadObservation {
        let uid = 61_000u32;
        WorkloadObservation::Systemd(systemd_audit_observation(restart_policy, uid))
    }

    fn systemd_audit_observation(restart_policy: &str, uid: u32) -> SystemdWorkloadObservation {
        SystemdWorkloadObservation {
            unit: format!("idunn-{}.service", "a".repeat(64)),
            unit_description: "Idunn test".into(),
            invocation_id: "inv".into(),
            exec_main_start_timestamp_monotonic: 1,
            service_type: "exec".into(),
            restart_policy: restart_policy.into(),
            kill_mode: "mixed".into(),
            dynamic_user: true,
            systemd_user: format!("u{uid}"),
            systemd_group: format!("u{uid}"),
            supplementary_groups: String::new(),
            capability_bounding_set: String::new(),
            ambient_capabilities: String::new(),
            private_mounts: true,
            private_pids: true,
            protect_proc: "invisible".into(),
            proc_subset: "all".into(),
            no_new_privileges: true,
            umask: "0007".into(),
            inaccessible_paths: String::new(),
            load_credential: String::new(),
            main_pid: 4242,
            process_start_time: 1,
            process_uids: [uid; 4],
            process_gids: [uid; 4],
            process_groups: vec![uid],
            process_cap_inheritable: 0,
            process_cap_permitted: 0,
            process_cap_effective: 0,
            process_cap_bounding: 0,
            process_cap_ambient: 0,
            process_no_new_privileges: true,
            process_namespace_pids: vec![1],
            mount_namespace_id: 7,
            pid_namespace_id: 8,
            executable: PathBuf::from("/opt/test/bin/service"),
            executable_device: 1,
            executable_inode: 1,
            executable_sha256: format!("sha256-{}", "1".repeat(64)),
            runtime_instance_id: format!("sha256-{}", "a".repeat(64)),
            working_directory: PathBuf::from("/opt/test"),
            runtime_bundle: PathBuf::from("/run/test"),
            command_line_sha256: format!("sha256-{}", "2".repeat(64)),
            environment_names: Vec::new(),
            environment_contract_sha256: format!("sha256-{}", "3".repeat(64)),
            control_group: "/system.slice/idunn-test.service".into(),
            credentials_directory: None,
            parent_only_file_descriptors: Vec::new(),
            activation_signer_identity_id: "activation".into(),
            activation_signer_public_key: vec![1; 32],
            service_credentials: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_published_projection_is_readable_by_a_workload() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("topology.cc");
        std::fs::write(&path, b"x").unwrap();
        // What Idunn's UMask=027 leaves behind.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let lock = temp.path().join("topology.cc.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o640)).unwrap();
        publish_projection_mode(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "every target must be able to read the projection"
        );
        let lock_mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(lock_mode, 0o644, "the lock is opened alongside the store");
    }

    #[cfg(unix)]
    #[test]
    fn an_untraversable_runtime_root_is_refused_rather_than_silently_empty() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let runtime_root = temp.path().join("runtime");
        let bundle = runtime_root.join("sha256-abc");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = ensure_bundle_is_reachable_by_workload(&bundle, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not traversable by the workload")
        );
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o711)).unwrap();
        ensure_bundle_is_reachable_by_workload(&bundle, None).unwrap();
        // A 0750 root group-owned by the state group is the other correct
        // shape, and is what the write-lease hardening expects: it derives the
        // record's group from this directory.
        std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o750)).unwrap();
        let owning_group =
            std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(&runtime_root).unwrap());
        ensure_bundle_is_reachable_by_workload(&bundle, Some(owning_group)).unwrap();
        assert!(ensure_bundle_is_reachable_by_workload(&bundle, Some(owning_group + 1)).is_err());
    }

    #[test]
    fn build_machine_id_is_well_formed_per_workspace_and_never_the_host() {
        let first = build_machine_id(Path::new("/var/lib/gamecult/idunn/staging/tx-a")).unwrap();
        let second = build_machine_id(Path::new("/var/lib/gamecult/idunn/staging/tx-b")).unwrap();
        assert_eq!(first.len(), 32);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(first, second);
        assert_eq!(
            first,
            build_machine_id(Path::new("/var/lib/gamecult/idunn/staging/tx-a")).unwrap(),
            "a workspace's build identity must be stable for the life of the build"
        );
        // The point of the derived identity is that it is not this host's: an
        // identity a build enrols must not unwrap anywhere but that build.
        if let Ok(host) = std::fs::read_to_string("/etc/machine-id") {
            assert_ne!(first, host.trim());
        }
    }

    use cultnet_rs::{
        GameCultProviderHealthIdentity, IDUNN_EXPECTED_INCARNATION_SCHEMA,
        IDUNN_PROCESS_WRITE_LEASE_SCHEMA, IdunnServiceIdentity,
        OdinRuntimeTopologyCorrelationPurpose, OdinTopologyAuthenticationContext,
        OdinTopologyIdentity, authenticate_odin_runtime_topology_correlation,
        enroll_service_identity_at, verify_runtime_authority,
    };

    fn digest(byte: char) -> String {
        format!("sha256-{}", byte.to_string().repeat(64))
    }

    fn parent_only_descriptor(
        fd_number: u32,
        fd_name: &str,
        source: &str,
    ) -> ParentOnlyFileDescriptorObservation {
        ParentOnlyFileDescriptorObservation {
            fd_number,
            fd_name: fd_name.into(),
            source_path: PathBuf::from(source),
            access: "read-only".into(),
            device: 1,
            inode: u64::from(fd_number),
            uid: 0,
            gid: 0,
            mode: 0o400,
            links: 1,
            size: 32,
            sha256: digest('a'),
        }
    }

    #[test]
    fn signer_sources_lower_to_exactly_two_ordered_parent_only_open_files() -> Result<()> {
        let activation = parent_only_descriptor(
            3,
            IDUNN_RUNTIME_ACTIVATION_CREDENTIAL_NAME,
            "/run/idunn/activation-credentials/activation.credential",
        );
        let presence = parent_only_descriptor(
            4,
            RUNTIME_PRESENCE_IDENTITY_FD_NAME,
            "/etc/gamecult/service/runtime-presence-identity.cc",
        );
        assert_eq!(
            parent_only_open_file_properties(&[activation.clone(), presence.clone()])?,
            vec![
                "/run/idunn/activation-credentials/activation.credential:gamecult-idunn-runtime-activation-key:read-only",
                "/etc/gamecult/service/runtime-presence-identity.cc:gamecult-runtime-presence-identity:read-only",
            ]
        );
        assert!(parent_only_open_file_properties(&[presence, activation]).is_err());
        Ok(())
    }

    #[cfg(unix)]
    fn git_at(repository: &Path, arguments: &[&str]) -> Result<String> {
        let output = Command::new("/usr/bin/git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .env_clear()
            .env("HOME", repository)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LANG", "C.UTF-8")
            .output()?;
        ensure!(
            output.status.success(),
            "test Git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn fixture_docker_runner_binding() -> DockerRunnerBinding {
        DockerRunnerBinding {
            image: "eureka-verify-rust".to_owned(),
            user: "65532:65532".to_owned(),
            affordances: Default::default(),
            cache_root: None,
            allowed_programs: std::collections::BTreeSet::from(["cargo".to_owned()]),
            environment: BTreeMap::new(),
            secret_files: BTreeMap::new(),
            network_profile: None,
            memory_mebibytes: 8192,
            cpu_quota_percent: 250,
            pids_limit: 512,
            tmpfs_mebibytes: 256,
        }
    }

    /// Cut 1, R-Cut1-1: `docker_run_args` lowers a `ContainerSpec` to an exact
    /// argv. Pinned byte for byte so a revert (dropping `--cap-drop ALL`), a
    /// loosening (defaulting the network to `bridge` instead of `none`), or a
    /// function-of-input change (`--cpus` computed as `quota/50` instead of
    /// `quota/100`, which this fixture's `cpu_quota_percent = 250` catches:
    /// `2.50` against `5.00`) all fail this test.
    #[cfg(unix)]
    #[test]
    fn container_spec_lowers_to_exact_docker_argv() -> Result<()> {
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-1/.runner-rust");
        let runner = fixture_docker_runner_binding();
        let spec = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        )?;
        let argv = vec!["cargo".to_owned(), "test".to_owned(), "--lib".to_owned()];
        let args = docker_run_args(&spec, &workspace, Path::new("build"), &argv)?;

        let machine_id = build_machine_id(&workspace)?;
        let expected: Vec<OsString> = [
            "run".to_owned(),
            "--rm".to_owned(),
            "--network".to_owned(),
            "none".to_owned(),
            "--memory".to_owned(),
            "8192m".to_owned(),
            "--cpus".to_owned(),
            "2.50".to_owned(),
            "--mount".to_owned(),
            format!("type=bind,src={},dst=/workspace", workspace.display()),
            "--user".to_owned(),
            "65532:65532".to_owned(),
            "--cap-drop".to_owned(),
            "ALL".to_owned(),
            "--security-opt".to_owned(),
            "no-new-privileges".to_owned(),
            "--read-only".to_owned(),
            "--pids-limit".to_owned(),
            "512".to_owned(),
            "--tmpfs".to_owned(),
            "/tmp:rw,nosuid,nodev,noexec,size=256m".to_owned(),
            "--mount".to_owned(),
            format!(
                "type=bind,src=/run/idunn/build-machine-ids/{machine_id},dst=/etc/machine-id,readonly"
            ),
            "--workdir".to_owned(),
            "/workspace/build".to_owned(),
            "--env".to_owned(),
            "IDUNN_SOURCE_REVISION=abc123".to_owned(),
            "eureka-verify-rust".to_owned(),
            "cargo".to_owned(),
            "test".to_owned(),
            "--lib".to_owned(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
        Ok(())
    }

    /// Cut 1, R-Cut1-2 (F6): the only environment a step's container receives
    /// is the source stamp plus the names it lists in `required_environment`.
    /// The fixture binding carries a variable the step does not name; its
    /// mutant is a `ContainerSpec::for_step` that copies every binding
    /// `environment` entry instead of only the required ones.
    #[test]
    fn required_environment_is_the_only_environment() -> Result<()> {
        let mut runner = fixture_docker_runner_binding();
        runner.environment = BTreeMap::from([
            ("USED".to_owned(), "1".to_owned()),
            ("UNUSED".to_owned(), "2".to_owned()),
        ]);
        let required = std::collections::BTreeSet::from(["USED".to_owned()]);
        let spec =
            ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"))?;
        assert_eq!(
            spec.environment,
            vec![
                ("IDUNN_SOURCE_REVISION".to_owned(), "abc123".to_owned()),
                ("USED".to_owned(), "1".to_owned()),
            ]
        );
        assert!(spec.secret_mounts.is_empty());
        Ok(())
    }

    /// Cut 1 fix batch, F1: `required_environment_is_the_only_environment`
    /// only pinned the exact-name case. The prefix-passthrough mutant (S3a)
    /// copies any binding environment entry whose name merely starts with a
    /// required name; the ambient mutant (S3b) copies any `GAMECULT_*` or
    /// `IDUNN_*` binding entry regardless of whether it is required. This
    /// fixture's binding carries both `USEDLONGER` (a prefix collision with
    /// the required `USED`) and `GAMECULT_AMBIENT` / `IDUNN_AMBIENT`, and the
    /// expected environment excludes every one of them.
    #[test]
    fn required_environment_rejects_prefix_and_ambient_passthrough() -> Result<()> {
        let mut runner = fixture_docker_runner_binding();
        runner.environment = BTreeMap::from([
            ("USED".to_owned(), "1".to_owned()),
            ("USEDLONGER".to_owned(), "prefix-collision".to_owned()),
            ("GAMECULT_AMBIENT".to_owned(), "ambient".to_owned()),
            ("IDUNN_AMBIENT".to_owned(), "ambient".to_owned()),
        ]);
        let required = std::collections::BTreeSet::from(["USED".to_owned()]);
        let spec =
            ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"))?;
        assert_eq!(
            spec.environment,
            vec![
                ("IDUNN_SOURCE_REVISION".to_owned(), "abc123".to_owned()),
                ("USED".to_owned(), "1".to_owned()),
            ]
        );
        Ok(())
    }

    /// Cut 1 fix batch, Stamp override: a runner cannot bind a step
    /// environment name that collides with the Idunn source stamp, even when
    /// the binding names it and the step requires it. This is enforced at
    /// `ContainerSpec::for_step`, the one place every runner's environment is
    /// assembled, rather than only at deploy's separate `admit` check.
    #[test]
    fn container_spec_for_step_refuses_stamp_override() {
        let mut runner = fixture_docker_runner_binding();
        runner.environment = BTreeMap::from([("STAMP".to_owned(), "override".to_owned())]);
        let required = std::collections::BTreeSet::from(["STAMP".to_owned()]);
        let result = ContainerSpec::for_step(&runner, &required, ("STAMP", "abc123"));
        assert!(
            result.is_err(),
            "expected a runner binding to be refused when it can override the source stamp"
        );
    }

    /// Cut 1 fix batch, F1: `docker_run_args` pins the argv exactly for every
    /// network branch, not only the default `None` case. The prior test's
    /// mutants that survived here were `--cap-drop ALL` dropped only when the
    /// network is `bridge` (S1) and `--read-only` dropped only when the
    /// network is `Named` (S5 covers the explicit-`none`-becomes-`bridge`
    /// loosening separately below). Every branch must carry every security
    /// flag identically; only `--network` differs.
    #[cfg(unix)]
    #[test]
    fn docker_run_args_pins_every_network_branch() -> Result<()> {
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-net/.runner-rust");
        let runner = fixture_docker_runner_binding();
        let argv = vec!["cargo".to_owned(), "test".to_owned()];
        let machine_id = build_machine_id(&workspace)?;

        let base = |network: &str| -> Vec<OsString> {
            [
                "run".to_owned(),
                "--rm".to_owned(),
                "--network".to_owned(),
                network.to_owned(),
                "--memory".to_owned(),
                "8192m".to_owned(),
                "--cpus".to_owned(),
                "2.50".to_owned(),
                "--mount".to_owned(),
                format!("type=bind,src={},dst=/workspace", workspace.display()),
                "--user".to_owned(),
                "65532:65532".to_owned(),
                "--cap-drop".to_owned(),
                "ALL".to_owned(),
                "--security-opt".to_owned(),
                "no-new-privileges".to_owned(),
                "--read-only".to_owned(),
                "--pids-limit".to_owned(),
                "512".to_owned(),
                "--tmpfs".to_owned(),
                "/tmp:rw,nosuid,nodev,noexec,size=256m".to_owned(),
                "--mount".to_owned(),
                format!(
                    "type=bind,src=/run/idunn/build-machine-ids/{machine_id},dst=/etc/machine-id,readonly"
                ),
                "--workdir".to_owned(),
                "/workspace/build".to_owned(),
                "--env".to_owned(),
                "IDUNN_SOURCE_REVISION=abc123".to_owned(),
                "eureka-verify-rust".to_owned(),
                "cargo".to_owned(),
                "test".to_owned(),
            ]
            .into_iter()
            .map(OsString::from)
            .collect()
        };

        for (profile, expected_network) in
            [(None, "none"), (Some("bridge"), "bridge"), (Some("odin-verse"), "odin-verse")]
        {
            let mut r = runner.clone();
            r.network_profile = profile.map(str::to_owned);
            let spec = ContainerSpec::for_step(
                &r,
                &std::collections::BTreeSet::new(),
                ("IDUNN_SOURCE_REVISION", "abc123"),
            )?;
            let args = docker_run_args(&spec, &workspace, Path::new("build"), &argv)?;
            assert_eq!(args, base(expected_network), "network branch {profile:?}");
        }
        Ok(())
    }

    /// Cut 1 fix batch, F1, S5: an explicit `network_profile = "none"` must
    /// lower to `ContainerNetwork::None`, not `Bridge`.
    #[test]
    fn explicit_none_network_profile_is_none_not_bridge() -> Result<()> {
        let mut runner = fixture_docker_runner_binding();
        runner.network_profile = Some("none".to_owned());
        let spec = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        )?;
        assert_eq!(spec.network, ContainerNetwork::None);
        Ok(())
    }

    /// Cut 1 fix batch, F1, S2a/S2b/S2c: `--cpus` is pinned across a range of
    /// quotas that a round-to-nearest-0.5 mutant, a floor-to-50 mutant, and a
    /// round-to-integer mutant each compute differently from the true
    /// `quota / 100`.
    #[test]
    fn docker_run_args_pins_cpu_quota_across_the_range() -> Result<()> {
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-cpu/.runner-rust");
        let argv = vec!["cargo".to_owned(), "test".to_owned()];
        for (quota, expected) in [
            (100u32, "1.00"),
            (150, "1.50"),
            (250, "2.50"),
            (333, "3.33"),
            (800, "8.00"),
        ] {
            let mut runner = fixture_docker_runner_binding();
            runner.cpu_quota_percent = quota;
            let spec = ContainerSpec::for_step(
                &runner,
                &std::collections::BTreeSet::new(),
                ("IDUNN_SOURCE_REVISION", "abc123"),
            )?;
            let args = docker_run_args(&spec, &workspace, Path::new("."), &argv)?;
            let cpus_index = args
                .iter()
                .position(|a| a == OsStr::new("--cpus"))
                .expect("--cpus flag present");
            assert_eq!(
                args[cpus_index + 1],
                OsString::from(expected),
                "quota {quota}"
            );
        }
        Ok(())
    }

    /// Cut 1 fix batch, F1, S6: a cache root adds exactly one `--mount` for
    /// `/cache`, never a second mount of the cache's parent directory. This
    /// fixture asserts the full argv, so any extra mount breaks equality.
    #[cfg(unix)]
    #[test]
    fn docker_run_args_mounts_cache_root_exactly_once() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        // Left uncreated: `ensure_runner_cache_root` creates it itself, owned
        // by the runner's exact identity at mode 0700.
        let cache = temp.path().join("cache");

        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-cache/.runner-rust");
        let argv = vec!["cargo".to_owned(), "test".to_owned()];
        let machine_id = build_machine_id(&workspace)?;

        // Absent: no cache mount at all.
        let mut runner = fixture_docker_runner_binding();
        let spec = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        )?;
        let args = docker_run_args(&spec, &workspace, Path::new("."), &argv)?;
        assert!(
            !args.iter().any(|a| a == OsStr::new("/cache") || a.to_string_lossy().contains("dst=/cache")),
            "no cache mount expected when cache_root is absent"
        );

        // Present: exactly one mount, of the cache root itself.
        runner.cache_root = Some(cache.clone());
        let spec = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        )?;
        let args = docker_run_args(&spec, &workspace, Path::new("."), &argv)?;
        let expected: Vec<OsString> = [
            "run".to_owned(),
            "--rm".to_owned(),
            "--network".to_owned(),
            "none".to_owned(),
            "--memory".to_owned(),
            "8192m".to_owned(),
            "--cpus".to_owned(),
            "2.50".to_owned(),
            "--mount".to_owned(),
            format!("type=bind,src={},dst=/workspace", workspace.display()),
            "--user".to_owned(),
            "65532:65532".to_owned(),
            "--cap-drop".to_owned(),
            "ALL".to_owned(),
            "--security-opt".to_owned(),
            "no-new-privileges".to_owned(),
            "--read-only".to_owned(),
            "--pids-limit".to_owned(),
            "512".to_owned(),
            "--tmpfs".to_owned(),
            "/tmp:rw,nosuid,nodev,noexec,size=256m".to_owned(),
            "--mount".to_owned(),
            format!(
                "type=bind,src=/run/idunn/build-machine-ids/{machine_id},dst=/etc/machine-id,readonly"
            ),
            "--mount".to_owned(),
            format!("type=bind,src={},dst=/cache", cache.display()),
            "--workdir".to_owned(),
            "/workspace/".to_owned(),
            "--env".to_owned(),
            "IDUNN_SOURCE_REVISION=abc123".to_owned(),
            "eureka-verify-rust".to_owned(),
            "cargo".to_owned(),
            "test".to_owned(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
        Ok(())
    }

    /// Cut 1 fix batch, F1, S4: a secret mount's bind is read-only. This
    /// fixture also exercises `for_step`'s live secret validation, so it
    /// doubles as the positive twin of S9 below.
    #[cfg(unix)]
    #[test]
    fn docker_run_args_mounts_secrets_read_only() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let secret = temp.path().join("secret.cc");
        fs::write(&secret, b"s").unwrap();
        ensure!(
            Command::new("/bin/chown")
                .arg("0:65532")
                .arg(&secret)
                .status()?
                .success(),
            "chowning fixture secret"
        );
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o440))?;

        let mut runner = fixture_docker_runner_binding();
        runner.secret_files = BTreeMap::from([("A_SECRET".to_owned(), secret.clone())]);
        let required = std::collections::BTreeSet::from(["A_SECRET".to_owned()]);
        let spec = ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"))?;
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-secret/.runner-rust");
        let args = docker_run_args(
            &spec,
            &workspace,
            Path::new("."),
            &vec!["cargo".to_owned(), "test".to_owned()],
        )?;
        let secret_text = secret.display().to_string();
        let mount = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .find(|a| a.starts_with("type=bind") && a.contains(&secret_text))
            .expect("secret mount present");
        assert!(mount.ends_with(",readonly"), "secret mount must be read-only: {mount}");
        Ok(())
    }

    /// F6/N1 (Soul, second Cut 1 fix batch): `--read-only` is present on a
    /// Named network runner even when a cache root is also mounted. Neither
    /// branch alone exercised this combination before.
    #[cfg(unix)]
    #[test]
    fn docker_run_args_keeps_read_only_with_a_named_network_and_a_cache_root() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let cache = temp.path().join("cache");
        let mut runner = fixture_docker_runner_binding();
        runner.network_profile = Some("build-net".to_owned());
        runner.cache_root = Some(cache);
        let spec = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        )?;
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-n1/.runner-rust");
        let args = docker_run_args(&spec, &workspace, Path::new("."), &vec!["cargo".to_owned(), "test".to_owned()])?;
        assert!(
            args.iter().any(|a| a == OsStr::new("--read-only")),
            "a Named network plus a cache root must not drop --read-only: {args:?}"
        );
        Ok(())
    }

    /// F6/N2 (Soul, second Cut 1 fix batch): a secret mount stays read-only
    /// even when the runner also carries a plain environment entry.
    #[cfg(unix)]
    #[test]
    fn docker_run_args_keeps_secret_mounts_read_only_alongside_plain_environment() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let secret = temp.path().join("secret.cc");
        fs::write(&secret, b"s").unwrap();
        ensure!(
            Command::new("/bin/chown").arg("0:65532").arg(&secret).status()?.success(),
            "chowning fixture secret"
        );
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o440))?;

        let mut runner = fixture_docker_runner_binding();
        runner.secret_files = BTreeMap::from([("A_SECRET".to_owned(), secret.clone())]);
        runner.environment = BTreeMap::from([("PLAIN_ONE".to_owned(), "x".to_owned())]);
        let required = std::collections::BTreeSet::from(["A_SECRET".to_owned(), "PLAIN_ONE".to_owned()]);
        let spec = ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"))?;
        let workspace = PathBuf::from("/var/lib/gamecult/idunn/staging/txn-n2/.runner-rust");
        let args = docker_run_args(&spec, &workspace, Path::new("."), &vec!["cargo".to_owned(), "test".to_owned()])?;
        let secret_text = secret.display().to_string();
        let mount = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .find(|a| a.starts_with("type=bind") && a.contains(&secret_text))
            .expect("secret mount present");
        assert!(
            mount.ends_with(",readonly"),
            "a plain environment entry must not make a secret mount writable: {mount}"
        );
        Ok(())
    }

    /// F6/N3 (Soul, second Cut 1 fix batch): the Idunn source stamp survives
    /// as the container's environment even when the runner carries both a
    /// secret and a plain environment entry.
    #[cfg(unix)]
    #[test]
    fn container_spec_for_step_keeps_the_source_stamp_with_a_secret_and_plain_environment() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let secret = temp.path().join("secret.cc");
        fs::write(&secret, b"s").unwrap();
        ensure!(
            Command::new("/bin/chown").arg("0:65532").arg(&secret).status()?.success(),
            "chowning fixture secret"
        );
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o440))?;

        let mut runner = fixture_docker_runner_binding();
        runner.secret_files = BTreeMap::from([("A_SECRET".to_owned(), secret)]);
        runner.environment = BTreeMap::from([("PLAIN_ONE".to_owned(), "x".to_owned())]);
        let required = std::collections::BTreeSet::from(["A_SECRET".to_owned(), "PLAIN_ONE".to_owned()]);
        let spec = ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"))?;
        assert_eq!(
            spec.environment.first(),
            Some(&("IDUNN_SOURCE_REVISION".to_owned(), "abc123".to_owned())),
            "the source stamp must survive alongside a secret and a plain environment entry"
        );
        Ok(())
    }

    /// F6/N5 (Soul, second Cut 1 fix batch): `freeze_exact` records that the
    /// frozen tree contains a Git LFS pointer, and the pointer bytes
    /// themselves -- never the real LFS content, which is never fetched --
    /// are what land on disk.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_sets_the_lfs_pointer_flag_for_a_pointer_file() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(&origin_repo, &["config", "user.email", "idunn-test@example.invalid"])?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 3\n";
        fs::write(origin_repo.join("lfs.bin"), pointer)?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown").args(["-R", "1000:1000"]).arg(&origin_repo).status()?.success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity { uid: 1000, gid: 1000 };
        let driver = GitSourceDriver::new(source_cache_root.clone(), frozen_source_root.clone(), Some(identity));
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let (tree_root, _snapshot_sha256, _recipe_bytes, contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-lfs", &frozen_source_root)?;
        assert!(contains_lfs_pointers, "an LFS pointer file must set the flag");
        assert_eq!(fs::read(tree_root.join("lfs.bin"))?, pointer);
        Ok(())
    }

    /// Cut 1 fix batch, F1, S9: `ContainerSpec::for_step` must call
    /// `validate_runner_secret`, not merely mount whatever path the binding
    /// names. This fixture's secret file has the wrong mode (`0644` instead
    /// of the required `0440`), so the real validator refuses it; a mutant
    /// that skips the call would let this through.
    #[cfg(unix)]
    #[test]
    fn container_spec_for_step_validates_secret_files() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let secret = temp.path().join("secret.cc");
        fs::write(&secret, b"s").unwrap();
        ensure!(
            Command::new("/bin/chown")
                .arg("0:65532")
                .arg(&secret)
                .status()?
                .success(),
            "chowning fixture secret"
        );
        // Wrong: group-writable/world-readable, not the required 0440.
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644))?;

        let mut runner = fixture_docker_runner_binding();
        runner.secret_files = BTreeMap::from([("A_SECRET".to_owned(), secret)]);
        let required = std::collections::BTreeSet::from(["A_SECRET".to_owned()]);
        let result = ContainerSpec::for_step(&runner, &required, ("IDUNN_SOURCE_REVISION", "abc123"));
        assert!(result.is_err(), "expected an invalid secret file to be refused");
        Ok(())
    }

    /// Cut 1 fix batch, F1, S10: `ContainerSpec::for_step` must call
    /// `ensure_runner_cache_root`. This fixture's cache root sits under a
    /// world-writable parent, which the real validator refuses; a mutant
    /// that skips the call would let this through.
    #[cfg(unix)]
    #[test]
    fn container_spec_for_step_validates_cache_root() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let bad_parent = temp.path().join("bad-parent");
        fs::create_dir(&bad_parent)?;
        // Wrong: world-writable, not root-owned-and-nonwritable.
        fs::set_permissions(&bad_parent, fs::Permissions::from_mode(0o777))?;

        let mut runner = fixture_docker_runner_binding();
        runner.cache_root = Some(bad_parent.join("cache"));
        let result = ContainerSpec::for_step(
            &runner,
            &std::collections::BTreeSet::new(),
            ("IDUNN_SOURCE_REVISION", "abc123"),
        );
        assert!(result.is_err(), "expected an unsafe cache root parent to be refused");
        Ok(())
    }

    /// Cut 1, R-Cut1-3: `freeze_exact` archives an exact revision through the
    /// new entry point (no `OperatorBinding` or compiled plan), and the
    /// archived recipe file must equal the recipe blob it read from the same
    /// tree. Reuses the bare-repository fixture pattern from
    /// `exact_git_archive_becomes_root_owned_immutable_source_without_git_metadata`.
    /// Its mutant skips the "materialized recipe equals the tree blob" check.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_is_byte_exact_and_recipe_checked() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        // The unprivileged identity below clones this fixture as a local
        // path, which puts Git's ownership check in play: a real origin is a
        // network remote with no local uid to compare against, but this
        // fixture is a directory this (root) test process just created. Hand
        // it to the unprivileged identity so the clone sees itself as owner,
        // the way a real remote never has to.
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;

        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        let (tree_root, snapshot_sha256, recipe_bytes, contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-1", &frozen_source_root)?;
        assert_eq!(recipe_bytes, b"target = 'test'\n".to_vec());
        assert!(!contains_lfs_pointers);
        assert!(tree_root.join("deployment.toml").is_file());
        assert!(!tree_root.join(".git").exists());
        assert!(snapshot_sha256.starts_with("sha256-"));
        Ok(())
    }

    /// Cut 1, R-Cut1-3 negative twin: an `export-subst` recipe means `git
    /// archive` writes a substituted `$Format:%H$` token into the archived
    /// file. Before the F2 fix batch, `git archive` (what `freeze_exact` used
    /// to materialize with) expanded that token while `git cat-file blob`
    /// (what `exact_recipe_and_gitlinks` reads) returned it unexpanded, so the
    /// two byte strings the recipe-bytes-equality check compares genuinely
    /// differed and freeze_exact refused. After F2, `freeze_exact` no longer
    /// archives anything — the recipe is written raw, the same as everything
    /// else — so `export-subst` is no longer a transform at all and the
    /// frozen recipe keeps its literal, unexpanded token. This is now a
    /// byte-exactness positive case rather than a rejection case; the
    /// recipe-bytes-equality check itself has no remaining fixture that can
    /// make it fail (not yet reached — see the F2 fix-batch report).
    #[cfg(unix)]
    #[test]
    fn freeze_exact_keeps_an_export_subst_recipe_literal() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(
            origin_repo.join(".gitattributes"),
            b"deployment.toml export-subst\n",
        )?;
        fs::write(
            origin_repo.join("deployment.toml"),
            b"target = 'test'\n# $Format:%H$\n",
        )?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;

        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        let (tree_root, _snapshot_sha256, recipe_bytes, _contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-2", &frozen_source_root)?;
        assert_eq!(recipe_bytes, b"target = 'test'\n# $Format:%H$\n".to_vec());
        assert_eq!(
            fs::read(tree_root.join("deployment.toml"))?,
            b"target = 'test'\n# $Format:%H$\n".to_vec()
        );
        Ok(())
    }

    /// Cut 1 fix batch, F5, S7: `freeze_exact` must materialize every
    /// declared Gitlink, not silently skip the loop. This fixture archives a
    /// main repository with one Gitlink and asserts the child's own file
    /// lands at the Gitlink's path in the frozen tree; a mutant that skips
    /// the Gitlink loop leaves that path absent.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_materializes_gitlinks() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let sub_repo = temp.path().join("sub");
        fs::create_dir(&sub_repo)?;
        git_at(&sub_repo, &["init", "--initial-branch=main"])?;
        git_at(&sub_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(&sub_repo, &["config", "user.email", "idunn-test@example.invalid"])?;
        fs::write(sub_repo.join("lib.txt"), b"vendored content\n")?;
        git_at(&sub_repo, &["add", "--all"])?;
        git_at(&sub_repo, &["commit", "-m", "sub fixture"])?;
        let sub_revision = git_at(&sub_repo, &["rev-parse", "HEAD"])?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(
            &origin_repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sub_revision},vendor/sub"),
            ],
        )?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        for repo in [&origin_repo, &sub_repo] {
            ensure!(
                Command::new("/bin/chown")
                    .args(["-R", "1000:1000"])
                    .arg(repo)
                    .status()?
                    .success(),
                "chowning the fixture repository"
            );
        }

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;

        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::from([(
                PathBuf::from("vendor/sub"),
                GitlinkBinding {
                    origin: sub_repo.to_string_lossy().into_owned(),
                },
            )]),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        let (tree_root, _snapshot_sha256, _recipe_bytes, contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-gitlink", &frozen_source_root)?;
        assert!(!contains_lfs_pointers);
        let materialized = fs::read_to_string(tree_root.join("vendor/sub/lib.txt"))
            .context("reading materialized Gitlink content")?;
        assert_eq!(materialized, "vendored content\n");
        Ok(())
    }

    /// F1 (Self's ruling, second Cut 1 fix batch): every blob a frozen tree
    /// needs is fetched in one bulk round trip, not lazily one object at a
    /// time as `cat-file --batch` reads them. A spy standing in for
    /// `/usr/bin/git` records every invocation; exactly one of them is the
    /// bulk `fetch --stdin` call.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_bulk_fetches_every_blob_in_one_round_trip() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        for i in 0..8 {
            fs::write(origin_repo.join(format!("f{i}.txt")), format!("file {i}\n"))?;
        }
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;

        let log = temp.path().join("git-invocations.log");
        // Pre-created and world-writable: the spy runs under the
        // unprivileged Git identity, which cannot create a new file in this
        // root-owned 0755 directory, only append to one that already exists.
        fs::write(&log, b"")?;
        fs::set_permissions(&log, fs::Permissions::from_mode(0o666))?;
        let spy = temp.path().join("git-spy.sh");
        fs::write(
            &spy,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nexec /usr/bin/git \"$@\"\n",
                log.display()
            ),
        )?;
        fs::set_permissions(&spy, fs::Permissions::from_mode(0o755))?;

        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let mut driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        driver.git_program = spy;
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        driver.freeze_exact(&source, &revision, "txn-bulk", &frozen_source_root)?;
        let log_text = fs::read_to_string(&log).unwrap_or_default();
        let bulk_fetches = log_text
            .lines()
            .filter(|line| line.contains("fetch") && line.contains("--stdin"))
            .count();
        assert_eq!(
            bulk_fetches, 1,
            "expected exactly one bulk --stdin fetch; invocations:\n{log_text}"
        );
        Ok(())
    }

    /// F2/F3 (Self's rulings, second Cut 1 fix batch): a tree with two
    /// entries literally named `a` at the same level -- one a symlink
    /// pointing outside the frozen root, one a subtree holding `pwn` -- must
    /// be refused before anything is written, not partially materialized.
    /// `git hash-object --literally` builds the tree directly, bypassing the
    /// checks `git mktree` would apply, the same construction Soul's probe
    /// used to find the regression this pins.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_refuses_a_duplicate_named_tree_that_would_write_outside_its_root() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let hash_object = |repo: &Path, content: &[u8]| -> Result<String> {
            let mut child = Command::new("git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(repo)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(content)?;
            let output = child.wait_with_output()?;
            ensure!(output.status.success(), "git hash-object failed");
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };

        let outside = temp.path().join("outside-root-owned");
        fs::create_dir(&outside)?;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755))?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "good"])?;
        let good = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            // `file://`, not a plain path: a plain local path triggers Git's
            // optimized same-filesystem clone, which never runs the
            // pack/transfer code path `transfer.fsckObjects` gates. `file://`
            // forces the real transfer, the same one a real remote gets, so
            // this fixture also exercises F2 layer (a)'s fsck defense.
            origin: format!("file://{}", origin_repo.display()),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        // Freeze the earlier, good commit first, while `main` still points
        // at it: this is what actually creates the checkout, through
        // `ensure_checkout`'s own (also fsck-guarded) clone. The malformed
        // commit built below moves `main` only after this call, so it is
        // fetched into an *already-existing* checkout: only the explicit,
        // per-revision `fetch` in `freeze_exact` -- not the initial clone --
        // is what has to refuse it. That isolates this fixture to the one
        // fetch F2 layer (a)'s fsck flag actually guards.
        driver.freeze_exact(&source, &good, "txn-good", &frozen_source_root)?;

        let recipe = hash_object(&origin_repo, b"target = 'test'\n")?;
        let pwn = hash_object(&origin_repo, b"written through a symlink by root\n")?;
        let link = hash_object(&origin_repo, outside.to_string_lossy().as_bytes())?;
        let subtree_output = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&origin_repo)
            .args(["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(format!("100644 blob {pwn}\tpwn\n").as_bytes())?;
                child.wait_with_output()
            })?;
        ensure!(subtree_output.status.success(), "git mktree failed");
        let subtree = String::from_utf8(subtree_output.stdout)?.trim().to_owned();

        let hex = |s: &str| -> Vec<u8> {
            (0..20)
                .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
                .collect()
        };
        let mut raw = Vec::new();
        for (mode, name, sha) in [
            ("120000", "a", &link),
            ("40000", "a", &subtree),
            ("100644", "deployment.toml", &recipe),
        ] {
            raw.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            raw.extend(hex(sha));
        }
        let mut child = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&origin_repo)
            .args(["hash-object", "-t", "tree", "--literally", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(&raw)?;
        let output = child.wait_with_output()?;
        ensure!(output.status.success(), "git hash-object --literally failed");
        let tree = String::from_utf8(output.stdout)?.trim().to_owned();
        // Not `git_at`: the repository is already chowned to the
        // unprivileged identity above, and this process is root, so plain
        // `git` here refuses it as "dubious ownership" without `safe.directory`.
        let git_owned = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .args([
                    "-c", "safe.directory=*",
                    "-c", "user.name=Idunn Test",
                    "-c", "user.email=idunn-test@example.invalid",
                    "-C",
                ])
                .arg(&origin_repo)
                .args(args)
                .output()?;
            ensure!(output.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let commit = git_owned(&["commit-tree", &tree, "-m", "dup"])?;
        git_owned(&["update-ref", "refs/heads/main", &commit])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let result = driver.freeze_exact(&source, &commit, "txn-dup", &frozen_source_root);
        assert!(
            result.is_err(),
            "a tree with a leaf entry aliasing another entry's ancestor must be refused"
        );
        assert!(
            !outside.join("pwn").exists(),
            "the writer must never place content outside the frozen root, even on refusal"
        );
        Ok(())
    }

    /// S7 (Self's ruling, third Cut 1 fix batch): two regular files whose
    /// names differ only by case are two distinct, individually valid Git
    /// blobs. `git fsck` accepts them, the host is case-sensitive ext4, and
    /// the writer places both side by side without ever colliding. The
    /// case-insensitive refusal this used to pin is deleted; this fixture now
    /// pins the opposite -- that a mutant reintroducing it is caught.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_accepts_two_paths_that_differ_only_by_case() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        fs::write(origin_repo.join("README.txt"), b"upper\n")?;
        fs::write(origin_repo.join("readme.txt"), b"lower\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let (frozen_root, ..) = driver.freeze_exact(&source, &revision, "txn-case", &frozen_source_root)?;
        assert!(
            frozen_root.join("README.txt").is_file() && frozen_root.join("readme.txt").is_file(),
            "two paths that differ only by case must both freeze, side by side, on a case-sensitive host"
        );
        Ok(())
    }

    /// F3 (Self's ruling, second Cut 1 fix batch), isolated from every layer
    /// above it: `ensure_frozen_directory` is the writer's own primitive, and
    /// it must refuse a path that already has *something* at it -- even a
    /// plain pre-existing directory this call did not itself create --
    /// rather than reuse it. Calling it directly, with a pre-seeded root a
    /// real freeze would never produce, is what proves the writer itself
    /// carries this invariant on its own, rather than relying on `git fsck`
    /// upstream or a write-order accident downstream to prevent the same
    /// outcome.
    #[cfg(unix)]
    #[test]
    fn ensure_frozen_directory_refuses_an_existing_entry_it_did_not_create() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;

        // A directory nobody's `created_dirs` bookkeeping knows about.
        let foreign_dir = root.join("foreign");
        fs::create_dir(&foreign_dir)?;
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(root.clone());
        assert!(
            ensure_frozen_directory(&root, &mut created_dirs, &foreign_dir).is_err(),
            "a pre-existing directory this call did not create must be refused, not reused"
        );

        // A symlink standing where a directory is wanted.
        let link_dir = root.join("link");
        symlink("/tmp", &link_dir)?;
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(root.clone());
        assert!(
            ensure_frozen_directory(&root, &mut created_dirs, &link_dir).is_err(),
            "a symlink standing where a directory is wanted must be refused"
        );

        // The positive case: an absent path is created and remembered.
        let fresh_dir = root.join("fresh");
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(root.clone());
        ensure_frozen_directory(&root, &mut created_dirs, &fresh_dir)?;
        assert!(fresh_dir.is_dir());
        assert!(created_dirs.contains(&fresh_dir));
        // Calling it again for the same, now-tracked path is a no-op.
        ensure_frozen_directory(&root, &mut created_dirs, &fresh_dir)?;
        Ok(())
    }

    /// F4 (Self's ruling, second Cut 1 fix batch): `freeze_exact` refuses a
    /// symlink that escapes the frozen root, whether the target is absolute
    /// or a relative `..` walk, before the tree is ever published. This
    /// exercises the guard through the public `freeze_exact` entry point
    /// (not the `harden_frozen_source` unit alone), so removing the call
    /// inside `freeze_exact` -- not just the function -- is what this pins.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_refuses_an_absolute_or_escaping_symlink() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for (label, make_link) in [
            (
                "absolute",
                Box::new(|repo: &Path| symlink("/etc/passwd", repo.join("l"))) as Box<dyn Fn(&Path) -> std::io::Result<()>>,
            ),
            (
                "escaping",
                Box::new(|repo: &Path| {
                    fs::create_dir(repo.join("d"))?;
                    symlink("../../../outside-the-root", repo.join("d/l"))
                }),
            ),
        ] {
            let temp = tempfile::tempdir()?;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
            let origin_repo = temp.path().join("origin");
            fs::create_dir(&origin_repo)?;
            git_at(&origin_repo, &["init", "--initial-branch=main"])?;
            git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
            git_at(
                &origin_repo,
                &["config", "user.email", "idunn-test@example.invalid"],
            )?;
            fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
            make_link(&origin_repo)?;
            git_at(&origin_repo, &["add", "--all"])?;
            git_at(&origin_repo, &["commit", "-m", "fixture"])?;
            let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
            ensure!(
                Command::new("/bin/chown")
                    .args(["-R", "1000:1000"])
                    .arg(&origin_repo)
                    .status()?
                    .success(),
                "chowning the fixture origin repository"
            );

            let source_cache_root = temp.path().join("source-cache");
            let frozen_source_root = temp.path().join("frozen-source");
            fs::create_dir(&frozen_source_root)?;
            fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
            let identity = ProcessIdentity {
                uid: 1000,
                gid: 1000,
            };
            let driver = GitSourceDriver::new(
                source_cache_root.clone(),
                frozen_source_root.clone(),
                Some(identity),
            );
            let source = ExactSource {
                origin: origin_repo.to_string_lossy().into_owned(),
                checkout: source_cache_root.join("checkout"),
                gitlinks: BTreeMap::new(),
                recipe_path: PathBuf::from("deployment.toml"),
            };
            let result = driver.freeze_exact(&source, &revision, "txn-escape", &frozen_source_root);
            assert!(result.is_err(), "{label} symlink must be refused");
        }
        Ok(())
    }

    /// F5 (Self's ruling, second Cut 1 fix batch): a lexical `..`-count is
    /// not enough once the chain passes back through another symlink. `D`
    /// points at `.` (itself), and `L` walks through a dozen `D` hops before
    /// finally leaving with `..` -- lexically that undercounts how far the
    /// chain really goes, because each `D` the OS resolves is a fresh copy
    /// of the root, not one ordinary path component. `L` actually resolves
    /// to `/etc/passwd`.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_refuses_a_symlink_chain_that_escapes_through_a_self_referential_directory() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        symlink(".", origin_repo.join("D"))?;
        let chain = "D/D/D/D/D/D/D/D/D/D/D/D/../../../../../../../../../../../../etc/passwd";
        symlink(chain, origin_repo.join("L"))?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let result = driver.freeze_exact(&source, &revision, "txn-chain", &frozen_source_root);
        assert!(
            result.is_err(),
            "a symlink chain that resolves outside the root through a self-referential directory must be refused"
        );
        Ok(())
    }

    /// F2 (Self's ruling, Cut 1 fix batch): `freeze_exact` writes blobs raw
    /// from the object store, with no attribute-driven transform, across
    /// every transform `.gitattributes` can name: `eol=crlf`, `export-ignore`,
    /// `export-subst`, `ident`, a symlink, the executable bit, and a Gitlink.
    /// Every frozen file's bytes are asserted equal to `git cat-file blob` at
    /// the selected revision — the same check the recipe already gets, now
    /// over the whole tree.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_is_byte_exact_across_every_attribute_transform() -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let sub_repo = temp.path().join("sub");
        fs::create_dir(&sub_repo)?;
        git_at(&sub_repo, &["init", "--initial-branch=main"])?;
        git_at(&sub_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(&sub_repo, &["config", "user.email", "idunn-test@example.invalid"])?;
        fs::write(sub_repo.join("lib.txt"), b"sub content\n")?;
        git_at(&sub_repo, &["add", "--all"])?;
        git_at(&sub_repo, &["commit", "-m", "sub fixture"])?;
        let sub_revision = git_at(&sub_repo, &["rev-parse", "HEAD"])?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(
            origin_repo.join(".gitattributes"),
            b"crlf.txt eol=crlf\nignored.txt export-ignore\nsubst.txt export-subst\nident.txt ident\n",
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        fs::write(origin_repo.join("crlf.txt"), b"a\nb\n")?;
        fs::write(origin_repo.join("ignored.txt"), b"ignored\n")?;
        fs::write(origin_repo.join("subst.txt"), b"rev $Format:%H$\n")?;
        fs::write(origin_repo.join("ident.txt"), b"$Id$\n")?;
        let script = origin_repo.join("run.sh");
        fs::write(&script, b"#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        symlink("deployment.toml", origin_repo.join("link"))?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(
            &origin_repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{sub_revision},vendor/sub"),
            ],
        )?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        for repo in [&origin_repo, &sub_repo] {
            ensure!(
                Command::new("/bin/chown")
                    .args(["-R", "1000:1000"])
                    .arg(repo)
                    .status()?
                    .success(),
                "chowning the fixture repository"
            );
        }

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::from([(
                PathBuf::from("vendor/sub"),
                GitlinkBinding {
                    origin: sub_repo.to_string_lossy().into_owned(),
                },
            )]),
            recipe_path: PathBuf::from("deployment.toml"),
        };

        let (tree_root, _snapshot_sha256, _recipe_bytes, contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-byte-exact", &frozen_source_root)?;
        assert!(!contains_lfs_pointers);

        for name in [
            "deployment.toml",
            "crlf.txt",
            "ignored.txt",
            "subst.txt",
            "ident.txt",
            "run.sh",
            ".gitattributes",
        ] {
            let output = Command::new("/usr/bin/git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(&origin_repo)
                .args(["cat-file", "blob", &format!("{revision}:{name}")])
                .env_clear()
                .env("HOME", &origin_repo)
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()?;
            ensure!(
                output.status.success(),
                "reading blob {name} for comparison: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let frozen = fs::read(tree_root.join(name))
                .with_context(|| format!("reading frozen {name}"))?;
            assert_eq!(frozen, output.stdout, "{name} is not byte-exact");
        }
        let script_metadata = fs::symlink_metadata(tree_root.join("run.sh"))?;
        assert_eq!(script_metadata.uid(), 0);
        assert_eq!(script_metadata.permissions().mode() & 0o111, 0o111);
        assert_eq!(
            fs::read_link(tree_root.join("link"))?,
            PathBuf::from("deployment.toml")
        );
        let gitlink_output = Command::new("/usr/bin/git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&sub_repo)
            .args(["cat-file", "blob", &format!("{sub_revision}:lib.txt")])
            .env_clear()
            .env("HOME", &sub_repo)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()?;
        ensure!(
            gitlink_output.status.success(),
            "reading Gitlink blob for comparison: {}",
            String::from_utf8_lossy(&gitlink_output.stderr)
        );
        assert_eq!(
            fs::read(tree_root.join("vendor/sub/lib.txt"))?,
            gitlink_output.stdout
        );
        Ok(())
    }

    /// The deploy-equivalence twin F2's ruling asked for: for a fixture with
    /// only a `text eol=lf` attribute (Eve's real `packages/*/dist/**`
    /// pattern) and no attribute that would actually transform bytes, the new
    /// raw freeze's tree equals today's `git archive` tree exactly, so this
    /// cut does not change deploy for a target like it. The second file in
    /// the same fixture carries `eol=crlf` over LF-stored content, an
    /// attribute that genuinely does transform on archive; the test asserts
    /// the two trees diverge there, so the equivalence check has teeth rather
    /// than trivially passing.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_matches_archive_when_untransformed_and_diverges_when_transformed() -> Result<()>
    {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;

        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(
            origin_repo.join(".gitattributes"),
            b"untransformed.txt text eol=lf\ntransformed.txt eol=crlf\n",
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        // Eve's actual shape: stored LF, `eol=lf` is a no-op transform.
        fs::write(origin_repo.join("untransformed.txt"), b"a\nb\n")?;
        // Stored LF, but `eol=crlf` genuinely rewrites it on checkout/archive.
        fs::write(origin_repo.join("transformed.txt"), b"a\nb\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: origin_repo.to_string_lossy().into_owned(),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let (tree_root, _snapshot_sha256, _recipe_bytes, _contains_lfs_pointers) =
            driver.freeze_exact(&source, &revision, "txn-equivalence", &frozen_source_root)?;

        // Reconstruct today's `git archive` tree independently of production
        // code, so this test proves equivalence against the actual old
        // mechanism rather than against another copy of the new one.
        let archived = temp.path().join("archived");
        fs::create_dir(&archived)?;
        let mut archive = Command::new("/usr/bin/git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&origin_repo)
            .args(["archive", "--format=tar", &revision])
            .env_clear()
            .env("HOME", &origin_repo)
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let archive_stdout = archive.stdout.take().context("git archive has no stdout")?;
        let extractor_status = Command::new("/bin/tar")
            .args(["--extract", "--file=-", "--directory"])
            .arg(&archived)
            .arg("--no-same-owner")
            .stdin(Stdio::from(archive_stdout))
            .status()?;
        let archive_output = archive.wait_with_output()?;
        ensure!(
            archive_output.status.success(),
            "git archive failed reconstructing the fixture tree: {}",
            String::from_utf8_lossy(&archive_output.stderr)
        );
        ensure!(
            extractor_status.success(),
            "reconstructing the archived fixture tree failed"
        );

        assert_eq!(
            fs::read(tree_root.join("untransformed.txt"))?,
            fs::read(archived.join("untransformed.txt"))?,
            "an eol=lf file over already-LF content must match today's archive exactly"
        );
        assert_ne!(
            fs::read(tree_root.join("transformed.txt"))?,
            fs::read(archived.join("transformed.txt"))?,
            "an eol=crlf file genuinely transforms on archive, so raw and archived bytes must diverge"
        );
        Ok(())
    }

    fn expected() -> IdunnExpectedIncarnationRecord {
        IdunnExpectedIncarnationRecord {
            schema_version: IDUNN_EXPECTED_INCARNATION_SCHEMA.into(),
            target: "service".into(),
            plan_id: digest('1'),
            incarnation_id: "incarnation-1".into(),
            sealed_release_id: digest('2'),
            source_repository: "github.com/GameCult/Service".into(),
            source_revision: "3".repeat(40),
            recipe_sha256: digest('4'),
            runtime_id: "service-runtime".into(),
            expected_signer_identity_id: "service-signer".into(),
            health_contract: "service.health.v1".into(),
            artifact_sha256: digest('5'),
            state_schema_generation: Some("state-v1".into()),
            state_contract_sha256: Some(digest('6')),
            write_lease_required: true,
            route: None,
            capabilities: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn authenticated_warming(
        root: &Path,
    ) -> Result<(
        IdunnExpectedIncarnationRecord,
        IdunnRuntimeActivationRecord,
        SequenceAdmittedWarming,
        ServiceIdentityTrustAnchor,
    )> {
        let provider = enroll_service_identity_at::<GameCultProviderHealthIdentity>(
            &root.join("provider.cc"),
        )?;
        let idunn = enroll_service_identity_at::<IdunnServiceIdentity>(&root.join("idunn.cc"))?;
        let odin = enroll_service_identity_at::<OdinTopologyIdentity>(&root.join("odin.cc"))?;
        let mut expected = expected();
        expected.expected_signer_identity_id = provider.entry().identity_id.clone();
        let launch = IdunnRuntimeActivationLaunch::issue(&expected, digest('7'), 100, &idunn)?;
        let activation = launch.activation().clone();
        launch.write_credential(std::io::sink())?;
        let authority = verify_runtime_authority(
            &expected,
            &activation,
            &idunn.trust_anchor()?,
            &provider.entry().public_key,
        )?;
        let mut warming = OdinRuntimeTopologyCorrelationRecord {
            schema_version: ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into(),
            target: expected.target.clone(),
            expected_projection_sha256: expected.canonical_sha256()?,
            expected: true,
            current_activation_sha256: Some(activation.canonical_sha256()?),
            signed_presence_sha256: Some(digest('9')),
            observed_presence_state: Some("warming".into()),
            observed_presence_publisher_sequence: Some(1),
            observed_write_lease_sha256: None,
            observed_capabilities: Vec::new(),
            runtime_id: expected.runtime_id.clone(),
            runtime_instance_id: Some(activation.runtime_instance_id.clone()),
            present: true,
            ready: false,
            dependencies: Vec::new(),
            disagreements: Vec::new(),
            signer_identity_id: odin.entry().identity_id.clone(),
            publisher_sequence: 1,
            observed_at_unix_millis: 110,
            signature_algorithm: "ed25519".into(),
            signature: Vec::new(),
        };
        warming.signature = odin
            .sign::<OdinRuntimeTopologyCorrelationPurpose>(&warming.unsigned_signature_payload()?)
            .signature;
        let warming = authenticate_odin_runtime_topology_correlation(
            &warming.canonical_bytes()?,
            &authority,
            None,
            &odin.entry().public_key,
            OdinTopologyAuthenticationContext {
                trusted_received_at_unix_millis: 120,
                maximum_age_millis: 30,
                maximum_future_skew_millis: 5,
            },
        )?;
        let warming = SequenceAdmittedWarming::for_test("test-transaction", warming, 120)?;
        Ok((expected, activation, warming, provider.trust_anchor()?))
    }

    fn lease(
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        warming: &SequenceAdmittedWarming,
    ) -> IdunnProcessWriteLeaseRecord {
        IdunnProcessWriteLeaseRecord {
            schema_version: IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into(),
            target: expected.target.clone(),
            expected_projection_sha256: expected.canonical_sha256().unwrap(),
            plan_id: expected.plan_id.clone(),
            incarnation_id: expected.incarnation_id.clone(),
            sealed_release_id: expected.sealed_release_id.clone(),
            activation_witness_sha256: activation.canonical_sha256().unwrap(),
            state_schema_generation: expected.state_schema_generation.clone().unwrap(),
            state_contract_sha256: expected.state_contract_sha256.clone().unwrap(),
            runtime_id: expected.runtime_id.clone(),
            runtime_instance_id: activation.runtime_instance_id.clone(),
            warming_presence_sha256: warming.signed_presence_sha256().to_owned(),
            lease_epoch: 1,
            issued_at_unix_millis: 200,
        }
    }

    fn topology_driver(root: &Path, name: &str) -> CultCacheTopologyDriver {
        CultCacheTopologyDriver {
            projection_store: root.join(format!("{name}.cc")),
            correlation_store: root.join(format!("{name}-correlation.cc")),
        }
    }

    fn project_activation(
        driver: &CultCacheTopologyDriver,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "test activation belongs to another Expected"
        );
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: incarnation_key(expected)?,
                r#type: IdunnRuntimeActivationRecord::TYPE.into(),
                payload: activation.canonical_bytes()?,
                stored_at: rfc3339_millis(activation.issued_at_unix_millis)?,
                schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
            },
        )
    }

    /// Withdrawing the last membership withdraws the endpoint's allow, and a
    /// rule that is already gone is the state being asked for, not an error.
    #[cfg(unix)]
    #[test]
    fn withdrawing_the_last_route_membership_withdraws_its_firewall_allow() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let program = |name: &str, body: &str| -> Result<PathBuf> {
            let path = temp.path().join(name);
            fs::write(&path, body)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            Ok(path)
        };
        let ufw = program(
            "ufw",
            "#!/bin/sh\nif [ \"$1\" = \"delete\" ] && [ -e \"$0.absent\" ]; then echo 'Could not delete non-existent rule' >&2; exit 1; fi\necho \"$*\" >> \"$0.calls\"\nexit 0\n",
        )?;
        let driver = NginxRouteDriver {
            binding: RouteBinding {
                driver: RouteDriver::NginxStreamUdp,
                route_id: "odin-rendezvous".into(),
                stable_endpoint: "rudp://10.77.0.1:17971".into(),
                private_host: "127.0.0.1".into(),
                private_port_start: 17972,
                private_port_end: 17979,
                config_path: temp.path().join("odin.conf"),
                reload_unit: "nginx.service".into(),
            },
            nginx_program: program("nginx", "#!/bin/sh\nexit 0\n")?,
            systemd_run_program: program("systemd-run", "#!/bin/sh\nexit 64\n")?,
            systemctl_program: program("systemctl", "#!/bin/sh\nexit 0\n")?,
            ufw_program: ufw.clone(),
            preflight_root: temp.path().join("preflight"),
        };
        assert_eq!(
            driver
                .endpoint_rule()?
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            "allow in to 10.77.0.1 port 17971 proto udp"
        );

        driver.restore(None)?;
        assert_eq!(
            fs::read_to_string(ufw.with_extension("calls"))?,
            "delete allow in to 10.77.0.1 port 17971 proto udp\n"
        );
        fs::write(ufw.with_extension("absent"), b"gone\n")?;
        driver.restore(None)?;
        Ok(())
    }

    #[test]
    fn an_empty_route_fragment_is_absence_not_a_membership() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("odin.conf");
        let driver = NginxRouteDriver::new(RouteBinding {
            driver: RouteDriver::NginxStreamUdp,
            route_id: "odin-rudp".into(),
            stable_endpoint: "rudp://127.0.0.1:17872".into(),
            private_host: "127.0.0.1".into(),
            private_port_start: 27872,
            private_port_end: 27879,
            config_path: config_path.clone(),
            reload_unit: "nginx.service".into(),
        });
        assert_eq!(driver.current_configuration()?, None);
        // What systemd leaves behind when it materializes the preflight's bind
        // mount point on a target's first deployment.
        std::fs::write(&config_path, b"")?;
        assert_eq!(
            driver.current_configuration()?,
            None,
            "an empty fragment must not read as an unadmitted incumbent"
        );
        std::fs::write(&config_path, b"server {}")?;
        assert_eq!(driver.current_configuration()?, Some(b"server {}".to_vec()));
        Ok(())
    }

    #[test]
    fn nginx_rudp_route_is_rendered_as_a_udp_stream_proxy() -> Result<()> {
        let driver = NginxRouteDriver::new(RouteBinding {
            driver: RouteDriver::NginxStreamUdp,
            route_id: "odin-rudp".into(),
            stable_endpoint: "rudp://127.0.0.1:17872".into(),
            private_host: "127.0.0.1".into(),
            private_port_start: 27872,
            private_port_end: 27879,
            config_path: "/etc/nginx/idunn-stream-routes/odin-rudp.conf".into(),
            reload_unit: "nginx.service".into(),
        });
        let mut candidate = expected();
        candidate.route = Some(cultnet_rs::IdunnExpectedRoute {
            route_id: "odin-rudp".into(),
            transport: "rudp".into(),
            stable_endpoint: "rudp://127.0.0.1:17872".into(),
            candidate_endpoint: "rudp://127.0.0.1:27872".into(),
        });
        let rendered = String::from_utf8(driver.render(&candidate)?)?;
        assert!(rendered.contains("listen 127.0.0.1:17872 udp reuseport;"));
        assert!(rendered.contains("server 127.0.0.1:27872;"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn nginx_preflight_binds_exact_incumbent_configuration_without_claiming_live_route()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        let config = temp.path().join("route.conf");
        let nginx = temp.path().join("nginx");
        let systemd_run = temp.path().join("systemd-run");
        let systemctl = temp.path().join("systemctl");
        fs::write(
            &nginx,
            format!(
                "#!/bin/sh\nconfig='{}'\nif [ \"$1\" = \"-t\" ]; then\n  candidate=${{IDUNN_TEST_SHADOW:-$config}}\n  /bin/grep -q 'server 127.0.0.1:4104' \"$candidate\"\n  exit $?\nfi\nexit 64\n",
                config.display(),
            ),
        )?;
        fs::write(
            &systemd_run,
            "#!/bin/sh\nshadow=\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --property=BindReadOnlyPaths=*) binding=${1#--property=BindReadOnlyPaths=}; shadow=${binding%%:*} ;;
    --) shift; break ;;
  esac\n  shift\ndone\nIDUNN_TEST_SHADOW=\"$shadow\"; export IDUNN_TEST_SHADOW\nexec \"$@\"\n",
        )?;
        fs::write(
            &systemctl,
            "#!/bin/sh\nif [ -e \"$0.fail\" ]; then exit 65; fi\nif [ \"$1\" = \"reload\" ]; then echo reload >> \"$0.calls\"; exit 0; fi\nexit 64\n",
        )?;
        fs::set_permissions(&nginx, fs::Permissions::from_mode(0o755))?;
        fs::set_permissions(&systemd_run, fs::Permissions::from_mode(0o755))?;
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755))?;
        let ufw = temp.path().join("ufw");
        fs::write(
            &ufw,
            "#!/bin/sh\nif [ \"$1\" = \"delete\" ] && [ -e \"$0.absent\" ]; then echo 'Could not delete non-existent rule' >&2; exit 1; fi\necho \"$*\" >> \"$0.calls\"\nexit 0\n",
        )?;
        fs::set_permissions(&ufw, fs::Permissions::from_mode(0o755))?;

        let binding = RouteBinding {
            driver: RouteDriver::NginxStreamTcp,
            route_id: "service-route".into(),
            stable_endpoint: "http://127.0.0.1:4103".into(),
            private_host: "127.0.0.1".into(),
            private_port_start: 4104,
            private_port_end: 4109,
            config_path: config.clone(),
            reload_unit: "nginx.service".into(),
        };
        let driver = NginxRouteDriver {
            binding,
            nginx_program: nginx,
            systemd_run_program: systemd_run,
            systemctl_program: systemctl.clone(),
            ufw_program: ufw.clone(),
            preflight_root: temp.path().join("preflight"),
        };
        let mut candidate = expected();
        candidate.route = Some(cultnet_rs::IdunnExpectedRoute {
            route_id: "service-route".into(),
            transport: "http".into(),
            stable_endpoint: "http://127.0.0.1:4103".into(),
            candidate_endpoint: "http://127.0.0.1:4104".into(),
        });
        let preflight = driver.preflight(&candidate, &digest('a'), None)?;
        assert!(!config.exists());
        assert_eq!(fs::read_dir(&driver.preflight_root)?.count(), 0);

        let candidate_bytes = driver.render(&candidate)?;
        fs::write(&config, &candidate_bytes)?;
        let membership_sha256 = driver.install(&candidate, &digest('a'), &preflight, true)?;
        assert_eq!(
            fs::read_to_string(systemctl.with_extension("calls"))?,
            "reload\n"
        );
        // The stable endpoint's allow is admitted with the fragment, before
        // the reload that makes the listener live.
        assert_eq!(
            fs::read_to_string(ufw.with_extension("calls"))?,
            "allow in to 127.0.0.1 port 4103 proto tcp comment Idunn route service-route\n"
        );
        assert!(driver.observe_membership(&candidate, &membership_sha256)?);
        let admitted_bytes = fs::read(&config)?;
        assert!(std::str::from_utf8(&admitted_bytes)?.contains("listen 127.0.0.1:4103;"));
        let admitted = RouteObservation {
            route_id: "service-route".into(),
            runtime_instance_id: digest('a'),
            membership_sha256,
            signed_presence_sha256: digest('c'),
            observed_at_unix_millis: 1,
        };

        let next_preflight = driver.preflight(&candidate, &digest('b'), Some(&admitted))?;
        assert_eq!(fs::read(&config)?, admitted_bytes);
        fs::write(systemctl.with_extension("fail"), b"fail\n")?;
        assert!(
            driver
                .install(&candidate, &digest('b'), &next_preflight, false)
                .is_err()
        );
        assert_eq!(fs::read(&config)?, admitted_bytes);
        fs::remove_file(systemctl.with_extension("fail"))?;
        driver.install(&candidate, &digest('b'), &next_preflight, false)?;
        fs::write(&config, b"foreign route\n")?;
        assert!(
            driver
                .install(&candidate, &digest('b'), &next_preflight, true)
                .is_err()
        );
        driver.restore_admitted_membership(&candidate, &admitted.membership_sha256)?;
        assert_eq!(fs::read(&config)?, admitted_bytes);
        assert!(driver.observe_membership(&candidate, &admitted.membership_sha256)?);

        fs::write(&config, b"foreign route after admission\n")?;
        fs::write(systemctl.with_extension("fail"), b"fail\n")?;
        assert!(
            driver
                .restore_admitted_membership(&candidate, &admitted.membership_sha256)
                .is_err()
        );
        assert!(!config.exists());
        fs::remove_file(systemctl.with_extension("fail"))?;
        driver.restore_admitted_membership(&candidate, &admitted.membership_sha256)?;
        assert_eq!(fs::read(&config)?, admitted_bytes);
        Ok(())
    }

    #[test]
    fn process_environment_witness_is_sorted_exact_and_secret_free() -> Result<()> {
        let entries = vec![
            b"B=two".to_vec(),
            b"A=one".to_vec(),
            b"UNRELATED=ignored".to_vec(),
        ];
        let selected = select_process_environment(&entries, &["A".into(), "B".into()])?;
        assert_eq!(selected.get("A").map(String::as_str), Some("one"));
        assert!(!selected.contains_key("UNRELATED"));
        assert!(select_process_environment(&entries, &["B".into(), "A".into()]).is_err());
        Ok(())
    }

    #[test]
    fn topology_transport_returns_opaque_odin_bytes_without_deciding_ready() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let correlation_store = temp.path().join("correlation.cc");
        let opaque = b"not a topology receipt".to_vec();
        upsert_record(
            &correlation_store,
            CultCacheEnvelope {
                key: incarnation_key_of("service", &digest('1')),
                r#type: OdinRuntimeTopologyCorrelationRecord::TYPE.into(),
                payload: opaque.clone(),
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into()),
            },
        )?;
        let driver = CultCacheTopologyDriver {
            projection_store: temp.path().join("projection.cc"),
            correlation_store,
        };

        assert_eq!(driver.receive("service", &digest('2'))?, None);
        assert_eq!(
            driver.receive("service", &digest('1'))?,
            Some(ReceivedOdinTopologyCorrelation {
                target: "service".into(),
                expected_sha256: digest('1'),
                canonical_bytes: opaque,
            })
        );
        Ok(())
    }

    #[test]
    fn expected_withdrawal_is_exact_atomic_and_idempotent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (expected, activation, warming, provider_anchor) = authenticated_warming(temp.path())?;
        let lease = lease(&expected, &activation, &warming);
        let driver = topology_driver(temp.path(), "projection");
        driver.publish_expected(&expected, &provider_anchor)?;
        let published = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        assert_eq!(published.len(), 2);
        assert!(published.iter().any(|entry| {
            entry.key == incarnation_key(&expected).unwrap()
                && entry.r#type == IdunnExpectedIncarnationRecord::TYPE
        }));
        let published_anchor = published
            .iter()
            .find(|entry| entry.r#type == GameCultServiceTrustAnchorRecord::TYPE)
            .context("atomic Expected set has no provider trust anchor")?;
        assert_eq!(
            service_trust_anchor_from_envelope(published_anchor)?,
            runtime_presence_trust_anchor(&expected, &provider_anchor)?
        );
        project_activation(&driver, &expected, &activation)?;
        driver.publish_process_write_lease(&expected, &activation, &lease)?;

        let mut substituted_expected = expected.clone();
        substituted_expected.incarnation_id = "another-incarnation".into();
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: incarnation_key(&expected)?,
                r#type: IdunnExpectedIncarnationRecord::TYPE.into(),
                payload: substituted_expected.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_EXPECTED_INCARNATION_SCHEMA.into()),
            },
        )?;
        assert!(
            driver
                .withdraw_incarnation(&expected, &provider_anchor, Some(&activation), Some(&lease))
                .is_err()
        );
        driver.publish_expected(&expected, &provider_anchor)?;

        let mut substituted_anchor = runtime_presence_trust_anchor(&expected, &provider_anchor)?;
        substituted_anchor.runtime_id = "another-runtime".into();
        substituted_anchor.validate()?;
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: substituted_anchor.trust_anchor_id.clone(),
                r#type: GameCultServiceTrustAnchorRecord::TYPE.into(),
                payload: rmp_serde::to_vec(&substituted_anchor)?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA.into()),
            },
        )?;
        assert!(
            driver
                .withdraw_incarnation(&expected, &provider_anchor, Some(&activation), Some(&lease))
                .is_err()
        );
        driver.publish_expected(&expected, &provider_anchor)?;

        let mut substituted_activation = activation.clone();
        substituted_activation.expected_projection_sha256 = digest('a');
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: incarnation_key(&expected)?,
                r#type: IdunnRuntimeActivationRecord::TYPE.into(),
                payload: substituted_activation.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
            },
        )?;
        assert!(
            driver
                .withdraw_incarnation(&expected, &provider_anchor, Some(&activation), None)
                .is_err()
        );
        project_activation(&driver, &expected, &activation)?;

        let mut substituted_lease = lease.clone();
        substituted_lease.lease_epoch += 1;
        assert!(
            driver
                .publish_process_write_lease(&expected, &activation, &substituted_lease)
                .is_err()
        );
        assert!(
            driver
                .withdraw_incarnation(
                    &expected,
                    &provider_anchor,
                    Some(&activation),
                    Some(&substituted_lease),
                )
                .is_err()
        );

        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: "unrelated".into(),
                r#type: "test.unrelated".into(),
                payload: vec![0x90],
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some("test.unrelated.v1".into()),
            },
        )?;
        driver.withdraw_incarnation(
            &expected,
            &provider_anchor,
            Some(&activation),
            Some(&lease),
        )?;
        let remaining = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, "unrelated");
        driver.withdraw_incarnation(
            &expected,
            &provider_anchor,
            Some(&activation),
            Some(&lease),
        )?;
        Ok(())
    }

    #[test]
    fn incumbent_projection_restoration_demotes_to_expected_only_and_rejects_unknown_authority()
    -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (incumbent, activation, warming, provider_anchor) = authenticated_warming(temp.path())?;
        let lease = lease(&incumbent, &activation, &warming);
        let driver = topology_driver(temp.path(), "restoration");
        driver.publish_expected(&incumbent, &provider_anchor)?;
        project_activation(&driver, &incumbent, &activation)?;
        driver.publish_process_write_lease(&incumbent, &activation, &lease)?;
        let incumbent_key = incarnation_key(&incumbent)?;

        let mut candidate = incumbent.clone();
        candidate.plan_id = digest('a');
        candidate.incarnation_id = "candidate-incarnation".into();
        candidate.sealed_release_id = digest('b');
        candidate.artifact_sha256 = digest('c');
        let mut candidate_activation = activation.clone();
        candidate_activation.expected_projection_sha256 = candidate.canonical_sha256()?;
        candidate_activation.runtime_instance_id = digest('d');
        let candidate_key = incarnation_key(&candidate)?;

        // Sealing the candidate beside the incumbent changes nothing the
        // incumbent owns: its Expected, activation and lease are still there
        // under its own key, byte for byte.
        let before_candidate = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        driver.publish_expected(&candidate, &provider_anchor)?;
        project_activation(&driver, &candidate, &candidate_activation)?;
        let with_candidate = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        for envelope in &before_candidate {
            if envelope.key == incumbent_key {
                assert!(with_candidate.contains(envelope));
            }
        }
        assert!(driver.admitted_runtime_projection_is_exact(
            &incumbent,
            &provider_anchor,
            &activation,
            Some(&lease),
        )?);
        assert_eq!(
            with_candidate
                .iter()
                .filter(|entry| entry.r#type == IdunnExpectedIncarnationRecord::TYPE)
                .count(),
            2
        );

        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: "unrelated".into(),
                r#type: "test.unrelated".into(),
                payload: vec![0x90],
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some("test.unrelated.v1".into()),
            },
        )?;

        // A pre-fencing abort withdraws the candidate and only the candidate.
        driver.withdraw_incarnation(
            &candidate,
            &provider_anchor,
            Some(&candidate_activation),
            None,
        )?;
        let after_withdrawal = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        assert!(
            after_withdrawal
                .iter()
                .all(|entry| entry.key != candidate_key)
        );
        assert!(driver.admitted_runtime_projection_is_exact(
            &incumbent,
            &provider_anchor,
            &activation,
            Some(&lease),
        )?);
        assert!(
            after_withdrawal
                .iter()
                .any(|entry| entry.r#type == GameCultServiceTrustAnchorRecord::TYPE)
        );

        // Demotion is for an incumbent whose process is gone: activation and
        // lease go, Expected and anchor stay.
        assert_eq!(
            driver.demote_to_expected_only(
                &incumbent,
                &provider_anchor,
                &activation,
                Some(&lease),
            )?,
            incumbent.canonical_sha256()?
        );
        let restored = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        assert_eq!(restored.len(), 3);
        assert_eq!(
            IdunnExpectedIncarnationRecord::decode_canonical(
                &restored
                    .iter()
                    .find(|entry| entry.r#type == IdunnExpectedIncarnationRecord::TYPE)
                    .unwrap()
                    .payload,
            )?,
            incumbent
        );
        assert_eq!(
            service_trust_anchor_from_envelope(
                restored
                    .iter()
                    .find(|entry| entry.r#type == GameCultServiceTrustAnchorRecord::TYPE)
                    .unwrap(),
            )?,
            runtime_presence_trust_anchor(&incumbent, &provider_anchor)?
        );
        assert!(
            restored
                .iter()
                .all(|entry| entry.r#type != IdunnRuntimeActivationRecord::TYPE
                    && entry.r#type != IdunnProcessWriteLeaseRecord::TYPE)
        );
        assert!(
            restored
                .iter()
                .any(|entry| entry.key == "unrelated" && entry.r#type == "test.unrelated")
        );
        assert!(!driver.projected_activation_is_present(&incumbent)?);
        let idempotent_bytes = fs::read(&driver.projection_store)?;
        driver.demote_to_expected_only(&incumbent, &provider_anchor, &activation, Some(&lease))?;
        assert_eq!(fs::read(&driver.projection_store)?, idempotent_bytes);

        // A record under the incumbent's key that is not the incumbent's is a
        // substitution; demotion refuses it and leaves the store as found.
        let admitted_anchor = runtime_presence_trust_anchor(&incumbent, &provider_anchor)?;
        let mut unknown_expected = incumbent.clone();
        unknown_expected.incarnation_id = "unknown-incarnation".into();
        let mut unknown_anchor = admitted_anchor.clone();
        unknown_anchor.runtime_id = "unknown-runtime".into();
        unknown_anchor.validate()?;
        let mut unknown_activation = activation.clone();
        unknown_activation.runtime_instance_id = digest('e');
        let mut unknown_lease = lease.clone();
        unknown_lease.lease_epoch += 1;
        let substitutions = [
            CultCacheEnvelope {
                key: incumbent_key.clone(),
                r#type: IdunnExpectedIncarnationRecord::TYPE.into(),
                payload: unknown_expected.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_EXPECTED_INCARNATION_SCHEMA.into()),
            },
            CultCacheEnvelope {
                key: admitted_anchor.trust_anchor_id.clone(),
                r#type: GameCultServiceTrustAnchorRecord::TYPE.into(),
                payload: rmp_serde::to_vec(&unknown_anchor)?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(GAMECULT_SERVICE_TRUST_ANCHOR_SCHEMA.into()),
            },
            CultCacheEnvelope {
                key: incumbent_key.clone(),
                r#type: IdunnRuntimeActivationRecord::TYPE.into(),
                payload: unknown_activation.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
            },
            CultCacheEnvelope {
                key: incumbent_key.clone(),
                r#type: IdunnProcessWriteLeaseRecord::TYPE.into(),
                payload: unknown_lease.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into()),
            },
        ];
        for substitution in substitutions {
            fs::write(&driver.projection_store, &idempotent_bytes)?;
            upsert_record(&driver.projection_store, substitution)?;
            let before = fs::read(&driver.projection_store)?;
            assert!(
                driver
                    .demote_to_expected_only(
                        &incumbent,
                        &provider_anchor,
                        &activation,
                        Some(&lease),
                    )
                    .is_err()
            );
            assert_eq!(fs::read(&driver.projection_store)?, before);
        }
        Ok(())
    }

    /// A replaced incarnation nothing owns is withdrawn whole; the one still
    /// admitted, and the target's anchor, are untouched.
    #[test]
    fn a_stale_incarnation_is_withdrawn_whole_beside_the_admitted_one() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (admitted, activation, warming, provider_anchor) = authenticated_warming(temp.path())?;
        let lease = lease(&admitted, &activation, &warming);
        let driver = topology_driver(temp.path(), "stale");
        let mut stale = admitted.clone();
        stale.plan_id = digest('a');
        stale.incarnation_id = "stale-incarnation".into();
        stale.sealed_release_id = digest('b');
        stale.artifact_sha256 = digest('c');
        let mut stale_activation = activation.clone();
        stale_activation.expected_projection_sha256 = stale.canonical_sha256()?;
        stale_activation.runtime_instance_id = digest('d');
        driver.publish_expected(&stale, &provider_anchor)?;
        project_activation(&driver, &stale, &stale_activation)?;
        driver.publish_expected(&admitted, &provider_anchor)?;
        project_activation(&driver, &admitted, &activation)?;
        driver.publish_process_write_lease(&admitted, &activation, &lease)?;

        let mut projected = driver.projected_incarnations(&admitted.target)?;
        projected.sort_by_key(|expected| expected.incarnation_id.clone());
        assert_eq!(projected.len(), 2);
        assert!(projected.contains(&stale) && projected.contains(&admitted));

        driver.withdraw_stale_incarnation(&stale, &provider_anchor)?;
        assert_eq!(
            driver.projected_incarnations(&admitted.target)?,
            vec![admitted.clone()]
        );
        assert!(driver.admitted_runtime_projection_is_exact(
            &admitted,
            &provider_anchor,
            &activation,
            Some(&lease),
        )?);
        let idempotent = fs::read(&driver.projection_store)?;
        driver.withdraw_stale_incarnation(&stale, &provider_anchor)?;
        assert_eq!(fs::read(&driver.projection_store)?, idempotent);
        Ok(())
    }

    /// The single-slot projection an older Idunn wrote is retired the first
    /// time this one publishes for the target, and never read.
    #[test]
    fn a_legacy_target_keyed_slot_is_retired_on_publish() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (expected, activation, _warming, provider_anchor) = authenticated_warming(temp.path())?;
        let driver = topology_driver(temp.path(), "legacy");
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: expected.target.clone(),
                r#type: IdunnExpectedIncarnationRecord::TYPE.into(),
                payload: expected.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_EXPECTED_INCARNATION_SCHEMA.into()),
            },
        )?;
        upsert_record(
            &driver.projection_store,
            CultCacheEnvelope {
                key: expected.target.clone(),
                r#type: IdunnRuntimeActivationRecord::TYPE.into(),
                payload: activation.canonical_bytes()?,
                stored_at: "2026-09-03T00:00:00Z".into(),
                schema_id: Some(IDUNN_RUNTIME_ACTIVATION_SCHEMA.into()),
            },
        )?;
        assert!(!driver.admitted_expected_projection_is_exact(&expected, &provider_anchor)?);
        assert!(!driver.projected_activation_is_present(&expected)?);
        driver.publish_expected(&expected, &provider_anchor)?;
        let published = SingleFileMessagePackBackingStore::new(&driver.projection_store)
            .pull_all_read_only_snapshot()?;
        assert!(published.iter().all(|entry| entry.key != expected.target));
        assert_eq!(published.len(), 2);
        assert!(driver.admitted_expected_projection_is_exact(&expected, &provider_anchor)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn service_credential_sources_require_one_root_owned_0400_inode() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        let root = temp.path().join("credentials");
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let source = root.join("token");
        fs::write(&source, b"secret")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o400))?;
        let mut sources = BTreeMap::from([("SERVICE_TOKEN".into(), source.clone())]);

        validate_service_credential_sources(&sources)?;

        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        assert!(validate_service_credential_sources(&sources).is_err());
        fs::set_permissions(&source, fs::Permissions::from_mode(0o400))?;

        let hardlink = root.join("token-hardlink");
        fs::hard_link(&source, &hardlink)?;
        assert!(validate_service_credential_sources(&sources).is_err());
        fs::remove_file(&hardlink)?;

        let symlink_path = root.join("token-symlink");
        symlink(&source, &symlink_path)?;
        sources.insert("SERVICE_TOKEN".into(), symlink_path);
        assert!(validate_service_credential_sources(&sources).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn write_lease_revoke_is_exact_and_never_deletes_a_surprise() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("lease.cc");
        let driver = CultCacheWriteLeaseDriver::new("service", &path);
        let (expected, activation, warming, provider_anchor) = authenticated_warming(temp.path())?;
        let lease = lease(&expected, &activation, &warming);
        let topology = topology_driver(temp.path(), "lease-projection");
        topology.publish_expected(&expected, &provider_anchor)?;
        project_activation(&topology, &expected, &activation)?;

        let lease_sha256 = driver.grant(&expected, &activation, &warming, &lease)?;
        assert_eq!(lease_sha256, lease.canonical_sha256()?);
        assert!(driver.observe(&expected, &activation, &warming, &lease)?);
        assert!(
            !SingleFileMessagePackBackingStore::new(&topology.projection_store)
                .pull_all_read_only_snapshot()?
                .iter()
                .any(|entry| entry.r#type == IdunnProcessWriteLeaseRecord::TYPE)
        );
        assert_eq!(
            topology.publish_process_write_lease(&expected, &activation, &lease)?,
            lease_sha256
        );

        let mut surprise = lease.clone();
        surprise.lease_epoch = 2;
        assert!(driver.revoke_exact(Some(&surprise)).is_err());
        assert!(driver.observe(&expected, &activation, &warming, &lease)?);

        SingleFileMessagePackBackingStore::new(&path).with_read_only_shared_snapshot(|_| {
            let denial = driver
                .revoke_exact(Some(&lease))
                .expect_err("shared lifetime holder must deny without blocking");
            assert!(
                denial
                    .to_string()
                    .contains("held by a shared-lock consumer")
            );
            Ok(())
        })?;
        assert!(driver.observe(&expected, &activation, &warming, &lease)?);

        driver.revoke_exact(Some(&lease))?;
        assert!(driver.observe_empty()?);
        topology.withdraw_process_write_lease(&expected, &activation, Some(&lease))?;
        assert!(
            !SingleFileMessagePackBackingStore::new(&topology.projection_store)
                .pull_all_read_only_snapshot()?
                .iter()
                .any(|entry| entry.r#type == IdunnProcessWriteLeaseRecord::TYPE)
        );
        topology.withdraw_process_write_lease(&expected, &activation, Some(&lease))?;
        driver.revoke_exact(Some(&lease))?;

        SingleFileMessagePackBackingStore::new(&path).with_read_only_shared_snapshot(
            |snapshot| {
                assert!(snapshot.is_empty());
                let denial = driver
                    .grant(&expected, &activation, &warming, &lease)
                    .expect_err("shared lifetime holder must deny grant without blocking");
                assert!(
                    denial
                        .to_string()
                        .contains("held by a shared-lock consumer")
                );
                Ok(())
            },
        )?;
        assert!(driver.observe_empty()?);
        assert_eq!(
            driver.grant(&expected, &activation, &warming, &lease)?,
            lease.canonical_sha256()?
        );
        driver.revoke_exact(Some(&lease))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn installed_release_is_root_owned_and_nonwritable() -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir()?;
        let root = temp.path().join("release");
        fs::create_dir(&root)?;
        let artifact = root.join("service");
        fs::write(&artifact, b"sealed")?;
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o777))?;
        harden_installed_release(
            &root,
            &[ArtifactReceipt {
                artifact_id: "service".into(),
                destination: PathBuf::from("service"),
                sha256: sha256_id(b"sealed"),
                size_bytes: 6,
                executable: true,
            }],
        )?;
        let root_metadata = fs::symlink_metadata(&root)?;
        let artifact_metadata = fs::symlink_metadata(&artifact)?;
        assert_eq!(root_metadata.uid(), 0);
        assert_eq!(root_metadata.permissions().mode() & 0o777, 0o555);
        assert_eq!(artifact_metadata.uid(), 0);
        assert_eq!(artifact_metadata.permissions().mode() & 0o777, 0o555);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn runner_cache_is_created_once_for_one_exact_identity() -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))?;
        let cache = temp.path().join("cache");
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        ensure_runner_cache_root(&cache, identity)?;
        let metadata = fs::symlink_metadata(&cache)?;
        assert_eq!((metadata.uid(), metadata.gid()), (1000, 1000));
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert!(ensure_runner_cache_root(&cache, identity).is_ok());
        assert!(
            ensure_runner_cache_root(
                &cache,
                ProcessIdentity {
                    uid: 1001,
                    gid: 1001
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn runner_secret_is_root_owned_and_bound_to_the_runner_group() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))?;
        let secret = temp.path().join("token");
        fs::write(&secret, b"secret")?;
        let secret_c = std::ffi::CString::new(secret.as_os_str().as_bytes())?;
        ensure!(unsafe { libc::lchown(secret_c.as_ptr(), 0, 1000) } == 0);
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o440))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        validate_runner_secret(&secret, identity)?;
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o444))?;
        assert!(validate_runner_secret(&secret, identity).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exact_git_blobs_become_root_owned_immutable_source_without_git_metadata() -> Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        let repository = temp.path().join("repository");
        fs::create_dir(&repository)?;
        git_at(&repository, &["init", "--initial-branch=main"])?;
        git_at(&repository, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &repository,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(repository.join("deployment.toml"), b"target = 'test'\n")?;
        let script = repository.join("build.sh");
        fs::write(&script, b"#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        fs::create_dir(repository.join("links"))?;
        symlink("../deployment.toml", repository.join("links/deployment"))?;
        git_at(&repository, &["add", "--all"])?;
        git_at(&repository, &["commit", "-m", "fixture"])?;
        let revision = git_at(&repository, &["rev-parse", "HEAD"])?;

        let parent = temp.path().join("transaction");
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let destination = parent.join("source");
        prepare_frozen_source_destination(&destination)?;
        let driver = GitSourceDriver::new(
            temp.path().join("source-cache"),
            temp.path().join("frozen-source"),
            None,
        );
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(destination.clone());
        driver.materialize_tree_raw(&repository, &revision, &destination, &destination, &mut created_dirs)?;
        driver.materialize_tree_raw(
            &repository,
            &revision,
            &destination.join("vendor/fixture"),
            &destination,
            &mut created_dirs,
        )?;
        harden_frozen_source(&destination)?;

        assert!(!destination.join(".git").exists());
        assert_eq!(
            fs::symlink_metadata(destination.join("deployment.toml"))?
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        let script_metadata = fs::symlink_metadata(destination.join("build.sh"))?;
        assert_eq!(script_metadata.uid(), 0);
        assert_eq!(script_metadata.permissions().mode() & 0o777, 0o555);
        assert!(destination.join("vendor/fixture/deployment.toml").is_file());
        validate_frozen_source_symlink(&destination, &destination.join("links/deployment"))?;
        validate_frozen_source(&destination)?;
        let digest = frozen_source_sha256(&destination)?;
        let recipe = destination.join("deployment.toml");
        fs::set_permissions(&recipe, fs::Permissions::from_mode(0o644))?;
        assert!(validate_frozen_source(&destination).is_err());
        fs::set_permissions(&recipe, fs::Permissions::from_mode(0o444))?;
        fs::write(&recipe, b"target = 'changed'\n")?;
        assert_ne!(frozen_source_sha256(&destination)?, digest);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn frozen_source_rejects_a_symlink_that_escapes_the_transaction_tree() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        let parent = temp.path().join("transaction");
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let destination = parent.join("source");
        prepare_frozen_source_destination(&destination)?;
        symlink("../../outside", destination.join("escape"))?;
        // S6: `harden_frozen_source` no longer resolves symlink chains --
        // only `target.is_relative()` -- because the chain must be judged
        // against the tree's real, final published path, which this
        // temporary destination already is (no `.partial` rename is
        // involved here). `validate_frozen_source_symlinks` is what now
        // owns the escape check.
        harden_frozen_source(&destination)?;
        assert!(validate_frozen_source_symlinks(&destination).is_err());
        Ok(())
    }

    // ---------------------------------------------------------------------
    // S5 (Self's ruling, third Cut 1 fix batch): a behavioural test for each
    // guard Soul's third pass found unpinned, through `freeze_exact` or its
    // own primitives wherever the guard is reachable from there.
    // ---------------------------------------------------------------------

    /// S5, root ownership: `harden_frozen_source` refuses any entry not
    /// owned by root, not only the top-level directory. A file left behind
    /// by a step that ran as the unprivileged Git identity, and never
    /// re-owned, must be refused rather than silently hardened.
    #[cfg(unix)]
    #[test]
    fn harden_frozen_source_refuses_a_non_root_owned_entry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        fs::write(root.join("f"), b"x")?;
        ensure!(
            Command::new("/bin/chown")
                .args(["1000:1000"])
                .arg(root.join("f"))
                .status()?
                .success(),
            "chowning the fixture file"
        );
        assert!(
            harden_frozen_source(&root).is_err(),
            "a non-root-owned entry must be refused"
        );
        Ok(())
    }

    /// S5, exact duplicate path: two raw tree entries that literally share
    /// one leaf path (not merely alias each other through a directory) must
    /// be refused before anything is written. `git ls-tree -r` emits the
    /// path twice; only `git_tree_entries`'s own "emits a path twice" check
    /// -- not fsck, which is content-agnostic about which of the two blobs a
    /// reader would see -- stands between this tree and a freeze that
    /// silently picks one of the two blobs.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_refuses_an_exact_duplicate_leaf_path() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "seed"])?;

        let hash_object = |content: &[u8]| -> Result<String> {
            let mut child = Command::new("git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(&origin_repo)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(content)?;
            let output = child.wait_with_output()?;
            ensure!(output.status.success(), "git hash-object failed");
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let recipe = hash_object(b"target = 'test'\n")?;
        let a = hash_object(b"a\n")?;
        let b = hash_object(b"b\n")?;
        let hex = |s: &str| -> Vec<u8> {
            (0..20)
                .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
                .collect()
        };
        let mut raw = Vec::new();
        for (mode, name, sha) in [
            ("100644", "deployment.toml", &recipe),
            ("100644", "same.txt", &a),
            ("100644", "same.txt", &b),
        ] {
            raw.extend_from_slice(format!("{mode} {name}\0").as_bytes());
            raw.extend(hex(sha));
        }
        let mut child = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&origin_repo)
            .args(["hash-object", "-t", "tree", "--literally", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(&raw)?;
        let output = child.wait_with_output()?;
        ensure!(output.status.success(), "git hash-object --literally failed");
        let tree = String::from_utf8(output.stdout)?.trim().to_owned();
        let git_owned = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .args([
                    "-c", "safe.directory=*",
                    "-c", "user.name=Idunn Test",
                    "-c", "user.email=idunn-test@example.invalid",
                    "-C",
                ])
                .arg(&origin_repo)
                .args(args)
                .output()?;
            ensure!(output.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let commit = git_owned(&["commit-tree", &tree, "-m", "dup-leaf"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: format!("file://{}", origin_repo.display()),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let result = driver.freeze_exact(&source, &commit, "txn-dup-leaf", &frozen_source_root);
        assert!(
            result.is_err(),
            "a raw tree with the same leaf path recorded twice must be refused"
        );
        Ok(())
    }

    /// S5, `ensure_frozen_directory` containment: its own top-of-function
    /// check that `path` starts with `root` must be pinned directly, not
    /// only observed indirectly through a symlink escape. A path that is
    /// simply outside the root -- no symlink involved at all -- must be
    /// refused, and nothing may be created along the way.
    #[cfg(unix)]
    #[test]
    fn ensure_frozen_directory_refuses_a_path_outside_its_root() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        let outside = temp.path().join("outside-dir");
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(root.clone());
        let result = ensure_frozen_directory(&root, &mut created_dirs, &outside);
        assert!(result.is_err(), "a path outside the root must be refused");
        assert!(!outside.exists(), "nothing outside the root may be created");
        Ok(())
    }

    /// S5, the writer's own layer: pass 1's creation call is `fs::create_dir`,
    /// which errors whenever something is already at the path, never
    /// `fs::create_dir_all`, which treats an already-existing directory as
    /// success. A second, independent `created_dirs` bookkeeping set --
    /// exactly what a later pass or a second freeze would carry -- must
    /// still refuse to reuse a directory an earlier call already made,
    /// rather than silently accept it as already there.
    #[cfg(unix)]
    #[test]
    fn ensure_frozen_directory_does_not_silently_reuse_a_directory_it_did_not_track() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;

        let target = root.join("fresh");
        let mut created_dirs = std::collections::HashSet::new();
        created_dirs.insert(root.clone());
        ensure_frozen_directory(&root, &mut created_dirs, &target)?;
        assert!(target.is_dir());

        let mut other_created_dirs = std::collections::HashSet::new();
        other_created_dirs.insert(root.clone());
        assert!(
            ensure_frozen_directory(&root, &mut other_created_dirs, &target).is_err(),
            "a directory this call did not itself create in this pass must be refused, not silently reused"
        );
        Ok(())
    }

    /// S5, `copy_artifact` containment: a source reached only by resolving a
    /// symlink that leaves the artifact root must be refused before anything
    /// is read from it or copied.
    #[cfg(unix)]
    #[test]
    fn copy_artifact_refuses_a_source_that_escapes_its_root() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"secret")?;
        let link = root.join("escape");
        symlink(&outside, &link)?;
        let destination = temp.path().join("dest.txt");

        let result = copy_artifact(&root, &link, &destination);
        assert!(
            result.is_err(),
            "an artifact source reached only by escaping the root must be refused"
        );
        assert!(!destination.exists(), "nothing may be copied on refusal");
        Ok(())
    }

    // ---------------------------------------------------------------------
    // S6 (Self's ruling, third Cut 1 fix batch): symlinks are judged against
    // the tree's real, final published path, not `.partial`.
    // ---------------------------------------------------------------------

    /// A symlink whose target does not exist anywhere, but whose target path
    /// stays inside the root once every real component along the way is
    /// resolved, must freeze -- not be refused merely for being dangling.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_accepts_a_dangling_symlink_whose_target_stays_inside_the_root() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        symlink("not-there.txt", origin_repo.join("dangling"))?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: format!("file://{}", origin_repo.display()),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let (frozen_root, ..) =
            driver.freeze_exact(&source, &revision, "txn-dangling", &frozen_source_root)?;
        assert!(
            fs::symlink_metadata(frozen_root.join("dangling"))?
                .file_type()
                .is_symlink(),
            "a dangling in-root symlink must freeze, not be refused"
        );
        // Consistency with observe-time, the other half of S6's fix.
        validate_frozen_source(&frozen_root)?;
        Ok(())
    }

    /// The published-location regression Soul found: a relative symlink
    /// naming `.partial`, the temporary directory the tree is built under
    /// before the atomic rename that publishes it. Judged against `.partial`
    /// it resolves back inside (a coincidence of the temporary name), but
    /// judged against the tree's real, final name -- what every later reader
    /// actually opens -- it resolves outside the root and must be refused at
    /// freeze time, not accepted then found broken later by `observe_frozen`.
    #[cfg(unix)]
    #[test]
    fn freeze_exact_refuses_a_symlink_that_only_resolves_inside_the_partial_name() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir()?;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
        let origin_repo = temp.path().join("origin");
        fs::create_dir(&origin_repo)?;
        git_at(&origin_repo, &["init", "--initial-branch=main"])?;
        git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
        git_at(
            &origin_repo,
            &["config", "user.email", "idunn-test@example.invalid"],
        )?;
        fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
        symlink("../.partial/deployment.toml", origin_repo.join("pl"))?;
        git_at(&origin_repo, &["add", "--all"])?;
        git_at(&origin_repo, &["commit", "-m", "fixture"])?;
        let revision = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
        ensure!(
            Command::new("/bin/chown")
                .args(["-R", "1000:1000"])
                .arg(&origin_repo)
                .status()?
                .success(),
            "chowning the fixture origin repository"
        );

        let source_cache_root = temp.path().join("source-cache");
        let frozen_source_root = temp.path().join("frozen-source");
        fs::create_dir(&frozen_source_root)?;
        fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
        let identity = ProcessIdentity {
            uid: 1000,
            gid: 1000,
        };
        let driver = GitSourceDriver::new(
            source_cache_root.clone(),
            frozen_source_root.clone(),
            Some(identity),
        );
        let source = ExactSource {
            origin: format!("file://{}", origin_repo.display()),
            checkout: source_cache_root.join("checkout"),
            gitlinks: BTreeMap::new(),
            recipe_path: PathBuf::from("deployment.toml"),
        };
        let result = driver.freeze_exact(&source, &revision, "txn-partial-name", &frozen_source_root);
        assert!(
            result.is_err(),
            "a symlink that resolves inside the root only under the temporary `.partial` name, and outside it under the real published name, must be refused"
        );
        Ok(())
    }

    // ---------------------------------------------------------------------
    // S1 (Self's ruling, third Cut 1 fix batch): fsck guards the fetch in
    // `resolve()` as well as the one in `freeze_exact`, and `freeze_exact`
    // also fscks the selected tree so that objects already local get
    // checked too.
    //
    // Discrepancy: `OperatorBinding::validate()` (`deployment.rs:1027-1040`)
    // requires `repository.origin` to start with `https://`, unconditionally
    // -- `resolve()` calls it first thing, and `freeze()` calls it through
    // `ResolvedSource::validate_against`. No test in this suite stands up a
    // real HTTPS Git origin (every existing fixture uses `file://`), and
    // adding one is out of this fix batch's scope. This fixture instead
    // reproduces `resolve()`'s exact fetch invocation -- literally the same
    // arguments `resolve()` passes to `self.git(...)`, this fix included --
    // against the checkout, the same approximation Soul's own probe used
    // (`resolve_like_fetch` in `probe4_soul3.rs`), followed by the real,
    // unmodified `freeze_exact`. That is "resolve() then freeze()" in every
    // way this suite can exercise without a live HTTPS remote: it proves the
    // fetch line S1 changed, not a hand-built substitute for it, refuses the
    // hostile object, and that `freeze_exact`'s own explicit fsck refuses it
    // independently even if the fetch line's guard were ever weakened.
    #[cfg(unix)]
    #[test]
    fn resolve_style_fetch_then_freeze_exact_refuses_hostile_trees() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        fn hash_object(repo: &Path, content: &[u8]) -> Result<String> {
            let mut child = Command::new("git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(repo)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(content)?;
            let output = child.wait_with_output()?;
            ensure!(output.status.success(), "git hash-object failed");
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        }
        fn literal_tree(repo: &Path, entries: &[(&str, &str, &str)]) -> Result<String> {
            let hex = |s: &str| -> Vec<u8> {
                (0..20)
                    .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
                    .collect()
            };
            let mut raw = Vec::new();
            for (mode, name, sha) in entries {
                raw.extend_from_slice(format!("{mode} {name}\0").as_bytes());
                raw.extend(hex(sha));
            }
            let mut child = Command::new("git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(repo)
                .args(["hash-object", "-t", "tree", "--literally", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            child.stdin.take().unwrap().write_all(&raw)?;
            let output = child.wait_with_output()?;
            ensure!(output.status.success(), "git hash-object --literally failed");
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        }
        fn dotgit_lookalike(repo: &Path, recipe: &str, name: &str) -> Result<Vec<(String, String, String)>> {
            let cfg = hash_object(repo, b"[core]\n\tfsmonitor = touch /tmp/soul3-pwned\n")?;
            let inner = literal_tree(repo, &[("100644", "config", &cfg)])?;
            let mut entries = vec![
                ("100644".to_owned(), "deployment.toml".to_owned(), recipe.to_owned()),
                ("40000".to_owned(), name.to_owned(), inner),
            ];
            entries.sort_by(|a, b| a.1.cmp(&b.1));
            Ok(entries)
        }
        fn chown_to_source_identity(repo: &Path) -> Result<()> {
            ensure!(
                Command::new("/bin/chown")
                    .args(["-R", "1000:1000"])
                    .arg(repo)
                    .status()?
                    .success(),
                "chowning the fixture origin repository"
            );
            Ok(())
        }

        #[allow(clippy::type_complexity)]
        let fixtures: Vec<(&str, Box<dyn Fn(&Path, &str) -> Result<Vec<(String, String, String)>>>)> = vec![
            (
                "dup-trees",
                Box::new(|repo: &Path, recipe: &str| {
                    let x = hash_object(repo, b"x\n")?;
                    let y = hash_object(repo, b"y\n")?;
                    let tx = literal_tree(repo, &[("100644", "x", &x)])?;
                    let ty = literal_tree(repo, &[("100644", "y", &y)])?;
                    Ok(vec![
                        ("40000".into(), "d".into(), tx),
                        ("40000".into(), "d".into(), ty),
                        ("100644".into(), "deployment.toml".into(), recipe.into()),
                    ])
                }) as Box<dyn Fn(&Path, &str) -> Result<Vec<(String, String, String)>>>,
            ),
            ("dotGIT", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, ".GIT"))),
            ("dotGit", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, ".Git"))),
            ("git-short-name", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, "git~1"))),
            ("dotgit-trailing-dot", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, ".git."))),
            ("dotgit-zwnj", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, ".git\u{200c}"))),
            ("dotgit-trailing-space", Box::new(|repo: &Path, recipe: &str| dotgit_lookalike(repo, recipe, ".git "))),
        ];

        for (label, build) in fixtures {
            let temp = tempfile::tempdir()?;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
            let origin_repo = temp.path().join("origin");
            fs::create_dir(&origin_repo)?;
            git_at(&origin_repo, &["init", "--initial-branch=main"])?;
            git_at(&origin_repo, &["config", "user.name", "Idunn Test"])?;
            git_at(&origin_repo, &["config", "user.email", "idunn-test@example.invalid"])?;
            fs::write(origin_repo.join("deployment.toml"), b"target = 'test'\n")?;
            git_at(&origin_repo, &["add", "--all"])?;
            git_at(&origin_repo, &["commit", "-m", "root"])?;
            let root_commit = git_at(&origin_repo, &["rev-parse", "HEAD"])?;
            let recipe_blob = git_at(&origin_repo, &["rev-parse", "HEAD:deployment.toml"])?;
            chown_to_source_identity(&origin_repo)?;

            let source_cache_root = temp.path().join("source-cache");
            let frozen_source_root = temp.path().join("frozen-source");
            fs::create_dir(&frozen_source_root)?;
            fs::set_permissions(&frozen_source_root, fs::Permissions::from_mode(0o700))?;
            let identity = ProcessIdentity { uid: 1000, gid: 1000 };
            let driver = GitSourceDriver::new(
                source_cache_root.clone(),
                frozen_source_root.clone(),
                Some(identity),
            );
            let source = ExactSource {
                origin: format!("file://{}", origin_repo.display()),
                checkout: source_cache_root.join("checkout"),
                gitlinks: BTreeMap::new(),
                recipe_path: PathBuf::from("deployment.toml"),
            };
            // The checkout is established on the good root commit first,
            // exactly as a real deploy target's checkout already exists by
            // the time a later push lands: this is what makes the objects
            // this fixture cares about arrive only through the *next*
            // fetch, not the initial clone.
            driver.prepare_source_root(&source)?;

            let entries = build(&origin_repo, &recipe_blob)
                .with_context(|| format!("building the {label} fixture"))?;
            let entry_refs: Vec<(&str, &str, &str)> = entries
                .iter()
                .map(|(mode, name, sha)| (mode.as_str(), name.as_str(), sha.as_str()))
                .collect();
            let hostile_tree = literal_tree(&origin_repo, &entry_refs)?;
            // Not `git_at`: the repository is already chowned to the
            // unprivileged identity above, and this process is root, so
            // plain `git` here refuses it as "dubious ownership" without
            // `safe.directory` (the same reason `freeze_exact_refuses_a_
            // duplicate_named_tree...` above uses its own `git_owned`).
            let git_owned = |args: &[&str]| -> Result<String> {
                let output = Command::new("git")
                    .args([
                        "-c", "safe.directory=*",
                        "-c", "user.name=Idunn Test",
                        "-c", "user.email=idunn-test@example.invalid",
                        "-C",
                    ])
                    .arg(&origin_repo)
                    .args(args)
                    .output()?;
                ensure!(output.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
                Ok(String::from_utf8(output.stdout)?.trim().to_owned())
            };
            let hostile_commit = git_owned(&["commit-tree", &hostile_tree, "-p", &root_commit, "-m", "hostile"])?;
            git_owned(&["update-ref", "refs/heads/main", &hostile_commit])?;

            // The exact fetch `resolve()` runs (`drivers.rs`, `fn resolve`),
            // S1's fix included: `-c transfer.fsckObjects=true`, `--force`,
            // `--no-tags`, fetching `refs/heads/main` into a resolution ref.
            let resolve_style_fetch = driver.git([
                OsString::from("-c"),
                OsString::from("transfer.fsckObjects=true"),
                OsString::from("-C"),
                source.checkout.as_os_str().to_owned(),
                OsString::from("fetch"),
                OsString::from("--force"),
                OsString::from("--no-tags"),
                OsString::from("origin"),
                OsString::from("+refs/heads/main:refs/idunn/resolutions/s1-fixture/r1"),
            ]);
            match resolve_style_fetch {
                Err(_) => {
                    // Refused at the fetch itself: the strongest outcome.
                }
                Ok(_) => {
                    // The fetch admitted the ref; `freeze_exact`'s own
                    // explicit fsck (defense in depth, for objects a fetch
                    // never re-checks) must still refuse it.
                    let result =
                        driver.freeze_exact(&source, &hostile_commit, "txn-hostile", &frozen_source_root);
                    assert!(
                        result.is_err(),
                        "{label}: a hostile tree that a resolve()-style fetch admitted must still be refused by freeze_exact's own fsck"
                    );
                }
            }
        }
        Ok(())
    }
}
