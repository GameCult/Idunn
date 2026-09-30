use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultcache_rs::{
    CultCacheEnvelope, CultCacheExpectedEnvelope, DatabaseEntry, SingleFileMessagePackBackingStore,
};
use cultnet_rs::{
    AuthenticatedOdinRuntimeTopologyCorrelation, GameCultProviderHealthIdentity,
    IDUNN_DEPLOYMENT_BRAKE_SCHEMA, IDUNN_LIFECYCLE_BRAKE_SCHEMA, IDUNN_PROCESS_WRITE_LEASE_SCHEMA,
    IdunnDeploymentBrakeObservation, IdunnDeploymentBrakeOperatorIdentity,
    IdunnDeploymentBrakeRecord, IdunnExpectedIncarnationRecord, IdunnLifecycleBrakeObservation,
    IdunnLifecycleBrakeRecord, IdunnProcessWriteLeaseRecord, IdunnRuntimeActivationLaunch,
    IdunnRuntimeActivationRecord, IdunnServiceIdentity, OdinTopologyAuthenticationContext,
    OdinTopologyDisagreement, OdinTopologyIdentity, RuntimePresenceAuthenticationContext,
    ServiceIdentityProfile, ServiceIdentitySigner, ServiceIdentityTrustAnchor,
    authenticate_odin_runtime_topology_correlation, authenticate_runtime_presence_claim,
    correlate_runtime_presence_claim, derive_service_identity_id,
    evaluate_idunn_continuity_restart, evaluate_idunn_deployment_brake, open_service_identity_at,
    verify_idunn_deployment_brake_authorization, verify_runtime_authority,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::deployment::{
    DependencyKind, OperatorBinding, RolloutStrategy, RouteBinding, WorkloadBinding,
    capability_compatible,
};
use crate::deployment_plan::{
    CompiledDeploymentPlan, DependencyProviderAuthority, SealedRelease, compile_deployment_plan,
};
use crate::drivers::{
    ChallengeFailure, CultCacheTopologyDriver, CultCacheWriteLeaseDriver, DockerRunnerDriver, FrozenSourceReceipt,
    GitSourceDriver, InstalledReleaseObservation, IsolationEvidence, NginxRouteDriver,
    ProcessIdentity, RouteActuation, RouteActuationGate, RouteActuationRefused, RouteActuators,
    RouteObservation, RoutePreflightReceipt, RunnerPort, SourcePort,
    SystemdTransientWorkloadDriver, TopologyPort, WorkloadObservation, WorkloadPort,
    WriteLeasePort,
};
use crate::host_actuator::{
    HostActuatorAccess, HostActuatorHub, HostActuatorRunnerDriver, HostActuatorWorkloadDriver,
    HostUnobservable, IdunnHostActuatorIdentity, SharedHostActuatorHub, spawn_hub_service,
};

const DEPLOYMENT_COMMAND_SCHEMA: &str = "idunn.deployment_command.v2";
const DEPLOYMENT_TRANSACTION_SCHEMA: &str = "idunn.deployment_transaction.v4";
const DEPLOYMENT_TRANSACTION_SCHEMA_V3: &str = "idunn.deployment_transaction.v3";
const DEPLOYMENT_TRANSACTION_SCHEMA_V2: &str = "idunn.deployment_transaction.v2";
const ADMITTED_GENERATION_SCHEMA: &str = "idunn.admitted_generation.v4";
const ADMITTED_GENERATION_SCHEMA_V3: &str = "idunn.admitted_generation.v3";
const ADMITTED_GENERATION_SCHEMA_V2: &str = "idunn.admitted_generation.v2";
/// The capability whose declaration makes a target Odin-correlated.
pub(crate) const ODIN_RENDEZVOUS_CAPABILITY: &str = "odin.verse-rendezvous";
const TARGET_SUPERVISION_SCHEMA: &str = "idunn.target_supervision.v1";
/// How many times continuity will restart one target inside one window before
/// it concludes the target itself is the problem. More than one because a
/// start can fail for a passing reason -- a port still held, a peer not yet up
/// -- and bounded because each attempt holds the target against any
/// deployment. Counted per target, so a restart that succeeds and dies again
/// does not start the count over.
const CONTINUITY_RESTART_ATTEMPTS: usize = 6;
const CONTINUITY_RESTART_WINDOW_MILLIS: u64 = 3_600_000;
/// The wait after the first restart in a window; it doubles with each further one.
const CONTINUITY_RESTART_BACKOFF_MILLIS: u64 = 5_000;

/// Forward route actuations (fragment write, firewall, `nginx -t`, reload,
/// private-mount validation) one target may perform per sliding window. A
/// healthy target performs none; a legitimate deployment performs one or two.
/// Survival actuations are counted against it but never refused.
const ROUTE_ACTUATION_CEILING: usize = 12;
const ROUTE_ACTUATION_WINDOW_MILLIS: u64 = 3_600_000;
/// The longest wait between route challenges while proofs keep failing. The
/// shortest is the observation max age.
const ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS: u64 = 600_000;

/// How long continuity waits after it could not prepare a restart (the
/// projection would not demote) before trying again.
const CONTINUITY_DEFERRAL_MILLIS: u64 = 30_000;

const DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS: u64 = 30_000;
const DEFAULT_TOPOLOGY_MAXIMUM_FUTURE_SKEW_MILLIS: u64 = 2_000;

/// An immutable request. Execution state deliberately does not fit in this
/// record; status is derived from the transactions created for it.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.deployment_command",
    schema = "idunn.deployment_command.v2"
)]
struct DeploymentCommand {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    command_id: String,
    #[cultcache(key = 2)]
    kind: CommandKind,
    #[cultcache(key = 3)]
    selector: String,
    #[cultcache(key = 4)]
    requested_by: String,
    #[cultcache(key = 5)]
    requested_at_unix_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CommandKind {
    Deploy,
    Continuity,
}

impl DeploymentCommand {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == DEPLOYMENT_COMMAND_SCHEMA,
            "deployment command schema is unsupported"
        );
        require_id(&self.command_id, "deployment command id")?;
        require_selector(&self.selector)?;
        require_value(&self.requested_by, "deployment requester")?;
        ensure!(
            self.requested_at_unix_millis > 0,
            "deployment command has no request time"
        );
        match self.kind {
            CommandKind::Deploy => ensure!(
                !self.command_id.starts_with("continuity-"),
                "operator deployment uses the continuity command namespace"
            ),
            CommandKind::Continuity => ensure!(
                self.command_id.starts_with("continuity-")
                    && !self.selector.starts_with("profile:"),
                "continuity command identity or selector is invalid"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
enum DeploymentPhase {
    Sealing,
    Starting,
    Warming,
    Fencing,
    Leasing,
    AwaitingReady,
    Routing,
    Committing,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentAuthorization {
    authorization_id: String,
    brake_sha256: String,
    canonical_brake_bytes: Vec<u8>,
    authorized_at_unix_millis: u64,
}

impl DeploymentAuthorization {
    fn validate_shape(&self) -> Result<()> {
        require_id(&self.authorization_id, "deployment authorization id")?;
        ensure!(
            !self.canonical_brake_bytes.is_empty()
                && self.brake_sha256 == sha256_id(&self.canonical_brake_bytes)
                && self.authorized_at_unix_millis > 0,
            "deployment authorization receipt is incomplete"
        );
        let record: IdunnDeploymentBrakeRecord =
            rmp_serde::from_slice(&self.canonical_brake_bytes)?;
        record.validate()?;
        ensure!(
            rmp_serde::to_vec(&record)? == self.canonical_brake_bytes
                && record.authorization_id.as_deref() == Some(&self.authorization_id),
            "deployment authorization receipt is noncanonical or mismatched"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TopologyEvidence {
    canonical_bytes: Vec<u8>,
    canonical_sha256: String,
    signer_identity_id: String,
    publisher_sequence: u64,
    admitted_at_unix_millis: u64,
}

impl TopologyEvidence {
    fn from_authenticated(
        topology: &AuthenticatedOdinRuntimeTopologyCorrelation,
        admitted_at_unix_millis: u64,
    ) -> Result<Self> {
        let record = topology.record();
        let evidence = Self {
            canonical_bytes: topology.canonical_bytes().to_vec(),
            canonical_sha256: sha256_id(topology.canonical_bytes()),
            signer_identity_id: record.signer_identity_id.clone(),
            publisher_sequence: record.publisher_sequence,
            admitted_at_unix_millis,
        };
        evidence.validate_shape()?;
        Ok(evidence)
    }

    fn validate_shape(&self) -> Result<()> {
        ensure!(
            !self.canonical_bytes.is_empty()
                && self.canonical_sha256 == sha256_id(&self.canonical_bytes),
            "topology evidence bytes or digest are invalid"
        );
        require_id(&self.signer_identity_id, "Odin topology signer")?;
        ensure!(
            self.publisher_sequence > 0 && self.admitted_at_unix_millis > 0,
            "topology evidence sequence or admission time is invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimePresenceEvidence {
    canonical_bytes: Vec<u8>,
    canonical_sha256: String,
    message_id: String,
    challenged_at_unix_millis: u64,
    admitted_at_unix_millis: u64,
}

impl RuntimePresenceEvidence {
    fn from_present(
        present: &cultnet_rs::VerifiedRuntimePresence,
        message_id: String,
        challenged_at_unix_millis: u64,
        admitted_at_unix_millis: u64,
    ) -> Result<Self> {
        let evidence = Self {
            canonical_bytes: present.canonical_bytes().to_vec(),
            canonical_sha256: present.signed_presence_sha256().to_owned(),
            message_id,
            challenged_at_unix_millis,
            admitted_at_unix_millis,
        };
        evidence.validate_shape()?;
        Ok(evidence)
    }

    fn validate_shape(&self) -> Result<()> {
        ensure!(
            !self.canonical_bytes.is_empty()
                && self.canonical_sha256 == sha256_id(&self.canonical_bytes),
            "runtime presence evidence bytes or digest are invalid"
        );
        require_id(&self.message_id, "runtime presence challenge")?;
        ensure!(
            self.challenged_at_unix_millis > 0
                && self.admitted_at_unix_millis >= self.challenged_at_unix_millis,
            "runtime presence evidence timeline is invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case", deny_unknown_fields)]
enum WarmingEvidence {
    OdinTopology { evidence: TopologyEvidence },
    FirstOdinDirect { evidence: RuntimePresenceEvidence },
    /// A route-proof target's candidate answered Idunn's challenge itself.
    RouteProofDirect { evidence: RuntimePresenceEvidence },
}

impl WarmingEvidence {
    fn validate_shape(&self) -> Result<()> {
        match self {
            Self::OdinTopology { evidence } => evidence.validate_shape(),
            Self::FirstOdinDirect { evidence } | Self::RouteProofDirect { evidence } => {
                evidence.validate_shape()
            }
        }
    }

    fn voucher(&self) -> Voucher {
        match self {
            Self::OdinTopology { .. } | Self::FirstOdinDirect { .. } => Voucher::Odin,
            Self::RouteProofDirect { .. } => Voucher::Candidate,
        }
    }

    /// The presence Idunn challenged for itself, when no Odin carried it.
    fn direct(&self) -> Option<&RuntimePresenceEvidence> {
        match self {
            Self::OdinTopology { .. } => None,
            Self::FirstOdinDirect { evidence } | Self::RouteProofDirect { evidence } => {
                Some(evidence)
            }
        }
    }
}

/// Opaque capability minted only after the transaction CAS stores exact
/// Warming evidence. Adjacent drivers receive only the candidate identity and
/// signed-presence digest needed to bind the write lease; they cannot inspect
/// or construct semantic admission.
#[derive(Clone, Debug)]
pub struct SequenceAdmittedWarming {
    transaction_id: String,
    signed_presence_sha256: String,
    runtime_instance_id: String,
}

impl SequenceAdmittedWarming {
    fn from_topology(
        transaction_id: String,
        authenticated: AuthenticatedOdinRuntimeTopologyCorrelation,
    ) -> Result<Self> {
        let record = authenticated.record();
        let signed_presence_sha256 = record
            .signed_presence_sha256
            .clone()
            .context("Warming topology has no signed presence")?;
        let runtime_instance_id = record
            .runtime_instance_id
            .clone()
            .context("Warming topology has no runtime instance")?;
        Ok(Self {
            transaction_id,
            signed_presence_sha256,
            runtime_instance_id,
        })
    }

    fn from_direct_presence(
        transaction_id: String,
        warming: WarmingEvidence,
        present: cultnet_rs::VerifiedRuntimePresence,
    ) -> Result<Self> {
        let evidence = warming
            .direct()
            .context("direct Warming evidence is an Odin topology receipt")?;
        ensure!(
            present.canonical_bytes() == evidence.canonical_bytes
                && present.signed_presence_sha256() == evidence.canonical_sha256,
            "direct Warming evidence differs from its authenticated presence"
        );
        let runtime_instance_id = present.record().runtime_instance_id.clone();
        Ok(Self {
            transaction_id,
            signed_presence_sha256: evidence.canonical_sha256.clone(),
            runtime_instance_id,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        transaction_id: impl Into<String>,
        authenticated: AuthenticatedOdinRuntimeTopologyCorrelation,
    ) -> Result<Self> {
        let record = authenticated.record();
        ensure!(
            record.present
                && !record.ready
                && record.observed_presence_state.as_deref() == Some("warming")
                && record.observed_write_lease_sha256.is_none()
                && record.disagreements.is_empty(),
            "test receipt is not exact Warming evidence"
        );
        let transaction_id = transaction_id.into();
        require_id(&transaction_id, "test transaction id")?;
        let signed_presence_sha256 = record
            .signed_presence_sha256
            .clone()
            .context("test Warming receipt has no signed presence")?;
        let runtime_instance_id = record
            .runtime_instance_id
            .clone()
            .context("test Warming receipt has no runtime instance")?;
        let token = Self::from_topology(transaction_id, authenticated)?;
        ensure!(
            token.signed_presence_sha256 == signed_presence_sha256
                && token.runtime_instance_id == runtime_instance_id,
            "test Warming token extraction changed"
        );
        Ok(token)
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) fn signed_presence_sha256(&self) -> &str {
        &self.signed_presence_sha256
    }

    pub(crate) fn runtime_instance_id(&self) -> &str {
        &self.runtime_instance_id
    }
}

/// Ready proof with the same construction boundary. Dependency planning and
/// promotion receive this token, never caller-supplied digest/sequence tuples.
#[derive(Clone, Debug)]
pub(crate) struct SequenceAdmittedReady {
    transaction_id: String,
    evidence: TopologyEvidence,
    expected: IdunnExpectedIncarnationRecord,
    authenticated: AuthenticatedOdinRuntimeTopologyCorrelation,
}

impl SequenceAdmittedReady {
    #[cfg(test)]
    pub(crate) fn for_test(
        expected: &IdunnExpectedIncarnationRecord,
        authenticated: AuthenticatedOdinRuntimeTopologyCorrelation,
        admitted_at_unix_millis: u64,
    ) -> Result<Self> {
        expected.validate()?;
        let record = authenticated.record();
        ensure!(
            record.expected_projection_sha256 == expected.canonical_sha256()?
                && record.target == expected.target
                && record.runtime_id == expected.runtime_id
                && record.present
                && record.ready
                && record.observed_presence_state.as_deref() == Some("active")
                && record.disagreements.is_empty(),
            "test receipt is not exact Ready evidence"
        );
        let evidence =
            TopologyEvidence::from_authenticated(&authenticated, admitted_at_unix_millis)?;
        Ok(Self {
            transaction_id: "test-sequence-admission".into(),
            evidence,
            expected: expected.clone(),
            authenticated,
        })
    }

    pub(crate) fn authenticated(&self) -> &AuthenticatedOdinRuntimeTopologyCorrelation {
        &self.authenticated
    }

    pub(crate) fn expected(&self) -> &IdunnExpectedIncarnationRecord {
        &self.expected
    }

    pub(crate) fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    pub(crate) fn evidence_sha256(&self) -> &str {
        &self.evidence.canonical_sha256
    }

    pub(crate) fn publisher_sequence(&self) -> u64 {
        self.evidence.publisher_sequence
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case", deny_unknown_fields)]
enum FencingEvidence {
    SkippedStateless,
    Revoked {
        incumbent_lease_sha256: Option<String>,
        candidate_lease_path_verified_empty: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case", deny_unknown_fields)]
enum LeasingEvidence {
    SkippedStateless,
    Prepared {
        lease: IdunnProcessWriteLeaseRecord,
        lease_sha256: String,
    },
    Granted {
        lease: IdunnProcessWriteLeaseRecord,
        lease_sha256: String,
    },
}

impl LeasingEvidence {
    fn lease(&self) -> Option<&IdunnProcessWriteLeaseRecord> {
        match self {
            Self::SkippedStateless | Self::Prepared { .. } => None,
            Self::Granted { lease, .. } => Some(lease),
        }
    }

    fn prepared_lease(&self) -> Option<(&IdunnProcessWriteLeaseRecord, &str)> {
        match self {
            Self::Prepared {
                lease,
                lease_sha256,
            } => Some((lease, lease_sha256)),
            Self::SkippedStateless | Self::Granted { .. } => None,
        }
    }

    fn lease_sha256(&self) -> Option<&str> {
        match self {
            Self::SkippedStateless | Self::Prepared { .. } => None,
            Self::Granted { lease_sha256, .. } => Some(lease_sha256),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case", deny_unknown_fields)]
enum RoutingEvidence {
    SkippedUnrouted,
    Promoted {
        observation: RouteObservation,
        promoted_at_unix_millis: u64,
    },
}

impl RoutingEvidence {
    fn observation(&self) -> Option<&RouteObservation> {
        match self {
            Self::SkippedUnrouted => None,
            Self::Promoted { observation, .. } => Some(observation),
        }
    }

    fn promoted_at_unix_millis(&self) -> Option<u64> {
        match self {
            Self::SkippedUnrouted => None,
            Self::Promoted {
                promoted_at_unix_millis,
                ..
            } => Some(*promoted_at_unix_millis),
        }
    }
}

/// The states a route-proof candidate may answer its Warming challenge in. A
/// stateful one is warming until it holds a lease it cannot hold yet, and the
/// lease binds that warming presence. A stateless one may have finished
/// warming before the first challenge landed.
fn route_proof_warming_states(expected: &IdunnExpectedIncarnationRecord) -> &'static [&'static str] {
    if expected.write_lease_required {
        &["warming"]
    } else {
        &["warming", "active"]
    }
}

/// A challenged runtime answered as something that disagrees with current
/// authority. Each disagreement is named, so a capacity below the Expected
/// minimum reads as that and not as a generic refusal. It stays the error's
/// typed value; callers read `disagreements`, and the text is only its
/// rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PresenceDisagrees {
    pub(crate) disagreements: Vec<OdinTopologyDisagreement>,
}

impl std::fmt::Display for PresenceDisagrees {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let named = self
            .disagreements
            .iter()
            .map(|disagreement| {
                format!(
                    "{} (expected {}, observed {})",
                    disagreement.code,
                    disagreement.expected.as_deref().unwrap_or("none"),
                    disagreement.observed.as_deref().unwrap_or("none"),
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        write!(
            formatter,
            "route proof answered with a runtime that disagrees with current authority: {named}"
        )
    }
}

impl std::error::Error for PresenceDisagrees {}

/// What a direct challenge to a candidate endpoint produced.
enum CandidateAnswer {
    /// No answer: the reason it is still being waited on.
    Silent(String),
    Answered {
        evidence: RuntimePresenceEvidence,
        present: cultnet_rs::VerifiedRuntimePresence,
    },
}

//// Which kind of proof admits a target as Ready, derived from what the target's
/// recipe declares and from nothing else. Idunn infers no way to prove
/// readiness: a target that provides `odin.verse-rendezvous` is Odin itself, one
/// that declares a `shared-infrastructure odin.verse-rendezvous` dependency
/// advertises into the Verse and is Odin-correlated, and a routed one that
/// declares neither answers Idunn's own challenge and is route-proof. A target
/// with no route and no Odin dependency declares no proof at all, and is
/// refused. Odin collects the same receipts an Odin-correlated target does, and
/// its admitted generation is what names the Odin authority, so it is never
/// route-proof.
///
/// `of` is the only place a class is decided, and Expected is digest-bound, so
/// the class cannot drift from the incarnation it describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadinessClass {
    OdinSelf,
    OdinCorrelated,
    RouteProof,
}

/// A recipe that declares neither a route nor an Odin dependency, and is not
/// Odin: it has said nothing about how it proves readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UndeclaredReadiness {
    pub(crate) target: String,
}

impl std::fmt::Display for UndeclaredReadiness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "target {} declares no way to prove readiness: its recipe must declare a route \
             (Idunn challenges it) or a shared-infrastructure {ODIN_RENDEZVOUS_CAPABILITY} \
             dependency (Odin correlates it)",
            self.target
        )
    }
}

impl std::error::Error for UndeclaredReadiness {}

impl ReadinessClass {
    /// The class of a recipe's declarations. Plan compilation and `of` both
    /// call it, so admission refuses exactly what `of` would refuse.
    pub(crate) fn declared(
        target: &str,
        provides_odin: bool,
        declares_odin: bool,
        routed: bool,
    ) -> Result<Self, UndeclaredReadiness> {
        if provides_odin {
            Ok(Self::OdinSelf)
        } else if declares_odin {
            Ok(Self::OdinCorrelated)
        } else if routed {
            Ok(Self::RouteProof)
        } else {
            Err(UndeclaredReadiness {
                target: target.to_owned(),
            })
        }
    }

    pub(crate) fn of(
        expected: &IdunnExpectedIncarnationRecord,
    ) -> Result<Self, UndeclaredReadiness> {
        Self::declared(
            &expected.target,
            expected
                .capabilities
                .iter()
                .any(|capability| capability.capability == ODIN_RENDEZVOUS_CAPABILITY),
            expected.dependencies.iter().any(|dependency| {
                dependency.kind == "shared-infrastructure"
                    && dependency.capability == ODIN_RENDEZVOUS_CAPABILITY
            }),
            expected.route.is_some(),
        )
    }

    /// The class of `expected`, provided every receipt collected under it was
    /// vouched for by that class's voucher. The one way a stored record is
    /// asked what class it is.
    fn confirmed(
        expected: &IdunnExpectedIncarnationRecord,
        collected: impl IntoIterator<Item = Voucher>,
    ) -> Result<Self, ReadinessDisagreement> {
        let required = Self::of(expected).map_err(ReadinessDisagreement::Undeclared)?;
        match collected
            .into_iter()
            .find(|voucher| *voucher != required.voucher())
        {
            None => Ok(required),
            Some(collected) => Err(ReadinessDisagreement::WrongVoucher {
                target: expected.target.clone(),
                required,
                collected,
            }),
        }
    }

    /// Who vouches for a target of this class: Odin's correlation, or the
    /// candidate answering Idunn.
    fn voucher(self) -> Voucher {
        match self {
            Self::OdinSelf | Self::OdinCorrelated => Voucher::Odin,
            Self::RouteProof => Voucher::Candidate,
        }
    }
}

/// Who vouched for a piece of stored readiness evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Voucher {
    Odin,
    Candidate,
}

/// A stored record whose readiness evidence is not what its own Expected
/// requires. A record admitted before the target's recipe declared how it
/// proves readiness looks like this. It is reported and held, never repaired:
/// re-deriving the evidence around it would make the evidence tag a second
/// authority for the class.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReadinessDisagreement {
    Undeclared(UndeclaredReadiness),
    WrongVoucher {
        target: String,
        required: ReadinessClass,
        collected: Voucher,
    },
}

impl std::fmt::Display for ReadinessDisagreement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undeclared(undeclared) => write!(formatter, "{undeclared}"),
            Self::WrongVoucher {
                target,
                required,
                collected,
            } => write!(
                formatter,
                "target {target} is {required:?} but its stored readiness evidence was vouched \
                 for by {collected:?}: it was collected under a different class, and is held \
                 until the target is redeployed"
            ),
        }
    }
}

impl std::error::Error for ReadinessDisagreement {}

/// The receipt that admitted a candidate as Ready, tagged by who vouched for
/// it. The tag records what was collected; what a target must collect is
/// `ReadinessClass::of(expected)`, and a tag that differs is a
/// `ReadinessDisagreement`, never a second opinion about the class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "kebab-case", deny_unknown_fields)]
enum ReadinessEvidence {
    OdinCorrelated { evidence: TopologyEvidence },
    RouteProof { evidence: RuntimePresenceEvidence },
}

impl ReadinessEvidence {
    fn voucher(&self) -> Voucher {
        match self {
            Self::OdinCorrelated { .. } => Voucher::Odin,
            Self::RouteProof { .. } => Voucher::Candidate,
        }
    }

    fn validate_shape(&self) -> Result<()> {
        match self {
            Self::OdinCorrelated { evidence } => evidence.validate_shape(),
            Self::RouteProof { evidence } => evidence.validate_shape(),
        }
    }

    fn odin(&self) -> Option<&TopologyEvidence> {
        match self {
            Self::OdinCorrelated { evidence } => Some(evidence),
            Self::RouteProof { .. } => None,
        }
    }
}

// Signed evidence that the candidate process picked up the lease Idunn
/// granted: its presence carries `write_lease_sha256` equal to the grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseAdoptionEvidence {
    write_lease_sha256: String,
    signed_presence_sha256: String,
    source: AdoptionSource,
    observed_at_unix_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AdoptionSource {
    Direct,
    OdinTopology,
}

impl LeaseAdoptionEvidence {
    /// Whether this evidence is about exactly the lease that was granted.
    fn names(&self, leasing: &LeasingEvidence) -> bool {
        leasing.lease_sha256() == Some(self.write_lease_sha256.as_str())
    }

    fn validate_shape(&self) -> Result<()> {
        require_id(&self.write_lease_sha256, "adopted lease digest")?;
        require_id(&self.signed_presence_sha256, "lease adoption presence digest")?;
        ensure!(
            self.observed_at_unix_millis > 0,
            "lease adoption has no observation time"
        );
        Ok(())
    }
}

/// When the transaction entered its current post-fencing phase and when that
/// phase must end. Durations come from the plan's frozen `PhaseDeadlines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseDeadline {
    phase: DeploymentPhase,
    entered_at_unix_millis: u64,
    deadline_at_unix_millis: u64,
}

impl PhaseDeadline {
    /// `None` for a phase that has no deadline (pre-fencing and Complete).
    fn entering(
        phase: DeploymentPhase,
        plan: &CompiledDeploymentPlan,
        now: u64,
    ) -> Option<Self> {
        let deadlines = plan.phase_deadlines();
        let seconds = match phase {
            DeploymentPhase::Fencing => deadlines.fencing_seconds,
            DeploymentPhase::Leasing => deadlines.leasing_seconds,
            DeploymentPhase::AwaitingReady => deadlines.awaiting_ready_seconds,
            DeploymentPhase::Routing => deadlines.routing_seconds,
            DeploymentPhase::Committing => deadlines.committing_seconds,
            _ => return None,
        };
        Some(Self {
            phase,
            entered_at_unix_millis: now,
            deadline_at_unix_millis: now.saturating_add(u64::from(seconds) * 1000),
        })
    }
}

/// How a post-fencing failure recovers. `RestoreIncumbent` is what a
/// post-fencing abort has always done and is the default for records that
/// predate the field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
enum TerminalRecovery {
    RestartAdmitted,
    #[default]
    RestoreIncumbent,
    OperatorRequired {
        reason: String,
    },
}

/// Route supervision's durable memory for one admitted routed incarnation:
/// challenge pacing and the degradation mark. It belongs to the incarnation, so
/// a new generation starts from `default()`. What a target may actuate outlives
/// every generation and lives in `TargetSupervision`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteSupervisionState {
    last_challenge_at_unix_millis: Option<u64>,
    consecutive_failures: u32,
    next_challenge_at_unix_millis: Option<u64>,
    /// Set while route proof keeps failing. Marks the route degraded so
    /// dependents stop selecting it; it never authorizes a restart.
    degraded_since_unix_millis: Option<u64>,
}

impl RouteSupervisionState {
    /// The next challenge is not due yet.
    fn is_waiting(&self, now: u64, maximum_age_millis: u64) -> bool {
        self.next_challenge_at_unix_millis.is_some_and(|next| {
            is_waiting(
                now,
                next,
                ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS.max(maximum_age_millis),
            )
        })
    }

    /// A failed proof, or a failed repair of the fragment it needs. Observation
    /// state only: the route is marked degraded and the next challenge waits,
    /// twice as long each time, from the observation max age up to a cap.
    fn record_failed_challenge(&mut self, now: u64, maximum_age_millis: u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let doublings = self.consecutive_failures.saturating_sub(1).min(16);
        let wait = maximum_age_millis
            .saturating_mul(1 << doublings)
            .min(ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS)
            .max(maximum_age_millis);
        self.last_challenge_at_unix_millis = Some(now);
        self.next_challenge_at_unix_millis = Some(now.saturating_add(wait));
        self.degraded_since_unix_millis.get_or_insert(now);
    }

    fn record_proved_challenge(&mut self, now: u64) {
        self.consecutive_failures = 0;
        self.last_challenge_at_unix_millis = Some(now);
        self.next_challenge_at_unix_millis = None;
        self.degraded_since_unix_millis = None;
    }

    fn validate(&self) -> Result<()> {
        if let Some(since) = self.degraded_since_unix_millis {
            ensure!(since > 0, "route degradation has no start time");
        }
        Ok(())
    }
}

/// What one target has been allowed to do, kept across every generation and
/// present before the first: route actuations and continuity restarts, each a
/// sliding log of the times they happened. Owned by the meter primitives below;
/// the route driver only asks, and no generation write carries it.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.target_supervision",
    schema = "idunn.target_supervision.v1"
)]
struct TargetSupervision {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    target: String,
    /// The newest `ROUTE_ACTUATION_CEILING` actuation times, ascending.
    #[cultcache(key = 2)]
    route_actuations: Vec<u64>,
    /// The newest `CONTINUITY_RESTART_ATTEMPTS` restart times, ascending.
    #[cultcache(key = 3)]
    continuity_restarts: Vec<u64>,
    /// A restart whose projection could not be prepared waits until this time.
    #[cultcache(key = 4)]
    continuity_deferred_until: Option<u64>,
    #[cultcache(key = 5)]
    continuity_deferral_reason: Option<String>,
}

/// The entries of a sliding log still inside `window` at `now`. An entry from
/// the future means the clock stepped back; it counts as `now`, so a backwards
/// step holds a meter for at most one window however far the clock moved.
fn live_entries(log: &[u64], now: u64, window: u64) -> Vec<u64> {
    log.iter()
        .map(|&at| at.min(now))
        .filter(|&at| now - at < window)
        .collect()
}

/// Record `now`, keeping the newest `limit` entries. That is exact for "at most
/// `limit` in any window", even after a forced entry pushes past the limit.
fn record_entry(log: &mut Vec<u64>, now: u64, limit: usize) {
    clamp_to(log, now);
    log.push(now);
    if log.len() > limit {
        log.drain(..log.len() - limit);
    }
}

/// Make a stepped-back clock's future entries the present. `live_entries`
/// only reads them that way, so a log that is never written would keep
/// looking recent until the clock caught up; writing the clamp is what makes
/// a backwards step cost one window and no more.
fn clamp_to(log: &mut [u64], now: u64) {
    for at in log.iter_mut() {
        *at = (*at).min(now);
    }
}

impl TargetSupervision {
    fn new(target: &str) -> Self {
        Self {
            schema_version: TARGET_SUPERVISION_SCHEMA.into(),
            target: target.into(),
            route_actuations: Vec::new(),
            continuity_restarts: Vec::new(),
            continuity_deferred_until: None,
            continuity_deferral_reason: None,
        }
    }

    /// Clamp both logs to `now`. Callers that decide from a log write the
    /// result back when it changed.
    fn settle(&mut self, now: u64) {
        clamp_to(&mut self.route_actuations, now);
        clamp_to(&mut self.continuity_restarts, now);
    }

    fn route_used(&self, now: u64) -> usize {
        live_entries(&self.route_actuations, now, ROUTE_ACTUATION_WINDOW_MILLIS).len()
    }

    /// When the oldest counted actuation leaves the window, if the ceiling is reached.
    fn route_reopens_at(&self, now: u64) -> Option<u64> {
        let live = live_entries(&self.route_actuations, now, ROUTE_ACTUATION_WINDOW_MILLIS);
        (live.len() >= ROUTE_ACTUATION_CEILING)
            .then(|| live[live.len() - ROUTE_ACTUATION_CEILING] + ROUTE_ACTUATION_WINDOW_MILLIS)
    }

    /// Count one route actuation. Only a `Forward` change can be refused; a
    /// `Survival` is always admitted, and counted whenever the ledger can record it.
    fn charge_route(&mut self, now: u64, kind: RouteActuation) -> Result<(), RouteActuationRefused> {
        self.settle(now);
        if kind == RouteActuation::Forward
            && let Some(reopens_at) = self.route_reopens_at(now)
        {
            return Err(RouteActuationRefused {
                target: self.target.clone(),
                used: self.route_used(now),
                reopens_at_unix_millis: reopens_at,
            });
        }
        record_entry(&mut self.route_actuations, now, ROUTE_ACTUATION_CEILING);
        Ok(())
    }

    fn restarts_used(&self, now: u64) -> usize {
        live_entries(&self.continuity_restarts, now, CONTINUITY_RESTART_WINDOW_MILLIS).len()
    }

    fn restarts_exhausted(&self, now: u64) -> bool {
        self.restarts_used(now) >= CONTINUITY_RESTART_ATTEMPTS
    }

    /// The next restart is due one doubling wait after the last, from the
    /// number of restarts still inside the window.
    fn next_restart_at(&self, now: u64) -> Option<u64> {
        let live = live_entries(&self.continuity_restarts, now, CONTINUITY_RESTART_WINDOW_MILLIS);
        if live.is_empty() {
            return None;
        }
        let last = *self.continuity_restarts.last()?;
        Some(last.saturating_add(CONTINUITY_RESTART_BACKOFF_MILLIS << (live.len() - 1)))
    }

    /// Whether continuity must still wait: the doubling wait after the last
    /// restart, or the deferral after a projection that would not demote.
    fn continuity_is_waiting(&self, now: u64) -> bool {
        self.next_restart_at(now).is_some_and(|due| {
            is_waiting(
                now,
                due,
                CONTINUITY_RESTART_BACKOFF_MILLIS << (CONTINUITY_RESTART_ATTEMPTS - 1),
            )
        }) || self
            .continuity_deferred_until
            .is_some_and(|due| is_waiting(now, due, CONTINUITY_DEFERRAL_MILLIS))
    }

    /// One restart scheduled. It ends any deferral: the projection was prepared.
    fn record_restart(&mut self, now: u64) {
        record_entry(&mut self.continuity_restarts, now, CONTINUITY_RESTART_ATTEMPTS);
        self.continuity_deferred_until = None;
        self.continuity_deferral_reason = None;
    }

    fn defer_continuity(&mut self, now: u64, reason: &str) {
        self.continuity_deferred_until = Some(now.saturating_add(CONTINUITY_DEFERRAL_MILLIS));
        self.continuity_deferral_reason = Some(reason.to_owned());
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == TARGET_SUPERVISION_SCHEMA,
            "target supervision schema is unsupported"
        );
        require_id(&self.target, "supervised target")?;
        for (log, limit, what) in [
            (&self.route_actuations, ROUTE_ACTUATION_CEILING, "route actuation"),
            (&self.continuity_restarts, CONTINUITY_RESTART_ATTEMPTS, "continuity restart"),
        ] {
            ensure!(log.len() <= limit, "{what} log exceeds its bound");
            ensure!(
                log.iter().all(|&at| at > 0) && log.windows(2).all(|pair| pair[0] <= pair[1]),
                "{what} log is not an ascending list of times"
            );
        }
        ensure!(
            self.continuity_deferred_until.is_some() == self.continuity_deferral_reason.is_some(),
            "continuity deferral has a time or a reason but not both"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
enum TransactionCompletion {
    Admitted { generation_id: String },
    FailedBeforeFencing { error: String },
    FailedAfterFencing {
        error: String,
        #[serde(default)]
        recovery: TerminalRecovery,
    },
}

use cleanup_evidence::CleanupEvidence;

/// Where the legacy marker lives, so that only the lift can make one.
///
/// `LegacyLift` has a private field: outside this module no expression can
/// construct it, so no other code can write `CleanupEvidence::Legacy(..)`.
/// Decoding a stored record is the one other way in, and it stays here too.
/// `transaction_envelope` refuses a record carrying the marker; the schema
/// migration, which persists what the lift produced, writes through
/// `migrate_transaction_record`, and a fault note on an existing record through
/// `last_error_envelope`; both are defined here beside the only constructor,
/// and the encoder they share is private to this module.
mod cleanup_evidence {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct LegacyLift(());

    const LEGACY_MARKER: &str = "legacy-skipped-before-b1";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum CleanupEvidence {
        Pending,
        Skipped,
        Complete,
        /// Set only by the legacy lift, on a terminal continuity abort written
        /// before the single resolution rule: it issued an activation and
        /// recorded `Skipped`. Boot reconciliation, not the record, owes that
        /// residue's demotion. The marker is what lets validation accept the
        /// old shape without reopening a finished transaction.
        Legacy(LegacyLift),
    }

    impl CleanupEvidence {
        pub(super) fn is_complete(self) -> bool {
            !matches!(self, Self::Pending)
        }

        pub(super) fn is_legacy_marker(self) -> bool {
            matches!(self, Self::Legacy(_))
        }
    }

    impl Serialize for CleanupEvidence {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(match self {
                Self::Pending => "pending",
                Self::Skipped => "skipped",
                Self::Complete => "complete",
                Self::Legacy(_) => LEGACY_MARKER,
            })
        }
    }

    impl<'de> Deserialize<'de> for CleanupEvidence {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let text = String::deserialize(deserializer)?;
            match text.as_str() {
                "pending" => Ok(Self::Pending),
                "skipped" => Ok(Self::Skipped),
                "complete" => Ok(Self::Complete),
                LEGACY_MARKER => Ok(Self::Legacy(LegacyLift(()))),
                other => Err(serde::de::Error::unknown_variant(
                    other,
                    &["pending", "skipped", "complete", LEGACY_MARKER],
                )),
            }
        }
    }

    impl DeploymentTransaction {
        pub(super) fn carries_legacy_marker(&self) -> bool {
            self.pre_fencing_abort
                .as_ref()
                .map(|abort| abort.topology_reconciliation)
                .into_iter()
                .chain(
                    self.post_fencing_abort
                        .as_ref()
                        .map(|abort| abort.topology_reconciliation),
                )
                .any(CleanupEvidence::is_legacy_marker)
        }
    }

    /// The schema migration's one entry point: read a stored v2, v3 or v4
    /// transaction (lifting older ones) and return its key and current-shape
    /// envelope. The record may carry the marker the lift just set.
    pub(super) fn migrate_transaction_record(
        envelope: &CultCacheEnvelope,
    ) -> Result<(String, CultCacheEnvelope)> {
        let value = read_transaction_record(envelope)?;
        let next = encode_transaction(&value, value.updated_at_unix_millis)?;
        Ok((value.transaction_id, next))
    }

    /// The envelope for an existing record with only its `last_error` set. The
    /// record may already carry the marker; this cannot add one.
    pub(super) fn last_error_envelope(
        value: &DeploymentTransaction,
        detail: &str,
    ) -> Result<CultCacheEnvelope> {
        let mut next = value.clone();
        next.last_error = Some(detail.to_string());
        encode_transaction(&next, next.updated_at_unix_millis)
    }

/// A continuity abort written before the single resolution rule recorded no
/// projection cleanup although its transaction had issued an activation, and
/// so left that activation standing.
///
/// Nothing is reopened. A record already terminal keeps its terminal phase and
/// completion and is marked the legacy marker, the one shape validation
/// accepts for a terminal abort that owes nothing yet issued an activation;
/// boot reconciliation, the single owner of that residue, demotes it by exact
/// activation. A record still in flight lifts to `Pending`: it already holds
/// its target's authority (a live record claims it before and after the lift),
/// so the lift adds no claimant, and its own abort demotes the activation.
/// Only the control store's read applies this: history describes what happened
/// and is lifted unchanged.
pub(super) fn owe_legacy_continuity_projection(transaction: &mut DeploymentTransaction) {
    if transaction.command_kind != CommandKind::Continuity || transaction.activation.is_none() {
        return;
    }
    let terminal = transaction.completion.is_some();
    if let Some(abort) = transaction.pre_fencing_abort.as_mut()
        && abort.topology_reconciliation == CleanupEvidence::Skipped
    {
        abort.topology_reconciliation = if terminal {
            CleanupEvidence::Legacy(LegacyLift(()))
        } else {
            CleanupEvidence::Pending
        };
    }
    if let Some(abort) = transaction.post_fencing_abort.as_mut()
        && abort.topology_reconciliation == CleanupEvidence::Skipped
        && !terminal
    {
        abort.topology_reconciliation = CleanupEvidence::Pending;
    }
}

}

/// Rollback of a transaction that has already fenced the incumbent.
///
/// Fencing stops the admitted incarnation and revokes its write lease, so a
/// candidate that then dies for good leaves the target with nothing serving it.
/// Retrying forever cannot fix that -- a transient unit with `Restart=no` will
/// not come back -- and it holds the target, leaving every later command queued
/// behind a transaction that can never finish.
///
/// So the candidate's own artifacts are withdrawn in the order that never
/// leaves two writers: route first, then the write lease, then the process,
/// then the projection. Reconciling the projection restores the incumbent's
/// admitted Expected where there is one, which is what lets continuity bring
/// the incumbent back; where there is none, the failed Expected is withdrawn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostFencingAbort {
    error: String,
    route_restoration: CleanupEvidence,
    lease_withdrawal: CleanupEvidence,
    candidate_cleanup: CleanupEvidence,
    topology_reconciliation: CleanupEvidence,
    source_cleanup: CleanupEvidence,
}

impl PostFencingAbort {
    fn is_complete(&self) -> bool {
        self.route_restoration.is_complete()
            && self.lease_withdrawal.is_complete()
            && self.candidate_cleanup.is_complete()
            && self.topology_reconciliation.is_complete()
            && self.source_cleanup.is_complete()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreFencingAbort {
    error: String,
    candidate_cleanup: CleanupEvidence,
    topology_reconciliation: CleanupEvidence,
    source_cleanup: CleanupEvidence,
}

impl PreFencingAbort {
    fn is_complete(&self) -> bool {
        self.candidate_cleanup.is_complete()
            && self.topology_reconciliation.is_complete()
            && self.source_cleanup.is_complete()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case", deny_unknown_fields)]
enum IncumbentCleanupEvidence {
    SkippedNoIncumbent,
    Pending {
        generation_id: String,
        workload: WorkloadObservation,
    },
    Complete {
        generation_id: String,
    },
}

impl IncumbentCleanupEvidence {
    fn is_complete(&self) -> bool {
        matches!(self, Self::SkippedNoIncumbent | Self::Complete { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SourceCleanupEvidence {
    SkippedContinuity,
    Pending,
    Complete,
}

impl SourceCleanupEvidence {
    fn is_complete(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostCommitCleanup {
    incumbent: IncumbentCleanupEvidence,
    source: SourceCleanupEvidence,
}

impl PostCommitCleanup {
    fn is_complete(&self) -> bool {
        self.incumbent.is_complete() && self.source.is_complete()
    }
}

/// The sole owner of an in-flight deployment decision. Every actuator result
/// is durable here before a later phase is entered.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.deployment_transaction",
    schema = "idunn.deployment_transaction.v4"
)]
struct DeploymentTransaction {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    transaction_id: String,
    #[cultcache(key = 2)]
    command_id: String,
    #[cultcache(key = 3)]
    command_kind: CommandKind,
    #[cultcache(key = 4)]
    target: String,
    #[cultcache(key = 5)]
    ordinal: u32,
    #[cultcache(key = 6)]
    phase: DeploymentPhase,
    #[cultcache(key = 7)]
    created_at_unix_millis: u64,
    #[cultcache(key = 8)]
    updated_at_unix_millis: u64,
    #[cultcache(key = 9)]
    incumbent_generation_id: Option<String>,
    #[cultcache(key = 10)]
    plan: Option<CompiledDeploymentPlan>,
    #[cultcache(key = 11)]
    frozen_source: Option<FrozenSourceReceipt>,
    #[cultcache(key = 12)]
    sealed_release: Option<SealedRelease>,
    #[cultcache(key = 13)]
    installed_release: Option<InstalledReleaseObservation>,
    #[cultcache(key = 14)]
    expected: Option<IdunnExpectedIncarnationRecord>,
    #[cultcache(key = 15)]
    expected_publication_sha256: Option<String>,
    #[cultcache(key = 16)]
    deployment_authorization: Option<DeploymentAuthorization>,
    #[cultcache(key = 17)]
    lifecycle_authorized_at_unix_millis: Option<u64>,
    #[cultcache(key = 18)]
    activation: Option<IdunnRuntimeActivationRecord>,
    #[cultcache(key = 19)]
    workload: Option<WorkloadObservation>,
    #[cultcache(key = 20)]
    activation_publication_sha256: Option<String>,
    #[cultcache(key = 21)]
    latest_odin_observation: Option<TopologyEvidence>,
    #[cultcache(key = 22)]
    warming: Option<WarmingEvidence>,
    #[cultcache(key = 23)]
    route_preflight: Option<RoutePreflightReceipt>,
    #[cultcache(key = 24)]
    isolation: Option<IsolationEvidence>,
    #[cultcache(key = 25)]
    fencing: Option<FencingEvidence>,
    #[cultcache(key = 26)]
    leasing: Option<LeasingEvidence>,
    #[cultcache(key = 27)]
    ready: Option<ReadinessEvidence>,
    #[cultcache(key = 28)]
    routing: Option<RoutingEvidence>,
    #[cultcache(key = 29)]
    odin_publisher_sequence_cursor: u64,
    #[cultcache(key = 30)]
    last_error: Option<String>,
    #[cultcache(key = 31)]
    completion: Option<TransactionCompletion>,
    #[cultcache(key = 32)]
    pre_fencing_abort: Option<PreFencingAbort>,
    #[cultcache(key = 33)]
    post_commit_cleanup: Option<PostCommitCleanup>,
    //  because every transaction already durable predates this field:
    // an absent key 34 decodes as None, which is exactly "this transaction has
    // no post-fencing abort". Without it the daemon refuses to read its own
    // control store and crashloops.
    #[cultcache(key = 34, default)]
    post_fencing_abort: Option<PostFencingAbort>,
    /// Set by `transition` on entry to each post-fencing phase.
    #[cultcache(key = 35)]
    phase_deadline: Option<PhaseDeadline>,
    #[cultcache(key = 36)]
    lease_adoption: Option<LeaseAdoptionEvidence>,
}

impl DeploymentTransaction {
    /// The one writer of `phase`. It stamps the update time and the phase
    /// deadline together, so a record outside Fencing..=Committing carries no
    /// deadline by construction. No other code assigns `phase`.
    fn enter_phase(&mut self, phase: DeploymentPhase, now: u64) {
        self.phase = phase;
        self.updated_at_unix_millis = now;
        self.phase_deadline = self
            .plan
            .as_ref()
            .and_then(|plan| PhaseDeadline::entering(phase, plan, now));
    }

    /// Whether this transaction's binding declares stop-then-start. A
    /// transaction without a plan yet cannot, so it reads as false.
    fn rollout_stops_incumbent_first(&self) -> bool {
        self.plan
            .as_ref()
            .and_then(|plan| plan.parsed_inputs().ok())
            .is_some_and(|(_, binding)| binding.rollout.strategy == RolloutStrategy::StopThenStart)
    }

    fn new(
        command: &DeploymentCommand,
        target: String,
        ordinal: u32,
        incumbent: Option<&AdmittedGeneration>,
        now: u64,
    ) -> Result<Self> {
        let transaction = Self {
            schema_version: DEPLOYMENT_TRANSACTION_SCHEMA.into(),
            transaction_id: format!("tx-{}", Uuid::new_v4()),
            command_id: command.command_id.clone(),
            command_kind: command.kind,
            target,
            ordinal,
            phase: DeploymentPhase::Sealing,
            created_at_unix_millis: now,
            updated_at_unix_millis: now,
            incumbent_generation_id: incumbent.map(|value| value.generation_id.clone()),
            plan: None,
            frozen_source: None,
            sealed_release: None,
            installed_release: None,
            expected: None,
            expected_publication_sha256: None,
            deployment_authorization: None,
            lifecycle_authorized_at_unix_millis: None,
            activation: None,
            workload: None,
            activation_publication_sha256: None,
            latest_odin_observation: None,
            warming: None,
            route_preflight: None,
            isolation: None,
            fencing: None,
            leasing: None,
            ready: None,
            routing: None,
            odin_publisher_sequence_cursor: incumbent
                .map_or(0, |value| value.odin_publisher_sequence_cursor),
            last_error: None,
            completion: None,
            pre_fencing_abort: None,
            post_fencing_abort: None,
            post_commit_cleanup: None,
            phase_deadline: None,
            lease_adoption: None,
        };
        transaction.validate()?;
        Ok(transaction)
    }

    fn from_continuity(
        command: &DeploymentCommand,
        incumbent: &AdmittedGeneration,
        now: u64,
    ) -> Result<Self> {
        let mut transaction =
            Self::new(command, incumbent.target.clone(), 0, Some(incumbent), now)?;
        transaction.plan = Some(incumbent.plan.clone());
        transaction.sealed_release = Some(incumbent.sealed_release.clone());
        transaction.installed_release = Some(incumbent.installed_release.clone());
        transaction.expected = Some(incumbent.expected.clone());
        transaction.validate()?;
        Ok(transaction)
    }

    /// Whether an abort owes the projection a reconciliation. A deployment
    /// published its own Expected key. A continuity shares the admitted key,
    /// whose Expected is never withdrawn, so it owes only the demotion of the
    /// activation it issued.
    fn abort_topology_reconciliation(&self) -> CleanupEvidence {
        let owes = match self.command_kind {
            CommandKind::Deploy => self.expected_publication_sha256.is_some(),
            CommandKind::Continuity => self.activation.is_some(),
        };
        if owes {
            CleanupEvidence::Pending
        } else {
            CleanupEvidence::Skipped
        }
    }

    fn rejected(command: &DeploymentCommand, error: anyhow::Error, now: u64) -> Result<Self> {
        let mut transaction = Self::new(command, command.selector.clone(), 0, None, now)?;
        let detail = truncate(&format!("{error:#}"), 2048);
        transaction.enter_phase(DeploymentPhase::Complete, now);
        transaction.last_error = Some(detail.clone());
        transaction.pre_fencing_abort = Some(PreFencingAbort {
            error: detail.clone(),
            candidate_cleanup: CleanupEvidence::Skipped,
            topology_reconciliation: CleanupEvidence::Skipped,
            source_cleanup: CleanupEvidence::Skipped,
        });
        transaction.completion = Some(TransactionCompletion::FailedBeforeFencing { error: detail });
        transaction.validate()?;
        Ok(transaction)
    }

    fn is_terminal(&self) -> bool {
        if self.phase != DeploymentPhase::Complete {
            return false;
        }
        match &self.completion {
            Some(TransactionCompletion::FailedBeforeFencing { .. }) => self
                .pre_fencing_abort
                .as_ref()
                .is_some_and(PreFencingAbort::is_complete),
            Some(TransactionCompletion::FailedAfterFencing { .. }) => self
                .post_fencing_abort
                .as_ref()
                .is_some_and(PostFencingAbort::is_complete),
            Some(TransactionCompletion::Admitted { .. }) => self
                .post_commit_cleanup
                .as_ref()
                .is_some_and(PostCommitCleanup::is_complete),
            None => false,
        }
    }

    /// Complete transactions retain retryable cleanup work, but admission has
    /// already transferred current-incarnation authority to AdmittedGeneration.
    fn owns_target_authority(&self) -> bool {
        self.phase != DeploymentPhase::Complete
    }

    /// Admission has moved to the new generation at Complete, but an exact
    /// draining incumbent still reserves its process and candidate endpoint.
    /// No later mutation for the same target may overlap that cleanup.
    fn blocks_new_target_mutation(&self) -> bool {
        self.owns_target_authority()
            || self
                .post_commit_cleanup
                .as_ref()
                .is_some_and(|cleanup| !cleanup.is_complete())
    }

    /// Where what this transaction has collected disagrees with what its own
    /// Expected requires. There is nothing to disagree with before Expected.
    fn readiness_disagreement(&self) -> Option<ReadinessDisagreement> {
        let expected = self.expected.as_ref()?;
        ReadinessClass::confirmed(
            expected,
            self.warming
                .iter()
                .map(WarmingEvidence::voucher)
                .chain(self.ready.iter().map(ReadinessEvidence::voucher)),
        )
        .err()
    }

    /// The disagreement that holds this transaction: evidence collected under
    /// another class is held and reported, not advanced and not repaired, since
    /// no step can make it the right kind and running one anyway only fails it
    /// every tick. What is finished, or being finished (a completion, or an
    /// abort under way), never reads the class, so it is left to run.
    fn held_disagreement(&self) -> Option<ReadinessDisagreement> {
        let finishing = self.completion.is_some()
            || self.pre_fencing_abort.is_some()
            || self.post_fencing_abort.is_some();
        self.readiness_disagreement().filter(|_| !finishing)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == DEPLOYMENT_TRANSACTION_SCHEMA,
            "deployment transaction schema is unsupported"
        );
        require_id(&self.transaction_id, "deployment transaction id")?;
        require_id(&self.command_id, "transaction command id")?;
        require_id(&self.target, "transaction target")?;
        ensure!(
            self.created_at_unix_millis > 0 && self.updated_at_unix_millis > 0,
            "deployment transaction timestamps are invalid"
        );
        if let Some(generation) = &self.incumbent_generation_id {
            require_id(generation, "incumbent generation id")?;
        }
        if let Some(plan) = &self.plan {
            plan.validate()?;
            ensure!(
                plan.parsed_inputs()?.1.target == self.target,
                "transaction plan belongs to another target"
            );
        }
        if let (Some(plan), Some(frozen)) = (&self.plan, &self.frozen_source) {
            frozen.validate_against(plan)?;
        }
        if let (Some(plan), Some(release)) = (&self.plan, &self.sealed_release) {
            release.validate_against(plan)?;
        }
        if let Some(expected) = &self.expected {
            expected.validate()?;
            ensure!(expected.target == self.target, "Expected target differs");
            if let Some(plan) = &self.plan {
                ensure!(expected.plan_id == plan.plan_id, "Expected plan differs");
            }
            if let Some(release) = &self.sealed_release {
                ensure!(
                    expected.sealed_release_id == release.sealed_release_id,
                    "Expected release differs"
                );
            }
            if let Some(digest) = &self.expected_publication_sha256 {
                ensure!(
                    digest == &expected.canonical_sha256()?,
                    "Expected publication receipt differs"
                );
            }
        }
        if let (Some(expected), Some(activation)) = (&self.expected, &self.activation) {
            activation.validate()?;
            ensure!(
                activation.expected_projection_sha256 == expected.canonical_sha256()?,
                "activation belongs to another Expected projection"
            );
            if let Some(workload) = &self.workload {
                ensure!(
                    workload.runtime_instance_id() == activation.runtime_instance_id,
                    "workload belongs to another activation"
                );
            }
            if let Some(digest) = &self.activation_publication_sha256 {
                ensure!(
                    digest == &activation.canonical_sha256()?,
                    "activation publication receipt differs"
                );
            }
        }
        ensure!(
            self.workload.is_none() || self.activation.is_some(),
            "workload observation exists without its prepared activation"
        );
        let ready_odin = self.ready.as_ref().and_then(ReadinessEvidence::odin);
        for evidence in [self.latest_odin_observation.as_ref(), ready_odin]
            .into_iter()
            .flatten()
        {
            evidence.validate_shape()?;
            ensure!(
                evidence.publisher_sequence <= self.odin_publisher_sequence_cursor,
                "topology evidence exceeds the transaction replay cursor"
            );
        }
        if let Some(ready) = &self.ready {
            ready.validate_shape()?;
            // A candidate-vouched receipt is only ever written by this code, so
            // one on a target that is not route-proof is forged. The other
            // direction -- an Odin receipt on a route-proof target -- is what a
            // pre-B3 record looks like, and is a ReadinessDisagreement to hold,
            // not a record to refuse: refusing it would refuse to boot.
            if let (Voucher::Candidate, Some(expected)) = (ready.voucher(), &self.expected) {
                ensure!(
                    ReadinessClass::of(expected).is_ok_and(|class| class == ReadinessClass::RouteProof),
                    "route-proof Ready evidence for a target that is not route-proof"
                );
            }
        }
        if let Some(warming) = &self.warming {
            warming.validate_shape()?;
            match warming {
                WarmingEvidence::OdinTopology { evidence } => ensure!(
                    evidence.publisher_sequence <= self.odin_publisher_sequence_cursor,
                    "Warming topology evidence exceeds the transaction replay cursor"
                ),
                WarmingEvidence::FirstOdinDirect { .. } => ensure!(
                    self.expected.as_ref().is_some_and(|expected| {
                        ReadinessClass::of(expected).is_ok_and(|class| class == ReadinessClass::OdinSelf)
                    }),
                    "direct Warming evidence is reserved for Odin observing itself"
                ),
                WarmingEvidence::RouteProofDirect { .. } => ensure!(
                    self.expected.as_ref().is_none_or(|expected| {
                        ReadinessClass::of(expected).is_ok_and(|class| class == ReadinessClass::RouteProof)
                    }),
                    "direct route-proof Warming evidence for a target that is not route-proof"
                ),
            }
        }
        if let Some(leasing) = &self.leasing {
            if let Some(expected) = &self.expected {
                ensure!(
                    matches!(
                        (expected.write_lease_required, leasing),
                        (true, LeasingEvidence::Prepared { .. })
                            | (true, LeasingEvidence::Granted { .. })
                            | (false, LeasingEvidence::SkippedStateless)
                    ),
                    "lease evidence differs from the Expected state contract"
                );
            }
            let lease_and_sha256 = match leasing {
                LeasingEvidence::SkippedStateless => None,
                LeasingEvidence::Prepared {
                    lease,
                    lease_sha256,
                }
                | LeasingEvidence::Granted {
                    lease,
                    lease_sha256,
                } => Some((lease, lease_sha256)),
            };
            if let Some((lease, lease_sha256)) = lease_and_sha256 {
                lease.validate()?;
                ensure!(
                    lease.canonical_sha256()? == *lease_sha256,
                    "lease evidence digest differs"
                );
            }
        }
        if let (Some(expected), Some(fencing)) = (&self.expected, &self.fencing) {
            match fencing {
                FencingEvidence::SkippedStateless => ensure!(
                    !expected.write_lease_required,
                    "stateful candidate claims stateless fencing"
                ),
                FencingEvidence::Revoked {
                    candidate_lease_path_verified_empty,
                    ..
                } => ensure!(
                    *candidate_lease_path_verified_empty == expected.write_lease_required,
                    "candidate lease-path fencing differs from Expected"
                ),
            }
        }
        if let Some(error) = &self.last_error {
            require_detail(error, "transaction error")?;
        }
        if let Some(deadline) = &self.phase_deadline {
            ensure!(
                deadline.phase == self.phase
                    && (DeploymentPhase::Fencing..=DeploymentPhase::Committing)
                        .contains(&deadline.phase)
                    && deadline.entered_at_unix_millis > 0
                    && deadline.deadline_at_unix_millis > deadline.entered_at_unix_millis,
                "phase deadline does not describe the current post-fencing phase"
            );
        }
        if let Some(adoption) = &self.lease_adoption {
            adoption.validate_shape()?;
            ensure!(
                self.leasing
                    .as_ref()
                    .is_some_and(|leasing| adoption.names(leasing)),
                "lease adoption does not name the granted lease"
            );
        }
        if let Some(authorization) = &self.deployment_authorization {
            authorization.validate_shape()?;
        }
        if let Some(abort) = &self.pre_fencing_abort {
            require_detail(&abort.error, "pre-fencing abort error")?;
            ensure!(
                self.post_commit_cleanup.is_none(),
                "aborted transaction also carries post-commit cleanup"
            );
            ensure!(
                matches!(
                    (
                        self.workload.is_some() || self.activation.is_some(),
                        abort.candidate_cleanup
                    ),
                    (true, CleanupEvidence::Pending | CleanupEvidence::Complete)
                        | (false, CleanupEvidence::Skipped)
                ),
                "abort candidate cleanup differs from its prepared activation"
            );
            ensure!(
                matches!(
                    (
                        self.abort_topology_reconciliation(),
                        abort.topology_reconciliation
                    ),
                    (
                        CleanupEvidence::Pending,
                        CleanupEvidence::Pending | CleanupEvidence::Complete
                    ) | (CleanupEvidence::Skipped, CleanupEvidence::Skipped)
                ) || (self.command_kind == CommandKind::Continuity
                    && self.phase == DeploymentPhase::Complete
                    && self.completion.is_some()
                    && abort.topology_reconciliation.is_legacy_marker()),
                "abort topology cleanup differs from what the transaction projected"
            );
            ensure!(
                matches!(
                    (self.command_kind, abort.source_cleanup),
                    (
                        CommandKind::Deploy,
                        CleanupEvidence::Pending | CleanupEvidence::Complete
                    ) | (CommandKind::Continuity, CleanupEvidence::Skipped)
                ) || (self.command_kind == CommandKind::Deploy
                    && abort.source_cleanup == CleanupEvidence::Skipped
                    && self.plan.is_none()
                    && self.frozen_source.is_none()
                    && self.expected.is_none()
                    && self.phase == DeploymentPhase::Complete),
                "abort source cleanup differs from command work"
            );
            ensure!(
                self.phase < DeploymentPhase::Fencing || self.phase == DeploymentPhase::Complete,
                "pre-fencing abort crossed the fencing boundary"
            );
        }

        if let Some(abort) = &self.post_fencing_abort {
            require_detail(&abort.error, "post-fencing abort error")?;
            ensure!(
                self.pre_fencing_abort.is_none() && self.post_commit_cleanup.is_none(),
                "post-fencing abort collides with another terminal path"
            );
            ensure!(
                self.phase >= DeploymentPhase::Fencing,
                "post-fencing abort exists before the fence"
            );
        }

        let failed_after_fencing = matches!(
            self.completion,
            Some(TransactionCompletion::FailedAfterFencing { .. })
        );
        if failed_after_fencing {
            ensure!(
                self.phase == DeploymentPhase::Complete,
                "terminal failure is not Complete"
            );
            let abort = required(
                &self.post_fencing_abort,
                "terminal post-fence abort evidence",
            )?;
            let TransactionCompletion::FailedAfterFencing { error, recovery } =
                self.completion.as_ref().unwrap()
            else {
                unreachable!()
            };
            if let TerminalRecovery::OperatorRequired { reason } = recovery {
                require_detail(reason, "operator-required recovery reason")?;
            }
            ensure!(
                abort.is_complete() && abort.error == *error,
                "terminal failure lacks complete matching abort evidence"
            );
            return Ok(());
        }

        let failed = matches!(
            self.completion,
            Some(TransactionCompletion::FailedBeforeFencing { .. })
        );
        if failed {
            ensure!(
                self.phase == DeploymentPhase::Complete,
                "terminal failure is not Complete"
            );
            let abort = required(&self.pre_fencing_abort, "terminal abort evidence")?;
            let TransactionCompletion::FailedBeforeFencing { error } =
                self.completion.as_ref().unwrap()
            else {
                unreachable!()
            };
            ensure!(
                abort.is_complete() && abort.error == *error,
                "terminal failure lacks complete matching abort evidence"
            );
        } else {
            ensure!(
                !matches!(
                    self.completion,
                    Some(TransactionCompletion::FailedBeforeFencing { .. })
                ),
                "non-failure branch carries failure completion"
            );
            if self.phase >= DeploymentPhase::Starting {
                ensure!(
                    self.plan.is_some()
                        && self.sealed_release.is_some()
                        && self.installed_release.is_some()
                        && self.expected.is_some(),
                    "Starting transaction lacks sealed release evidence"
                );
                // Expected is published as Starting's first step, after the
                // brake; from the activation onward it must be there.
                ensure!(
                    self.expected_publication_sha256.is_some()
                        || (self.activation.is_none() && self.phase == DeploymentPhase::Starting),
                    "started transaction has not published its Expected"
                );
                match self.command_kind {
                    CommandKind::Deploy => ensure!(
                        self.frozen_source.is_some() && self.deployment_authorization.is_some(),
                        "deployment entered Starting without frozen source and brake consumption"
                    ),
                    CommandKind::Continuity => ensure!(
                        self.lifecycle_authorized_at_unix_millis.is_some()
                            && self.deployment_authorization.is_none(),
                        "continuity entered Starting without its lifecycle gate"
                    ),
                }
            }
            if self.phase >= DeploymentPhase::Warming {
                ensure!(
                    self.activation.is_some()
                        && self.workload.is_some()
                        && self.activation_publication_sha256.is_some(),
                    "Warming transaction lacks observed activation"
                );
            }
            if self.phase >= DeploymentPhase::Fencing {
                ensure!(
                    self.warming.is_some() && self.isolation.is_some(),
                    "Fencing transaction lacks warming or isolation evidence"
                );
                ensure!(
                    self.expected
                        .as_ref()
                        .and_then(|expected| expected.route.as_ref())
                        .is_some()
                        == self.route_preflight.is_some(),
                    "route preflight does not match routed Expected"
                );
                if let Some(preflight) = &self.route_preflight {
                    preflight.validate()?;
                }
            }
            if self.phase >= DeploymentPhase::Leasing {
                ensure!(self.fencing.is_some(), "Leasing lacks fencing evidence");
            }
            if self.phase >= DeploymentPhase::AwaitingReady {
                ensure!(
                    matches!(
                        self.leasing.as_ref(),
                        Some(LeasingEvidence::SkippedStateless)
                            | Some(LeasingEvidence::Granted { .. })
                    ),
                    "AwaitingReady lacks finalized lease evidence"
                );
            }
            if self.phase >= DeploymentPhase::Routing {
                ensure!(self.ready.is_some(), "Routing lacks Ready evidence");
            }
            if self.phase >= DeploymentPhase::Committing {
                ensure!(self.routing.is_some(), "Committing lacks routing evidence");
            }
            if let Some(RoutingEvidence::Promoted {
                observation,
                promoted_at_unix_millis,
            }) = &self.routing
            {
                observation.validate()?;
                ensure!(*promoted_at_unix_millis > 0, "route promotion has no time");
            }
            if self.phase == DeploymentPhase::Complete {
                ensure!(
                    matches!(
                        self.completion,
                        Some(TransactionCompletion::Admitted { .. })
                    ),
                    "successful Complete transaction lacks admission receipt"
                );
                ensure!(
                    self.pre_fencing_abort.is_none() && self.post_commit_cleanup.is_some(),
                    "admitted transaction lacks exclusive post-commit cleanup evidence"
                );
                let TransactionCompletion::Admitted { generation_id } =
                    self.completion.as_ref().unwrap()
                else {
                    unreachable!()
                };
                ensure!(
                    generation_id == &format!("generation-{}", self.transaction_id),
                    "admission completion names another transaction generation"
                );
            } else {
                ensure!(
                    self.completion.is_none() && self.post_commit_cleanup.is_none(),
                    "non-Complete transaction has completion or post-commit state"
                );
            }
        }
        if let Some(cleanup) = &self.post_commit_cleanup {
            ensure!(
                self.phase == DeploymentPhase::Complete
                    && matches!(
                        self.completion,
                        Some(TransactionCompletion::Admitted { .. })
                    ),
                "post-commit cleanup exists without an admitted Complete transaction"
            );
            match (&self.incumbent_generation_id, &cleanup.incumbent) {
                (None, IncumbentCleanupEvidence::SkippedNoIncumbent) => {}
                (
                    Some(expected),
                    IncumbentCleanupEvidence::Pending { generation_id, .. }
                    | IncumbentCleanupEvidence::Complete { generation_id },
                ) if expected == generation_id => {}
                _ => bail!("post-commit incumbent cleanup names another generation"),
            }
            ensure!(
                matches!(
                    (self.command_kind, cleanup.source),
                    (CommandKind::Deploy, SourceCleanupEvidence::Pending)
                        | (CommandKind::Deploy, SourceCleanupEvidence::Complete)
                        | (
                            CommandKind::Continuity,
                            SourceCleanupEvidence::SkippedContinuity
                        )
                ),
                "post-commit source cleanup differs from command kind"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmittedOdinAuthority {
    signer_identity_id: String,
    signer_public_key: Vec<u8>,
}

impl AdmittedOdinAuthority {
    fn from_anchor(anchor: &ServiceIdentityTrustAnchor) -> Result<Self> {
        ensure!(
            derive_service_identity_id::<OdinTopologyIdentity>(&anchor.public_key)?
                == anchor.identity_id,
            "Odin topology anchor identity differs from its key"
        );
        Ok(Self {
            signer_identity_id: anchor.identity_id.clone(),
            signer_public_key: anchor.public_key.clone(),
        })
    }

    fn validate(&self) -> Result<()> {
        require_id(&self.signer_identity_id, "admitted Odin signer")?;
        ensure!(
            derive_service_identity_id::<OdinTopologyIdentity>(&self.signer_public_key)?
                == self.signer_identity_id,
            "admitted Odin signer identity differs from its key"
        );
        Ok(())
    }
}

/// The only current-incarnation owner. Unit state, route state, topology
/// projections, and CLI output are observations of this record, never peers.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.admitted_generation",
    schema = "idunn.admitted_generation.v4"
)]
struct AdmittedGeneration {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    target: String,
    #[cultcache(key = 2)]
    generation_id: String,
    #[cultcache(key = 3)]
    command_id: String,
    #[cultcache(key = 4)]
    transaction_id: String,
    #[cultcache(key = 5)]
    admitted_at_unix_millis: u64,
    #[cultcache(key = 6)]
    plan: CompiledDeploymentPlan,
    #[cultcache(key = 7)]
    sealed_release: SealedRelease,
    #[cultcache(key = 8)]
    installed_release: InstalledReleaseObservation,
    #[cultcache(key = 9)]
    expected: IdunnExpectedIncarnationRecord,
    #[cultcache(key = 10)]
    activation: IdunnRuntimeActivationRecord,
    #[cultcache(key = 11)]
    workload: WorkloadObservation,
    #[cultcache(key = 12)]
    leasing: LeasingEvidence,
    /// The receipt that admitted this incarnation, tagged by proof class.
    #[cultcache(key = 13)]
    ready: ReadinessEvidence,
    /// Present exactly when `ready` is Odin-correlated.
    #[cultcache(key = 14)]
    latest_odin_observation: Option<TopologyEvidence>,
    #[cultcache(key = 15)]
    routing: RoutingEvidence,
    /// Present exactly when `ready` is Odin-correlated.
    #[cultcache(key = 16)]
    odin_authority: Option<AdmittedOdinAuthority>,
    /// Meaningful only for an Odin-correlated generation.
    #[cultcache(key = 17)]
    odin_publisher_sequence_cursor: u64,
    // Keys 18 (route repair start) and 20 (continuity backoff) are retired.
    // The slots stay as gaps: the layout is positional, so a slot cannot be
    // reused without a bump.
    /// Present exactly when `routing` is Promoted.
    #[cultcache(key = 19)]
    route_supervision: Option<RouteSupervisionState>,
    /// Why this generation is held or down, for `status`. Cleared when the
    /// generation is replaced.
    #[cultcache(key = 21)]
    last_error: Option<String>,
}

/// The Odin-side receipts of an Odin-correlated generation. Decisions that
/// consume Odin evidence read it through here, so a route-proof generation
/// cannot be mistaken for one.
struct OdinReceipts<'a> {
    ready: &'a TopologyEvidence,
    latest: &'a TopologyEvidence,
}

impl AdmittedGeneration {
    /// This generation's class, or where its evidence disagrees with it. The
    /// class comes from its Expected, never from the tag on its Ready receipt.
    fn readiness(&self) -> Result<ReadinessClass, ReadinessDisagreement> {
        ReadinessClass::confirmed(&self.expected, [self.ready.voucher()])
    }

    fn odin_receipts(&self) -> Result<OdinReceipts<'_>> {
        match (
            &self.ready,
            &self.latest_odin_observation,
            &self.odin_authority,
        ) {
            (ReadinessEvidence::OdinCorrelated { evidence }, Some(latest), Some(_)) => {
                Ok(OdinReceipts {
                    ready: evidence,
                    latest,
                })
            }
            _ => bail!("admitted generation of {} is not Odin-correlated", self.target),
        }
    }

    fn from_transaction(
        transaction: &DeploymentTransaction,
        odin_authority: Option<AdmittedOdinAuthority>,
        now: u64,
    ) -> Result<Self> {
        ensure!(
            transaction.phase == DeploymentPhase::Committing,
            "only Committing can create an admitted generation"
        );
        let ready = required(&transaction.ready, "Ready receipt")?.clone();
        let (latest_odin_observation, odin_authority) = match &ready {
            ReadinessEvidence::OdinCorrelated { .. } => (
                Some(
                    required(&transaction.latest_odin_observation, "latest Odin receipt")?
                        .clone(),
                ),
                Some(required(&odin_authority, "Odin authority")?.clone()),
            ),
            ReadinessEvidence::RouteProof { .. } => (None, None),
        };
        let routing = required(&transaction.routing, "route disposition")?.clone();
        let route_supervision = matches!(&routing, RoutingEvidence::Promoted { .. })
            .then(RouteSupervisionState::default);
        let generation = Self {
            schema_version: ADMITTED_GENERATION_SCHEMA.into(),
            target: transaction.target.clone(),
            generation_id: format!("generation-{}", transaction.transaction_id),
            command_id: transaction.command_id.clone(),
            transaction_id: transaction.transaction_id.clone(),
            admitted_at_unix_millis: now,
            plan: required(&transaction.plan, "transaction plan")?.clone(),
            sealed_release: required(&transaction.sealed_release, "sealed release")?.clone(),
            installed_release: required(&transaction.installed_release, "installed release")?
                .clone(),
            expected: required(&transaction.expected, "Expected projection")?.clone(),
            activation: required(&transaction.activation, "activation")?.clone(),
            workload: required(&transaction.workload, "workload")?.clone(),
            leasing: required(&transaction.leasing, "lease disposition")?.clone(),
            ready,
            latest_odin_observation,
            routing,
            odin_authority,
            odin_publisher_sequence_cursor: transaction.odin_publisher_sequence_cursor,
            route_supervision,
            last_error: None,
        };
        generation.validate()?;
        Ok(generation)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == ADMITTED_GENERATION_SCHEMA,
            "admitted generation schema is unsupported"
        );
        require_id(&self.target, "admitted target")?;
        require_id(&self.generation_id, "admitted generation id")?;
        require_id(&self.command_id, "admitted command id")?;
        require_id(&self.transaction_id, "admitted transaction id")?;
        ensure!(self.admitted_at_unix_millis > 0, "admission has no time");
        self.plan.validate()?;
        self.sealed_release.validate_against(&self.plan)?;
        self.expected.validate()?;
        self.activation.validate()?;
        self.ready.validate_shape()?;
        match (
            &self.ready,
            &self.latest_odin_observation,
            &self.odin_authority,
        ) {
            (ReadinessEvidence::OdinCorrelated { evidence }, Some(latest), Some(authority)) => {
                latest.validate_shape()?;
                authority.validate()?;
                ensure!(
                    evidence.publisher_sequence <= self.odin_publisher_sequence_cursor
                        && latest.publisher_sequence == self.odin_publisher_sequence_cursor,
                    "admitted generation evidence does not describe one incarnation"
                );
            }
            (ReadinessEvidence::RouteProof { .. }, None, None) => ensure!(
                ReadinessClass::of(&self.expected).is_ok_and(|class| class == ReadinessClass::RouteProof),
                "route-proof Ready evidence for a target that is not route-proof"
            ),
            _ => bail!("admitted readiness evidence and Odin receipts disagree"),
        }
        ensure!(
            self.target == self.expected.target
                && self.expected.plan_id == self.plan.plan_id
                && self.expected.sealed_release_id == self.sealed_release.sealed_release_id
                && self.activation.expected_projection_sha256
                    == self.expected.canonical_sha256()?
                && self.activation.runtime_instance_id == self.workload.runtime_instance_id(),
            "admitted generation evidence does not describe one incarnation"
        );
        ensure!(
            self.expected.write_lease_required == self.leasing.lease().is_some(),
            "admitted write-lease disposition differs from Expected"
        );
        ensure!(
            self.expected.route.is_some() == self.routing.observation().is_some(),
            "admitted route disposition differs from Expected"
        );
        if let Some(route) = self.routing.observation() {
            route.validate()?;
            ensure!(
                route.runtime_instance_id == self.activation.runtime_instance_id,
                "admitted route observation belongs to another runtime instance"
            );
        }
        ensure!(
            self.route_supervision.is_some()
                == matches!(&self.routing, RoutingEvidence::Promoted { .. }),
            "route supervision state exists exactly for a promoted route"
        );
        if let Some(state) = &self.route_supervision {
            state.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeOptions {
    state_store: PathBuf,
    bindings_dir: PathBuf,
    source_root: PathBuf,
    staging_root: PathBuf,
    topology_store: PathBuf,
    odin_correlation_store: PathBuf,
    odin_trust_anchor: PathBuf,
    idunn_identity_store: PathBuf,
    deployment_brake_operator_anchor: PathBuf,
    source_identity: Option<ProcessIdentity>,
    topology_maximum_age_millis: u64,
    topology_maximum_future_skew_millis: u64,
    poll_millis: u64,
    /// Where host actuators dial in. `None` means no host-actuator workload
    /// can be served; a binding that names one then fails at its first
    /// driver call, not at startup, so an Idunn without hosts is unchanged.
    host_actuator_bind: Option<SocketAddr>,
    /// Where route drivers find nginx, systemctl, ufw and systemd-run, and
    /// their scratch directory. Every route driver is built from this.
    route_actuators: RouteActuators,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            state_store: PathBuf::from("/var/lib/gamecult/idunn/control.cc"),
            bindings_dir: PathBuf::from("/etc/gamecult/idunn/bindings"),
            source_root: PathBuf::from("/var/lib/gamecult/idunn/sources"),
            staging_root: PathBuf::from("/var/lib/gamecult/idunn/staging"),
            topology_store: PathBuf::from("/var/lib/gamecult/idunn/topology.cc"),
            odin_correlation_store: PathBuf::from(
                "/var/lib/gamecult/odin/idunn-runtime-topology.cc",
            ),
            odin_trust_anchor: PathBuf::from("/etc/gamecult/idunn/odin-topology-anchor.cc"),
            idunn_identity_store: PathBuf::from(
                "/var/lib/gamecult/idunn/idunn-service-identity.cc",
            ),
            deployment_brake_operator_anchor: PathBuf::from(
                "/etc/gamecult/idunn/deployment-brake-operator-anchor.cc",
            ),
            source_identity: None,
            topology_maximum_age_millis: DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS,
            topology_maximum_future_skew_millis: DEFAULT_TOPOLOGY_MAXIMUM_FUTURE_SKEW_MILLIS,
            poll_millis: 500,
            host_actuator_bind: None,
            route_actuators: RouteActuators::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Serve(RuntimeOptions),
    Up {
        selector: String,
        requested_by: String,
        state_store: PathBuf,
        wait: bool,
        timeout_seconds: u64,
    },
    Status {
        state_store: PathBuf,
        command_id: Option<String>,
    },
    Cancel {
        state_store: PathBuf,
        command_id: String,
        requested_by: String,
    },
    Validate {
        recipe: PathBuf,
        binding: Option<PathBuf>,
    },
}

pub fn run(args: impl Iterator<Item = String>) -> Result<()> {
    match parse(args)? {
        Command::Serve(options) => serve(options),
        Command::Up {
            selector,
            requested_by,
            state_store,
            wait,
            timeout_seconds,
        } => submit(
            &state_store,
            &selector,
            &requested_by,
            wait,
            timeout_seconds,
        ),
        Command::Status {
            state_store,
            command_id,
        } => status(&state_store, command_id.as_deref()),
        Command::Cancel {
            state_store,
            command_id,
            requested_by,
        } => cancel(&state_store, &command_id, &requested_by),
        Command::Validate { recipe, binding } => validate(&recipe, binding.as_deref()),
    }
}

/// Offline admission check for authoring. Reads a recipe, optionally reads a
/// binding, and runs the same `admit` the daemon runs at load. It opens no
/// store, contacts no host, and actuates nothing, so it is safe to run against
/// a candidate binding before installing it.
fn validate(recipe_path: &Path, binding_path: Option<&Path>) -> Result<()> {
    let recipe_text = fs::read_to_string(recipe_path)
        .with_context(|| format!("reading recipe {}", recipe_path.display()))?;
    let recipe = crate::deployment::TargetDeclaration::parse(&recipe_text)
        .with_context(|| format!("validating recipe {}", recipe_path.display()))?;
    println!(
        "recipe ok: {} declares target {}",
        recipe_path.display(),
        recipe.target
    );

    let Some(binding_path) = binding_path else {
        return Ok(());
    };
    let binding_text = fs::read_to_string(binding_path)
        .with_context(|| format!("reading operator binding {}", binding_path.display()))?;
    let binding = OperatorBinding::parse(&binding_text)
        .with_context(|| format!("validating operator binding {}", binding_path.display()))?;
    ensure!(
        binding.target == recipe.target,
        "operator binding targets {} but the recipe declares {}",
        binding.target,
        recipe.target
    );
    binding
        .admit(&recipe)
        .with_context(|| format!("admitting {} against its recipe", binding.target))?;
    println!(
        "binding ok: {} admits target {}",
        binding_path.display(),
        binding.target
    );
    Ok(())
}

fn parse(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut args = args.peekable();
    let command = args.next().ok_or_else(|| anyhow!(usage()))?;
    match command.as_str() {
        "serve" => parse_serve(args),
        "up" => parse_up(args),
        "status" => parse_status(args),
        "cancel" => parse_cancel(args),
        "validate" => parse_validate(args),
        "--help" | "-h" | "help" => bail!(usage()),
        _ => bail!("unknown Idunn command {command:?}\n\n{}", usage()),
    }
}

fn parse_serve(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut options = RuntimeOptions::default();
    let mut source_uid = None;
    let mut source_gid = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--state-store" => options.state_store = path_value(&mut args, &argument)?,
            "--bindings-dir" => options.bindings_dir = path_value(&mut args, &argument)?,
            "--source-root" => options.source_root = path_value(&mut args, &argument)?,
            "--staging-root" => options.staging_root = path_value(&mut args, &argument)?,
            "--topology-store" => options.topology_store = path_value(&mut args, &argument)?,
            "--odin-correlation-store" => {
                options.odin_correlation_store = path_value(&mut args, &argument)?
            }
            "--odin-trust-anchor" => options.odin_trust_anchor = path_value(&mut args, &argument)?,
            "--idunn-identity-store" => {
                options.idunn_identity_store = path_value(&mut args, &argument)?
            }
            "--deployment-brake-operator-anchor" => {
                options.deployment_brake_operator_anchor = path_value(&mut args, &argument)?
            }
            "--topology-maximum-age-millis" => {
                options.topology_maximum_age_millis = u64_value(&mut args, &argument)?
            }
            "--topology-maximum-future-skew-millis" => {
                options.topology_maximum_future_skew_millis = u64_value(&mut args, &argument)?
            }
            "--source-uid" => source_uid = Some(u32_value(&mut args, &argument)?),
            "--source-gid" => source_gid = Some(u32_value(&mut args, &argument)?),
            "--poll-millis" => options.poll_millis = u64_value(&mut args, &argument)?,
            "--host-actuator-bind" => {
                let value = string_value(&mut args, &argument)?;
                options.host_actuator_bind = Some(
                    value
                        .parse()
                        .with_context(|| format!("{argument} is not a socket address"))?,
                );
            }
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown Idunn serve option {argument:?}"),
        }
    }
    ensure!(
        options.poll_millis > 0 && options.topology_maximum_age_millis > 0,
        "poll and topology maximum age must be positive"
    );
    options.source_identity = match (source_uid, source_gid) {
        (Some(uid), Some(gid)) => {
            ensure!(
                uid > 0 && gid > 0,
                "source UID and GID must be unprivileged"
            );
            Some(ProcessIdentity { uid, gid })
        }
        (None, None) => None,
        _ => bail!("--source-uid and --source-gid must be supplied together"),
    };
    Ok(Command::Serve(options))
}

fn parse_up(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let selector = args
        .next()
        .ok_or_else(|| anyhow!("idunn up requires a service or profile selector"))?;
    require_selector(&selector)?;
    let mut state_store = RuntimeOptions::default().state_store;
    let mut requested_by = env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "operator".into());
    let mut wait = true;
    let mut timeout_seconds = 1800;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--state-store" => state_store = path_value(&mut args, &argument)?,
            "--requested-by" => requested_by = string_value(&mut args, &argument)?,
            "--no-wait" => wait = false,
            "--timeout-seconds" => timeout_seconds = u64_value(&mut args, &argument)?,
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown Idunn up option {argument:?}"),
        }
    }
    ensure!(timeout_seconds > 0, "--timeout-seconds must be positive");
    require_value(&requested_by, "deployment requester")?;
    Ok(Command::Up {
        selector,
        requested_by,
        state_store,
        wait,
        timeout_seconds,
    })
}

fn parse_status(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut state_store = RuntimeOptions::default().state_store;
    let mut command_id = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--state-store" => state_store = path_value(&mut args, &argument)?,
            "--command" => command_id = Some(string_value(&mut args, &argument)?),
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown Idunn status option {argument:?}"),
        }
    }
    Ok(Command::Status {
        state_store,
        command_id,
    })
}

fn parse_cancel(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let command_id = args
        .next()
        .ok_or_else(|| anyhow!("idunn cancel requires a deployment command id"))?;
    require_id(&command_id, "deployment command id")?;
    let mut state_store = RuntimeOptions::default().state_store;
    let mut requested_by = env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("USERNAME"))
        .unwrap_or_else(|_| "operator".into());
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--state-store" => state_store = path_value(&mut args, &argument)?,
            "--requested-by" => requested_by = string_value(&mut args, &argument)?,
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown Idunn cancel option {argument:?}"),
        }
    }
    require_value(&requested_by, "cancellation requester")?;
    Ok(Command::Cancel {
        state_store,
        command_id,
        requested_by,
    })
}

/// Withdraw queued work, a held deployment that has not fenced, or ask Idunn to
/// clean up a live stateless candidate. Stateful candidates past fencing cannot be cancelled because their prior
/// writer may already be stopped and its state boundary needs explicit repair.
fn cancel(store_path: &Path, command_id: &str, requested_by: &str) -> Result<()> {
    let snapshot = ControlSnapshot::read(store_path)?;
    let command = snapshot
        .commands
        .iter()
        .find(|stored| stored.value.command_id == command_id)
        .context("deployment command is unknown or already retired")?;
    ensure!(
        command.value.kind == CommandKind::Deploy,
        "only deployment commands can be cancelled; continuity is Idunn's own"
    );
    let mut live = snapshot
        .transactions
        .iter()
        .filter(|stored| stored.value.command_id == command_id && !stored.value.is_terminal());
    if let Some(current) = live.next() {
        ensure!(
            live.next().is_none(),
            "deployment command has multiple live transactions"
        );
        ensure!(
            cancel_is_safe_for_live_stateless(&current.value)
                || cancel_is_safe_for_held_prefence(&current.value),
            "live deployment can only be cancelled before fencing when Idunn holds it, or after fencing when its candidate is stateless"
        );
        let error = format!("cancelled by {requested_by}");
        let mut next = current.value.clone();
        if cancel_is_safe_for_held_prefence(&next) {
            next.pre_fencing_abort = Some(pre_fencing_abort_intent(&next, &error));
        } else {
            next.post_fencing_abort = Some(post_fencing_abort_intent(&next, &error));
        }
        next.last_error = Some(error);
        next.updated_at_unix_millis = now_millis()?;
        replace_transaction(store_path, current, &next)?;
        println!("{command_id} cancellation requested; Idunn is performing exact candidate cleanup");
        return Ok(());
    }
    let now = now_millis()?;
    let refusal = DeploymentTransaction::rejected(
        &command.value,
        anyhow!("cancelled by {requested_by} before freezing"),
        now,
    )?;
    let envelope = transaction_envelope(&refusal, now)?;
    ensure!(
        SingleFileMessagePackBackingStore::new(store_path).compare_exchange(
            &[
                CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command_id.to_owned(),
                    current: Some(command.envelope.clone()),
                },
                CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: refusal.transaction_id.clone(),
                    current: None,
                },
            ],
            std::slice::from_ref(&envelope),
        )?,
        "deployment command changed before it could be cancelled"
    );
    archive_terminal_transaction(store_path, &envelope)?;
    println!("{command_id} cancelled");
    Ok(())
}

/// A deployment Idunn holds before it fenced anything has touched nothing of
/// the incumbent, so an operator may withdraw it. Idunn never does so itself.
fn cancel_is_safe_for_held_prefence(transaction: &DeploymentTransaction) -> bool {
    transaction.command_kind == CommandKind::Deploy
        && transaction.phase < DeploymentPhase::Fencing
        && transaction.held_disagreement().is_some()
}

fn cancel_is_safe_for_live_stateless(transaction: &DeploymentTransaction) -> bool {
        transaction.command_kind == CommandKind::Deploy
        && transaction.phase >= DeploymentPhase::Fencing
        && matches!(
            transaction.fencing.as_ref(),
            Some(FencingEvidence::SkippedStateless)
        )
        && transaction.completion.is_none()
        && transaction.pre_fencing_abort.is_none()
        && transaction.post_fencing_abort.is_none()
        && transaction.post_commit_cleanup.is_none()
}

fn parse_validate(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let mut recipe = None;
    let mut binding = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--recipe" => recipe = Some(path_value(&mut args, &argument)?),
            "--binding" => binding = Some(path_value(&mut args, &argument)?),
            "--help" | "-h" => bail!(usage()),
            _ => bail!("unknown Idunn validate option {argument:?}"),
        }
    }
    let recipe = recipe.ok_or_else(|| anyhow!("validate requires --recipe"))?;
    Ok(Command::Validate { recipe, binding })
}

#[derive(Clone)]
struct Stored<T> {
    envelope: CultCacheEnvelope,
    value: T,
}

#[derive(Default)]
struct ControlSnapshot {
    commands: Vec<Stored<DeploymentCommand>>,
    transactions: Vec<Stored<DeploymentTransaction>>,
    admitted: Vec<Stored<AdmittedGeneration>>,
    targets: Vec<Stored<TargetSupervision>>,
}

impl ControlSnapshot {
    fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let mut snapshot = Self::default();
        for envelope in SingleFileMessagePackBackingStore::new(path)
            .pull_all_read_only_snapshot()
            .context("reading Idunn control snapshot")?
        {
            match envelope.r#type.as_str() {
                DeploymentCommand::TYPE => {
                    ensure!(
                        envelope.schema_id.as_deref() == Some(DEPLOYMENT_COMMAND_SCHEMA),
                        "Idunn control store contains an unsupported command"
                    );
                    let value: DeploymentCommand = decode_record(&envelope)?;
                    value.validate()?;
                    ensure!(
                        envelope.key == value.command_id,
                        "deployment command key differs from its identity"
                    );
                    snapshot.commands.push(Stored { envelope, value });
                }
                DeploymentTransaction::TYPE => {
                    let value = read_transaction_record(&envelope)?;
                    ensure!(
                        envelope.key == value.transaction_id,
                        "deployment transaction key differs from its identity"
                    );
                    snapshot.transactions.push(Stored { envelope, value });
                }
                AdmittedGeneration::TYPE => {
                    let value = read_generation_record(&envelope)?;
                    ensure!(
                        envelope.key == value.target,
                        "admitted generation key is not its target"
                    );
                    snapshot.admitted.push(Stored { envelope, value });
                }
                TargetSupervision::TYPE => {
                    ensure!(
                        envelope.schema_id.as_deref() == Some(TARGET_SUPERVISION_SCHEMA),
                        "Idunn control store contains an unsupported target supervision record"
                    );
                    let value: TargetSupervision = decode_record(&envelope)?;
                    value.validate()?;
                    ensure!(
                        envelope.key == value.target,
                        "target supervision key is not its target"
                    );
                    snapshot.targets.push(Stored { envelope, value });
                }
                _ => bail!("Idunn control store contains a foreign document"),
            }
        }
        snapshot.validate_relations()?;
        Ok(snapshot)
    }

    fn validate_relations(&self) -> Result<()> {
        let command_ids = self
            .commands
            .iter()
            .map(|stored| stored.value.command_id.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            command_ids.len() == self.commands.len(),
            "Idunn control store contains duplicate commands"
        );
        let transaction_ids = self
            .transactions
            .iter()
            .map(|stored| stored.value.transaction_id.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            transaction_ids.len() == self.transactions.len(),
            "Idunn control store contains duplicate transactions"
        );
        let admitted_targets = self
            .admitted
            .iter()
            .map(|stored| stored.value.target.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            admitted_targets.len() == self.admitted.len(),
            "Idunn control store contains duplicate current generations"
        );
        ensure!(
            self.targets
                .iter()
                .map(|stored| stored.value.target.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                == self.targets.len(),
            "Idunn control store contains duplicate target supervision records"
        );
        for transaction in &self.transactions {
            let command = self
                .commands
                .iter()
                .find(|candidate| candidate.value.command_id == transaction.value.command_id)
                .context("deployment transaction has no immutable command")?;
            ensure!(
                command.value.kind == transaction.value.command_kind,
                "transaction kind differs from its immutable command"
            );
        }
        let mut live_targets = BTreeSet::new();
        for transaction in self
            .transactions
            .iter()
            .filter(|stored| stored.value.owns_target_authority())
        {
            ensure!(
                live_targets.insert(transaction.value.target.as_str()),
                "multiple transactions claim current-incarnation authority for one target"
            );
            if let Some(fencing) = transaction
                .value
                .fencing
                .as_ref()
                .filter(|_| transaction.value.phase < DeploymentPhase::Complete)
            {
                let incumbent = self.admitted_for(&transaction.value.target);
                let incumbent_matches =
                    match (&transaction.value.incumbent_generation_id, incumbent) {
                        (None, None) => true,
                        (Some(expected), Some(current)) => current.value.generation_id == *expected,
                        _ => false,
                    };
                ensure!(
                    incumbent_matches,
                    "fenced transaction incumbent differs from current admission"
                );
                let incumbent_lease_sha256 = incumbent
                    .and_then(|stored| stored.value.leasing.lease())
                    .map(IdunnProcessWriteLeaseRecord::canonical_sha256)
                    .transpose()?;
                match fencing {
                    FencingEvidence::SkippedStateless => ensure!(
                        incumbent_lease_sha256.is_none(),
                        "stateless fencing omitted the admitted incumbent lease"
                    ),
                    FencingEvidence::Revoked {
                        incumbent_lease_sha256: recorded,
                        ..
                    } => {
                        ensure!(
                            recorded == &incumbent_lease_sha256,
                            "fencing evidence names another incumbent lease"
                        );
                        ensure!(
                            incumbent_lease_sha256.is_some()
                                || transaction
                                    .value
                                    .expected
                                    .as_ref()
                                    .is_some_and(|expected| expected.write_lease_required),
                            "stateless transaction did not use its explicit fencing skip"
                        );
                    }
                }
            }
        }
        let mut authorizations = BTreeSet::new();
        for authorization in self
            .transactions
            .iter()
            .filter_map(|stored| stored.value.deployment_authorization.as_ref())
        {
            ensure!(
                authorizations.insert(authorization.authorization_id.as_str()),
                "deployment authorization was consumed more than once"
            );
        }
        Ok(())
    }

    fn admitted_for(&self, target: &str) -> Option<&Stored<AdmittedGeneration>> {
        self.admitted
            .iter()
            .find(|stored| stored.value.target == target)
    }

    fn supervision_for(&self, target: &str) -> Option<&Stored<TargetSupervision>> {
        self.targets
            .iter()
            .find(|stored| stored.value.target == target)
    }

    /// The target's meters, or empty ones: a target that was never metered has
    /// used nothing.
    fn supervision_or_new(&self, target: &str) -> TargetSupervision {
        self.supervision_for(target)
            .map_or_else(|| TargetSupervision::new(target), |stored| stored.value.clone())
    }

    /// The admitted generation of the target that provides
    /// `odin.verse-rendezvous`: Odin is whichever target says so, not
    /// whichever is named "odin".
    fn admitted_odin(&self) -> Option<&Stored<AdmittedGeneration>> {
        self.admitted.iter().find(|stored| {
            ReadinessClass::of(&stored.value.expected)
                .is_ok_and(|class| class == ReadinessClass::OdinSelf)
        })
    }

    fn transaction_for_command(&self, command_id: &str) -> Vec<&DeploymentTransaction> {
        let mut values = self
            .transactions
            .iter()
            .filter(|stored| stored.value.command_id == command_id)
            .map(|stored| &stored.value)
            .collect::<Vec<_>>();
        values.sort_by_key(|value| value.ordinal);
        values
    }

    fn has_earlier_authority_sibling(&self, transaction: &DeploymentTransaction) -> bool {
        self.transactions.iter().any(|stored| {
            stored.value.owns_target_authority()
                && stored.value.command_id == transaction.command_id
                && stored.value.ordinal < transaction.ordinal
        })
    }

    /// The highest Odin publisher sequence this target has ever *acted on*.
    ///
    /// A transaction that failed contributes nothing. Its observations were
    /// never built upon -- the deployment was abandoned and its projection
    /// withdrawn -- so treating them as admitted history only raises the bar
    /// for the next attempt. Odin does not reuse a sequence, so nothing is
    /// weakened by forgetting the readings of an attempt that came to nothing;
    /// what would be weakened is refusing every retry after a failure, which is
    /// how a target ends up unable to deploy at all.
    fn max_odin_sequence(&self, target: &str, signer_identity_id: &str) -> u64 {
        let transaction_max = self
            .transactions
            .iter()
            .filter_map(|stored| {
                stored
                    .value
                    .latest_odin_observation
                    .as_ref()
                    .filter(|evidence| {
                        stored.value.target == target
                            && evidence.signer_identity_id == signer_identity_id
                            && !matches!(
                                stored.value.completion,
                                Some(TransactionCompletion::FailedBeforeFencing { .. })
                                    | Some(TransactionCompletion::FailedAfterFencing { .. })
                            )
                    })
                    .map(|evidence| evidence.publisher_sequence)
            })
            .max()
            .unwrap_or(0);
        let admitted_max = self
            .admitted
            .iter()
            .filter(|stored| {
                stored.value.target == target
                    && stored
                        .value
                        .odin_authority
                        .as_ref()
                        .is_some_and(|authority| authority.signer_identity_id == signer_identity_id)
            })
            .map(|stored| stored.value.odin_publisher_sequence_cursor)
            .max()
            .unwrap_or(0);
        transaction_max.max(admitted_max)
    }
}

fn decode_record<T>(envelope: &CultCacheEnvelope) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let value: T = rmp_serde::from_slice(&envelope.payload)?;
    ensure!(
        rmp_serde::to_vec(&value)? == envelope.payload,
        "Idunn control store contains a noncanonical record"
    );
    Ok(value)
}

fn command_envelope(value: &DeploymentCommand, now: u64) -> Result<CultCacheEnvelope> {
    value.validate()?;
    typed_envelope(
        &value.command_id,
        DeploymentCommand::TYPE,
        DEPLOYMENT_COMMAND_SCHEMA,
        value,
        now,
    )
}

/// The transaction layout of v2 and v3 records: `ready` is a bare Odin receipt,
/// and there are no phase deadline or lease adoption slots. It exists only to
/// decode records written before v4. `migrate_control_store_to_current_schema`
/// rewrites every one in `control.cc` at boot, but `history.cc` is never
/// migrated or pruned, so this struct dies only when history is pruned or
/// migrated.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.deployment_transaction",
    schema = "idunn.deployment_transaction.v3"
)]
struct LegacyDeploymentTransaction {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    transaction_id: String,
    #[cultcache(key = 2)]
    command_id: String,
    #[cultcache(key = 3)]
    command_kind: CommandKind,
    #[cultcache(key = 4)]
    target: String,
    #[cultcache(key = 5)]
    ordinal: u32,
    #[cultcache(key = 6)]
    phase: DeploymentPhase,
    #[cultcache(key = 7)]
    created_at_unix_millis: u64,
    #[cultcache(key = 8)]
    updated_at_unix_millis: u64,
    #[cultcache(key = 9)]
    incumbent_generation_id: Option<String>,
    #[cultcache(key = 10)]
    plan: Option<CompiledDeploymentPlan>,
    #[cultcache(key = 11)]
    frozen_source: Option<FrozenSourceReceipt>,
    #[cultcache(key = 12)]
    sealed_release: Option<SealedRelease>,
    #[cultcache(key = 13)]
    installed_release: Option<InstalledReleaseObservation>,
    #[cultcache(key = 14)]
    expected: Option<IdunnExpectedIncarnationRecord>,
    #[cultcache(key = 15)]
    expected_publication_sha256: Option<String>,
    #[cultcache(key = 16)]
    deployment_authorization: Option<DeploymentAuthorization>,
    #[cultcache(key = 17)]
    lifecycle_authorized_at_unix_millis: Option<u64>,
    #[cultcache(key = 18)]
    activation: Option<IdunnRuntimeActivationRecord>,
    #[cultcache(key = 19)]
    workload: Option<WorkloadObservation>,
    #[cultcache(key = 20)]
    activation_publication_sha256: Option<String>,
    #[cultcache(key = 21)]
    latest_odin_observation: Option<TopologyEvidence>,
    #[cultcache(key = 22)]
    warming: Option<WarmingEvidence>,
    #[cultcache(key = 23)]
    route_preflight: Option<RoutePreflightReceipt>,
    #[cultcache(key = 24)]
    isolation: Option<IsolationEvidence>,
    #[cultcache(key = 25)]
    fencing: Option<FencingEvidence>,
    #[cultcache(key = 26)]
    leasing: Option<LeasingEvidence>,
    #[cultcache(key = 27)]
    ready: Option<TopologyEvidence>,
    #[cultcache(key = 28)]
    routing: Option<RoutingEvidence>,
    #[cultcache(key = 29)]
    odin_publisher_sequence_cursor: u64,
    #[cultcache(key = 30)]
    last_error: Option<String>,
    #[cultcache(key = 31)]
    completion: Option<TransactionCompletion>,
    #[cultcache(key = 32)]
    pre_fencing_abort: Option<PreFencingAbort>,
    #[cultcache(key = 33)]
    post_commit_cleanup: Option<PostCommitCleanup>,
    // A v2 record has no key 34: it predates post-fencing aborts.
    #[cultcache(key = 34, default)]
    post_fencing_abort: Option<PostFencingAbort>,
}

impl LegacyDeploymentTransaction {
    /// Every legacy Ready receipt is an Odin receipt: no route-proof class
    /// existed. The new slots start empty, which is what an older transaction
    /// had.
    fn into_current(self) -> DeploymentTransaction {
        DeploymentTransaction {
            schema_version: DEPLOYMENT_TRANSACTION_SCHEMA.into(),
            transaction_id: self.transaction_id,
            command_id: self.command_id,
            command_kind: self.command_kind,
            target: self.target,
            ordinal: self.ordinal,
            phase: self.phase,
            created_at_unix_millis: self.created_at_unix_millis,
            updated_at_unix_millis: self.updated_at_unix_millis,
            incumbent_generation_id: self.incumbent_generation_id,
            plan: self.plan,
            frozen_source: self.frozen_source,
            sealed_release: self.sealed_release,
            installed_release: self.installed_release,
            expected: self.expected,
            expected_publication_sha256: self.expected_publication_sha256,
            deployment_authorization: self.deployment_authorization,
            lifecycle_authorized_at_unix_millis: self.lifecycle_authorized_at_unix_millis,
            activation: self.activation,
            workload: self.workload,
            activation_publication_sha256: self.activation_publication_sha256,
            latest_odin_observation: self.latest_odin_observation,
            warming: self.warming,
            route_preflight: self.route_preflight,
            isolation: self.isolation,
            fencing: self.fencing,
            leasing: self.leasing,
            ready: self
                .ready
                .map(|evidence| ReadinessEvidence::OdinCorrelated { evidence }),
            routing: self.routing,
            odin_publisher_sequence_cursor: self.odin_publisher_sequence_cursor,
            last_error: self.last_error,
            completion: self.completion,
            pre_fencing_abort: self.pre_fencing_abort,
            post_commit_cleanup: self.post_commit_cleanup,
            post_fencing_abort: self.post_fencing_abort,
            phase_deadline: None,
            lease_adoption: None,
        }
    }
}

/// The v2 admitted-generation layout. Same lifetime rule as
/// `LegacyDeploymentTransaction`.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.admitted_generation",
    schema = "idunn.admitted_generation.v2"
)]
struct LegacyAdmittedGeneration {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    target: String,
    #[cultcache(key = 2)]
    generation_id: String,
    #[cultcache(key = 3)]
    command_id: String,
    #[cultcache(key = 4)]
    transaction_id: String,
    #[cultcache(key = 5)]
    admitted_at_unix_millis: u64,
    #[cultcache(key = 6)]
    plan: CompiledDeploymentPlan,
    #[cultcache(key = 7)]
    sealed_release: SealedRelease,
    #[cultcache(key = 8)]
    installed_release: InstalledReleaseObservation,
    #[cultcache(key = 9)]
    expected: IdunnExpectedIncarnationRecord,
    #[cultcache(key = 10)]
    activation: IdunnRuntimeActivationRecord,
    #[cultcache(key = 11)]
    workload: WorkloadObservation,
    #[cultcache(key = 12)]
    leasing: LeasingEvidence,
    #[cultcache(key = 13)]
    ready: TopologyEvidence,
    #[cultcache(key = 14)]
    latest_odin_observation: TopologyEvidence,
    #[cultcache(key = 15)]
    routing: RoutingEvidence,
    #[cultcache(key = 16)]
    odin_authority: AdmittedOdinAuthority,
    #[cultcache(key = 17)]
    odin_publisher_sequence_cursor: u64,
    #[cultcache(key = 18)]
    route_repair_started_at_unix_millis: Option<u64>,
}

impl LegacyAdmittedGeneration {
    /// Every v2 generation was admitted on Odin receipts, so it migrates as
    /// Odin-correlated with those receipts unchanged. Route supervision state
    /// starts empty for a promoted route. The v2 repair start decided nothing
    /// after S1 and is not carried.
    fn into_current(self) -> AdmittedGeneration {
        let route_supervision = matches!(&self.routing, RoutingEvidence::Promoted { .. })
            .then(RouteSupervisionState::default);
        AdmittedGeneration {
            schema_version: ADMITTED_GENERATION_SCHEMA.into(),
            target: self.target,
            generation_id: self.generation_id,
            command_id: self.command_id,
            transaction_id: self.transaction_id,
            admitted_at_unix_millis: self.admitted_at_unix_millis,
            plan: self.plan,
            sealed_release: self.sealed_release,
            installed_release: self.installed_release,
            expected: self.expected,
            activation: self.activation,
            workload: self.workload,
            leasing: self.leasing,
            ready: ReadinessEvidence::OdinCorrelated {
                evidence: self.ready,
            },
            latest_odin_observation: Some(self.latest_odin_observation),
            routing: self.routing,
            odin_authority: Some(self.odin_authority),
            odin_publisher_sequence_cursor: self.odin_publisher_sequence_cursor,
            route_supervision,
            last_error: None,
        }
    }
}

/// The v3 admitted-generation layout. Its meters (`actuations`, the continuity
/// backoff, the repair start) are retired: no released Idunn ever wrote a
/// nonzero one, and a record that shows one is refused instead of lifted.
#[derive(Clone, Debug, PartialEq, Eq, DatabaseEntry)]
#[cultcache(
    type = "idunn.admitted_generation",
    schema = "idunn.admitted_generation.v3"
)]
struct LegacyAdmittedGenerationV3 {
    #[cultcache(key = 0)]
    schema_version: String,
    #[cultcache(key = 1)]
    target: String,
    #[cultcache(key = 2)]
    generation_id: String,
    #[cultcache(key = 3)]
    command_id: String,
    #[cultcache(key = 4)]
    transaction_id: String,
    #[cultcache(key = 5)]
    admitted_at_unix_millis: u64,
    #[cultcache(key = 6)]
    plan: CompiledDeploymentPlan,
    #[cultcache(key = 7)]
    sealed_release: SealedRelease,
    #[cultcache(key = 8)]
    installed_release: InstalledReleaseObservation,
    #[cultcache(key = 9)]
    expected: IdunnExpectedIncarnationRecord,
    #[cultcache(key = 10)]
    activation: IdunnRuntimeActivationRecord,
    #[cultcache(key = 11)]
    workload: WorkloadObservation,
    #[cultcache(key = 12)]
    leasing: LeasingEvidence,
    #[cultcache(key = 13)]
    ready: ReadinessEvidence,
    #[cultcache(key = 14)]
    latest_odin_observation: Option<TopologyEvidence>,
    #[cultcache(key = 15)]
    routing: RoutingEvidence,
    #[cultcache(key = 16)]
    odin_authority: Option<AdmittedOdinAuthority>,
    #[cultcache(key = 17)]
    odin_publisher_sequence_cursor: u64,
    #[cultcache(key = 18)]
    route_repair_started_at_unix_millis: Option<u64>,
    #[cultcache(key = 19)]
    route_supervision: Option<LegacyRouteSupervisionStateV3>,
    #[cultcache(key = 20)]
    continuity_backoff: LegacyContinuityBackoffV3,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRouteSupervisionStateV3 {
    last_challenge_at_unix_millis: Option<u64>,
    consecutive_failures: u32,
    next_challenge_at_unix_millis: Option<u64>,
    degraded_since_unix_millis: Option<u64>,
    actuations: LegacyActuationWindowV3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyActuationWindowV3 {
    window_started_at_unix_millis: u64,
    count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyContinuityBackoffV3 {
    window_started_at_unix_millis: Option<u64>,
    attempts: u32,
    next_restart_at_unix_millis: Option<u64>,
}

impl LegacyAdmittedGenerationV3 {
    /// Drops the retired meters. The lift creates no `TargetSupervision`: every
    /// target starts unmetered, which is what no released binary ever changed.
    fn into_current(self) -> Result<AdmittedGeneration> {
        ensure!(
            self.continuity_backoff.attempts == 0
                && self
                    .route_supervision
                    .as_ref()
                    .is_none_or(|state| state.actuations.count == 0),
            "admitted generation of {} records route actuations or continuity restarts that no released Idunn wrote; refusing to drop them",
            self.target
        );
        Ok(AdmittedGeneration {
            schema_version: ADMITTED_GENERATION_SCHEMA.into(),
            target: self.target,
            generation_id: self.generation_id,
            command_id: self.command_id,
            transaction_id: self.transaction_id,
            admitted_at_unix_millis: self.admitted_at_unix_millis,
            plan: self.plan,
            sealed_release: self.sealed_release,
            installed_release: self.installed_release,
            expected: self.expected,
            activation: self.activation,
            workload: self.workload,
            leasing: self.leasing,
            ready: self.ready,
            latest_odin_observation: self.latest_odin_observation,
            routing: self.routing,
            odin_authority: self.odin_authority,
            odin_publisher_sequence_cursor: self.odin_publisher_sequence_cursor,
            route_supervision: self.route_supervision.map(|state| RouteSupervisionState {
                last_challenge_at_unix_millis: state.last_challenge_at_unix_millis,
                consecutive_failures: state.consecutive_failures,
                next_challenge_at_unix_millis: state.next_challenge_at_unix_millis,
                degraded_since_unix_millis: state.degraded_since_unix_millis,
            }),
            last_error: None,
        })
    }
}

/// Decode a v2 or v3 transaction and lift it to the current shape, without
/// validating it. History reads stop here; the control store validates.
fn lift_legacy_transaction(envelope: &CultCacheEnvelope) -> Result<DeploymentTransaction> {
    let schema = match envelope.schema_id.as_deref() {
        Some(schema @ (DEPLOYMENT_TRANSACTION_SCHEMA_V2 | DEPLOYMENT_TRANSACTION_SCHEMA_V3)) => {
            schema
        }
        _ => bail!("Idunn control store contains an unsupported transaction"),
    };
    let value: LegacyDeploymentTransaction = rmp_serde::from_slice(&envelope.payload)
        .with_context(|| format!("decoding a {schema} deployment transaction"))?;
    ensure!(
        value.schema_version == schema,
        "stored transaction schema differs from its envelope"
    );
    ensure!(
        schema != DEPLOYMENT_TRANSACTION_SCHEMA_V2 || value.post_fencing_abort.is_none(),
        "a v2 transaction cannot carry post-fencing abort evidence"
    );
    Ok(value.into_current())
}

/// Read one stored transaction, lifting a v2 or v3 record to the current shape.
///
/// The control store refuses noncanonical bytes: a record must re-encode to
/// exactly what is stored. That check is what makes tampering visible, and it
/// also means adding a field is a schema change: an older record re-encodes
/// with extra keys and would be read as tampered.
///
/// So an older record is decoded by its own layout and lifted. Byte-exactness
/// cannot apply across a version boundary -- the bytes are a different schema
/// by definition -- so the lift leans on the full semantic `validate()`
/// instead, and everything downstream sees only the current shape.
fn read_transaction_record(envelope: &CultCacheEnvelope) -> Result<DeploymentTransaction> {
    match envelope.schema_id.as_deref() {
        Some(DEPLOYMENT_TRANSACTION_SCHEMA) => {
            let value: DeploymentTransaction = decode_record(envelope)?;
            value.validate()?;
            Ok(value)
        }
        _ => {
            let mut value = lift_legacy_transaction(envelope)?;
            cleanup_evidence::owe_legacy_continuity_projection(&mut value);
            value.validate()?;
            Ok(value)
        }
    }
}

/// Read one stored admitted generation, lifting a v2 or v3 record to the
/// current shape by the same rule as `read_transaction_record`.
fn read_generation_record(envelope: &CultCacheEnvelope) -> Result<AdmittedGeneration> {
    match envelope.schema_id.as_deref() {
        Some(ADMITTED_GENERATION_SCHEMA) => {
            let value: AdmittedGeneration = decode_record(envelope)?;
            value.validate()?;
            Ok(value)
        }
        Some(ADMITTED_GENERATION_SCHEMA_V3) => {
            let legacy: LegacyAdmittedGenerationV3 = rmp_serde::from_slice(&envelope.payload)
                .context("decoding a v3 admitted generation")?;
            ensure!(
                legacy.schema_version == ADMITTED_GENERATION_SCHEMA_V3,
                "stored generation schema differs from its envelope"
            );
            let value = legacy.into_current()?;
            value.validate()?;
            Ok(value)
        }
        Some(ADMITTED_GENERATION_SCHEMA_V2) => {
            let legacy: LegacyAdmittedGeneration = rmp_serde::from_slice(&envelope.payload)
                .context("decoding a v2 admitted generation")?;
            ensure!(
                legacy.schema_version == ADMITTED_GENERATION_SCHEMA_V2,
                "stored generation schema differs from its envelope"
            );
            let value = legacy.into_current();
            value.validate()?;
            Ok(value)
        }
        _ => bail!("Idunn control store contains an unsupported admitted generation"),
    }
}

/// Rewrite every older record as current, once, before the engine runs.
///
/// The read path lifts older records on its own, so this is convergence rather
/// than correctness: without it the store keeps records in several shapes for
/// as long as the oldest one survives. Each rewrite is a compare-exchange
/// against the exact stored envelope, so a record that changed underneath is
/// left alone rather than clobbered.
fn migrate_control_store_to_current_schema(store_path: &Path) -> Result<usize> {
    if !store_path.exists() {
        return Ok(0);
    }
    let store = SingleFileMessagePackBackingStore::new(store_path);
    let stale = store
        .pull_all_read_only_snapshot()
        .context("reading Idunn control snapshot for migration")?
        .into_iter()
        .filter(|envelope| {
            let schema = envelope.schema_id.as_deref();
            (envelope.r#type == DeploymentTransaction::TYPE
                && matches!(
                    schema,
                    Some(DEPLOYMENT_TRANSACTION_SCHEMA_V2 | DEPLOYMENT_TRANSACTION_SCHEMA_V3)
                ))
                || (envelope.r#type == AdmittedGeneration::TYPE
                    && matches!(
                        schema,
                        Some(ADMITTED_GENERATION_SCHEMA_V2 | ADMITTED_GENERATION_SCHEMA_V3)
                    ))
        })
        .collect::<Vec<_>>();
    let mut migrated = 0;
    for envelope in stale {
        let (record_type, key, mut next) = if envelope.r#type == DeploymentTransaction::TYPE {
            let (key, next) = cleanup_evidence::migrate_transaction_record(&envelope)?;
            (DeploymentTransaction::TYPE, key, next)
        } else {
            let value = read_generation_record(&envelope)?;
            let next = admitted_envelope(&value, value.admitted_at_unix_millis)?;
            (AdmittedGeneration::TYPE, value.target, next)
        };
        next.stored_at = envelope.stored_at.clone();
        ensure!(
            store.compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: record_type.into(),
                    key,
                    current: Some(envelope.clone()),
                }],
                &[next],
            )?,
            "control record changed during schema migration"
        );
        migrated += 1;
    }
    Ok(migrated)
}

fn transaction_envelope(value: &DeploymentTransaction, now: u64) -> Result<CultCacheEnvelope> {
    ensure!(
        !value.carries_legacy_marker(),
        "only the legacy lift may produce a legacy-marked record; it cannot be written as a new one"
    );
    encode_transaction(value, now)
}

fn encode_transaction(value: &DeploymentTransaction, now: u64) -> Result<CultCacheEnvelope> {
    value.validate()?;
    typed_envelope(
        &value.transaction_id,
        DeploymentTransaction::TYPE,
        DEPLOYMENT_TRANSACTION_SCHEMA,
        value,
        now,
    )
}

fn admitted_envelope(value: &AdmittedGeneration, now: u64) -> Result<CultCacheEnvelope> {
    value.validate()?;
    typed_envelope(
        &value.target,
        AdmittedGeneration::TYPE,
        ADMITTED_GENERATION_SCHEMA,
        value,
        now,
    )
}

fn target_supervision_envelope(value: &TargetSupervision, now: u64) -> Result<CultCacheEnvelope> {
    value.validate()?;
    typed_envelope(
        &value.target,
        TargetSupervision::TYPE,
        TARGET_SUPERVISION_SCHEMA,
        value,
        now,
    )
}

fn typed_envelope<T: Serialize>(
    key: &str,
    record_type: &str,
    schema: &str,
    value: &T,
    now: u64,
) -> Result<CultCacheEnvelope> {
    Ok(CultCacheEnvelope {
        key: key.into(),
        r#type: record_type.into(),
        payload: rmp_serde::to_vec(value)?,
        stored_at: rfc3339_millis(now)?,
        schema_id: Some(schema.into()),
    })
}

/// `history.cc`, beside the control store. Terminal transactions live here so
/// the control store holds only what still gates a decision.
fn history_store_path(state_store: &Path) -> PathBuf {
    state_store.with_file_name("history.cc")
}

/// Move one finished transaction out of the live set.
///
/// History first, then the live copy: a crash between the two leaves the
/// record in both, which reads as still-resident and is archived again on the
/// next attempt. The other order can lose it, and a completion nobody can
/// observe is the failure that actually costs an operator something.
///
/// `insert_entry_if_absent` makes the repeat a no-op, and the delete is a
/// compare-and-swap on the exact envelope archived, so a transaction that
/// changed underneath us is left alone rather than dropped.
fn archive_terminal_transaction(state_store: &Path, envelope: &CultCacheEnvelope) -> Result<()> {
    SingleFileMessagePackBackingStore::new(&history_store_path(state_store))
        .insert_entry_if_absent(envelope.clone())
        .context("archiving a finished transaction to history")?;
    SingleFileMessagePackBackingStore::new(state_store)
        .delete_batch_if_unchanged(std::slice::from_ref(envelope))
        .context("retiring a finished transaction from the control store")?;
    // The command goes with its last transaction. A command is consumed by
    // being frozen once; left resident after its transaction retired, it read
    // as queued again and was frozen again on the next tick, which is how one
    // `idunn up` became an unbounded series of deployment attempts.
    let transaction = read_transaction_record(envelope)?;
    let snapshot = ControlSnapshot::read(state_store)?;
    if !snapshot
        .transaction_for_command(&transaction.command_id)
        .is_empty()
    {
        return Ok(());
    }
    if let Some(command) = snapshot
        .commands
        .iter()
        .find(|stored| stored.value.command_id == transaction.command_id)
    {
        SingleFileMessagePackBackingStore::new(&history_store_path(state_store))
            .insert_entry_if_absent(command.envelope.clone())
            .context("archiving a consumed command to history")?;
        SingleFileMessagePackBackingStore::new(state_store)
            .delete_batch_if_unchanged(std::slice::from_ref(&command.envelope))
            .context("retiring a consumed command from the control store")?;
    }
    Ok(())
}

/// What `history.cc` yielded: the transactions it decoded and the entries it
/// could not, each with its key and reason.
struct HistoryRead {
    transactions: Vec<DeploymentTransaction>,
    undecodable: Vec<(String, String)>,
}

impl HistoryRead {
    /// What is missing from `transactions`, or `None` when nothing is.
    fn report(&self) -> Option<String> {
        if self.undecodable.is_empty() {
            return None;
        }
        let keys = self
            .undecodable
            .iter()
            .map(|(key, error)| format!("{key} ({error})"))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "Idunn history: {} archived transaction(s) could not be decoded and are not counted \
             by status or continuity backoff: {keys}",
            self.undecodable.len()
        ))
    }
}

/// Finished transactions. `control.cc` keeps the byte-exact canonical check
/// because its records still gate decisions. History describes what already
/// happened, so an undecodable entry is reported and skipped rather than
/// allowed to refuse the read -- that asymmetry is the whole reason the two
/// files are separate. A file that cannot be read at all is an error: the
/// caller decides whether an empty answer is safe.
fn read_history(state_store: &Path) -> Result<HistoryRead> {
    let path = history_store_path(state_store);
    if !path.exists() {
        return Ok(HistoryRead {
            transactions: Vec::new(),
            undecodable: Vec::new(),
        });
    }
    let envelopes = SingleFileMessagePackBackingStore::new(&path)
        .pull_all_read_only_snapshot()
        .with_context(|| format!("Idunn cannot read {}", path.display()))?;
    let (transactions, undecodable) = decode_history_transactions(envelopes);
    Ok(HistoryRead {
        transactions,
        undecodable,
    })
}

/// History for display only (status). An unreadable file shows as empty and
/// says so; decisions use `Engine::history_for_decision`, which does not.
fn read_history_transactions(state_store: &Path) -> Vec<DeploymentTransaction> {
    match read_history(state_store) {
        Ok(history) => {
            if let Some(report) = history.report() {
                eprintln!("{report}");
            }
            history.transactions
        }
        Err(error) => {
            eprintln!("{error:#}; every archived transaction is invisible to status");
            Vec::new()
        }
    }
}

/// How long to wait after `failures` consecutive failures: one poll interval
/// after the first, doubling each time, never past the ceiling.
fn backoff_wait(poll_millis: u64, failures: u32) -> u64 {
    poll_millis
        .saturating_mul(1u64 << failures.min(20))
        .min(RESUME_BACKOFF_CEILING_MILLIS)
}

/// Whether an attempt due at `not_before` must still wait at `now`. A due time
/// further ahead than any wait its owner can set means the clock stepped back,
/// so the attempt is due: a step stalls nothing longer than the wait itself.
fn is_waiting(now: u64, not_before: u64, longest_wait: u64) -> bool {
    now < not_before && not_before - now <= longest_wait
}

/// Whether a transaction moved between two reads. An error note (`last_error`)
/// and its timestamp are commentary on a stuck step, not a step.
fn state_advanced(before: &DeploymentTransaction, after: &DeploymentTransaction) -> bool {
    let mut after = after.clone();
    after.last_error.clone_from(&before.last_error);
    after.updated_at_unix_millis = before.updated_at_unix_millis;
    &after != before
}

/// Says a fault once while it lasts. A scheduler tick that hits the same
/// unreadable file every half second must not print it every half second.
#[derive(Default)]
struct ReportOnce {
    last: Mutex<Option<String>>,
}

impl ReportOnce {
    /// The text to print now: `report` when it differs from the last one
    /// offered, nothing while the same fault stands or once it has cleared.
    fn offer(&self, report: Option<String>) -> Option<String> {
        let mut last = self.last.lock().expect("report mutex");
        if *last == report {
            return None;
        }
        last.clone_from(&report);
        report
    }
}

/// Decode every archived transaction. An entry that fails is returned with its
/// key and reason instead of vanishing, so the reader can say how many records
/// its answer is missing.
///
/// `LegacyDeploymentTransaction` decodes the v2 and v3 layouts. `history.cc` is
/// never migrated or pruned, so it dies only when history is pruned or
/// migrated, not at boot.
fn decode_history_transactions(
    envelopes: Vec<CultCacheEnvelope>,
) -> (Vec<DeploymentTransaction>, Vec<(String, String)>) {
    let mut transactions = Vec::new();
    let mut undecodable = Vec::new();
    for envelope in envelopes
        .into_iter()
        .filter(|envelope| envelope.r#type == DeploymentTransaction::TYPE)
    {
        let decoded = match envelope.schema_id.as_deref() {
            Some(DEPLOYMENT_TRANSACTION_SCHEMA_V2 | DEPLOYMENT_TRANSACTION_SCHEMA_V3) => {
                lift_legacy_transaction(&envelope)
            }
            _ => rmp_serde::from_slice::<DeploymentTransaction>(&envelope.payload)
                .map_err(anyhow::Error::from),
        };
        match decoded {
            Ok(transaction) => transactions.push(transaction),
            Err(error) => undecodable.push((envelope.key, format!("{error:#}"))),
        }
    }
    (transactions, undecodable)
}

/// Consumed commands, read as leniently as their transactions.
fn read_history_commands(state_store: &Path) -> Vec<DeploymentCommand> {
    let path = history_store_path(state_store);
    if !path.exists() {
        return Vec::new();
    }
    let Ok(envelopes) = SingleFileMessagePackBackingStore::new(&path).pull_all_read_only_snapshot()
    else {
        return Vec::new();
    };
    envelopes
        .into_iter()
        .filter(|envelope| envelope.r#type == DeploymentCommand::TYPE)
        .filter_map(|envelope| rmp_serde::from_slice::<DeploymentCommand>(&envelope.payload).ok())
        .collect()
}

fn replace_transaction(
    store_path: &Path,
    current: &Stored<DeploymentTransaction>,
    next: &DeploymentTransaction,
) -> Result<()> {
    next.validate()?;
    ensure!(
        current.value.transaction_id == next.transaction_id,
        "transaction replacement changes identity"
    );
    let envelope = transaction_envelope(next, next.updated_at_unix_millis)?;
    ensure!(
        SingleFileMessagePackBackingStore::new(store_path).compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: DeploymentTransaction::TYPE.into(),
                key: current.value.transaction_id.clone(),
                current: Some(current.envelope.clone()),
            }],
            std::slice::from_ref(&envelope),
        )?,
        "deployment transaction changed before its compare-exchange"
    );
    if next.is_terminal() {
        archive_terminal_transaction(store_path, &envelope)?;
    }
    Ok(())
}

#[derive(Clone)]
struct LoadedBinding {
    binding: OperatorBinding,
    bytes: Vec<u8>,
}

fn load_bindings(directory: &Path) -> Result<BTreeMap<String, LoadedBinding>> {
    let mut paths = fs::read_dir(directory)
        .with_context(|| format!("reading Idunn binding directory {}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension() == Some(std::ffi::OsStr::new("toml")));
    paths.sort();
    let mut bindings = BTreeMap::new();
    for path in paths {
        let bytes = fs::read(&path)
            .with_context(|| format!("reading operator binding {}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("operator binding {} is not UTF-8", path.display()))?;
        let binding = OperatorBinding::parse(text)
            .with_context(|| format!("validating operator binding {}", path.display()))?;
        let target = binding.target.clone();
        ensure!(
            bindings
                .insert(target.clone(), LoadedBinding { binding, bytes })
                .is_none(),
            "operator binding target {target} is duplicated"
        );
    }
    ensure!(!bindings.is_empty(), "Idunn binding directory is empty");
    validate_route_bindings(&bindings)?;
    Ok(bindings)
}

fn validate_route_bindings(bindings: &BTreeMap<String, LoadedBinding>) -> Result<()> {
    let routes = bindings
        .iter()
        .filter_map(|(target, loaded)| {
            loaded
                .binding
                .route
                .as_ref()
                .map(|route| (target.as_str(), route))
        })
        .collect::<Vec<_>>();
    validate_route_binding_set(&routes)
}

fn validate_route_binding_set(routes: &[(&str, &RouteBinding)]) -> Result<()> {
    for (index, (target, route)) in routes.iter().enumerate() {
        let (stable_host, stable_port) = route.stable_socket()?;
        let private_host = route
            .private_host
            .parse::<std::net::IpAddr>()
            .context("validated route private host stopped being an IP address")?;
        ensure!(
            stable_host != private_host
                || !(route.private_port_start..=route.private_port_end).contains(&stable_port),
            "route {target} stable socket overlaps its candidate port range"
        );

        for (other_target, other) in &routes[index + 1..] {
            ensure!(
                route.route_id != other.route_id,
                "route id {} is shared by targets {target} and {other_target}",
                route.route_id
            );
            ensure!(
                route.config_path != other.config_path,
                "route fragment {} is shared by targets {target} and {other_target}",
                route.config_path.display()
            );
            let other_stable = other.stable_socket()?;
            ensure!(
                route.driver != other.driver || (stable_host, stable_port) != other_stable,
                "stable route socket {}:{} is shared by targets {target} and {other_target}",
                stable_host,
                stable_port
            );
            if route.driver != other.driver {
                continue;
            }
            let other_private_host = other
                .private_host
                .parse::<std::net::IpAddr>()
                .context("validated route private host stopped being an IP address")?;
            let ranges_overlap = route.private_host == other.private_host
                && route.private_port_start <= other.private_port_end
                && other.private_port_start <= route.private_port_end;
            ensure!(
                !ranges_overlap,
                "candidate port ranges overlap for targets {target} and {other_target}"
            );
            ensure!(
                stable_host != other_private_host
                    || !(other.private_port_start..=other.private_port_end).contains(&stable_port),
                "stable route socket for {target} overlaps {other_target}'s candidate range"
            );
            ensure!(
                other_stable.0 != private_host
                    || !(route.private_port_start..=route.private_port_end)
                        .contains(&other_stable.1),
                "stable route socket for {other_target} overlaps {target}'s candidate range"
            );
        }
    }
    Ok(())
}

fn resolve_selector(
    bindings: &BTreeMap<String, LoadedBinding>,
    selector: &str,
) -> Result<Vec<String>> {
    let mut targets = if let Some(profile) = selector.strip_prefix("profile:") {
        bindings
            .values()
            .filter(|binding| binding.binding.profiles.contains(profile))
            .map(|binding| binding.binding.target.clone())
            .collect::<Vec<_>>()
    } else {
        ensure!(
            bindings.contains_key(selector),
            "unknown deployment target {selector}"
        );
        vec![selector.to_owned()]
    };
    ensure!(
        !targets.is_empty(),
        "unknown or empty deployment profile {selector}"
    );
    targets.sort_by(|left, right| {
        (left != "odin")
            .cmp(&(right != "odin"))
            .then_with(|| left.cmp(right))
    });
    Ok(targets)
}

fn submit(
    store_path: &Path,
    selector: &str,
    requested_by: &str,
    wait: bool,
    timeout_seconds: u64,
) -> Result<()> {
    if let Some(parent) = store_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let now = now_millis()?;
    let command = DeploymentCommand {
        schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
        command_id: format!("up-{}", Uuid::new_v4()),
        kind: CommandKind::Deploy,
        selector: selector.into(),
        requested_by: requested_by.into(),
        requested_at_unix_millis: now,
    };
    command.validate()?;
    ensure!(
        SingleFileMessagePackBackingStore::new(store_path).compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: DeploymentCommand::TYPE.into(),
                key: command.command_id.clone(),
                current: None,
            }],
            &[command_envelope(&command, now)?],
        )?,
        "deployment command id collided"
    );
    println!("{}", command.command_id);
    if !wait {
        return Ok(());
    }
    let deadline = now.saturating_add(timeout_seconds.saturating_mul(1000));
    loop {
        let snapshot = ControlSnapshot::read(store_path)?;
        ensure!(
            snapshot
                .commands
                .iter()
                .any(|stored| stored.value.command_id == command.command_id),
            "submitted deployment command disappeared"
        );
        let transactions = snapshot.transaction_for_command(&command.command_id);
        if !transactions.is_empty() && transactions.iter().all(|value| value.is_terminal()) {
            if let Some(error) =
                transactions
                    .iter()
                    .find_map(|transaction| match &transaction.completion {
                        Some(TransactionCompletion::FailedBeforeFencing { error })
                        | Some(TransactionCompletion::FailedAfterFencing { error, .. }) => {
                            Some(error.as_str())
                        }
                        _ => None,
                    })
            {
                bail!("deployment failed: {error}")
            }
            println!("succeeded {} target(s)", transactions.len());
            return Ok(());
        }
        if now_millis()? >= deadline {
            bail!("deployment command timed out")
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn status(store_path: &Path, command_id: Option<&str>) -> Result<()> {
    let snapshot = ControlSnapshot::read(store_path)?;
    // Commands stay resident; their finished transactions do not. Without
    // history a completed command would report as though it had never run,
    // which is the operator surface R11 refused to trade away for a smaller
    // live set.
    let archived = read_history_transactions(store_path);
    let archived_commands = read_history_commands(store_path);
    let mut commands = snapshot
        .commands
        .iter()
        .map(|stored| &stored.value)
        .chain(archived_commands.iter())
        .collect::<Vec<_>>();
    commands.sort_by_key(|command| command.requested_at_unix_millis);
    if let Some(command_id) = command_id {
        commands.retain(|command| command.command_id == command_id);
        ensure!(!commands.is_empty(), "deployment command is unknown");
    }
    for command in commands {
        let mut transactions = snapshot.transaction_for_command(&command.command_id);
        transactions.extend(
            archived
                .iter()
                .filter(|value| value.command_id == command.command_id),
        );
        transactions.sort_by_key(|value| value.ordinal);
        let (state, detail) = derived_command_status(&transactions);
        println!(
            "{} {} {} {}",
            command.command_id, command.selector, state, detail
        );
        // Naming one command asks about that command, so print what a stuck
        // transaction is actually waiting on. A gate reason lives in
        // `last_error` and was never rendered anywhere, which left "Sealing"
        // looking identical whether the brake had not authorized the
        // transaction or the phase was simply slow. The release and deployment
        // ids are here because they are exactly what a brake release must name.
        if command_id.is_some() {
            for transaction in transactions {
                println!("  transaction {}", transaction.transaction_id);
                println!(
                    "    target {} phase {:?}",
                    transaction.target, transaction.phase
                );
                if let Some(expected) = &transaction.expected {
                    println!("    runtime {}", expected.runtime_id);
                    println!("    release {}", expected.sealed_release_id);
                }
                println!(
                    "    odin publisher cursor {}",
                    transaction.odin_publisher_sequence_cursor
                );
                if let Some(evidence) = &transaction.latest_odin_observation {
                    println!(
                        "    latest odin observation sequence {}",
                        evidence.publisher_sequence
                    );
                }
                if let Some(reason) = &transaction.last_error {
                    println!("    waiting on {reason}");
                }
                // A Complete transaction that is not terminal still owns its
                // target through unfinished cleanup. Without this an operator
                // sees "Complete" and a stale reason and cannot tell what is
                // holding the target.
                if let Some(completion) = &transaction.completion {
                    println!("    completion {completion:?}");
                }
                if let Some(abort) = &transaction.pre_fencing_abort {
                    println!("    pre-fencing abort {abort:?}");
                }
                if let Some(abort) = &transaction.post_fencing_abort {
                    println!("    post-fencing abort {abort:?}");
                }
                if let Some(cleanup) = &transaction.post_commit_cleanup {
                    println!("    post-commit cleanup {cleanup:?}");
                }
                println!(
                    "    terminal {} owns-target {}",
                    transaction.is_terminal(),
                    transaction.blocks_new_target_mutation()
                );
            }
        }
    }
    // What supervision holds for each target: the union of the admitted
    // generations and the metered targets, so a target that has only been
    // deployed to (no generation yet) still shows what it has used.
    let now = now_millis()?;
    for target in supervised_targets(&snapshot) {
        for line in render_supervision(
            target,
            snapshot.admitted_for(target).map(|stored| &stored.value),
            snapshot.supervision_for(target).map(|stored| &stored.value),
            now,
        ) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Every target supervision holds something for: the admitted generations
/// and the metered targets.
fn supervised_targets(snapshot: &ControlSnapshot) -> BTreeSet<&str> {
    snapshot
        .admitted
        .iter()
        .map(|stored| stored.value.target.as_str())
        .chain(snapshot.targets.iter().map(|stored| stored.value.target.as_str()))
        .collect()
}

/// One target's meters and route pacing, one line each, times in unix
/// milliseconds. The counts are what is still inside the window at `now`.
fn render_supervision(
    target: &str,
    generation: Option<&AdmittedGeneration>,
    supervision: Option<&TargetSupervision>,
    now: u64,
) -> Vec<String> {
    let at = |time: Option<u64>| time.map_or("none".to_owned(), |millis| millis.to_string());
    let meters = supervision.cloned().unwrap_or_else(|| TargetSupervision::new(target));
    let mut lines = vec![format!(
        "target {target} {}",
        generation.map_or("no-admitted-generation", |value| value.generation_id.as_str())
    )];
    if let Some(reason) = generation.and_then(|value| value.last_error.as_deref()) {
        lines.push(format!("  held: {reason}"));
    }
    lines.push(format!(
        "  continuity restarts {}/{} next-restart-at {}",
        meters.restarts_used(now),
        CONTINUITY_RESTART_ATTEMPTS,
        at(meters.next_restart_at(now))
    ));
    if let (Some(until), Some(reason)) = (
        meters.continuity_deferred_until,
        &meters.continuity_deferral_reason,
    ) {
        lines.push(format!("  continuity deferred until {until}: {reason}"));
    }
    if let Some(route) = generation.and_then(|value| value.route_supervision.as_ref()) {
        lines.push(format!(
            "  route {} consecutive-failures {} last-challenge-at {} next-challenge-at {}",
            match route.degraded_since_unix_millis {
                Some(since) => format!("degraded-since {since}"),
                None => "healthy".to_owned(),
            },
            route.consecutive_failures,
            at(route.last_challenge_at_unix_millis),
            at(route.next_challenge_at_unix_millis)
        ));
    }
    if generation.is_none_or(|value| value.route_supervision.is_some())
        || !meters.route_actuations.is_empty()
    {
        lines.push(format!(
            "  route actuations {}/{} in window, reopens-at {}",
            meters.route_used(now),
            ROUTE_ACTUATION_CEILING,
            at(meters.route_reopens_at(now))
        ));
    }
    lines
}

fn derived_command_status(transactions: &[&DeploymentTransaction]) -> (&'static str, String) {
    if transactions.is_empty() {
        return ("queued", String::new());
    }
    // A live transaction is the command's present tense. Reporting an older
    // attempt's failure while a new one is sealing hid a running deployment
    // behind the word "failed".
    let current = transactions
        .iter()
        .filter(|transaction| !transaction.is_terminal())
        .map(|transaction| format!("{}:{:?}", transaction.target, transaction.phase))
        .collect::<Vec<_>>();
    if !current.is_empty() {
        return ("running", current.join(","));
    }
    if let Some(error) = transactions
        .iter()
        .find_map(|transaction| match &transaction.completion {
            Some(TransactionCompletion::FailedBeforeFencing { error })
            | Some(TransactionCompletion::FailedAfterFencing { error, .. }) => Some(error.clone()),
            _ => None,
        })
    {
        return ("failed", error);
    }
    if transactions.iter().all(|transaction| {
        transaction.is_terminal()
            && matches!(
                transaction.completion,
                Some(TransactionCompletion::Admitted { .. })
            )
    }) {
        return (
            "succeeded",
            transactions
                .iter()
                .map(|transaction| transaction.target.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    ("complete", String::new())
}

struct ProcessLock {
    file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl ProcessLock {
    fn acquire(store_path: &Path) -> Result<Self> {
        let path = sibling_path(store_path, ".daemon.lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("opening Idunn daemon lock {}", path.display()))?;
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            ensure!(result == 0, "another Idunn daemon owns this control store");
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("another Idunn daemon owns {}", path.display()))?;
            Ok(Self { file, path })
        }
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Engine {
    options: RuntimeOptions,
    idunn_signer: ServiceIdentitySigner<IdunnServiceIdentity>,
    idunn_anchor: ServiceIdentityTrustAnchor,
    bootstrap_odin_authority: AdmittedOdinAuthority,
    source: Arc<dyn SourcePort>,
    docker_runner: DockerRunnerDriver,
    systemd_workload: Arc<dyn WorkloadPort>,
    host_actuators: Option<SharedHostActuatorHub>,
    history_report: ReportOnce,
    /// One `ReportOnce` per faulting record, so one wedged transaction says
    /// its fault once and never drowns another's.
    fault_reports: Mutex<BTreeMap<String, ReportOnce>>,
    /// Retry pacing for a transaction whose step keeps failing, by
    /// transaction id: consecutive failures and the earliest next attempt.
    /// Process-local on purpose: a restart retries at once, and no schema
    /// carries a clock that only pacing reads.
    resume_backoff: Mutex<BTreeMap<String, ResumeBackoff>>,
}

/// The route driver's door to the actuation ceiling: every group of host
/// mutations a driver makes for `target` is counted through here. The gate
/// owns which charge a request is: a driver asks for the change it is making,
/// and the command that owns the transaction decides whether that change is
/// deployment (`Forward`, refusable) or the continuity of the admitted
/// generation (`Survival`, never refused).
struct EngineRouteGate<'a> {
    engine: &'a Engine,
    target: &'a str,
    command: CommandKind,
}

impl RouteActuationGate for EngineRouteGate<'_> {
    fn admit(&self, requested: RouteActuation) -> Result<()> {
        let kind = match self.command {
            CommandKind::Deploy => requested,
            CommandKind::Continuity => RouteActuation::Survival,
        };
        self.engine.charge_route_actuation(self.target, kind)
    }
}

#[derive(Clone, Copy)]
struct ResumeBackoff {
    failures: u32,
    not_before_unix_millis: u64,
}

/// Longest wait between attempts at a failing transaction step.
const RESUME_BACKOFF_CEILING_MILLIS: u64 = 60_000;

impl Engine {
    fn open(options: RuntimeOptions) -> Result<Self> {
        Self::open_with_systemd_workload(
            options,
            Arc::new(SystemdTransientWorkloadDriver::default()),
        )
    }

    /// `open` with the systemd workload port supplied. It is the seam that
    /// lets the phase engine run past Fencing without a systemd to talk to.
    fn open_with_systemd_workload(
        options: RuntimeOptions,
        systemd_workload: Arc<dyn WorkloadPort>,
    ) -> Result<Self> {
        let idunn_signer =
            open_service_identity_at::<IdunnServiceIdentity>(&options.idunn_identity_store)
                .context("opening Idunn activation identity")?;
        let idunn_anchor = idunn_signer.trust_anchor()?;
        let odin_anchor = read_trust_anchor::<OdinTopologyIdentity>(&options.odin_trust_anchor)
            .context("reading bootstrap Odin topology anchor")?;
        let bootstrap_odin_authority = AdmittedOdinAuthority::from_anchor(&odin_anchor)?;
        let source: Arc<dyn SourcePort> = Arc::new(GitSourceDriver::new(
            &options.source_root,
            options.staging_root.join("frozen-sources"),
            options.source_identity,
        ));
        let host_actuators = options
            .host_actuator_bind
            .map(|bind| HostActuatorHub::bind(bind).map(|hub| Arc::new(Mutex::new(hub))))
            .transpose()?;
        if let Some(hub) = &host_actuators {
            eprintln!(
                "Idunn host actuator hub listening on {}",
                hub.lock().expect("hub mutex").local_addr()?
            );
            let service_signer =
                open_service_identity_at::<IdunnServiceIdentity>(&options.idunn_identity_store)
                    .context("opening Idunn identity for the host actuator hub thread")?;
            let bindings_dir = options.bindings_dir.clone();
            spawn_hub_service(Arc::clone(hub), service_signer, move || {
                host_anchors_from(&bindings_dir)
            });
        }
        Ok(Self {
            options,
            idunn_signer,
            idunn_anchor,
            bootstrap_odin_authority,
            source,
            docker_runner: DockerRunnerDriver::default(),
            systemd_workload,
            host_actuators,
            history_report: ReportOnce::default(),
            fault_reports: Mutex::default(),
            resume_backoff: Mutex::default(),
        })
    }

    fn host_anchors(&self) -> BTreeMap<String, ServiceIdentityTrustAnchor> {
        host_anchors_from(&self.options.bindings_dir)
    }

    /// Every route driver comes from here, so the actuator paths are the
    /// options' and nothing else's.
    fn route_driver(&self, binding: RouteBinding) -> NginxRouteDriver {
        NginxRouteDriver::with_actuators(binding, &self.options.route_actuators)
    }

    fn route_gate<'a>(&'a self, target: &'a str, command: CommandKind) -> EngineRouteGate<'a> {
        EngineRouteGate {
            engine: self,
            target,
            command,
        }
    }

    /// Count one route actuation against the target's sliding ceiling, in its
    /// `TargetSupervision`, before the driver acts. The record is created by
    /// the first charge, so a first deployment is metered like any other. A
    /// refused `Forward` is the typed `RouteActuationRefused`, and a `Forward`
    /// that cannot be recorded is refused too: no charge, no actuation. A
    /// `Survival` is never refused, so it is counted when it can be and runs
    /// regardless when it cannot: an unwritable store must not stop a rollback,
    /// a withdrawal or a repair.
    fn charge_route_actuation(&self, target: &str, kind: RouteActuation) -> Result<()> {
        match self.record_route_actuation(target, kind) {
            Ok(charged) => charged.map_err(anyhow::Error::new),
            Err(error) if kind == RouteActuation::Survival => {
                eprintln!(
                    "Idunn could not count a survival route actuation of {target}; it proceeds uncounted: {error:#}"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The outer error is the ledger failing to read or write; the inner one
    /// is the ceiling refusing.
    fn record_route_actuation(
        &self,
        target: &str,
        kind: RouteActuation,
    ) -> Result<Result<(), RouteActuationRefused>> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let seen = snapshot.supervision_for(target);
        let mut next = snapshot.supervision_or_new(target);
        let now = now_millis()?;
        let charged = next.charge_route(now, kind);
        // A refusal writes nothing unless the clock had stepped back and the
        // log needed settling.
        if charged.is_ok() || seen.is_some_and(|stored| stored.value != next) {
            self.write_target_supervision(seen, &next, now)?;
        }
        Ok(charged)
    }

    /// Replace a target's meters, or create them when `seen` is `None`, by
    /// compare-exchange against exactly what was read.
    fn write_target_supervision(
        &self,
        seen: Option<&Stored<TargetSupervision>>,
        next: &TargetSupervision,
        now: u64,
    ) -> Result<()> {
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: TargetSupervision::TYPE.into(),
                    key: next.target.clone(),
                    current: seen.map(|stored| stored.envelope.clone()),
                }],
                &[target_supervision_envelope(next, now)?],
            )?,
            "target supervision changed before its meters were written"
        );
        Ok(())
    }

    /// Replace an admitted generation by compare-exchange against the envelope
    /// that was read.
    fn replace_generation(
        &self,
        seen: &Stored<AdmittedGeneration>,
        next: &AdmittedGeneration,
        now: u64,
    ) -> Result<()> {
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: AdmittedGeneration::TYPE.into(),
                    key: seen.value.target.clone(),
                    current: Some(seen.envelope.clone()),
                }],
                &[admitted_envelope(next, now)?],
            )?,
            "admitted generation changed before its write"
        );
        Ok(())
    }

    /// Say a fault once while it lasts, under `key`.
    fn report_once(&self, key: &str, message: String) {
        let offered = self
            .fault_reports
            .lock()
            .expect("fault report mutex")
            .entry(key.to_string())
            .or_default()
            .offer(Some(message));
        if let Some(message) = offered {
            eprintln!("{message}");
        }
    }

    fn host_access(&self) -> Result<HostActuatorAccess<'_>> {
        let hub = self
            .host_actuators
            .as_ref()
            .context("this Idunn serves no host actuators (no --host-actuator-bind)")?;
        Ok(HostActuatorAccess {
            hub: Arc::clone(hub),
            anchors: self.host_anchors(),
            signer: &self.idunn_signer,
        })
    }
}

/// The trust anchor of every host a binding names. A binding whose anchor
/// cannot be read leaves its host unattachable and is logged; the other
/// hosts are unaffected.
fn host_anchors_from(bindings_dir: &Path) -> BTreeMap<String, ServiceIdentityTrustAnchor> {
    {
        let mut anchors = BTreeMap::new();
        let bindings = match load_bindings(bindings_dir) {
            Ok(bindings) => bindings,
            Err(error) => {
                eprintln!("Idunn cannot read bindings for host anchors: {error:#}");
                return anchors;
            }
        };
        for loaded in bindings.values() {
            let WorkloadBinding::HostActuator(workload) = &loaded.binding.workload else {
                continue;
            };
            match read_trust_anchor::<IdunnHostActuatorIdentity>(&workload.host_trust_anchor_store)
            {
                Ok(anchor) => {
                    anchors.insert(workload.host.clone(), anchor);
                }
                Err(error) => eprintln!(
                    "Idunn cannot read the actuator anchor for host {}: {error:#}",
                    workload.host
                ),
            }
        }
        anchors
    }
}

impl Engine {
    /// The runner the plan's binding declares. Every runner in one binding
    /// is of one kind; the binding validated that.
    fn runner_for(&self, plan: &CompiledDeploymentPlan) -> Result<Box<dyn RunnerPort + '_>> {
        let (_, binding) = plan.parsed_inputs()?;
        Ok(match &binding.workload {
            WorkloadBinding::SystemdTransient(_) => Box::new(self.docker_runner.clone()),
            WorkloadBinding::HostActuator(_) => Box::new(HostActuatorRunnerDriver {
                access: self.host_access()?,
            }),
        })
    }

    /// The workload driver the plan's binding declares.
    fn workload_for(&self, plan: &CompiledDeploymentPlan) -> Result<Arc<dyn WorkloadPort + '_>> {
        let (_, binding) = plan.parsed_inputs()?;
        Ok(match &binding.workload {
            WorkloadBinding::SystemdTransient(_) => Arc::clone(&self.systemd_workload),
            WorkloadBinding::HostActuator(_) => Arc::new(HostActuatorWorkloadDriver {
                access: self.host_access()?,
            }),
        })
    }

    /// History for a decision that would be wrong on an empty answer: the
    /// continuity restart ceiling and the check that a command is consumed.
    /// `None` means `history.cc` cannot be read, so the decision is not made;
    /// a history that merely has no entries is `Some`. The fault is printed
    /// once while it lasts, not on every tick.
    fn history_for_decision(&self) -> Option<Vec<DeploymentTransaction>> {
        let (report, transactions) = match read_history(&self.options.state_store) {
            Ok(history) => (history.report(), Some(history.transactions)),
            Err(error) => (
                Some(format!(
                    "{error:#}; continuity and command retirement stop until it is readable"
                )),
                None,
            ),
        };
        if let Some(text) = self.history_report.offer(report) {
            eprintln!("{text}");
        }
        transactions
    }

    fn topology(&self) -> CultCacheTopologyDriver {
        CultCacheTopologyDriver {
            projection_store: self.options.topology_store.clone(),
            correlation_store: self.options.odin_correlation_store.clone(),
        }
    }

    fn current_odin_authority(&self, snapshot: &ControlSnapshot) -> Result<AdmittedOdinAuthority> {
        // The bootstrap key stands only until Odin itself is admitted. Once it
        // is, its own generation names the authority; an admitted Odin that
        // carries none has no authority to name, and falling back to the
        // bootstrap key would let a stale key vouch for the Verse.
        let authority = match snapshot.admitted_odin() {
            None => self.bootstrap_odin_authority.clone(),
            Some(stored) => stored.value.odin_authority.clone().with_context(|| {
                "admitted Odin generation carries no Odin authority (route-proof readiness)"
            })?,
        };
        authority.validate()?;
        Ok(authority)
    }

    fn trusted_topology_context(&self, now: u64) -> OdinTopologyAuthenticationContext {
        OdinTopologyAuthenticationContext {
            trusted_received_at_unix_millis: now,
            maximum_age_millis: self.options.topology_maximum_age_millis,
            maximum_future_skew_millis: self.options.topology_maximum_future_skew_millis,
        }
    }

    /// Re-prove the evidence that still gates a decision.
    ///
    /// Terminal transactions are skipped: they describe what already happened
    /// and authorize nothing further, so re-proving them only creates ways for
    /// history to refuse a boot. On yggdrasil that is 259 of 260 records.
    ///
    /// The operator anchor is read on demand rather than up front. Reading it
    /// unconditionally meant an absent brake artifact refused startup against
    /// an *empty* store -- a deployment brake gating Idunn itself, which
    /// `F:\Projects\CLAUDE.md` forbids outright: a target brake "may never gate
    /// Idunn itself or unrelated service lifecycle".
    fn validate_durable_authority(&self, snapshot: &ControlSnapshot) -> Result<()> {
        let mut operator_anchor = None;
        for stored in &snapshot.transactions {
            let transaction = &stored.value;
            if transaction.is_terminal() {
                continue;
            }
            if let Some(authorization) = &transaction.deployment_authorization {
                authorization.validate_shape()?;
                let record: IdunnDeploymentBrakeRecord =
                    rmp_serde::from_slice(&authorization.canonical_brake_bytes)?;
                if operator_anchor.is_none() {
                    operator_anchor =
                        Some(read_trust_anchor::<IdunnDeploymentBrakeOperatorIdentity>(
                            &self.options.deployment_brake_operator_anchor,
                        )?);
                }
                verify_idunn_deployment_brake_authorization(
                    &record,
                    operator_anchor.as_ref().expect("read directly above"),
                )?;
                let expected = required(&transaction.expected, "authorized Expected projection")?;
                ensure!(
                    record.authorized_release_id.as_deref()
                        == Some(expected.sealed_release_id.as_str())
                        && record.authorized_deployment_id.as_deref()
                            == Some(transaction.transaction_id.as_str())
                        && record.runtime_id == expected.runtime_id,
                    "durable deployment authorization names another release or transaction"
                );
            }
            let lease = transaction
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256);
            if let Some(evidence) = &transaction.latest_odin_observation {
                let authenticated = self.authenticate_topology_bytes(
                    snapshot,
                    transaction,
                    &evidence.canonical_bytes,
                    lease,
                    evidence.admitted_at_unix_millis,
                )?;
                validate_authenticated_evidence(evidence, &authenticated)?;
            }
            if let Some(evidence) = &transaction.warming {
                match evidence {
                    WarmingEvidence::OdinTopology { evidence } => {
                        let authenticated = self.authenticate_topology_bytes(
                            snapshot,
                            transaction,
                            &evidence.canonical_bytes,
                            None,
                            evidence.admitted_at_unix_millis,
                        )?;
                        validate_authenticated_evidence(evidence, &authenticated)?;
                        let incumbent_lease_sha256 =
                            self.incumbent_lease_sha256_for_warming(snapshot, transaction)?;
                        ensure!(
                            is_semantic_warming(
                                required(&transaction.expected, "Warming Expected projection",)?,
                                required(&transaction.activation, "Warming activation")?,
                                incumbent_lease_sha256.as_deref(),
                                &authenticated,
                            )?,
                            "durable Warming gate is not supported by current runtime evidence"
                        );
                    }
                    WarmingEvidence::FirstOdinDirect { evidence } => {
                        self.authenticate_first_odin_warming_presence(
                            transaction,
                            &evidence.message_id,
                            evidence.challenged_at_unix_millis,
                            evidence.admitted_at_unix_millis,
                            &evidence.canonical_bytes,
                        )?;
                    }
                    WarmingEvidence::RouteProofDirect { evidence } => {
                        self.reauthenticate_route_proof(
                            transaction,
                            evidence,
                            route_proof_warming_states(required(
                                &transaction.expected,
                                "Warming Expected projection",
                            )?),
                            None,
                            evidence.admitted_at_unix_millis,
                        )?;
                    }
                }
            }
            if let Some(ReadinessEvidence::RouteProof { evidence }) = &transaction.ready {
                self.reauthenticate_route_proof(
                    transaction,
                    evidence,
                    &["active"],
                    lease,
                    evidence.admitted_at_unix_millis,
                )?;
            }
            if let Some(evidence) = transaction.ready.as_ref().and_then(ReadinessEvidence::odin) {
                let authenticated = self.authenticate_topology_bytes(
                    snapshot,
                    transaction,
                    &evidence.canonical_bytes,
                    lease,
                    evidence.admitted_at_unix_millis,
                )?;
                validate_authenticated_evidence(evidence, &authenticated)?;
                ensure!(
                    is_semantic_ready(&authenticated),
                    "durable Ready label is not exact semantic Ready"
                );
            }
        }
        let odin_authority = self.current_odin_authority(snapshot)?;
        for stored in &snapshot.admitted {
            let generation = &stored.value;
            // Only a generation whose own Expected says Odin vouches for it has
            // Odin receipts to re-prove. One whose evidence disagrees with its
            // class is held and reported by supervision, not re-proved here.
            if !matches!(generation.readiness(), Ok(class) if class != ReadinessClass::RouteProof)
            {
                continue;
            }
            let odin = generation.odin_receipts()?;
            let authority = self.runtime_authority_parts(
                &generation.plan,
                &generation.expected,
                &generation.activation,
            )?;
            let latest = authenticate_odin_runtime_topology_correlation(
                &odin.latest.canonical_bytes,
                &authority,
                generation.leasing.lease_sha256(),
                &odin_authority.signer_public_key,
                self.trusted_topology_context(odin.latest.admitted_at_unix_millis),
            )?;
            validate_authenticated_evidence(odin.latest, &latest)?;
            let ready = authenticate_odin_runtime_topology_correlation(
                &odin.ready.canonical_bytes,
                &authority,
                generation.leasing.lease_sha256(),
                &odin_authority.signer_public_key,
                self.trusted_topology_context(odin.ready.admitted_at_unix_millis),
            )?;
            validate_authenticated_evidence(odin.ready, &ready)?;
            ensure!(
                is_semantic_ready(&ready),
                "admitted generation Ready label is not exact semantic Ready"
            );
        }
        Ok(())
    }
}

/// What the boot reconciliation of failed continuity projections found.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProjectionReconciliation {
    /// Failed continuity transactions whose exact activation was demoted.
    demoted: Vec<String>,
    /// Targets whose projection names an activation that neither the admitted
    /// generation, a live transaction, nor a failed issuer accounts for. They
    /// are reported and left alone.
    unexplained: Vec<String>,
}

impl Engine {
    /// Once at boot: demote the activation a failed continuity left standing,
    /// where the projection still names exactly the activation that failed
    /// transaction issued. A continuity abort before the single resolution
    /// rule left such residue (and the lift owes the same demotion to any
    /// abort still resident). The issuer is identified by exact activation,
    /// from history and from resident terminal records not yet retired to it.
    ///
    /// Nothing is adopted: an activation with no failed issuer is never
    /// demoted or kept on anyone's behalf, only reported.
    fn reconcile_failed_continuity_projections(&self) -> Result<ProjectionReconciliation> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let mut outcome = ProjectionReconciliation::default();
        // Resident issuers need no history; only the history issuers wait on it.
        let history = self.history_for_decision();
        if history.is_none() {
            eprintln!(
                "Idunn cannot read history at boot: failed continuities archived there are not \
                 reconciled, resident ones still are"
            );
        }
        let topology = self.topology();
        let issuers = history
            .iter()
            .flatten()
            .chain(snapshot.transactions.iter().map(|stored| &stored.value))
            .filter(|transaction| {
                transaction.command_kind == CommandKind::Continuity
                    && matches!(
                        transaction.completion,
                        Some(TransactionCompletion::FailedBeforeFencing { .. })
                            | Some(TransactionCompletion::FailedAfterFencing { .. })
                    )
            });
        for issuer in issuers {
            let (Some(expected), Some(activation), Some(plan)) =
                (&issuer.expected, &issuer.activation, &issuer.plan)
            else {
                continue;
            };
            let demoted = (|| -> Result<bool> {
                if topology.projected_activation(expected)?.as_ref() != Some(activation) {
                    return Ok(false);
                }
                let provider_anchor = self.provider_anchor_for_plan(plan)?;
                topology.demote_to_expected_only(expected, &provider_anchor, activation, None)?;
                Ok(true)
            })();
            match demoted {
                Ok(true) => outcome.demoted.push(issuer.transaction_id.clone()),
                Ok(false) => {}
                Err(error) => eprintln!(
                    "Idunn could not reconcile the projection of failed continuity {}: {error:#}",
                    issuer.transaction_id
                ),
            }
        }
        for generation in &snapshot.admitted {
            let projected = match topology.projected_activation(&generation.value.expected) {
                Ok(projected) => projected,
                Err(error) => {
                    eprintln!(
                        "Idunn cannot read the projected activation of {}: {error:#}",
                        generation.value.target
                    );
                    continue;
                }
            };
            let Some(projected) = projected else {
                continue;
            };
            let accounted = projected == generation.value.activation
                || snapshot.transactions.iter().any(|stored| {
                    stored.value.target == generation.value.target
                        && !stored.value.is_terminal()
                        && stored.value.activation.as_ref() == Some(&projected)
                });
            if !accounted {
                outcome.unexplained.push(generation.value.target.clone());
            }
        }
        Ok(outcome)
    }
}

/// Everything the daemon does once, before its loop: lock, migrate, validate,
/// reconcile the projection. The lock is returned so the caller holds it for as
/// long as the engine runs.
fn boot(options: RuntimeOptions) -> Result<(ProcessLock, Engine)> {
    for path in [
        &options.state_store,
        &options.topology_store,
        &options.staging_root,
    ] {
        let directory = if path.extension().is_some() {
            path.parent()
                .context("configured Idunn path has no parent")?
        } else {
            path.as_path()
        };
        fs::create_dir_all(directory)
            .with_context(|| format!("creating Idunn directory {}", directory.display()))?;
    }
    let lock = ProcessLock::acquire(&options.state_store)?;
    let migrated = migrate_control_store_to_current_schema(&options.state_store)
        .context("migrating Idunn control records to the current schema")?;
    if migrated > 0 {
        println!(
            "migrated {migrated} control record(s) to {DEPLOYMENT_TRANSACTION_SCHEMA} / {ADMITTED_GENERATION_SCHEMA}"
        );
    }
    ControlSnapshot::read(&options.state_store).context("validating all Idunn records")?;
    let engine = Engine::open(options)?;
    engine.validate_durable_authority(&ControlSnapshot::read(&engine.options.state_store)?)?;
    match engine.reconcile_failed_continuity_projections() {
        Ok(outcome) => {
            for transaction_id in &outcome.demoted {
                println!("demoted the activation failed continuity {transaction_id} left projected");
            }
            for target in &outcome.unexplained {
                eprintln!(
                    "Idunn found an activation projected for {target} that no admitted generation, \
                     live transaction or failed continuity accounts for; left as it is"
                );
            }
        }
        Err(error) => eprintln!("Idunn boot projection reconciliation failed: {error:#}"),
    }
    Ok((lock, engine))
}

fn serve(options: RuntimeOptions) -> Result<()> {
    let (_lock, engine) = boot(options)?;
    loop {
        match engine.run_scheduler_tick() {
            Ok(true) => continue,
            Ok(false) => {}
            // A fault in one transaction is that transaction's problem. Idunn
            // runs under Restart=always, so propagating it here turns a single
            // unreadable record into a crashloop that takes every unrelated
            // target's continuity down with it -- the daemon-survival organ
            // killed by the thing it exists to survive. Log against the tick
            // and keep going; the fault recurs every poll until it is fixed,
            // which is louder than a restart loop and cheaper than an outage.
            Err(error) => eprintln!("Idunn scheduler tick failed: {error:#}"),
        }
        thread::sleep(Duration::from_millis(engine.options.poll_millis));
    }
}

impl Engine {
    /// One resident terminal transaction goes to history per tick.
    ///
    /// Transactions that finished before finished transactions travelled to
    /// history are still resident with their commands, gating nothing and
    /// consuming their commands correctly, but growing the store every decision
    /// reads. They leave the same way a transaction finishing today does.
    fn retire_one_terminal_transaction(&self) -> Result<bool> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        // A record that cannot be archived (history unreadable, a torn write)
        // is that record's fault: the next terminal record is still tried, and
        // the tick goes on to supervise and freeze.
        for stored in snapshot
            .transactions
            .iter()
            .filter(|stored| stored.value.is_terminal())
        {
            match archive_terminal_transaction(&self.options.state_store, &stored.envelope) {
                Ok(()) => {
                    eprintln!(
                        "Idunn retired resident terminal transaction {} to history",
                        stored.value.transaction_id
                    );
                    return Ok(true);
                }
                Err(error) => self.note_fault(
                    "could not retire terminal transaction",
                    &stored.value.transaction_id,
                    &error,
                ),
            }
        }
        Ok(false)
    }

    /// One pass of the scheduler. An unreadable control store is the tick's
    /// fault and is returned; anything one transaction or one admitted
    /// generation does wrong is logged against it and never stops the others.
    fn run_scheduler_tick(&self) -> Result<bool> {
        if self.retire_one_terminal_transaction()? {
            return Ok(true);
        }
        let transaction_progress = self.resume_one_transaction()?;
        let continuity_progress = self.supervise_one_admitted_generation()?;
        // Freezing is not gated on the others' progress: a queued command
        // whose target is free must not wait behind an unrelated target's
        // wedge. `freeze_command` already refuses a busy target, so this is
        // still at most one freeze per tick and never a second claimant.
        let froze = self.freeze_one_queued_command()?;
        Ok(transaction_progress || continuity_progress || froze)
    }

    /// Say a per-record fault once while it lasts and leave it in the
    /// record's `last_error`, which status renders. Recording is best effort:
    /// a store too broken to take the note is already in stderr.
    fn note_fault(&self, what: &str, transaction_id: &str, error: &anyhow::Error) {
        let detail = truncate(&format!("{error:#}"), 2048);
        let offered = {
            let mut reports = self.fault_reports.lock().expect("fault report mutex");
            reports
                .entry(transaction_id.to_string())
                .or_default()
                .offer(Some(detail.clone()))
        };
        let recorded = self.record_last_error(transaction_id, &detail);
        if offered.is_some() {
            eprintln!("Idunn {what} {transaction_id}: {detail}");
            if let Err(error) = recorded {
                eprintln!("Idunn could not leave that in the record of {transaction_id}: {error:#}");
            }
        }
    }

    fn clear_fault(&self, transaction_id: &str) {
        self.fault_reports
            .lock()
            .expect("fault report mutex")
            .remove(transaction_id);
    }

    /// Write `detail` as the record's `last_error`, touching nothing else and
    /// not archiving: a terminal record whose archive is the fault stays put.
    fn record_last_error(&self, transaction_id: &str, detail: &str) -> Result<()> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let Some(stored) = snapshot
            .transactions
            .iter()
            .find(|stored| stored.value.transaction_id == transaction_id)
        else {
            return Ok(());
        };
        if stored.value.last_error.as_deref() == Some(detail) {
            return Ok(());
        }
        let envelope = cleanup_evidence::last_error_envelope(&stored.value, detail)?;
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: transaction_id.to_string(),
                    current: Some(stored.envelope.clone()),
                }],
                &[envelope],
            )?,
            "transaction changed before its fault note"
        );
        Ok(())
    }

    /// Startup and every later loop use the same order: unfinished ownership
    /// work first, admitted-body continuity second, new commands last. A
    /// waiting transaction yields without relaxing ordinal order inside its
    /// own command, so one target brake cannot suspend unrelated continuity.
    fn resume_one_transaction(&self) -> Result<bool> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let mut progressed = false;
        let mut candidates = snapshot
            .transactions
            .iter()
            .filter(|stored| !stored.value.is_terminal())
            .collect::<Vec<_>>();
        candidates.sort_by_key(|stored| {
            (
                stored.value.created_at_unix_millis,
                stored.value.ordinal,
                stored.value.transaction_id.as_str(),
            )
        });
        for current in candidates {
            if snapshot.has_earlier_authority_sibling(&current.value) {
                continue;
            }
            match self.resume_candidate(current) {
                Ok(moved) => progressed |= moved,
                Err(error) => {
                    self.note_fault(
                        "could not resume transaction",
                        &current.value.transaction_id,
                        &error,
                    );
                }
            }
        }
        Ok(progressed)
    }

    /// Advance one transaction, turning its failure into the durable record
    /// its phase calls for. Returns whether its record changed.
    ///
    /// "Moved" means the transaction's state advanced: a phase change, a
    /// completion, or a durable abort step. Rewriting `last_error` to say the
    /// same step failed again in new words is not movement, or a wedged
    /// transaction would report progress every tick.
    fn resume_candidate(&self, current: &Stored<DeploymentTransaction>) -> Result<bool> {
        let id = current.value.transaction_id.as_str();
        if self
            .resume_backoff
            .lock()
            .expect("resume backoff mutex")
            .get(id)
            .is_some_and(|backoff| {
                now_millis().is_ok_and(|now| {
                    is_waiting(now, backoff.not_before_unix_millis, RESUME_BACKOFF_CEILING_MILLIS)
                })
            })
        {
            return Ok(false);
        }
        if let Some(disagreement) = current.value.held_disagreement() {
            self.note_fault("holds transaction", id, &anyhow::Error::new(disagreement));
            return Ok(false);
        }
        let advanced = self.advance_transaction(current);
        if advanced.is_ok() {
            // The step ran and succeeded: the fault, if any, has recovered, so
            // a later failure is news. A skipped or failed attempt clears nothing.
            self.resume_backoff
                .lock()
                .expect("resume backoff mutex")
                .remove(id);
            self.clear_fault(id);
        }
        if let Err(error) = advanced {
            let latest_snapshot = ControlSnapshot::read(&self.options.state_store)?;
            let latest = latest_snapshot
                .transactions
                .iter()
                .find(|stored| stored.value.transaction_id == current.value.transaction_id)
                .context("transaction disappeared while recording an execution error")?;
            if latest.value.is_terminal() {
                self.resume_backoff
                    .lock()
                    .expect("resume backoff mutex")
                    .remove(id);
                return Ok(true);
            }
            self.back_off(id)?;
            if latest.value.phase < DeploymentPhase::Fencing {
                // Before the fence an abort that cannot finish a step is
                // resumable, never a post-fence abort: that path refuses a
                // pre-fence phase, so choosing it only wedged the tick.
                if latest.value.pre_fencing_abort.is_none() {
                    self.begin_pre_fencing_abort(latest, error)?;
                } else {
                    self.record_resumable_error(latest, &error)?;
                }
            } else if latest.value.post_fencing_abort.is_none()
                && self.candidate_is_permanently_stopped(&latest.value)?
            {
                // Past the fence an error is resumable while the candidate
                // can still recover. This one cannot: its transient unit
                // has failed and carries Restart=no, so retrying would hold
                // the target forever behind a transaction that can never
                // finish.
                self.begin_post_fencing_abort(latest, error)?;
            } else {
                self.record_resumable_error(latest, &error)?;
            }
        }
        let after = ControlSnapshot::read(&self.options.state_store)?;
        let live = after
            .transactions
            .iter()
            .find(|stored| stored.value.transaction_id == current.value.transaction_id)
            .context("transaction disappeared while checking scheduler progress")?;
        Ok(state_advanced(&current.value, &live.value))
    }

    /// Wait twice as long after each consecutive failure of a transaction's
    /// step, from one poll interval up to a ceiling.
    fn back_off(&self, transaction_id: &str) -> Result<()> {
        let now = now_millis()?;
        let mut all = self.resume_backoff.lock().expect("resume backoff mutex");
        let failures = all.get(transaction_id).map_or(0, |backoff| backoff.failures);
        let wait = backoff_wait(self.options.poll_millis, failures);
        all.insert(
            transaction_id.to_string(),
            ResumeBackoff {
                failures: failures.saturating_add(1),
                not_before_unix_millis: now.saturating_add(wait),
            },
        );
        Ok(())
    }

    fn freeze_one_queued_command(&self) -> Result<bool> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let live_commands = snapshot
            .transactions
            .iter()
            .map(|stored| stored.value.command_id.clone())
            .collect::<BTreeSet<_>>();
        let mut candidates = snapshot
            .commands
            .iter()
            .filter(|stored| {
                stored.value.kind == CommandKind::Deploy
                    && !live_commands.contains(&stored.value.command_id)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(false);
        }
        // A command is consumed by any transaction, live or retired. Commands
        // whose transactions were retired to history before commands travelled
        // with them are still resident; they are not queued, they are
        // history that never moved. Retire them here and never freeze them.
        // With history unreadable, whether a command was consumed is unknown:
        // freezing it again could run a finished deployment twice.
        let Some(history) = self.history_for_decision() else {
            return Ok(false);
        };
        let historical_commands = history
            .into_iter()
            .map(|transaction| transaction.command_id)
            .collect::<BTreeSet<_>>();
        let mut retired = false;
        candidates.retain(|stored| {
            if !historical_commands.contains(&stored.value.command_id) {
                return true;
            }
            match SingleFileMessagePackBackingStore::new(&history_store_path(
                &self.options.state_store,
            ))
            .insert_entry_if_absent(stored.envelope.clone())
            .and_then(|_| {
                SingleFileMessagePackBackingStore::new(&self.options.state_store)
                    .delete_batch_if_unchanged(std::slice::from_ref(&stored.envelope))
            }) {
                Ok(_) => {
                    eprintln!(
                        "Idunn retired consumed command {}: its transactions are history",
                        stored.value.command_id
                    );
                    retired = true;
                }
                Err(error) => eprintln!(
                    "Idunn left consumed command {} resident: {error:#}",
                    stored.value.command_id
                ),
            }
            false
        });
        if retired {
            return Ok(true);
        }
        candidates.sort_by_key(|stored| stored.value.requested_at_unix_millis);
        // Oldest first, but a command whose target is busy does not block the
        // ones behind it for other targets.
        for command in candidates {
            if self.freeze_command(&snapshot, command)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Freeze one queued command into its transactions. `Ok(false)` means its
    /// target is busy and nothing was written.
    fn freeze_command(
        &self,
        snapshot: &ControlSnapshot,
        command: &Stored<DeploymentCommand>,
    ) -> Result<bool> {
        let bindings = match load_bindings(&self.options.bindings_dir) {
            Ok(bindings) => bindings,
            Err(error) => {
                eprintln!(
                    "Idunn left command {} queued because operator bindings are invalid: {error:#}",
                    command.value.command_id
                );
                return Ok(false);
            }
        };
        let targets = match resolve_selector(&bindings, &command.value.selector) {
            Ok(targets) => targets,
            Err(error) => {
                let now = now_millis()?;
                let rejected = DeploymentTransaction::rejected(&command.value, error, now)?;
                ensure!(
                    SingleFileMessagePackBackingStore::new(&self.options.state_store)
                        .compare_exchange(
                            &[CultCacheExpectedEnvelope {
                                r#type: DeploymentTransaction::TYPE.into(),
                                key: rejected.transaction_id.clone(),
                                current: None,
                            }],
                            &[transaction_envelope(&rejected, now)?],
                        )?,
                    "bad selector changed before refusal was recorded"
                );
                return Ok(true);
            }
        };
        let busy = snapshot
            .transactions
            .iter()
            .filter(|stored| stored.value.blocks_new_target_mutation())
            .map(|stored| stored.value.target.as_str())
            .collect::<BTreeSet<_>>();
        if targets.iter().any(|target| busy.contains(target.as_str())) {
            return Ok(false);
        }
        let now = now_millis()?;
        let transactions = targets
            .into_iter()
            .enumerate()
            .map(|(ordinal, target)| {
                DeploymentTransaction::new(
                    &command.value,
                    target.clone(),
                    u32::try_from(ordinal)?,
                    snapshot.admitted_for(&target).map(|stored| &stored.value),
                    now,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let expected = transactions
            .iter()
            .map(|transaction| CultCacheExpectedEnvelope {
                r#type: DeploymentTransaction::TYPE.into(),
                key: transaction.transaction_id.clone(),
                current: None,
            })
            .collect::<Vec<_>>();
        let next = transactions
            .iter()
            .map(|transaction| transaction_envelope(transaction, now))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store)
                .compare_exchange(&expected, &next)?,
            "queued command lost its ordered transaction creation CAS"
        );
        Ok(true)
    }

    fn supervise_one_admitted_generation(&self) -> Result<bool> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let mut progressed = false;
        let mut admitted = snapshot.admitted.iter().collect::<Vec<_>>();
        admitted.sort_by_key(|stored| stored.value.target.as_str());
        for current in admitted {
            if let Err(disagreement) = current.value.readiness() {
                self.note_fault(
                    "holds admitted generation",
                    &format!("generation:{}", current.value.target),
                    &anyhow::Error::new(disagreement),
                );
            }
            let blocker = snapshot.transactions.iter().find(|stored| {
                stored.value.target == current.value.target
                    && stored.value.blocks_new_target_mutation()
            });
            if blocker.is_some_and(|stored| {
                let owns_from = if stored.value.rollout_stops_incumbent_first() {
                    // The incumbent is stopped at Starting by design; a
                    // continuity restart in that window would fight the
                    // candidate for the same host resources.
                    DeploymentPhase::Starting
                } else {
                    DeploymentPhase::Fencing
                };
                stored.value.phase >= owns_from && stored.value.phase < DeploymentPhase::Complete
            }) {
                continue;
            }
            match self.supervise_admitted_route(current) {
                Ok(true) => {
                    progressed = true;
                    continue;
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "Idunn rejected admitted {} route continuity: {error:#}",
                    current.value.target
                ),
            }
            match self.refresh_admitted_topology(&snapshot, current) {
                Ok(true) => {
                    progressed = true;
                    continue;
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "Idunn preserved admitted {} after rejecting topology observation: {error:#}",
                    current.value.target
                ),
            }
            match self.retire_stale_incarnations(&snapshot, current) {
                Ok(true) => {
                    progressed = true;
                    continue;
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "Idunn preserved a stale {} incarnation projection: {error:#}",
                    current.value.target
                ),
            }
            // Restore the admitted Expected before anything is asked of the
            // workload, and only the Expected.
            //
            // A target that cannot see its own Expected cannot start, and an
            // aborted deployment leaves exactly that gap, so this cannot wait
            // for an observation the dead process will never provide. The
            // activation is deliberately not restored: it names one running
            // incarnation, every restart issues a fresh one, and republishing
            // the admitted one leaves the projection naming an incarnation that
            // no longer exists -- which the workload rightly refuses to adopt.
            // That record is published from observation once the process is up.
            if blocker.is_none()
                || blocker.is_some_and(|stored| stored.value.phase == DeploymentPhase::Complete)
            {
                let repair = (|| -> Result<()> {
                    let topology = self.topology();
                    let provider_anchor = self.provider_anchor_for_plan(&current.value.plan)?;
                    if !topology.admitted_expected_projection_is_exact(
                        &current.value.expected,
                        &provider_anchor,
                    )? {
                        ensure!(
                            topology.publish_expected(&current.value.expected, &provider_anchor)?
                                == current.value.expected.canonical_sha256()?,
                            "admitted Expected projection repair differs"
                        );
                    }
                    Ok(())
                })();
                if let Err(error) = repair {
                    eprintln!(
                        "Idunn preserved admitted {} after refusing topology projection repair: {error:#}",
                        current.value.target
                    );
                }
            }
            let mut operational_error = None;
            let observation = match self.workload_for(&current.value.plan).and_then(|workload| {
                workload.observe(
                    &current.value.expected,
                    &current.value.activation,
                    &current.value.workload,
                )
            }) {
                Ok(observation) => {
                    let lease_is_missing = if let Some(lease) = current.value.leasing.lease() {
                        let lease_health = (|| -> Result<bool> {
                            let lease_path = current
                                .value
                                .plan
                                .parsed_inputs()?
                                .1
                                .process_write_lease
                                .context("admitted lease has no operator binding")?
                                .record_path;
                            let driver =
                                CultCacheWriteLeaseDriver::new(&current.value.target, lease_path);
                            if driver.observe_exact(lease)? {
                                return Ok(true);
                            }
                            if driver.observe_empty()? {
                                return Ok(false);
                            }
                            bail!("write-lease store contains unexpected authority")
                        })();
                        match lease_health {
                            Ok(true) => false,
                            Ok(false) => {
                                operational_error =
                                    Some(anyhow!("admitted physical write lease is missing"));
                                true
                            }
                            Err(error) => {
                                eprintln!(
                                    "Idunn preserved admitted {} after refusing an unsafe write-lease mutation: {error:#}",
                                    current.value.target
                                );
                                continue;
                            }
                        }
                    } else {
                        false
                    };
                    if lease_is_missing {
                        None
                    } else {
                        Some(observation)
                    }
                }
                Err(error) if error.downcast_ref::<HostUnobservable>().is_some() => {
                    // The host cannot be asked right now (its actuator is
                    // between sessions, or Idunn just restarted). That is
                    // silence, not a death; counting it as one burned every
                    // continuity attempt in the reattach window on
                    // 2026-09-11 and left a healthy Muninn demoted.
                    eprintln!(
                        "Idunn cannot observe admitted {} this tick: {error:#}",
                        current.value.target
                    );
                    continue;
                }
                Err(error) => {
                    operational_error = Some(error);
                    None
                }
            };
            if observation.is_some() {
                continue;
            }
            let workload_error = operational_error
                .context("admitted operational state has neither observation nor error")?;
            // A held generation is a record the recipe never declared
            // readiness for. Idunn reports it and does not decide for it: a
            // continuity minted over it copies the undeclared Expected, is
            // held at once, and owns the target forever, so the declaring
            // redeploy that would clear the hold could never freeze. The dead
            // held target stays down and free. Nothing yields to it either:
            // there is no continuity to yield to.
            if current.value.readiness().is_err() {
                self.note_fault(
                    "holds a dead admitted generation down; declare readiness in its recipe and redeploy",
                    &format!("generation-down:{}", current.value.target),
                    &anyhow!("admitted generation {} is not running", current.value.generation_id),
                );
                continue;
            }
            // Continuity restarts a release; it cannot repair one. When the
            // admitted release will not start, rescheduling it forever keeps
            // the target permanently occupied -- and a target with a live
            // transaction accepts no deployment, so the one action that could
            // fix it is exactly the one that is locked out. Restarts are
            // counted in the target's own log, in a sliding window, and spaced
            // by a doubling wait. The log outlives every generation, so a
            // release that starts, dies and starts again is bounded the same as
            // one that never starts. Nothing here reads history: an unreadable
            // history file must not stop crash recovery.
            let now = now_millis()?;
            let supervision = snapshot.supervision_or_new(&current.value.target);
            // A stepped-back clock leaves entries in the future. Settle them
            // before deciding from them; the write ends this target's pass.
            let mut settled = supervision.clone();
            settled.settle(now);
            if settled != supervision {
                self.write_target_supervision(
                    snapshot.supervision_for(&current.value.target),
                    &settled,
                    now,
                )?;
                progressed = true;
                continue;
            }
            let continuity_key = format!("continuity:{}", current.value.target);

            if let Some(blocker) = blocker {
                // Yielding a *deployment* to continuity is right: the incumbent
                // died, so changing it can wait. Doing the same to a continuity
                // transaction is self-defeating -- that transaction exists to
                // restart the very workload whose absence triggers the yield,
                // so aborting it schedules another, which is aborted in turn.
                // Odin sat in that loop, down, while Idunn cancelled its own
                // recovery every few seconds.
                // Only yield to a continuity that is still trying. Once it has
                // given up on this release there is nothing to yield to, and
                // the deployment is the only thing left that can fix the
                // target -- aborting it would close the last door.
                if blocker.value.command_kind == CommandKind::Deploy
                    && !supervision.restarts_exhausted(now)
                    && blocker.value.phase < DeploymentPhase::Fencing
                    && blocker.value.pre_fencing_abort.is_none()
                    // A held record is reported and never aborted by Idunn.
                    && blocker.value.held_disagreement().is_none()
                {
                    self.begin_pre_fencing_abort(
                        blocker,
                        anyhow!(
                            "admitted incumbent failed before candidate fencing; deployment yielded to continuity: {workload_error:#}"
                        ),
                    )?;
                    progressed = true;
                }
                continue;
            }
            if supervision.restarts_exhausted(now) {
                self.report_once(
                    &continuity_key,
                    format!(
                        "Idunn stopped restarting admitted {}: {} restarts inside the window. The target is free for a deployment to replace it: {workload_error:#}",
                        current.value.target,
                        supervision.restarts_used(now)
                    ),
                );
                continue;
            }
            self.clear_fault(&continuity_key);
            if supervision.continuity_is_waiting(now) {
                continue;
            }

            // The projected activation names an incarnation that is gone. Every
            // restart is issued a fresh one, so leaving the old record standing
            // makes the projection describe a process that no longer exists --
            // and a workload that reads it refuses to adopt an activation that
            // is not its own, which is a restart loop rather than a recovery.
            // Demote to Expected-only, the same shape an aborted deployment
            // leaves, and let the restart publish its own activation once it is
            // observed.
            let demotion = (|| -> Result<()> {
                let topology = self.topology();
                let provider_anchor = self.provider_anchor_for_plan(&current.value.plan)?;
                if !topology.projected_activation_is_present(&current.value.expected)? {
                    return Ok(());
                }
                topology.demote_to_expected_only(
                    &current.value.expected,
                    &provider_anchor,
                    &current.value.activation,
                    current.value.leasing.lease(),
                )?;
                Ok(())
            })();
            // Expected-only is the precondition of a restart: a continuity
            // minted over a projection that still names an activation would
            // fail between preparing and publishing its own, and its abort
            // could not resolve. So a failed demotion mints nothing. The
            // failure is written to the generation, and the next attempt is
            // not before the deferral ends.
            if let Err(error) = demotion {
                eprintln!(
                    "Idunn deferred continuity for admitted {} after refusing to demote its projection: {error:#}",
                    current.value.target
                );
                let mut deferred = supervision.clone();
                deferred.defer_continuity(now, &truncate(&format!("{error:#}"), 2048));
                self.write_target_supervision(
                    snapshot.supervision_for(&current.value.target),
                    &deferred,
                    now,
                )?;
                progressed = true;
                continue;
            }
            // An engaged lifecycle brake means no continuity transaction is
            // minted at all. A transaction that exists and waits on the brake
            // owns the target, and a target owned by a parked restart accepts
            // no deployment -- which is the one thing a lifecycle brake must
            // never gate. The Sealing-phase check remains as the guard for a
            // transaction minted just before the brake was engaged.
            if !self.lifecycle_allows_generation(current, now)? {
                continue;
            }
            let command = DeploymentCommand {
                schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
                command_id: format!("continuity-{}", Uuid::new_v4()),
                kind: CommandKind::Continuity,
                selector: current.value.target.clone(),
                requested_by: "idunn-continuity".into(),
                requested_at_unix_millis: now,
            };
            command.validate()?;
            let transaction =
                DeploymentTransaction::from_continuity(&command, &current.value, now)?;
            // The restart is counted in the same CAS that mints it, so a crash
            // cannot schedule one the ceiling never saw.
            let mut counted = supervision;
            counted.record_restart(now);
            ensure!(
                SingleFileMessagePackBackingStore::new(&self.options.state_store)
                    .compare_exchange(
                        &[
                            CultCacheExpectedEnvelope {
                                r#type: DeploymentCommand::TYPE.into(),
                                key: command.command_id.clone(),
                                current: None,
                            },
                            CultCacheExpectedEnvelope {
                                r#type: DeploymentTransaction::TYPE.into(),
                                key: transaction.transaction_id.clone(),
                                current: None,
                            },
                            CultCacheExpectedEnvelope {
                                r#type: TargetSupervision::TYPE.into(),
                                key: counted.target.clone(),
                                current: snapshot
                                    .supervision_for(&counted.target)
                                    .map(|stored| stored.envelope.clone()),
                            },
                        ],
                        &[
                            command_envelope(&command, now)?,
                            transaction_envelope(&transaction, now)?,
                            target_supervision_envelope(&counted, now)?,
                        ],
                    )?,
                "continuity scheduling lost its command/transaction/meter CAS"
            );
            progressed = true;
        }
        Ok(progressed)
    }

    fn supervise_admitted_route(&self, current: &Stored<AdmittedGeneration>) -> Result<bool> {
        let Some(expected_route) = current.value.expected.route.as_ref() else {
            ensure!(
                matches!(&current.value.routing, RoutingEvidence::SkippedUnrouted),
                "unrouted admitted generation carries route authority"
            );
            return Ok(false);
        };
        let RoutingEvidence::Promoted {
            observation,
            promoted_at_unix_millis,
        } = &current.value.routing
        else {
            bail!("routed admitted generation has no promoted route receipt")
        };
        let now = now_millis()?;
        // A route whose proofs keep failing is challenged on a widening
        // schedule, not every tick.
        if current
            .value
            .route_supervision
            .as_ref()
            .is_some_and(|state| state.is_waiting(now, self.options.topology_maximum_age_millis))
        {
            return Ok(false);
        }
        ensure!(
            observation.route_id == expected_route.route_id
                && observation.runtime_instance_id == current.value.activation.runtime_instance_id,
            "admitted route receipt names another incarnation"
        );
        let binding = current.value.plan.parsed_inputs()?.1;
        let lease_driver = if let Some(lease) = current.value.leasing.lease() {
            let lease_path = &binding
                .process_write_lease
                .as_ref()
                .context("stateful admitted generation has no write-lease binding")?
                .record_path;
            let driver = CultCacheWriteLeaseDriver::new(&current.value.target, lease_path);
            ensure!(
                driver.observe_exact(lease)?,
                "admitted process write lease is no longer exact"
            );
            Some(driver)
        } else {
            None
        };
        let route_binding = binding
            .route
            .context("routed admitted generation has no operator route binding")?;
        let driver = self.route_driver(route_binding);
        let gate = self.route_gate(&current.value.target, CommandKind::Continuity);

        if driver.observe_membership(&current.value.expected, &observation.membership_sha256)?
            && route_observation_is_current(
                observation.observed_at_unix_millis,
                now,
                self.options.topology_maximum_age_millis,
                self.options.topology_maximum_future_skew_millis,
            )
        {
            return Ok(false);
        }
        let route_key = format!("route:{}", current.value.target);
        // A failed repair or a failed proof changes observation state only: it
        // marks the route degraded and widens the wait. It is a write, so it
        // ends this target's supervision pass (`Ok(true)`), exactly as a
        // proved challenge does: the caller's snapshot no longer matches the
        // generation, and a later step in the same pass would lose its CAS.
        let challenge = (|| -> Result<RouteObservation> {
            // Actuates only when the fragment on disk differs from the
            // admitted membership; an exact fragment makes this a no-op. It is
            // survival: restoring the admitted route is never refused.
            driver.restore_admitted_membership(
                &current.value.expected,
                &observation.membership_sha256,
                &gate,
            )?;
            let authority = self.runtime_authority_parts(
                &current.value.plan,
                &current.value.expected,
                &current.value.activation,
            )?;
            let refreshed = self.prove_stable_route_against(
                &current.value.expected,
                &current.value.activation,
                &authority,
                current.value.leasing.lease_sha256(),
                &driver,
                observation.membership_sha256.clone(),
            )?;
            ensure!(
                driver.observe_membership(&current.value.expected, &refreshed.membership_sha256)?,
                "admitted route membership changed during its continuity challenge"
            );
            if let (Some(lease_driver), Some(lease)) = (&lease_driver, current.value.leasing.lease())
            {
                ensure!(
                    lease_driver.observe_exact(lease)?,
                    "admitted process write lease changed during its continuity challenge"
                );
            }
            Ok(refreshed)
        })();
        let mut next = current.value.clone();
        match challenge {
            Ok(refreshed) => {
                next.routing = RoutingEvidence::Promoted {
                    observation: refreshed,
                    promoted_at_unix_millis: *promoted_at_unix_millis,
                };
                if let Some(state) = next.route_supervision.as_mut() {
                    state.record_proved_challenge(now);
                }
                self.clear_fault(&route_key);
                if let Err(record) = self.replace_generation(current, &next, now) {
                    eprintln!(
                        "Idunn could not record the proved route challenge of {}: {record:#}",
                        current.value.target
                    );
                }
            }
            Err(error) => {
                if let Some(state) = next.route_supervision.as_mut() {
                    state.record_failed_challenge(now, self.options.topology_maximum_age_millis);
                }
                self.report_once(
                    &route_key,
                    format!(
                        "Idunn could not prove admitted {} on its stable route: {error:#}",
                        current.value.target
                    ),
                );
                if let Err(record) = self.replace_generation(current, &next, now) {
                    eprintln!(
                        "Idunn could not record the failed route challenge of {}: {record:#}",
                        current.value.target
                    );
                }
            }
        }
        Ok(true)
    }

    fn refresh_admitted_topology(
        &self,
        snapshot: &ControlSnapshot,
        current: &Stored<AdmittedGeneration>,
    ) -> Result<bool> {
        if !matches!(current.value.readiness(), Ok(class) if class != ReadinessClass::RouteProof) {
            return Ok(false);
        }
        let odin = current.value.odin_receipts()?;
        let Some(received) = self.topology().receive(
            &current.value.target,
            &current.value.expected.canonical_sha256()?,
        )?
        else {
            return Ok(false);
        };
        ensure!(
            received.target == current.value.target,
            "topology transport substituted admitted target"
        );
        let now = now_millis()?;
        let authority = self.runtime_authority_parts(
            &current.value.plan,
            &current.value.expected,
            &current.value.activation,
        )?;
        let odin_authority = self.current_odin_authority(snapshot)?;
        let authenticated = match authenticate_odin_runtime_topology_correlation(
            &received.canonical_bytes,
            &authority,
            current.value.leasing.lease_sha256(),
            &odin_authority.signer_public_key,
            self.trusted_topology_context(now),
        ) {
            Ok(authenticated) => authenticated,
            // An aged-out correlation is nothing new about the admitted
            // generation, not a rejected observation.
            Err(error) if is_stale_observation(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let evidence = TopologyEvidence::from_authenticated(&authenticated, now)?;
        if !sequence_requires_admission(
            Some(odin.latest),
            snapshot.max_odin_sequence(&current.value.target, &evidence.signer_identity_id),
            &evidence,
        )? {
            return Ok(false);
        }
        let mut next = current.value.clone();
        next.latest_odin_observation = Some(evidence.clone());
        next.odin_publisher_sequence_cursor = evidence.publisher_sequence;
        if is_semantic_ready(&authenticated) {
            next.ready = ReadinessEvidence::OdinCorrelated { evidence };
        }
        next.validate()?;
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: AdmittedGeneration::TYPE.into(),
                    key: current.value.target.clone(),
                    current: Some(current.envelope.clone()),
                }],
                &[admitted_envelope(&next, now)?],
            )?,
            "admitted generation changed before topology sequence CAS"
        );
        Ok(true)
    }

    fn advance_transaction(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        if current.value.post_fencing_abort.is_some() && current.value.completion.is_none() {
            return self.advance_post_fencing_abort(current);
        }
        if current.value.pre_fencing_abort.is_some() && current.value.completion.is_none() {
            return self.advance_pre_fencing_abort(current);
        }
        match current.value.phase {
            DeploymentPhase::Sealing => self.advance_sealing(current),
            DeploymentPhase::Starting => self.advance_starting(current),
            DeploymentPhase::Warming => self.advance_warming(current),
            DeploymentPhase::Fencing => self.advance_fencing(current),
            DeploymentPhase::Leasing => self.advance_leasing(current),
            DeploymentPhase::AwaitingReady => self.advance_awaiting_ready(current),
            DeploymentPhase::Routing => self.advance_routing(current),
            DeploymentPhase::Committing => self.advance_committing(current),
            DeploymentPhase::Complete => self.advance_post_commit_cleanup(current),
        }
    }

    fn advance_sealing(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        if current.value.plan.is_none() {
            ensure!(
                current.value.command_kind == CommandKind::Deploy,
                "continuity transaction lost its admitted plan"
            );
            let bindings = load_bindings(&self.options.bindings_dir)?;
            let loaded = bindings
                .get(&current.value.target)
                .context("operator binding disappeared before sealing")?;
            let now = now_millis()?;
            let resolved =
                self.source
                    .resolve(&loaded.binding, &current.value.transaction_id, now)?;
            let provider_snapshot = ControlSnapshot::read(&self.options.state_store)?;
            let providers = self.current_ready_provider_tokens(&provider_snapshot)?;
            let candidate_port = self.select_candidate_port(&loaded.binding)?;
            let plan = compile_deployment_plan(
                &resolved.recipe_bytes,
                &loaded.bytes,
                resolved.facts,
                format!("incarnation-{}", current.value.transaction_id),
                candidate_port,
                now,
                &providers,
            )?;
            // Idunn infers no way to prove readiness. A recipe that declares
            // none is refused here, before anything is frozen, built or installed.
            plan.readiness_class()?;
            return self.persist_same_phase(current, |next| {
                next.plan = Some(plan);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }

        if current.value.command_kind == CommandKind::Deploy
            && current.value.frozen_source.is_none()
        {
            let now = now_millis()?;
            let plan = required(&current.value.plan, "transaction plan")?;
            let receipt = self.source.freeze(&current.value.transaction_id, plan)?;
            return self.persist_same_phase(current, |next| {
                next.frozen_source = Some(receipt);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }

        if current.value.sealed_release.is_none()
            || current.value.installed_release.is_none()
            || current.value.expected.is_none()
        {
            ensure!(
                current.value.command_kind == CommandKind::Deploy,
                "continuity transaction lost its admitted release evidence"
            );
            let now = now_millis()?;
            let plan = required(&current.value.plan, "transaction plan")?;
            let frozen_receipt = required(&current.value.frozen_source, "frozen source")?;
            let frozen = self.source.observe_frozen(plan, frozen_receipt)?;
            let materialized = self.runner_for(plan)?.materialize(
                &frozen,
                plan,
                &self.options.staging_root,
                now,
            )?;
            let installed = self.workload_for(plan)?.install(plan, &materialized)?;
            let expected = materialized.release.expected_projection(plan)?;
            return self.persist_same_phase(current, |next| {
                next.sealed_release = Some(materialized.release);
                next.installed_release = Some(installed);
                next.expected = Some(expected);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }

        // The candidate's Expected is published in Starting, after the brake
        // has admitted this exact transaction. Publishing it is the first
        // Verse-visible change a deployment makes, and the brake gates changes.
        let now = now_millis()?;
        let mut next = current.value.clone();
        match current.value.command_kind {
            CommandKind::Deploy => {
                let Some(authorization) = self.deployment_authorization(current, now)? else {
                    return self.record_gate_wait(
                        current,
                        "deployment brake has not released this exact transaction",
                    );
                };
                next.deployment_authorization = Some(authorization);
            }
            CommandKind::Continuity => {
                if !self.lifecycle_allows(current, now)? {
                    return self
                        .record_gate_wait(current, "lifecycle brake denies continuity restart");
                }
                next.lifecycle_authorized_at_unix_millis = Some(now);
            }
        }
        validate_live_providers_for_deploy(current.value.command_kind, || {
            self.validate_selected_providers_current(required(
                &current.value.plan,
                "transaction plan",
            )?)
        })?;
        next.enter_phase(DeploymentPhase::Starting, now);
        next.last_error = None;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn advance_starting(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let expected = required(&current.value.expected, "Expected projection")?;
        let plan = required(&current.value.plan, "transaction plan")?;
        let release = required(&current.value.sealed_release, "sealed release")?;
        let installed = required(&current.value.installed_release, "installed release")?;
        if current.value.expected_publication_sha256.is_none() {
            // Published under the candidate's own incarnation key, beside
            // whatever the target's admitted incarnation currently projects.
            // Nothing the incumbent reads about itself changes here.
            let now = now_millis()?;
            let provider_anchor = self.provider_anchor_for_plan(plan)?;
            let digest = self
                .topology()
                .publish_expected(expected, &provider_anchor)?;
            return self.persist_same_phase(current, |next| {
                next.expected_publication_sha256 = Some(digest);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }
        if current.value.activation.is_none() {
            validate_live_providers_for_deploy(current.value.command_kind, || {
                self.validate_selected_providers_current(plan)
            })?;
            let now = now_millis()?;
            let runtime_instance_id = runtime_instance_id(&current.value.transaction_id)?;
            let launch = IdunnRuntimeActivationLaunch::issue(
                expected,
                runtime_instance_id,
                now,
                &self.idunn_signer,
            )?;
            let activation = self
                .workload_for(plan)?
                .prepare_activation(plan, expected, launch)?;
            return self.persist_same_phase(current, |next| {
                next.activation = Some(activation);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }
        if current.value.workload.is_none() {
            let now = now_millis()?;
            let activation = required(&current.value.activation, "activation")?;
            if current.value.rollout_stops_incumbent_first() {
                let snapshot = ControlSnapshot::read(&self.options.state_store)?;
                if let Some(incumbent) = self.exact_incumbent(&snapshot, &current.value)? {
                    self.workload_for(&incumbent.value.plan)?
                        .stop(&incumbent.value.workload)
                        .context("stopping the incumbent before the candidate starts")?;
                }
            }
            let observation = self
                .workload_for(plan)?
                .start_prepared(plan, release, installed, expected, activation)?;
            return self.persist_same_phase(current, |next| {
                next.workload = Some(observation);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }
        if current.value.activation_publication_sha256.is_none() {
            let now = now_millis()?;
            let activation = required(&current.value.activation, "activation")?;
            let workload = required(&current.value.workload, "workload")?;
            self.workload_for(required(&current.value.plan, "transaction plan")?)?
                .observe(expected, activation, workload)?;
            let digest = self
                .topology()
                .publish_observed_activation(expected, activation, workload)?;
            return self.persist_same_phase(current, |next| {
                next.activation_publication_sha256 = Some(digest);
                next.updated_at_unix_millis = now;
                next.last_error = None;
                Ok(())
            });
        }
        let mut next = current.value.clone();
        next.enter_phase(DeploymentPhase::Warming, now_millis()?);
        next.last_error = None;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn advance_warming(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let expected = required(&current.value.expected, "Expected projection")?;
        let activation = required(&current.value.activation, "activation")?;
        let workload = required(&current.value.workload, "workload")?;
        self.workload_for(required(&current.value.plan, "transaction plan")?)?
            .observe(expected, activation, workload)?;
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let incumbent_lease_sha256 =
            self.incumbent_lease_sha256_for_warming(&snapshot, &current.value)?;
        let class = ReadinessClass::of(expected)?;
        if current.value.warming.is_none() && class == ReadinessClass::RouteProof {
            // A route-proof target's warming is Idunn's own challenge to the
            // candidate endpoint. No Odin is read: the target declared no
            // reason to be aware of one.
            return match self.challenge_candidate(
                &current.value,
                route_proof_warming_states(expected),
                None,
            )? {
                CandidateAnswer::Silent(reason) => self.record_gate_wait(current, &reason),
                CandidateAnswer::Answered { evidence, present } => {
                    let warming = WarmingEvidence::RouteProofDirect { evidence };
                    let _token = SequenceAdmittedWarming::from_direct_presence(
                        current.value.transaction_id.clone(),
                        warming.clone(),
                        present,
                    )?;
                    self.persist_same_phase(current, |next| {
                        next.warming = Some(warming);
                        Ok(())
                    })
                }
            };
        }
        if current.value.warming.is_none() {
            // Odin's warming is observed directly when nothing else can observe
            // it. That is the first bootstrap, and it is also every continuity
            // restart of Odin: the incumbent that would do the observing is the
            // very process being restarted, so it cannot report on its own
            // return. A deployment of Odin is not included -- there the healthy
            // incumbent observes the candidate, which is the point.
            // Odin's warming is observed directly exactly when no Odin can
            // observe it: the first bootstrap, a continuity restart of Odin
            // itself, or any moment the admitted Odin has no usable correlation
            // to report through. The rule is the same one that justified the
            // bootstrap case -- "observed through Odin" is not available -- and
            // it stays scoped to this one target. Every other target keeps
            // waiting for Odin, which is what makes Odin the root of the chain.
            //
            // Asked once. Admitting the latest topology persists the sequence
            // cursor, so asking twice with the same record fails the second
            // time as "transaction changed before topology admission".
            let is_odin = class == ReadinessClass::OdinSelf;
            let odin_observation = if is_odin
                && snapshot.admitted_odin().is_some()
                && current.value.command_kind != CommandKind::Continuity
            {
                self.admit_latest_topology(current, None)?
            } else {
                None
            };
            let odin_observes_itself = is_odin && odin_observation.is_none();
            if odin_observes_itself {
                ensure!(
                    expected.write_lease_required,
                    "direct Odin warming must be a stateful incarnation"
                );
                let (evidence, present) = self.observe_first_odin_warming(&current.value)?;
                let warming = WarmingEvidence::FirstOdinDirect { evidence };
                let _token = SequenceAdmittedWarming::from_direct_presence(
                    current.value.transaction_id.clone(),
                    warming.clone(),
                    present,
                )?;
                return self.persist_same_phase(current, |next| {
                    next.warming = Some(warming);
                    Ok(())
                });
            }
            let observation = match odin_observation {
                Some(observation) => Some(observation),
                None => self.admit_latest_topology(current, None)?,
            };
            let Some((admitted, authenticated)) = observation else {
                // Only the first-Odin bootstrap above observes presence
                // directly. Every other target's warming presence arrives as
                // Odin's authenticated runtime topology correlation, so until
                // Odin is admitted and publishing, a candidate warms forever
                // with nothing to say why.
                return self.record_gate_wait(
                    current,
                    "no authenticated Odin topology correlation yet: warming presence for a \
                     non-Odin target is observed through Odin, which must be admitted first",
                );
            };
            if admitted.envelope != current.envelope {
                return Ok(());
            }
            let semantic_warming = is_semantic_warming(
                expected,
                activation,
                incumbent_lease_sha256.as_deref(),
                &authenticated,
            )?;
            let latest = required(
                &admitted.value.latest_odin_observation,
                "sequence-admitted warming evidence",
            )?;
            if semantic_warming {
                let evidence = latest.clone();
                let _token = SequenceAdmittedWarming::from_topology(
                    admitted.value.transaction_id.clone(),
                    authenticated,
                )?;
                return self.persist_same_phase(&admitted, |next| {
                    next.warming = Some(WarmingEvidence::OdinTopology { evidence });
                    Ok(())
                });
            } else {
                return Ok(());
            }
        }
        let warming = self.rehydrate_warming_token(&current.value, now_millis()?, false)?;
        ensure!(
            warming.transaction_id() == current.value.transaction_id
                && warming.runtime_instance_id() == activation.runtime_instance_id.as_str(),
            "durable Warming evidence belongs to another candidate"
        );

        if expected.route.is_some() && current.value.route_preflight.is_none() {
            let now = now_millis()?;
            let snapshot = ControlSnapshot::read(&self.options.state_store)?;
            let incumbent = self.exact_incumbent(&snapshot, &current.value)?;
            let incumbent_route =
                incumbent.and_then(|generation| generation.value.routing.observation().cloned());
            let binding = current.value.plan.as_ref().unwrap().parsed_inputs()?.1;
            let route_binding = binding
                .route
                .context("routed Expected lost route binding")?;
            let driver = self.route_driver(route_binding);
            let receipt = driver.preflight(
                expected,
                &activation.runtime_instance_id,
                incumbent_route.as_ref(),
                &self.route_gate(&current.value.target, current.value.command_kind),
            )?;
            return self.persist_same_phase(current, |next| {
                next.route_preflight = Some(receipt);
                next.updated_at_unix_millis = now;
                Ok(())
            });
        }

        if current.value.isolation.is_none() {
            let now = now_millis()?;
            let snapshot = ControlSnapshot::read(&self.options.state_store)?;
            let incumbent = self.exact_incumbent(&snapshot, &current.value)?;
            // Isolation is a property of two processes running at once. A
            // stopped incumbent has already released its DynamicUser UID, and
            // systemd is free to hand that same UID to the candidate -- so
            // comparing the candidate against the incumbent's *recorded*
            // identity turns ordinary UID reuse into a permanent refusal to
            // fence. There is nothing left to be isolated from.
            let incumbent_workload = match incumbent {
                Some(value)
                    if self
                        .workload_for(&value.value.plan)?
                        .is_permanently_stopped(&value.value.workload)? =>
                {
                    None
                }
                other => other.map(|value| &value.value.workload),
            };
            let isolation = WorkloadObservation::prove_isolation(workload, incumbent_workload)?;
            return self.persist_same_phase(current, |next| {
                next.isolation = Some(isolation);
                next.updated_at_unix_millis = now;
                Ok(())
            });
        }

        self.transition(current, DeploymentPhase::Fencing)
    }

    fn advance_fencing(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        if current.value.fencing.is_none() {
            let now = now_millis()?;
            let snapshot = ControlSnapshot::read(&self.options.state_store)?;
            let incumbent = self.exact_incumbent(&snapshot, &current.value)?;
            let incumbent_lease = incumbent.and_then(|generation| generation.value.leasing.lease());
            let expected = required(&current.value.expected, "Expected projection")?;
            let evidence = if expected.write_lease_required || incumbent_lease.is_some() {
                let incumbent_lease_path = incumbent_lease
                    .map(|_| -> Result<PathBuf> {
                        Ok(incumbent
                            .context("incumbent lease has no admitted generation")?
                            .value
                            .plan
                            .parsed_inputs()?
                            .1
                            .process_write_lease
                            .context("incumbent lease has no admitted binding")?
                            .record_path)
                    })
                    .transpose()?;
                if let (Some(lease), Some(path)) = (incumbent_lease, &incumbent_lease_path) {
                    let incumbent = incumbent.context("incumbent lease lost its generation")?;
                    self.workload_for(&incumbent.value.plan)?
                        .stop(&incumbent.value.workload)
                        .context("stopping the exact incumbent before revoking its lifetime-held write lease")?;
                    let driver = CultCacheWriteLeaseDriver::new(&current.value.target, path);
                    driver.revoke_exact(Some(lease))?;
                    ensure!(
                        driver.observe_empty()?,
                        "incumbent write lease remained after exact fencing"
                    );
                    self.topology().withdraw_process_write_lease(
                        &incumbent.value.expected,
                        &incumbent.value.activation,
                        Some(lease),
                    )?;
                }
                let candidate_lease_path = if expected.write_lease_required {
                    Some(
                        current
                            .value
                            .plan
                            .as_ref()
                            .unwrap()
                            .parsed_inputs()?
                            .1
                            .process_write_lease
                            .context("stateful candidate has no write-lease binding")?
                            .record_path,
                    )
                } else {
                    None
                };
                if let Some(path) = &candidate_lease_path {
                    if incumbent_lease_path.as_ref() != Some(path) {
                        let driver = CultCacheWriteLeaseDriver::new(&current.value.target, path);
                        driver.revoke_exact(None)?;
                        ensure!(
                            driver.observe_empty()?,
                            "candidate write-lease path was not empty before grant"
                        );
                    }
                }
                self.topology().withdraw_process_write_lease(
                    expected,
                    required(&current.value.activation, "candidate activation")?,
                    None,
                )?;
                FencingEvidence::Revoked {
                    incumbent_lease_sha256: incumbent_lease
                        .map(IdunnProcessWriteLeaseRecord::canonical_sha256)
                        .transpose()?,
                    candidate_lease_path_verified_empty: candidate_lease_path.is_some(),
                }
            } else {
                FencingEvidence::SkippedStateless
            };
            return self.persist_same_phase(current, |next| {
                next.fencing = Some(evidence);
                next.updated_at_unix_millis = now;
                Ok(())
            });
        }
        self.transition(current, DeploymentPhase::Leasing)
    }

    fn advance_leasing(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let expected = required(&current.value.expected, "Expected projection")?;
        if !expected.write_lease_required {
            if current.value.leasing.is_none() {
                let now = now_millis()?;
                return self.persist_same_phase(current, |next| {
                    next.leasing = Some(LeasingEvidence::SkippedStateless);
                    next.updated_at_unix_millis = now;
                    Ok(())
                });
            }
            return self.transition(current, DeploymentPhase::AwaitingReady);
        }

        let binding = current.value.plan.as_ref().unwrap().parsed_inputs()?.1;
        let lease_path = binding
            .process_write_lease
            .context("stateful target has no write-lease binding")?
            .record_path;
        let driver = CultCacheWriteLeaseDriver::new(&current.value.target, lease_path);

        if matches!(
            current.value.leasing.as_ref(),
            Some(LeasingEvidence::Granted { .. })
        ) {
            let activation = required(&current.value.activation, "activation")?;
            let warming = self.rehydrate_warming_token(&current.value, now_millis()?, false)?;
            let lease = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease)
                .context("granted Leasing evidence has no write lease")?;
            let recorded_sha256 = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256)
                .context("granted Leasing evidence has no lease digest")?;
            ensure!(
                driver.observe_exact(lease)?,
                "physical write lease disappeared after Granted became durable"
            );
            self.workload_for(required(&current.value.plan, "transaction plan")?)?
                .observe(
                    expected,
                    activation,
                    required(&current.value.workload, "candidate workload")?,
                )?;
            ensure!(
                driver.grant(expected, activation, &warming, lease)? == recorded_sha256,
                "replayed physical write lease differs from Granted evidence"
            );
            ensure!(
                self.topology()
                    .publish_process_write_lease(expected, activation, lease)?
                    == recorded_sha256,
                "replayed write-lease projection differs from Granted evidence"
            );
            return self.transition(current, DeploymentPhase::AwaitingReady);
        }

        if let Some((lease, prepared_sha256)) = current
            .value
            .leasing
            .as_ref()
            .and_then(LeasingEvidence::prepared_lease)
        {
            let now = now_millis()?;
            let activation = required(&current.value.activation, "activation")?;
            let historical_warming = self.rehydrate_warming_token(&current.value, now, false)?;
            let physical_is_exact = driver.observe_exact(lease)?;
            let warming = if physical_is_exact {
                historical_warming
            } else {
                ensure!(
                    driver.observe_empty()?,
                    "candidate write-lease store contains authority other than its durable prepared lease"
                );
                match self.rehydrate_warming_token(&current.value, now, true) {
                    Ok(warming) => warming,
                    Err(_) => {
                        let Some((admitted, fresh_evidence, fresh_warming)) =
                            self.fresh_warming_for_lease(current, now)?
                        else {
                            return Ok(());
                        };
                        let replacement = self.prepare_candidate_write_lease(
                            &admitted.value,
                            &fresh_warming,
                            now,
                        )?;
                        let replacement_sha256 = replacement.canonical_sha256()?;
                        return self.persist_same_phase(&admitted, |next| {
                            next.warming = Some(fresh_evidence);
                            next.leasing = Some(LeasingEvidence::Prepared {
                                lease: replacement,
                                lease_sha256: replacement_sha256,
                            });
                            next.updated_at_unix_millis = now;
                            Ok(())
                        });
                    }
                }
            };
            self.workload_for(required(&current.value.plan, "transaction plan")?)?
                .observe(
                    expected,
                    activation,
                    required(&current.value.workload, "candidate workload")?,
                )?;
            if !physical_is_exact {
                ensure!(
                    driver.observe_empty()?,
                    "candidate write-lease store changed before physical grant"
                );
            }
            let granted_sha256 = driver.grant(expected, activation, &warming, lease)?;
            ensure!(
                granted_sha256 == prepared_sha256,
                "granted write lease differs from the durable prepared lease"
            );
            let projected_sha256 = self
                .topology()
                .publish_process_write_lease(expected, activation, lease)?;
            ensure!(
                projected_sha256 == granted_sha256,
                "projected write lease differs from the granted process authority"
            );
            let lease = lease.clone();
            return self.persist_same_phase(current, |next| {
                next.leasing = Some(LeasingEvidence::Granted {
                    lease,
                    lease_sha256: projected_sha256,
                });
                next.updated_at_unix_millis = now;
                Ok(())
            });
        }

        ensure!(
            current.value.leasing.is_none(),
            "stateful Leasing phase carries invalid lease evidence"
        );
        ensure!(
            driver.observe_empty()?,
            "candidate write-lease store contains authority before lease preparation"
        );
        let now = now_millis()?;
        let Some((admitted, fresh_evidence, fresh_warming)) =
            self.fresh_warming_for_lease(current, now)?
        else {
            return Ok(());
        };
        let lease = self.prepare_candidate_write_lease(&admitted.value, &fresh_warming, now)?;
        let lease_sha256 = lease.canonical_sha256()?;
        self.persist_same_phase(&admitted, |next| {
            next.warming = Some(fresh_evidence);
            next.leasing = Some(LeasingEvidence::Prepared {
                lease,
                lease_sha256,
            });
            next.updated_at_unix_millis = now;
            Ok(())
        })
    }

    fn advance_awaiting_ready(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let now = now_millis()?;
        let expected = required(&current.value.expected, "Expected projection")?;
        self.observe_candidate_before_waiting(&current.value)?;
        if expected.write_lease_required {
            let activation = required(&current.value.activation, "activation")?;
            let warming = self.rehydrate_warming_token(&current.value, now, false)?;
            let lease = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease)
                .context("stateful transaction has no process write lease")?;
            let binding = current.value.plan.as_ref().unwrap().parsed_inputs()?.1;
            let path = binding
                .process_write_lease
                .context("stateful target has no write-lease binding")?
                .record_path;
            ensure!(
                CultCacheWriteLeaseDriver::new(&current.value.target, path)
                    .observe(expected, activation, &warming, lease)?,
                "candidate process write lease is no longer exact"
            );
        }
        if ReadinessClass::of(expected)? == ReadinessClass::RouteProof {
            // Ready is the candidate answering Idunn's own challenge Active,
            // holding the exact lease Idunn granted. Once recorded it is
            // history; the stable route's challenge is what keeps proving it.
            if matches!(
                current.value.ready,
                Some(ReadinessEvidence::RouteProof { .. })
            ) {
                return self.transition(current, DeploymentPhase::Routing);
            }
            let lease = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256);
            return match self.challenge_candidate(&current.value, &["warming", "active"], lease)? {
                CandidateAnswer::Silent(reason) => self.record_gate_wait(current, &reason),
                CandidateAnswer::Answered { present, .. } if present.record().state == "warming" => {
                    self.record_gate_wait(current, "candidate is still warming")
                }
                CandidateAnswer::Answered { evidence, .. } => {
                    self.persist_same_phase(current, |next| {
                        next.ready = Some(ReadinessEvidence::RouteProof { evidence });
                        Ok(())
                    })
                }
            };
        }
        {
            let current_lease = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256);
            let Some((admitted, authenticated)) =
                self.admit_latest_topology(current, current_lease)?
            else {
                return Ok(());
            };
            if admitted.envelope != current.envelope {
                return Ok(());
            }
            let semantic_ready = is_semantic_ready(&authenticated);
            let latest = required(
                &admitted.value.latest_odin_observation,
                "sequence-admitted Ready evidence",
            )?;
            if current.value.ready.as_ref().and_then(ReadinessEvidence::odin) == Some(latest) {
                ensure!(semantic_ready, "stored Ready receipt changed meaning");
            } else if semantic_ready {
                let evidence = latest.clone();
                let _token = SequenceAdmittedReady {
                    transaction_id: admitted.value.transaction_id.clone(),
                    evidence: evidence.clone(),
                    expected: expected.clone(),
                    authenticated,
                };
                return self.persist_same_phase(&admitted, |next| {
                    next.ready = Some(ReadinessEvidence::OdinCorrelated { evidence });
                    Ok(())
                });
            } else {
                return Ok(());
            }
        }
        self.transition(current, DeploymentPhase::Routing)
    }

    fn advance_routing(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let expected = required(&current.value.expected, "Expected projection")?;
        let activation = required(&current.value.activation, "activation")?;
        self.observe_candidate_before_waiting(&current.value)?;
        if current.value.routing.is_none() {
            let admitted = if ReadinessClass::of(expected)? == ReadinessClass::RouteProof {
                // Ready is the candidate's own proof, recorded in AwaitingReady.
                // Validation refuses Routing without a Ready receipt, and a
                // receipt the other party vouched for is held before any step
                // runs. No Odin correlation is read to admit a route.
                current.clone()
            } else {
                let current_lease = current
                    .value
                    .leasing
                    .as_ref()
                    .and_then(LeasingEvidence::lease_sha256);
                let Some((admitted, authenticated)) =
                    self.admit_latest_topology(current, current_lease)?
                else {
                    return Ok(());
                };
                if admitted.envelope != current.envelope {
                    return Ok(());
                }
                ensure!(
                    is_semantic_ready(&authenticated),
                    "latest Odin observation is not Ready at route admission"
                );
                let latest = required(
                    &admitted.value.latest_odin_observation,
                    "current route-admission topology evidence",
                )?;
                if admitted.value.ready.as_ref().and_then(ReadinessEvidence::odin)
                    != Some(latest)
                {
                    let latest = latest.clone();
                    return self.persist_same_phase(&admitted, |next| {
                        next.ready = Some(ReadinessEvidence::OdinCorrelated { evidence: latest });
                        Ok(())
                    });
                }
                let ready = self.rehydrate_ready_token(&admitted.value, now_millis()?, true)?;
                ensure!(
                    ready.transaction_id() == admitted.value.transaction_id,
                    "Ready token belongs to another transaction"
                );
                admitted
            };
            validate_live_providers_for_deploy(admitted.value.command_kind, || {
                self.validate_selected_providers_current(required(
                    &admitted.value.plan,
                    "transaction plan",
                )?)
            })?;
            self.ensure_transaction_write_lease_current(&admitted.value, now_millis()?)?;
            let evidence = if expected.route.is_some() {
                let binding = admitted.value.plan.as_ref().unwrap().parsed_inputs()?.1;
                let route_binding = binding.route.context("routed plan has no route binding")?;
                let preflight = required(&admitted.value.route_preflight, "route preflight")?;
                let driver = self.route_driver(route_binding);
                let gate = self.route_gate(&admitted.value.target, admitted.value.command_kind);
                ensure!(
                    preflight.candidate_runtime_instance_id == activation.runtime_instance_id,
                    "route preflight belongs to another runtime instance"
                );
                let rollback_allowed = may_rollback_route_after_failed_proof(required(
                    &admitted.value.fencing,
                    "route fencing evidence",
                )?);
                let membership_sha256 = driver.install(
                    expected,
                    &activation.runtime_instance_id,
                    preflight,
                    rollback_allowed,
                    &gate,
                )?;
                let observation = match self.prove_stable_route(
                    &admitted.value,
                    &driver,
                    membership_sha256,
                ) {
                    Ok(observation) => observation,
                    Err(proof_error) if !rollback_allowed => {
                        return Err(proof_error).context(
                            "incumbent authority was fenced; candidate route remains installed for fail-closed retry",
                        );
                    }
                    Err(proof_error) => {
                        return match driver.rollback(
                            expected,
                            &activation.runtime_instance_id,
                            preflight,
                            &gate,
                        ) {
                            Ok(()) => Err(proof_error)
                                .context("candidate did not answer its stable route challenge"),
                            Err(rollback_error) => Err(proof_error).context(format!(
                                "candidate did not answer its stable route challenge; exact route rollback also failed: {rollback_error:#}"
                            )),
                        };
                    }
                };
                ensure!(
                    driver.observe_membership(expected, &observation.membership_sha256)?,
                    "route membership changed during its signed stable-listener observation"
                );
                self.ensure_transaction_write_lease_current(&admitted.value, now_millis()?)?;
                let promoted_at_unix_millis = observation.observed_at_unix_millis;
                RoutingEvidence::Promoted {
                    observation,
                    promoted_at_unix_millis,
                }
            } else {
                RoutingEvidence::SkippedUnrouted
            };
            let now = now_millis()?;
            return self.persist_same_phase(&admitted, |next| {
                next.routing = Some(evidence);
                next.updated_at_unix_millis = now;
                Ok(())
            });
        }
        self.transition(current, DeploymentPhase::Committing)
    }

    fn advance_committing(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        self.observe_candidate_before_waiting(&current.value)?;
        let route_proof = ReadinessClass::of(required(
            &current.value.expected,
            "Expected projection",
        )?)? == ReadinessClass::RouteProof;
        let ready_current = if route_proof {
            // The candidate's Ready proof is durable, and the stable route's
            // challenge below is its currency. No Odin is read to commit.
            current.clone()
        } else {
            let current_lease_sha256 = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256)
                .map(str::to_owned);
            let Some((ready_current, authenticated)) =
                self.admit_latest_topology(current, current_lease_sha256.as_deref())?
            else {
                return Ok(());
            };
            if ready_current.envelope != current.envelope {
                return Ok(());
            }
            ensure!(
                is_semantic_ready(&authenticated),
                "latest Odin observation is not Ready at admission commit"
            );
            // Not require_current: the Ready receipt is durable evidence, so
            // authenticating it against `now` asks a stored record to be live and
            // refuses it once it ages past the 30s observation window. Route
            // admission (:3906) refreshes the receipt to the latest observation and
            // can therefore demand currency; by Committing the receipt is history.
            // Currency here is answered by the latest observation, authenticated
            // and checked for semantic readiness immediately above.
            self.rehydrate_ready_token(&ready_current.value, now_millis()?, false)?;
            ready_current
        };
        validate_live_providers_for_deploy(ready_current.value.command_kind, || {
            self.validate_selected_providers_current(required(
                &ready_current.value.plan,
                "transaction plan",
            )?)
        })?;

        let expected = required(&ready_current.value.expected, "Expected projection")?;
        let activation = required(&ready_current.value.activation, "activation")?;
        let workload = required(&ready_current.value.workload, "workload")?;
        self.workload_for(required(&ready_current.value.plan, "transaction plan")?)?
            .observe(expected, activation, workload)?;
        self.ensure_transaction_write_lease_current(&ready_current.value, now_millis()?)?;
        if let Some(route) = ready_current
            .value
            .routing
            .as_ref()
            .and_then(RoutingEvidence::observation)
        {
            let binding = ready_current
                .value
                .plan
                .as_ref()
                .unwrap()
                .parsed_inputs()?
                .1;
            let driver =
                self.route_driver(binding.route.context("routed plan has no route binding")?);
            ensure!(
                driver.observe_membership(expected, &route.membership_sha256)?,
                "route membership changed before admission commit"
            );
            let current_route = self.prove_stable_route(
                &ready_current.value,
                &driver,
                route.membership_sha256.clone(),
            )?;
            ensure!(
                current_route.route_id == route.route_id
                    && current_route.runtime_instance_id == route.runtime_instance_id,
                "stable route changed incarnation before admission commit"
            );
            ensure!(
                driver.observe_membership(expected, &route.membership_sha256)?,
                "route membership changed during the final signed admission challenge"
            );
            self.ensure_transaction_write_lease_current(&ready_current.value, now_millis()?)?;
        }

        let commit_current = if route_proof {
            ready_current
        } else {
            let current_lease_sha256 = ready_current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256)
                .map(str::to_owned);
            let Some((commit_current, authenticated)) =
                self.admit_latest_topology(&ready_current, current_lease_sha256.as_deref())?
            else {
                return Ok(());
            };
            if commit_current.envelope != ready_current.envelope {
                return Ok(());
            }
            ensure!(
                is_semantic_ready(&authenticated),
                "latest Odin observation is not Ready after the final admission challenge"
            );
            self.rehydrate_ready_token(&commit_current.value, now_millis()?, false)?;
            commit_current
        };
        let now = now_millis()?;
        validate_live_providers_for_deploy(commit_current.value.command_kind, || {
            self.validate_selected_providers_current(required(
                &commit_current.value.plan,
                "transaction plan",
            )?)
        })?;
        self.workload_for(required(&commit_current.value.plan, "transaction plan")?)?
            .observe(
                required(&commit_current.value.expected, "Expected projection")?,
                required(&commit_current.value.activation, "activation")?,
                required(&commit_current.value.workload, "workload")?,
            )?;
        self.ensure_transaction_write_lease_current(&commit_current.value, now)?;

        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let incumbent = self.exact_incumbent(&snapshot, &commit_current.value)?;
        let odin_authority = (!route_proof)
            .then(|| self.current_odin_authority(&snapshot))
            .transpose()?;
        let generation = AdmittedGeneration::from_transaction(
            &commit_current.value,
            odin_authority,
            now,
        )?;
        let post_commit_cleanup = PostCommitCleanup {
            incumbent: match incumbent {
                Some(incumbent)
                    if incumbent_was_stopped_during_fencing(required(
                        &commit_current.value.fencing,
                        "commit fencing evidence",
                    )?) =>
                {
                    IncumbentCleanupEvidence::Complete {
                        generation_id: incumbent.value.generation_id.clone(),
                    }
                }
                Some(incumbent) => IncumbentCleanupEvidence::Pending {
                    generation_id: incumbent.value.generation_id.clone(),
                    workload: incumbent.value.workload.clone(),
                },
                None => IncumbentCleanupEvidence::SkippedNoIncumbent,
            },
            source: match commit_current.value.command_kind {
                CommandKind::Deploy => SourceCleanupEvidence::Pending,
                CommandKind::Continuity => SourceCleanupEvidence::SkippedContinuity,
            },
        };
        let mut complete = commit_current.value.clone();
        complete.enter_phase(DeploymentPhase::Complete, now);
        complete.last_error = None;
        complete.completion = Some(TransactionCompletion::Admitted {
            generation_id: generation.generation_id.clone(),
        });
        complete.post_commit_cleanup = Some(post_commit_cleanup);
        complete.validate()?;

        let admitted_expected = CultCacheExpectedEnvelope {
            r#type: AdmittedGeneration::TYPE.into(),
            key: generation.target.clone(),
            current: incumbent.map(|stored| stored.envelope.clone()),
        };
        ensure!(
            SingleFileMessagePackBackingStore::new(&self.options.state_store).compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: commit_current.value.transaction_id.clone(),
                        current: Some(commit_current.envelope.clone()),
                    },
                    admitted_expected,
                ],
                &[
                    transaction_envelope(&complete, now)?,
                    admitted_envelope(&generation, now)?,
                ],
            )?,
            "incumbent or transaction changed before atomic admission commit"
        );
        Ok(())
    }

    fn exact_incumbent<'a>(
        &self,
        snapshot: &'a ControlSnapshot,
        transaction: &DeploymentTransaction,
    ) -> Result<Option<&'a Stored<AdmittedGeneration>>> {
        let current = snapshot.admitted_for(&transaction.target);
        match (&transaction.incumbent_generation_id, current) {
            (None, None) => Ok(None),
            (Some(expected), Some(current)) if current.value.generation_id == *expected => {
                Ok(Some(current))
            }
            _ => bail!("target incumbent changed after transaction creation"),
        }
    }

    fn incumbent_lease_sha256_for_warming(
        &self,
        snapshot: &ControlSnapshot,
        transaction: &DeploymentTransaction,
    ) -> Result<Option<String>> {
        if let Some(fencing) = &transaction.fencing {
            return match fencing {
                FencingEvidence::SkippedStateless => Ok(None),
                FencingEvidence::Revoked {
                    incumbent_lease_sha256,
                    ..
                } => Ok(incumbent_lease_sha256.clone()),
            };
        }
        self.exact_incumbent(snapshot, transaction)?
            .and_then(|incumbent| incumbent.value.leasing.lease())
            .map(IdunnProcessWriteLeaseRecord::canonical_sha256)
            .transpose()
    }

    fn fresh_warming_for_lease(
        &self,
        current: &Stored<DeploymentTransaction>,
        now: u64,
    ) -> Result<
        Option<(
            Stored<DeploymentTransaction>,
            WarmingEvidence,
            SequenceAdmittedWarming,
        )>,
    > {
        let prior = self.rehydrate_warming_token(&current.value, now, false)?;
        match required(&current.value.warming, "pre-fence Warming evidence")?.clone() {
            WarmingEvidence::FirstOdinDirect { .. } => {
                // Whether this transaction may observe Odin directly was
                // settled when the Warming evidence was recorded, and that
                // decision is durable in the transaction. Re-deciding it here
                // with the narrower "no admitted Odin" test contradicted the
                // caller and failed the refresh. What the refresh owes is
                // freshness, and the replay check below is what provides it.
                // That the target is Odin was already required by the
                // rehydration above, which authenticates the recorded answer.
                let (evidence, present) = self.observe_first_odin_warming(&current.value)?;
                let warming = WarmingEvidence::FirstOdinDirect { evidence };
                let token = SequenceAdmittedWarming::from_direct_presence(
                    current.value.transaction_id.clone(),
                    warming.clone(),
                    present,
                )?;
                ensure!(
                    token.signed_presence_sha256() != prior.signed_presence_sha256(),
                    "first Odin replayed its pre-fence Warming presence"
                );
                Ok(Some((current.clone(), warming, token)))
            }
            WarmingEvidence::RouteProofDirect { .. } => {
                // A stateful route-proof candidate is warming until it holds
                // the lease this refresh is about to bind, so the fresh
                // presence must be warming again, and must be a new one.
                let CandidateAnswer::Answered { evidence, present } =
                    self.challenge_candidate(&current.value, &["warming"], None)?
                else {
                    return Ok(None);
                };
                let warming = WarmingEvidence::RouteProofDirect { evidence };
                let token = SequenceAdmittedWarming::from_direct_presence(
                    current.value.transaction_id.clone(),
                    warming.clone(),
                    present,
                )?;
                ensure!(
                    token.signed_presence_sha256() != prior.signed_presence_sha256(),
                    "route-proof candidate replayed its pre-fence Warming presence"
                );
                Ok(Some((current.clone(), warming, token)))
            }
            WarmingEvidence::OdinTopology {
                evidence: prior_evidence,
            } => {
                let Some((admitted, authenticated)) = self.admit_latest_topology(current, None)?
                else {
                    return Ok(None);
                };
                let fresh_evidence = required(
                    &admitted.value.latest_odin_observation,
                    "post-fence Odin Warming observation",
                )?
                .clone();
                let snapshot = ControlSnapshot::read(&self.options.state_store)?;
                let incumbent_lease_sha256 =
                    self.incumbent_lease_sha256_for_warming(&snapshot, &admitted.value)?;
                if !is_semantic_warming(
                    required(&admitted.value.expected, "Warming Expected projection")?,
                    required(&admitted.value.activation, "Warming activation")?,
                    incumbent_lease_sha256.as_deref(),
                    &authenticated,
                )? {
                    return Ok(None);
                }
                let token = SequenceAdmittedWarming::from_topology(
                    admitted.value.transaction_id.clone(),
                    authenticated,
                )?;
                if !provider_warming_advanced(
                    prior_evidence.publisher_sequence,
                    prior.signed_presence_sha256(),
                    fresh_evidence.publisher_sequence,
                    token.signed_presence_sha256(),
                ) {
                    return Ok(None);
                }
                Ok(Some((
                    admitted,
                    WarmingEvidence::OdinTopology {
                        evidence: fresh_evidence,
                    },
                    token,
                )))
            }
        }
    }

    fn prepare_candidate_write_lease(
        &self,
        transaction: &DeploymentTransaction,
        warming: &SequenceAdmittedWarming,
        now: u64,
    ) -> Result<IdunnProcessWriteLeaseRecord> {
        let expected = required(&transaction.expected, "Expected projection")?;
        let activation = required(&transaction.activation, "activation")?;
        ensure!(
            expected.write_lease_required
                && warming.transaction_id() == transaction.transaction_id
                && warming.runtime_instance_id() == activation.runtime_instance_id,
            "fresh Warming token does not own this stateful candidate"
        );
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let epoch = self
            .exact_incumbent(&snapshot, transaction)?
            .and_then(|generation| generation.value.leasing.lease())
            .map_or(1, |lease| lease.lease_epoch.saturating_add(1));
        let lease = IdunnProcessWriteLeaseRecord {
            schema_version: IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into(),
            target: expected.target.clone(),
            expected_projection_sha256: expected.canonical_sha256()?,
            plan_id: expected.plan_id.clone(),
            incarnation_id: expected.incarnation_id.clone(),
            sealed_release_id: expected.sealed_release_id.clone(),
            activation_witness_sha256: activation.canonical_sha256()?,
            state_schema_generation: expected
                .state_schema_generation
                .clone()
                .context("stateful Expected has no schema generation")?,
            state_contract_sha256: expected
                .state_contract_sha256
                .clone()
                .context("stateful Expected has no state contract")?,
            runtime_id: expected.runtime_id.clone(),
            runtime_instance_id: activation.runtime_instance_id.clone(),
            warming_presence_sha256: warming.signed_presence_sha256().to_owned(),
            lease_epoch: epoch,
            issued_at_unix_millis: now,
        };
        lease.validate()?;
        Ok(lease)
    }

    fn ensure_transaction_write_lease_current(
        &self,
        transaction: &DeploymentTransaction,
        now: u64,
    ) -> Result<()> {
        let expected = required(&transaction.expected, "Expected projection")?;
        let lease = transaction
            .leasing
            .as_ref()
            .and_then(LeasingEvidence::lease);
        ensure!(
            expected.write_lease_required == lease.is_some(),
            "transaction write-lease disposition differs from Expected"
        );
        let Some(lease) = lease else {
            return Ok(());
        };
        let activation = required(&transaction.activation, "activation")?;
        let warming = self.rehydrate_warming_token(transaction, now, false)?;
        let lease_path = required(&transaction.plan, "transaction plan")?
            .parsed_inputs()?
            .1
            .process_write_lease
            .context("stateful target has no write-lease binding")?
            .record_path;
        ensure!(
            CultCacheWriteLeaseDriver::new(&transaction.target, lease_path)
                .observe(expected, activation, &warming, lease,)?,
            "process write lease changed across the authority boundary"
        );
        Ok(())
    }

    fn select_candidate_port(&self, binding: &OperatorBinding) -> Result<Option<u16>> {
        let Some(route) = &binding.route else {
            return Ok(None);
        };
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let mut used = BTreeSet::new();
        for plan in snapshot
            .admitted
            .iter()
            .map(|stored| &stored.value.plan)
            .chain(
                snapshot
                    .transactions
                    .iter()
                    .filter(|stored| stored.value.blocks_new_target_mutation())
                    .filter_map(|stored| stored.value.plan.as_ref()),
            )
        {
            if let Some(port) = plan.candidate_port {
                used.insert(port);
            }
        }
        (route.private_port_start..=route.private_port_end)
            .find(|port| !used.contains(port))
            .map(Some)
            .context("no private candidate port remains in the operator range")
    }

    fn deployment_authorization(
        &self,
        current: &Stored<DeploymentTransaction>,
        now: u64,
    ) -> Result<Option<DeploymentAuthorization>> {
        let expected = required(&current.value.expected, "Expected projection")?;
        let plan = required(&current.value.plan, "transaction plan")?;
        let binding = plan.parsed_inputs()?.1;
        let Some((record, canonical_bytes)) =
            read_deployment_brake(&binding.brakes.deployment_store)?
        else {
            return Ok(None);
        };
        let anchor = read_trust_anchor::<IdunnDeploymentBrakeOperatorIdentity>(
            &self.options.deployment_brake_operator_anchor,
        )?;
        if evaluate_idunn_deployment_brake(
            IdunnDeploymentBrakeObservation::Present(&record),
            &anchor,
            &expected.runtime_id,
            &expected.sealed_release_id,
            &current.value.transaction_id,
            now,
        )
        .is_err()
        {
            return Ok(None);
        }
        let authorization_id = record
            .authorization_id
            .clone()
            .context("released brake has no authorization id")?;
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        ensure!(
            !snapshot.transactions.iter().any(|stored| {
                stored
                    .value
                    .deployment_authorization
                    .as_ref()
                    .is_some_and(|used| used.authorization_id == authorization_id)
            }),
            "deployment authorization was already consumed"
        );
        Ok(Some(DeploymentAuthorization {
            authorization_id,
            brake_sha256: sha256_id(&canonical_bytes),
            canonical_brake_bytes: canonical_bytes,
            authorized_at_unix_millis: now,
        }))
    }

    fn lifecycle_allows(&self, current: &Stored<DeploymentTransaction>, now: u64) -> Result<bool> {
        let plan = required(&current.value.plan, "continuity plan")?;
        let binding = plan.parsed_inputs()?.1;
        let expected = required(&current.value.expected, "continuity Expected")?;
        match read_lifecycle_brake(&binding.brakes.lifecycle_store) {
            Ok(Some(record)) => Ok(evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Present(&record),
                &expected.runtime_id,
                &current.value.target,
                now,
            )
            .is_ok()),
            Ok(None) => Ok(evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Missing,
                &expected.runtime_id,
                &current.value.target,
                now,
            )
            .is_ok()),
            Err(_) => Ok(false),
        }
    }

    /// A phase that waits on Odin's evidence about the candidate must first
    /// confirm the candidate still exists. Otherwise a candidate that died
    /// while its correlation was pending is never observed again: the phase
    /// returns early on "no evidence yet" every tick, the error that would have
    /// triggered the post-fencing abort never happens, and the transaction
    /// owns the target forever behind a process that is gone.
    fn observe_candidate_before_waiting(&self, transaction: &DeploymentTransaction) -> Result<()> {
        self.workload_for(required(&transaction.plan, "transaction plan")?)?
            .observe(
                required(&transaction.expected, "Expected projection")?,
                required(&transaction.activation, "activation")?,
                required(&transaction.workload, "candidate workload")?,
            )
            .map(|_| ())
    }

    /// Withdraw one projected incarnation of this target that nothing owns:
    /// not the admitted generation, not any live transaction. A replaced
    /// incumbent, an aborted candidate whose reconciliation was skipped, or a
    /// record left by the projection migration all end here. One per tick;
    /// the projection is a shared file and each withdrawal is one CAS.
    fn retire_stale_incarnations(
        &self,
        snapshot: &ControlSnapshot,
        current: &Stored<AdmittedGeneration>,
    ) -> Result<bool> {
        let topology = self.topology();
        let target = current.value.target.as_str();
        let owned = snapshot
            .transactions
            .iter()
            .filter(|stored| stored.value.target == target && !stored.value.is_terminal())
            .filter_map(|stored| stored.value.expected.as_ref())
            .map(IdunnExpectedIncarnationRecord::canonical_sha256)
            .collect::<Result<BTreeSet<_>>>()?;
        let admitted_sha256 = current.value.expected.canonical_sha256()?;
        let Some(stale) = topology
            .projected_incarnations(target)?
            .into_iter()
            .find(|expected| {
                expected
                    .canonical_sha256()
                    .is_ok_and(|sha256| sha256 != admitted_sha256 && !owned.contains(&sha256))
            })
        else {
            return Ok(false);
        };
        let provider_anchor = self.provider_anchor_for_plan(&current.value.plan)?;
        topology.withdraw_stale_incarnation(&stale, &provider_anchor)?;
        eprintln!(
            "Idunn withdrew stale {} incarnation {} from the projection",
            target, stale.incarnation_id
        );
        Ok(true)
    }

    /// The lifecycle brake as it applies to restarting one admitted
    /// generation: the same evaluation `lifecycle_allows` makes for a
    /// continuity transaction, asked before one is minted.
    fn lifecycle_allows_generation(
        &self,
        current: &Stored<AdmittedGeneration>,
        now: u64,
    ) -> Result<bool> {
        let binding = current.value.plan.parsed_inputs()?.1;
        let expected = &current.value.expected;
        match read_lifecycle_brake(&binding.brakes.lifecycle_store) {
            Ok(Some(record)) => Ok(evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Present(&record),
                &expected.runtime_id,
                &current.value.target,
                now,
            )
            .is_ok()),
            Ok(None) => Ok(evaluate_idunn_continuity_restart(
                IdunnLifecycleBrakeObservation::Missing,
                &expected.runtime_id,
                &current.value.target,
                now,
            )
            .is_ok()),
            Err(_) => Ok(false),
        }
    }

    fn record_gate_wait(
        &self,
        current: &Stored<DeploymentTransaction>,
        detail: &str,
    ) -> Result<()> {
        if current.value.last_error.as_deref() == Some(detail) {
            return Ok(());
        }
        let mut next = current.value.clone();
        next.last_error = Some(detail.into());
        next.updated_at_unix_millis = now_millis()?;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn runtime_authority(
        &self,
        transaction: &DeploymentTransaction,
    ) -> Result<cultnet_rs::VerifiedRuntimeAuthority> {
        self.runtime_authority_parts(
            required(&transaction.plan, "transaction plan")?,
            required(&transaction.expected, "Expected projection")?,
            required(&transaction.activation, "activation")?,
        )
    }

    /// The stable route's proof: a challenged presence that is Active.
    fn authenticate_routed_presence(
        &self,
        authority: &cultnet_rs::VerifiedRuntimeAuthority,
        current_write_lease_sha256: Option<&str>,
        message_id: &str,
        challenged_at_unix_millis: u64,
        received_at_unix_millis: u64,
        canonical_presence: &[u8],
    ) -> Result<(String, u64)> {
        let present = self.authenticate_challenged_presence(
            authority,
            &["active"],
            current_write_lease_sha256,
            message_id,
            challenged_at_unix_millis,
            received_at_unix_millis,
            canonical_presence,
        )?;
        Ok((
            present.signed_presence_sha256().to_owned(),
            received_at_unix_millis,
        ))
    }

    /// A presence Idunn challenged for itself, from the stable endpoint or the
    /// candidate's: signed by the provider and the launch's activation key,
    /// bound to current authority, minted after the challenge, answering that
    /// exact challenge, in one of `states`. A warming presence holds no write
    /// lease; a presence in any other state holds exactly the current one.
    /// Every disagreement with authority is named, so a capacity below the
    /// Expected minimum reads as that and not as a generic refusal.
    fn authenticate_challenged_presence(
        &self,
        authority: &cultnet_rs::VerifiedRuntimeAuthority,
        states: &[&str],
        current_write_lease_sha256: Option<&str>,
        message_id: &str,
        challenged_at_unix_millis: u64,
        received_at_unix_millis: u64,
        canonical_presence: &[u8],
    ) -> Result<cultnet_rs::VerifiedRuntimePresence> {
        ensure!(
            received_at_unix_millis >= challenged_at_unix_millis,
            "route proof predates its challenge"
        );
        let authenticated = authenticate_runtime_presence_claim(
            canonical_presence,
            authority,
            RuntimePresenceAuthenticationContext {
                trusted_received_at_unix_millis: received_at_unix_millis,
                maximum_age_millis: self.options.topology_maximum_age_millis,
                maximum_future_skew_millis: self.options.topology_maximum_future_skew_millis,
            },
        )?;
        let correlation = correlate_runtime_presence_claim(authenticated, authority)?;
        if !correlation.disagreements().is_empty() {
            return Err(PresenceDisagrees {
                disagreements: correlation.disagreements().to_vec(),
            }
            .into());
        }
        let present = correlation.into_undisputed_present()?;
        let presence = present.record();
        ensure!(
            presence.observed_at_unix_millis >= challenged_at_unix_millis,
            "route proof returned a presence minted before its challenge"
        );
        ensure!(
            states.contains(&presence.state.as_str()),
            "route proof runtime is {}, not {}",
            presence.state,
            states.join(" or ")
        );
        ensure!(
            presence.detail == format!("route-observation:{message_id}"),
            "route proof response is not bound to the exact challenge"
        );
        // A warming presence is exempt from the lease comparison: it is
        // Idunn's own lease that a warming candidate cannot hold yet, and the
        // authenticator above already refuses one that claims a lease.
        let lease_is_current = presence.state == "warming"
            || presence.write_lease_sha256.as_deref() == current_write_lease_sha256;
        ensure!(
            lease_is_current,
            "route proof runtime does not hold the exact current process write lease"
        );
        Ok(present)
    }

    fn authenticate_first_odin_warming_presence(
        &self,
        transaction: &DeploymentTransaction,
        message_id: &str,
        challenged_at_unix_millis: u64,
        received_at_unix_millis: u64,
        canonical_presence: &[u8],
    ) -> Result<cultnet_rs::VerifiedRuntimePresence> {
        // Whether direct observation is warranted at all is decided in
        // advance_warming, which asks the question that matters: can any Odin
        // report on this one. Re-deciding it here with the narrower "has an
        // Odin ever been admitted" test only contradicted that caller. What is
        // still checked is what this function itself depends on -- the target
        // is Odin, and the incarnation is stateful.
        let expected = required(&transaction.expected, "Odin Expected")?;
        ensure!(
            ReadinessClass::of(expected)? == ReadinessClass::OdinSelf
                && expected.write_lease_required,
            "direct Warming presence is reserved for a stateful Odin incarnation"
        );
        ensure!(
            received_at_unix_millis >= challenged_at_unix_millis,
            "first Odin Warming observation predates its challenge"
        );
        let authority = self.runtime_authority(transaction)?;
        let authenticated = authenticate_runtime_presence_claim(
            canonical_presence,
            &authority,
            RuntimePresenceAuthenticationContext {
                trusted_received_at_unix_millis: received_at_unix_millis,
                maximum_age_millis: self.options.topology_maximum_age_millis,
                maximum_future_skew_millis: self.options.topology_maximum_future_skew_millis,
            },
        )?;
        let correlation = correlate_runtime_presence_claim(authenticated, &authority)?;
        ensure!(
            correlation.disagreements().is_empty(),
            "first Odin Warming presence disagrees with its Expected activation"
        );
        let present = correlation.into_undisputed_present()?;
        let presence = present.record();
        ensure!(
            presence.observed_at_unix_millis >= challenged_at_unix_millis
                && presence.state == "warming"
                && presence.write_lease_sha256.is_none()
                && presence.detail == format!("idunn-warming:{message_id}"),
            "first Odin candidate did not return exact fresh pre-lease Warming evidence"
        );
        Ok(present)
    }

    fn observe_first_odin_warming(
        &self,
        transaction: &DeploymentTransaction,
    ) -> Result<(RuntimePresenceEvidence, cultnet_rs::VerifiedRuntimePresence)> {
        let expected = required(&transaction.expected, "first Odin Expected")?;
        let binding = required(&transaction.plan, "first Odin plan")?
            .parsed_inputs()?
            .1;
        let driver = self.route_driver(
            binding
                .route
                .context("first Odin bootstrap has no candidate route binding")?,
        );
        let message_id = format!("warming-{}", Uuid::new_v4().simple());
        let challenged_at_unix_millis = now_millis()?;
        let response = driver.request_candidate_runtime_presence(expected, &message_id)?;
        ensure!(
            response.message_id == message_id,
            "first Odin candidate transport substituted its challenge identity"
        );
        let admitted_at_unix_millis = now_millis()?;
        let present = self.authenticate_first_odin_warming_presence(
            transaction,
            &message_id,
            challenged_at_unix_millis,
            admitted_at_unix_millis,
            &response.canonical_presence,
        )?;
        let evidence = RuntimePresenceEvidence::from_present(
            &present,
            message_id,
            challenged_at_unix_millis,
            admitted_at_unix_millis,
        )?;
        Ok((evidence, present))
    }

    fn prove_stable_route(
        &self,
        transaction: &DeploymentTransaction,
        driver: &NginxRouteDriver,
        membership_sha256: String,
    ) -> Result<RouteObservation> {
        let expected = required(&transaction.expected, "route Expected projection")?;
        let activation = required(&transaction.activation, "route activation")?;
        let authority = self.runtime_authority(transaction)?;
        let current_write_lease_sha256 = transaction
            .leasing
            .as_ref()
            .and_then(LeasingEvidence::lease_sha256);
        self.prove_stable_route_against(
            expected,
            activation,
            &authority,
            current_write_lease_sha256,
            driver,
            membership_sha256,
        )
    }

    fn prove_stable_route_against(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        authority: &cultnet_rs::VerifiedRuntimeAuthority,
        current_write_lease_sha256: Option<&str>,
        driver: &NginxRouteDriver,
        membership_sha256: String,
    ) -> Result<RouteObservation> {
        let message_id = format!("route-{}", Uuid::new_v4().simple());
        let challenged_at_unix_millis = now_millis()?;
        let response = driver.request_runtime_presence(expected, &message_id)?;
        ensure!(
            response.message_id == message_id,
            "route transport substituted its challenge identity"
        );
        let received_at_unix_millis = now_millis()?;
        let (signed_presence_sha256, observed_at_unix_millis) = self.authenticate_routed_presence(
            authority,
            current_write_lease_sha256,
            &message_id,
            challenged_at_unix_millis,
            received_at_unix_millis,
            &response.canonical_presence,
        )?;
        let observation = RouteObservation {
            route_id: driver.binding.route_id.clone(),
            runtime_instance_id: activation.runtime_instance_id.clone(),
            membership_sha256,
            signed_presence_sha256,
            observed_at_unix_millis,
        };
        observation.validate()?;
        Ok(observation)
    }

    fn runtime_authority_parts(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<cultnet_rs::VerifiedRuntimeAuthority> {
        let provider_anchor = self.provider_anchor_for_plan(plan)?;
        verify_runtime_authority(
            expected,
            activation,
            &self.idunn_anchor,
            &provider_anchor.public_key,
        )
    }

    fn provider_anchor_for_plan(
        &self,
        plan: &CompiledDeploymentPlan,
    ) -> Result<ServiceIdentityTrustAnchor> {
        let binding = plan.parsed_inputs()?.1;
        let provider_anchor = read_trust_anchor::<GameCultProviderHealthIdentity>(
            &binding.runtime_identity.trust_anchor_store,
        )?;
        ensure!(
            provider_anchor.schema_version
                == <GameCultProviderHealthIdentity as ServiceIdentityProfile>::TRUST_ANCHOR_SCHEMA,
            "provider runtime presence trust anchor schema is unsupported"
        );
        Ok(provider_anchor)
    }

    fn authenticate_topology_bytes(
        &self,
        snapshot: &ControlSnapshot,
        transaction: &DeploymentTransaction,
        canonical_bytes: &[u8],
        current_write_lease_sha256: Option<&str>,
        trusted_received_at: u64,
    ) -> Result<AuthenticatedOdinRuntimeTopologyCorrelation> {
        let authority = self.runtime_authority(transaction)?;
        let odin_authority = self.current_odin_authority(snapshot)?;
        authenticate_odin_runtime_topology_correlation(
            canonical_bytes,
            &authority,
            current_write_lease_sha256,
            &odin_authority.signer_public_key,
            self.trusted_topology_context(trusted_received_at),
        )
    }

    /// Persist every newly authenticated publisher sequence before deciding
    /// whether it means Warming, Ready, degraded, or disagreement.
    fn admit_latest_topology(
        &self,
        current: &Stored<DeploymentTransaction>,
        current_write_lease_sha256: Option<&str>,
    ) -> Result<
        Option<(
            Stored<DeploymentTransaction>,
            AuthenticatedOdinRuntimeTopologyCorrelation,
        )>,
    > {
        // Odin keeps one correlation per incarnation, so the transaction's own
        // Expected selects exactly the evidence about it. A correlation about
        // the incarnation this one is replacing lives under another key and is
        // never handed back here.
        let expected_sha256 =
            required(&current.value.expected, "Expected projection")?.canonical_sha256()?;
        let Some(received) = self
            .topology()
            .receive(&current.value.target, &expected_sha256)?
        else {
            return Ok(None);
        };
        ensure!(
            received.target == current.value.target,
            "topology transport substituted target"
        );
        let now = now_millis()?;
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let live = snapshot
            .transactions
            .iter()
            .find(|stored| stored.value.transaction_id == current.value.transaction_id)
            .context("transaction disappeared before topology admission")?;
        ensure!(
            live.envelope == current.envelope,
            "transaction changed before topology admission"
        );
        let authenticated = match self.authenticate_topology_bytes(
            &snapshot,
            &live.value,
            &received.canonical_bytes,
            current_write_lease_sha256,
            now,
        ) {
            Ok(authenticated) => authenticated,
            // Odin re-stamps a correlation only when its facts change, and a
            // live presence changes them every heartbeat. A correlation that
            // has aged out therefore says one thing: nothing about this
            // incarnation has arrived lately. That is absence to wait on, not
            // evidence to refuse; it failed every Heimdall deployment and the
            // first observed Odin deployment as a fault.
            Err(error) if is_stale_observation(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let evidence = TopologyEvidence::from_authenticated(&authenticated, now)?;
        if !sequence_requires_admission(
            live.value.latest_odin_observation.as_ref(),
            snapshot.max_odin_sequence(&live.value.target, &evidence.signer_identity_id),
            &evidence,
        )? {
            return Ok(Some((live.clone(), authenticated)));
        }
        let mut next = live.value.clone();
        next.latest_odin_observation = Some(evidence.clone());
        next.odin_publisher_sequence_cursor = evidence.publisher_sequence;
        next.updated_at_unix_millis = now;
        next.last_error = None;
        replace_transaction(&self.options.state_store, live, &next)?;
        let admitted = ControlSnapshot::read(&self.options.state_store)?
            .transactions
            .into_iter()
            .find(|stored| stored.value.transaction_id == next.transaction_id)
            .context("sequence-admitted transaction disappeared")?;
        ensure!(
            admitted.value.latest_odin_observation.as_ref() == Some(&evidence)
                && admitted.value.odin_publisher_sequence_cursor == evidence.publisher_sequence,
            "topology sequence was not durably admitted"
        );
        Ok(Some((admitted, authenticated)))
    }

    fn rehydrate_warming_token(
        &self,
        transaction: &DeploymentTransaction,
        now: u64,
        require_current: bool,
    ) -> Result<SequenceAdmittedWarming> {
        let evidence = required(&transaction.warming, "durable warming evidence")?.clone();
        match evidence {
            WarmingEvidence::OdinTopology { evidence } => {
                let snapshot = ControlSnapshot::read(&self.options.state_store)?;
                let authenticated = self.authenticate_topology_bytes(
                    &snapshot,
                    transaction,
                    &evidence.canonical_bytes,
                    None,
                    if require_current {
                        now
                    } else {
                        evidence.admitted_at_unix_millis
                    },
                )?;
                validate_authenticated_evidence(&evidence, &authenticated)?;
                let incumbent_lease_sha256 =
                    self.incumbent_lease_sha256_for_warming(&snapshot, transaction)?;
                ensure!(
                    is_semantic_warming(
                        required(&transaction.expected, "Warming Expected projection")?,
                        required(&transaction.activation, "Warming activation")?,
                        incumbent_lease_sha256.as_deref(),
                        &authenticated,
                    )?,
                    "durable warming evidence no longer satisfies the Warming gate"
                );
                SequenceAdmittedWarming::from_topology(
                    transaction.transaction_id.clone(),
                    authenticated,
                )
            }
            WarmingEvidence::FirstOdinDirect { evidence } => {
                let present = self.authenticate_first_odin_warming_presence(
                    transaction,
                    &evidence.message_id,
                    evidence.challenged_at_unix_millis,
                    if require_current {
                        now
                    } else {
                        evidence.admitted_at_unix_millis
                    },
                    &evidence.canonical_bytes,
                )?;
                SequenceAdmittedWarming::from_direct_presence(
                    transaction.transaction_id.clone(),
                    WarmingEvidence::FirstOdinDirect { evidence },
                    present,
                )
            }
            WarmingEvidence::RouteProofDirect { evidence } => {
                let present = self.reauthenticate_route_proof(
                    transaction,
                    &evidence,
                    route_proof_warming_states(required(
                        &transaction.expected,
                        "Warming Expected projection",
                    )?),
                    None,
                    if require_current {
                        now
                    } else {
                        evidence.admitted_at_unix_millis
                    },
                )?;
                SequenceAdmittedWarming::from_direct_presence(
                    transaction.transaction_id.clone(),
                    WarmingEvidence::RouteProofDirect { evidence },
                    present,
                )
            }
        }
    }

    /// A route-proof presence Idunn recorded, authenticated again as of `at`.
    fn reauthenticate_route_proof(
        &self,
        transaction: &DeploymentTransaction,
        evidence: &RuntimePresenceEvidence,
        states: &[&str],
        current_write_lease_sha256: Option<&str>,
        at_unix_millis: u64,
    ) -> Result<cultnet_rs::VerifiedRuntimePresence> {
        let authority = self.runtime_authority(transaction)?;
        self.authenticate_challenged_presence(
            &authority,
            states,
            current_write_lease_sha256,
            &evidence.message_id,
            evidence.challenged_at_unix_millis,
            at_unix_millis,
            &evidence.canonical_bytes,
        )
    }

    /// One direct challenge to a route-proof target's candidate endpoint.
    /// Failing to reach or hear the candidate is only silence: a process that
    /// is still starting does not answer, and that is waiting, not a fault.
    /// What does answer is authenticated, and a bad answer is an error, as is
    /// a challenge Idunn could not make: those stay a `ChallengeFailure` in the
    /// error's chain.
    fn challenge_candidate(
        &self,
        transaction: &DeploymentTransaction,
        states: &[&str],
        current_write_lease_sha256: Option<&str>,
    ) -> Result<CandidateAnswer> {
        let expected = required(&transaction.expected, "candidate Expected")?;
        let binding = required(&transaction.plan, "candidate plan")?
            .parsed_inputs()?
            .1;
        let driver = self.route_driver(
            binding
                .route
                .context("route-proof candidate has no route binding")?,
        );
        let message_id = format!("candidate-{}", Uuid::new_v4().simple());
        let challenged_at_unix_millis = now_millis()?;
        let response = match driver.request_candidate_runtime_presence(expected, &message_id) {
            Ok(response) => response,
            Err(ChallengeFailure::Silent(error)) => {
                return Ok(CandidateAnswer::Silent(format!(
                    "candidate endpoint did not answer its challenge: {error:#}"
                )));
            }
            Err(bad @ ChallengeFailure::Refused(_)) => return Err(bad.into()),
        };
        let admitted_at_unix_millis = now_millis()?;
        let authority = self.runtime_authority(transaction)?;
        let present = self.authenticate_challenged_presence(
            &authority,
            states,
            current_write_lease_sha256,
            &message_id,
            challenged_at_unix_millis,
            admitted_at_unix_millis,
            &response.canonical_presence,
        )?;
        let evidence = RuntimePresenceEvidence::from_present(
            &present,
            message_id,
            challenged_at_unix_millis,
            admitted_at_unix_millis,
        )?;
        Ok(CandidateAnswer::Answered { evidence, present })
    }

    fn rehydrate_ready_token(
        &self,
        transaction: &DeploymentTransaction,
        now: u64,
        require_current: bool,
    ) -> Result<SequenceAdmittedReady> {
        let evidence = required(&transaction.ready, "durable Ready evidence")?
            .odin()
            .context("durable Ready evidence is not an Odin receipt")?
            .clone();
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        let current_lease = transaction
            .leasing
            .as_ref()
            .and_then(LeasingEvidence::lease_sha256);
        let authenticated = self.authenticate_topology_bytes(
            &snapshot,
            transaction,
            &evidence.canonical_bytes,
            current_lease,
            if require_current {
                now
            } else {
                evidence.admitted_at_unix_millis
            },
        )?;
        validate_authenticated_evidence(&evidence, &authenticated)?;
        ensure!(
            is_semantic_ready(&authenticated),
            "durable Ready evidence no longer authenticates as Ready"
        );
        Ok(SequenceAdmittedReady {
            transaction_id: transaction.transaction_id.clone(),
            evidence,
            expected: required(&transaction.expected, "Expected projection")?.clone(),
            authenticated,
        })
    }

    fn current_ready_provider_tokens(
        &self,
        snapshot: &ControlSnapshot,
    ) -> Result<Vec<SequenceAdmittedReady>> {
        let mut providers = Vec::new();
        for stored in &snapshot.admitted {
            // No pre-filter on ready == latest: a provider that published again
            // after going ready is still ready. rehydrate_admitted_ready owns
            // that judgment now, and reports why when it refuses.
            match self.rehydrate_admitted_ready(snapshot, &stored.value) {
                Ok(provider) => providers.push(provider),
                Err(error) => eprintln!(
                    "Idunn excluded non-current provider {}: {error:#}",
                    stored.value.target
                ),
            }
        }
        Ok(providers)
    }

    fn rehydrate_admitted_ready(
        &self,
        snapshot: &ControlSnapshot,
        generation: &AdmittedGeneration,
    ) -> Result<SequenceAdmittedReady> {
        let odin = generation.odin_receipts()?;
        let authority = self.runtime_authority_parts(
            &generation.plan,
            &generation.expected,
            &generation.activation,
        )?;
        let odin_authority = self.current_odin_authority(snapshot)?;
        let current_lease = generation.leasing.lease_sha256();
        let authenticated = authenticate_odin_runtime_topology_correlation(
            &odin.ready.canonical_bytes,
            &authority,
            current_lease,
            &odin_authority.signer_public_key,
            self.trusted_topology_context(odin.ready.admitted_at_unix_millis),
        )?;
        validate_authenticated_evidence(odin.ready, &authenticated)?;
        ensure!(
            is_semantic_ready(&authenticated),
            "admitted provider's Ready receipt is not Ready at its admission time"
        );
        // A provider that published after going ready is the ordinary case, not
        // a stale one, so the cursor is authenticated on its own terms rather
        // than compared to the receipt. Authentication is what binds it to this
        // incarnation -- it requires the record's current_activation_sha256 and
        // runtime_instance_id to match this generation's activation -- so a
        // separate identity check here would only restate it more weakly.
        // Note this is the cursor as frozen at admission, not a live read of
        // Odin. Both receipts are historical proofs; current workload health
        // is owned by continuity and route observation, not receipt age.
        let authenticated_latest = authenticate_odin_runtime_topology_correlation(
            &odin.latest.canonical_bytes,
            &authority,
            current_lease,
            &odin_authority.signer_public_key,
            self.trusted_topology_context(odin.latest.admitted_at_unix_millis),
        )?;
        validate_authenticated_evidence(odin.latest, &authenticated_latest)?;
        ensure!(
            is_semantic_ready(&authenticated_latest),
            "admitted provider's latest receipt is not Ready at its admission time"
        );
        Ok(SequenceAdmittedReady {
            transaction_id: generation.transaction_id.clone(),
            evidence: odin.ready.clone(),
            expected: generation.expected.clone(),
            authenticated,
        })
    }

    fn validate_selected_providers_current(
        &self,
        plan: &CompiledDeploymentPlan,
    ) -> Result<()> {
        let snapshot = ControlSnapshot::read(&self.options.state_store)?;
        for selection in &plan.dependencies {
            let Some(provider) = &selection.provider else {
                ensure!(
                    selection.requirement.kind == DependencyKind::Optional,
                    "required dependency has no provider"
                );
                continue;
            };
            let DependencyProviderAuthority::ManagedReady {
                target,
                incarnation_id,
                plan_id,
                sealed_release_id,
                expected_projection_sha256,
                odin_topology_correlation_sha256,
                odin_topology_publisher_sequence,
            } = &provider.authority
            else {
                continue;
            };
            let admitted = snapshot
                .admitted_for(target)
                .context("selected managed dependency is no longer admitted")?;
            let token = self.rehydrate_admitted_ready(&snapshot, &admitted.value)?;
            ensure!(
                admitted.value.expected.incarnation_id == *incarnation_id
                    && admitted.value.plan.plan_id == *plan_id
                    && admitted.value.sealed_release.sealed_release_id == *sealed_release_id
                    && admitted.value.expected.canonical_sha256()? == *expected_projection_sha256
                    && token.publisher_sequence() >= *odin_topology_publisher_sequence,
                "selected dependency authority changed before actuation"
            );
            ensure!(
                token.publisher_sequence() != *odin_topology_publisher_sequence
                    || token.evidence_sha256() == odin_topology_correlation_sha256,
                "selected dependency Odin sequence changed evidence"
            );
            ensure!(
                token
                    .authenticated()
                    .record()
                    .observed_capabilities
                    .iter()
                    .any(|capability| {
                        capability_compatible(
                            &selection.requirement.capability,
                            &selection.requirement.schema,
                            &selection.requirement.compatibility,
                            &capability.capability,
                            &capability.schema,
                            &capability.compatibility,
                        ) && capability.capacity >= selection.requirement.minimum_capacity
                    }),
                "selected dependency no longer provides its required capability"
            );
        }
        Ok(())
    }

    fn persist_same_phase<F>(
        &self,
        current: &Stored<DeploymentTransaction>,
        mutation: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut DeploymentTransaction) -> Result<()>,
    {
        let mut next = current.value.clone();
        mutation(&mut next)?;
        ensure!(
            next.phase == current.value.phase,
            "same-phase evidence update changed phase"
        );
        next.updated_at_unix_millis = now_millis()?;
        next.last_error = None;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn transition(
        &self,
        current: &Stored<DeploymentTransaction>,
        next_phase: DeploymentPhase,
    ) -> Result<()> {
        ensure!(
            next_phase as u8 == current.value.phase as u8 + 1,
            "deployment phase transition is not adjacent"
        );
        let mut next = current.value.clone();
        next.enter_phase(next_phase, now_millis()?);
        next.last_error = None;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn record_resumable_error(
        &self,
        current: &Stored<DeploymentTransaction>,
        error: &anyhow::Error,
    ) -> Result<()> {
        let detail = truncate(&format!("{error:#}"), 2048);
        if current.value.last_error.as_deref() == Some(detail.as_str()) {
            return Ok(());
        }
        let mut next = current.value.clone();
        next.last_error = Some(detail);
        next.updated_at_unix_millis = now_millis()?;
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn begin_pre_fencing_abort(
        &self,
        current: &Stored<DeploymentTransaction>,
        error: anyhow::Error,
    ) -> Result<()> {
        ensure!(
            current.value.phase < DeploymentPhase::Fencing,
            "post-fence transaction cannot terminal-fail"
        );
        ensure!(
            current.value.pre_fencing_abort.is_none(),
            "pre-fencing abort intent is already durable"
        );
        let abort = pre_fencing_abort_intent(&current.value, &format!("{error:#}"));
        self.persist_same_phase(current, |next| {
            next.pre_fencing_abort = Some(abort);
            Ok(())
        })
    }

    /// Whether this transaction's candidate can no longer run at all.
    ///
    /// A transaction that has never started one has no candidate to be dead, so
    /// it is not permanently stopped -- it is simply not there yet.
    fn candidate_is_permanently_stopped(
        &self,
        transaction: &DeploymentTransaction,
    ) -> Result<bool> {
        let Some(workload) = &transaction.workload else {
            return Ok(false);
        };
        self.workload_for(required(&transaction.plan, "transaction plan")?)?
            .is_permanently_stopped(workload)
    }

    fn begin_post_fencing_abort(
        &self,
        current: &Stored<DeploymentTransaction>,
        error: anyhow::Error,
    ) -> Result<()> {
        ensure!(
            current.value.phase >= DeploymentPhase::Fencing,
            "pre-fence transaction must abort through the pre-fencing path"
        );
        ensure!(
            current.value.post_fencing_abort.is_none(),
            "post-fencing abort intent is already durable"
        );
        let abort = post_fencing_abort_intent(&current.value, &truncate(&format!("{error:#}"), 2048));
        self.persist_same_phase(current, |next| {
            next.post_fencing_abort = Some(abort);
            Ok(())
        })
    }

    /// Withdraw the candidate in the one order that never leaves two writers:
    /// take away its route, then its write lease, then the process itself, and
    /// only then reconcile the projection.
    fn advance_post_fencing_abort(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let abort = required(
            &current.value.post_fencing_abort,
            "post-fencing abort intent",
        )?;
        if abort.route_restoration == CleanupEvidence::Pending {
            let preflight = required(&current.value.route_preflight, "route preflight receipt")?;
            let binding = current.value.plan.as_ref().unwrap().parsed_inputs()?.1;
            let driver =
                self.route_driver(binding.route.context("routed plan has no route binding")?);
            driver
                .withdraw_candidate_membership(
                    preflight,
                    &self.route_gate(&current.value.target, current.value.command_kind),
                )
                .context("restoring the route the candidate found")?;
            return self.persist_same_phase(current, |next| {
                next.post_fencing_abort.as_mut().unwrap().route_restoration =
                    CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.lease_withdrawal == CleanupEvidence::Pending {
            let expected = required(&current.value.expected, "Expected projection")?;
            let activation = required(&current.value.activation, "activation")?;
            let lease = current
                .value
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease)
                .context("post-fencing abort lost the lease it must withdraw")?;
            let binding = current.value.plan.as_ref().unwrap().parsed_inputs()?.1;
            let lease_path = binding
                .process_write_lease
                .context("leased target has no write-lease binding")?
                .record_path;
            let driver = CultCacheWriteLeaseDriver::new(&current.value.target, lease_path);
            driver
                .revoke_exact(Some(lease))
                .context("revoking the abandoned candidate write lease")?;
            ensure!(
                driver.observe_empty()?,
                "candidate write lease remained after post-fencing withdrawal"
            );
            self.topology()
                .withdraw_process_write_lease(expected, activation, Some(lease))
                .context("withdrawing the abandoned candidate write-lease projection")?;
            return self.persist_same_phase(current, |next| {
                next.post_fencing_abort.as_mut().unwrap().lease_withdrawal =
                    CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.candidate_cleanup == CleanupEvidence::Pending {
            let plan = required(&current.value.plan, "abandoned candidate plan")?;
            if let Some(workload) = &current.value.workload {
                self.workload_for(plan)?
                    .stop(workload)
                    .context("stopping the abandoned candidate")?;
            }
            self.workload_for(plan)?
                .discard_prepared(
                    required(&current.value.plan, "abandoned candidate plan")?,
                    required(&current.value.expected, "abandoned Expected projection")?,
                    required(&current.value.activation, "abandoned activation")?,
                )
                .context("discarding the abandoned activation material")?;
            return self.persist_same_phase(current, |next| {
                next.post_fencing_abort.as_mut().unwrap().candidate_cleanup =
                    CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.topology_reconciliation == CleanupEvidence::Pending {
            let snapshot = ControlSnapshot::read(&self.options.state_store)?;
            // The failed candidate's projection is resolved first. For a
            // deployment, fencing also stopped the incumbent and revoked its
            // lease, so the incumbent is demoted to Expected-only under its
            // key with its own exact activation, which is what lets
            // continuity bring it back. A continuity candidate is the
            // incumbent's key: it has nothing further to demote.
            let topology = self.topology();
            let failed_provider_anchor = self.provider_anchor_for_plan(required(
                &current.value.plan,
                "failed transaction plan",
            )?)?;
            reconcile_failed_candidate_projection(
                &topology,
                &current.value,
                &failed_provider_anchor,
            )?;
            if let Some(incumbent) = self
                .exact_incumbent(&snapshot, &current.value)?
                .filter(|_| current.value.command_kind == CommandKind::Deploy)
            {
                let admitted_provider_anchor =
                    self.provider_anchor_for_plan(&incumbent.value.plan)?;
                let expected_sha256 = topology.demote_to_expected_only(
                    &incumbent.value.expected,
                    &admitted_provider_anchor,
                    &incumbent.value.activation,
                    incumbent.value.leasing.lease(),
                )?;
                ensure!(
                    expected_sha256 == incumbent.value.expected.canonical_sha256()?,
                    "restored incumbent Expected receipt differs"
                );
            }
            return self.persist_same_phase(current, |next| {
                next.post_fencing_abort
                    .as_mut()
                    .unwrap()
                    .topology_reconciliation = CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.source_cleanup == CleanupEvidence::Pending {
            self.source
                .cleanup(
                    &current.value.transaction_id,
                    current.value.frozen_source.as_ref(),
                )
                .context("cleaning the abandoned source")?;
            return self.persist_same_phase(current, |next| {
                next.post_fencing_abort.as_mut().unwrap().source_cleanup =
                    CleanupEvidence::Complete;
                Ok(())
            });
        }
        ensure!(
            abort.is_complete(),
            "post-fencing abort cleanup is incomplete"
        );
        let mut next = current.value.clone();
        next.enter_phase(DeploymentPhase::Complete, now_millis()?);
        next.last_error = Some(abort.error.clone());
        next.completion = Some(TransactionCompletion::FailedAfterFencing {
            error: abort.error.clone(),
            recovery: TerminalRecovery::RestoreIncumbent,
        });
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn advance_pre_fencing_abort(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        ensure!(
            current.value.phase < DeploymentPhase::Fencing,
            "pre-fencing abort crossed the fencing boundary"
        );
        let abort = required(&current.value.pre_fencing_abort, "pre-fencing abort intent")?;
        if abort.candidate_cleanup == CleanupEvidence::Pending {
            let plan = required(&current.value.plan, "transaction plan")?;
            if let Some(workload) = &current.value.workload {
                self.workload_for(plan)?
                    .stop(workload)
                    .context("stopping exact pre-fence candidate")?;
            }
            self.workload_for(plan)?
                .discard_prepared(
                    required(&current.value.plan, "pre-fencing candidate plan")?,
                    required(&current.value.expected, "pre-fencing Expected projection")?,
                    required(&current.value.activation, "pre-fencing activation")?,
                )
                .context("discarding exact pre-fence activation material")?;
            return self.persist_same_phase(current, |next| {
                next.pre_fencing_abort.as_mut().unwrap().candidate_cleanup =
                    CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.topology_reconciliation == CleanupEvidence::Pending {
            // Before fencing a deployment never touched its incumbent: it is
            // still running under its own key with its activation and lease,
            // so only the candidate's records are withdrawn. A continuity
            // candidate shares the admitted key and demotes what it issued.
            let plan = required(&current.value.plan, "failed transaction plan")?;
            let provider_anchor = self.provider_anchor_for_plan(plan)?;
            reconcile_failed_candidate_projection(
                &self.topology(),
                &current.value,
                &provider_anchor,
            )?;
            return self.persist_same_phase(current, |next| {
                next.pre_fencing_abort
                    .as_mut()
                    .unwrap()
                    .topology_reconciliation = CleanupEvidence::Complete;
                Ok(())
            });
        }
        if abort.source_cleanup == CleanupEvidence::Pending {
            self.source
                .cleanup(
                    &current.value.transaction_id,
                    current.value.frozen_source.as_ref(),
                )
                .context("cleaning failed pre-fence source")?;
            return self.persist_same_phase(current, |next| {
                next.pre_fencing_abort.as_mut().unwrap().source_cleanup = CleanupEvidence::Complete;
                Ok(())
            });
        }
        ensure!(
            abort.is_complete(),
            "pre-fencing abort cleanup is incomplete"
        );
        let mut next = current.value.clone();
        next.enter_phase(DeploymentPhase::Complete, now_millis()?);
        next.last_error = Some(abort.error.clone());
        next.completion = Some(TransactionCompletion::FailedBeforeFencing {
            error: abort.error.clone(),
        });
        replace_transaction(&self.options.state_store, current, &next)
    }

    fn advance_post_commit_cleanup(&self, current: &Stored<DeploymentTransaction>) -> Result<()> {
        let cleanup = required(
            &current.value.post_commit_cleanup,
            "post-commit cleanup evidence",
        )?;
        if let IncumbentCleanupEvidence::Pending {
            generation_id,
            workload,
        } = &cleanup.incumbent
        {
            let binding = required(&current.value.plan, "committed transaction plan")?
                .parsed_inputs()?
                .1;
            if let Some(promoted_at_unix_millis) = required(
                &current.value.routing,
                "committed transaction routing evidence",
            )?
            .promoted_at_unix_millis()
            {
                let retire_not_before =
                    route_drain_deadline(promoted_at_unix_millis, binding.rollout.drain_seconds)?;
                if now_millis()? < retire_not_before {
                    return Ok(());
                }
            }
            // The incumbent is the same target under the same binding kind;
            // an incumbent of another kind is refused by the driver, not
            // guessed at.
            self.workload_for(required(&current.value.plan, "committed transaction plan")?)?
                .stop(workload)
                .with_context(|| format!("retiring admitted incumbent {generation_id}"))?;
            let generation_id = generation_id.clone();
            return self.persist_same_phase(current, |next| {
                next.post_commit_cleanup.as_mut().unwrap().incumbent =
                    IncumbentCleanupEvidence::Complete { generation_id };
                Ok(())
            });
        }
        if cleanup.source == SourceCleanupEvidence::Pending {
            self.source
                .cleanup(
                    &current.value.transaction_id,
                    current.value.frozen_source.as_ref(),
                )
                .context("cleaning committed deployment source")?;
            return self.persist_same_phase(current, |next| {
                next.post_commit_cleanup.as_mut().unwrap().source = SourceCleanupEvidence::Complete;
                Ok(())
            });
        }
        ensure!(cleanup.is_complete(), "post-commit cleanup is incomplete");
        Ok(())
    }
}

fn route_drain_deadline(promoted_at_unix_millis: u64, drain_seconds: u32) -> Result<u64> {
    promoted_at_unix_millis
        .checked_add(
            u64::from(drain_seconds)
                .checked_mul(1_000)
                .context("route drain duration overflows milliseconds")?,
        )
        .context("route drain deadline overflows Unix milliseconds")
}

/// CultLib refuses a correlation older than the trusted window with this
/// text. Idunn reads that one refusal as "nothing new", everywhere it waits
/// on Odin's evidence; every other refusal stays a refusal.
fn is_stale_observation(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("outside the trusted observation window")
}

fn route_observation_is_current(
    observed_at_unix_millis: u64,
    now_unix_millis: u64,
    maximum_age_millis: u64,
    maximum_future_skew_millis: u64,
) -> bool {
    observed_at_unix_millis <= now_unix_millis.saturating_add(maximum_future_skew_millis)
        && now_unix_millis.saturating_sub(observed_at_unix_millis) <= maximum_age_millis
}

fn provider_warming_advanced(
    prior_odin_sequence: u64,
    prior_signed_presence_sha256: &str,
    candidate_odin_sequence: u64,
    candidate_signed_presence_sha256: &str,
) -> bool {
    candidate_odin_sequence > prior_odin_sequence
        && candidate_signed_presence_sha256 != prior_signed_presence_sha256
}

/// Resolve the projection a failed candidate left. A deployment candidate owns
/// its own incarnation key and withdraws it whole. A continuity candidate
/// shares the admitted key: the admitted Expected is never withdrawn, and the
/// only thing demoted is the activation this transaction issued, exactly.
fn reconcile_failed_candidate_projection(
    topology: &CultCacheTopologyDriver,
    transaction: &DeploymentTransaction,
    provider_anchor: &ServiceIdentityTrustAnchor,
) -> Result<()> {
    let expected = required(&transaction.expected, "failed Expected projection")?;
    match transaction.command_kind {
        CommandKind::Deploy => topology
            .withdraw_incarnation(
                expected,
                provider_anchor,
                transaction.activation.as_ref(),
                None,
            )
            .context("withdrawing the failed candidate projection"),
        CommandKind::Continuity => topology
            .demote_to_expected_only(
                expected,
                provider_anchor,
                required(&transaction.activation, "failed continuity activation")?,
                None,
            )
            .map(drop)
            .context("demoting the failed continuity activation"),
    }
}

fn may_rollback_route_after_failed_proof(fencing: &FencingEvidence) -> bool {
    matches!(fencing, FencingEvidence::SkippedStateless)
}

fn incumbent_was_stopped_during_fencing(fencing: &FencingEvidence) -> bool {
    matches!(
        fencing,
        FencingEvidence::Revoked {
            incumbent_lease_sha256: Some(_),
            ..
        }
    )
}

fn pre_fencing_abort_intent(transaction: &DeploymentTransaction, error: &str) -> PreFencingAbort {
    PreFencingAbort {
        error: truncate(error, 2048),
        candidate_cleanup: candidate_cleanup_requirement(
            transaction.activation.is_some(),
            transaction.workload.is_some(),
        ),
        topology_reconciliation: transaction.abort_topology_reconciliation(),
        source_cleanup: if transaction.command_kind == CommandKind::Deploy {
            CleanupEvidence::Pending
        } else {
            CleanupEvidence::Skipped
        },
    }
}

fn post_fencing_abort_intent(
    transaction: &DeploymentTransaction,
    error: &str,
) -> PostFencingAbort {
    PostFencingAbort {
        error: truncate(error, 2048),
        route_restoration: if transaction.route_preflight.is_some() {
            CleanupEvidence::Pending
        } else {
            CleanupEvidence::Skipped
        },
        lease_withdrawal: if transaction
            .leasing
            .as_ref()
            .and_then(LeasingEvidence::lease)
            .is_some()
        {
            CleanupEvidence::Pending
        } else {
            CleanupEvidence::Skipped
        },
        candidate_cleanup: candidate_cleanup_requirement(
            transaction.activation.is_some(),
            transaction.workload.is_some(),
        ),
        topology_reconciliation: transaction.abort_topology_reconciliation(),
        source_cleanup: if transaction.command_kind == CommandKind::Deploy {
            CleanupEvidence::Pending
        } else {
            CleanupEvidence::Skipped
        },
    }
}

fn candidate_cleanup_requirement(
    has_prepared_activation: bool,
    has_workload_observation: bool,
) -> CleanupEvidence {
    if has_prepared_activation || has_workload_observation {
        CleanupEvidence::Pending
    } else {
        CleanupEvidence::Skipped
    }
}

fn sequence_requires_admission(
    latest: Option<&TopologyEvidence>,
    maximum_admitted_sequence: u64,
    candidate: &TopologyEvidence,
) -> Result<bool> {
    candidate.validate_shape()?;
    if latest.is_some_and(|existing| {
        existing.canonical_bytes == candidate.canonical_bytes
            && existing.canonical_sha256 == candidate.canonical_sha256
            && existing.signer_identity_id == candidate.signer_identity_id
            && existing.publisher_sequence == candidate.publisher_sequence
    }) {
        return Ok(false);
    }
    ensure!(
        candidate.publisher_sequence > maximum_admitted_sequence,
        "Odin topology publisher sequence was replayed or reordered"
    );
    Ok(true)
}

fn validate_live_providers_for_deploy<F>(command_kind: CommandKind, validation: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    match command_kind {
        CommandKind::Deploy => validation(),
        CommandKind::Continuity => Ok(()),
    }
}

fn validate_authenticated_evidence(
    evidence: &TopologyEvidence,
    authenticated: &AuthenticatedOdinRuntimeTopologyCorrelation,
) -> Result<()> {
    let record = authenticated.record();
    ensure!(
        authenticated.canonical_bytes() == evidence.canonical_bytes.as_slice()
            && sha256_id(authenticated.canonical_bytes()) == evidence.canonical_sha256
            && record.signer_identity_id == evidence.signer_identity_id
            && record.publisher_sequence == evidence.publisher_sequence,
        "durable topology evidence differs from its authenticated receipt"
    );
    Ok(())
}

fn is_semantic_warming(
    expected: &IdunnExpectedIncarnationRecord,
    activation: &IdunnRuntimeActivationRecord,
    incumbent_lease_sha256: Option<&str>,
    authenticated: &AuthenticatedOdinRuntimeTopologyCorrelation,
) -> Result<bool> {
    let record = authenticated.record();
    if !record.present || record.observed_write_lease_sha256.is_some() {
        return Ok(false);
    }
    let expected_projection_detail = format!(
        "expected:{};activation:{}",
        expected.canonical_sha256()?,
        activation.canonical_sha256()?
    );
    if !warming_disagreements_match_incumbent(
        &expected_projection_detail,
        incumbent_lease_sha256,
        &record.disagreements,
    ) {
        return Ok(false);
    }
    Ok(if expected.write_lease_required {
        !record.ready && record.observed_presence_state.as_deref() == Some("warming")
    } else {
        matches!(
            record.observed_presence_state.as_deref(),
            Some("warming" | "active")
        )
    })
}

fn warming_disagreements_match_incumbent(
    expected_projection_detail: &str,
    incumbent_lease_sha256: Option<&str>,
    disagreements: &[OdinTopologyDisagreement],
) -> bool {
    if disagreements.is_empty() {
        true
    } else if let (Some(incumbent_lease_sha256), [disagreement]) =
        (incumbent_lease_sha256, disagreements)
    {
        disagreement.code == "projected-write-lease"
            && disagreement.expected.as_deref() == Some(expected_projection_detail)
            && disagreement.observed.as_deref() == Some(incumbent_lease_sha256)
    } else {
        false
    }
}

fn is_semantic_ready(authenticated: &AuthenticatedOdinRuntimeTopologyCorrelation) -> bool {
    let record = authenticated.record();
    record.present
        && record.ready
        && record.observed_presence_state.as_deref() == Some("active")
        && record.disagreements.is_empty()
}

fn read_deployment_brake(path: &Path) -> Result<Option<(IdunnDeploymentBrakeRecord, Vec<u8>)>> {
    let Some(envelope) = read_single_envelope(path)? else {
        return Ok(None);
    };
    ensure!(
        envelope.r#type == IdunnDeploymentBrakeRecord::TYPE
            && envelope.schema_id.as_deref() == Some(IDUNN_DEPLOYMENT_BRAKE_SCHEMA),
        "deployment brake store contains a foreign record"
    );
    let record: IdunnDeploymentBrakeRecord = rmp_serde::from_slice(&envelope.payload)?;
    record.validate()?;
    ensure!(
        rmp_serde::to_vec(&record)? == envelope.payload && envelope.key == record.brake_id,
        "deployment brake is noncanonical or keyed by another authority"
    );
    Ok(Some((record, envelope.payload)))
}

fn read_lifecycle_brake(path: &Path) -> Result<Option<IdunnLifecycleBrakeRecord>> {
    let Some(envelope) = read_single_envelope(path)? else {
        return Ok(None);
    };
    ensure!(
        envelope.r#type == IdunnLifecycleBrakeRecord::TYPE
            && envelope.schema_id.as_deref() == Some(IDUNN_LIFECYCLE_BRAKE_SCHEMA),
        "lifecycle brake store contains a foreign record"
    );
    IdunnLifecycleBrakeRecord::decode_canonical(&envelope.payload).map(Some)
}

fn read_single_envelope(path: &Path) -> Result<Option<CultCacheEnvelope>> {
    if !path.exists() {
        return Ok(None);
    }
    let entries = SingleFileMessagePackBackingStore::new(path).pull_all_read_only_snapshot()?;
    match entries.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some(entry.clone())),
        _ => bail!("single-record authority store is ambiguous"),
    }
}

pub(crate) fn read_trust_anchor<P: ServiceIdentityProfile>(
    path: &Path,
) -> Result<ServiceIdentityTrustAnchor> {
    let envelope = read_single_envelope(path)?
        .with_context(|| format!("service identity trust anchor {} is absent", path.display()))?;
    ensure!(
        envelope.r#type == P::TRUST_ANCHOR_TYPE
            && envelope.key == P::TRUST_ANCHOR_KEY
            && envelope.schema_id.as_deref() == Some(P::TRUST_ANCHOR_SCHEMA),
        "service identity trust anchor belongs to another profile"
    );
    let anchor: ServiceIdentityTrustAnchor = rmp_serde::from_slice(&envelope.payload)?;
    ensure!(
        rmp_serde::to_vec(&anchor)? == envelope.payload
            && derive_service_identity_id::<P>(&anchor.public_key)? == anchor.identity_id,
        "service identity trust anchor is noncanonical or self-inconsistent"
    );
    Ok(anchor)
}

fn required<'a, T>(value: &'a Option<T>, label: &str) -> Result<&'a T> {
    value.as_ref().with_context(|| format!("{label} is absent"))
}

fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock predates Unix epoch")?
        .as_millis()
        .try_into()?)
}

fn rfc3339_millis(millis: u64) -> Result<String> {
    chrono::DateTime::from_timestamp_millis(i64::try_from(millis)?)
        .context("timestamp is out of range")
        .map(|value| value.to_rfc3339())
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn string_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{name} requires a value"))
}

fn path_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(string_value(args, name)?))
}

fn u32_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<u32> {
    string_value(args, name)?
        .parse()
        .with_context(|| format!("{name} requires a u32"))
}

fn u64_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<u64> {
    string_value(args, name)?
        .parse()
        .with_context(|| format!("{name} requires a u64"))
}

fn require_selector(value: &str) -> Result<()> {
    let value = value.strip_prefix("profile:").unwrap_or(value);
    require_id(value, "deployment selector")
}

fn require_id(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            }),
        "{label} is invalid"
    );
    Ok(())
}

fn require_value(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty()
            && value == value.trim()
            && value.len() <= 1024
            && !value.contains('\0'),
        "{label} is invalid"
    );
    Ok(())
}

fn require_detail(value: &str, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 2048 && !value.contains('\0'),
        "{label} is invalid"
    );
    Ok(())
}

fn truncate(value: &str, length: usize) -> String {
    value.chars().take(length).collect()
}

fn sha256_id(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256-");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn runtime_instance_id(transaction_id: &str) -> Result<String> {
    require_id(transaction_id, "runtime transaction id")?;
    Ok(sha256_id(
        format!("gamecult.idunn.runtime-instance.v1:{transaction_id}").as_bytes(),
    ))
}

fn usage() -> &'static str {
    "Idunn deployment, admission, and continuity control plane\n\n\
     idunn serve [runtime options] [--host-actuator-bind ADDR]\n\
     idunn up <service|profile:name> [--state-store PATH] [--no-wait]\n\
     idunn status [--state-store PATH] [--command ID]\n\
     idunn cancel <command-id> [--state-store PATH]\n\
     idunn validate --recipe PATH [--binding PATH]\n\n\
     Recipes describe capability and process requirements. Idunn seals exact\n\
     source and artifacts, admits one incarnation, and delegates execution to\n\
     systemd and routing mechanics to the configured proxy."
}

#[cfg(test)]
mod tests {
    use cultnet_rs::{
        GameCultProviderHealthIdentity, OdinRuntimeTopologyCorrelationPurpose,
        OdinRuntimeTopologyCorrelationRecord, enroll_service_identity_at,
        export_service_identity_trust_anchor,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::drivers::{SystemdWorkloadObservation, incarnation_key};

    /// Fixed clock for the signing fixture; correlations must land inside
    /// DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS of it to authenticate.
    const NOW: u64 = 1_700_000_000_000;

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn route_binding(
        route_id: &str,
        stable_port: u16,
        private_port_start: u16,
        private_port_end: u16,
    ) -> RouteBinding {
        RouteBinding {
            driver: crate::deployment::RouteDriver::NginxStreamTcp,
            route_id: route_id.into(),
            stable_endpoint: format!("tcp://127.0.0.1:{stable_port}"),
            private_host: "127.0.0.1".into(),
            private_port_start,
            private_port_end,
            config_path: PathBuf::from(format!("/etc/nginx/idunn-stream-routes/{route_id}.conf")),
            reload_unit: "nginx.service".into(),
        }
    }

    fn command(kind: CommandKind) -> DeploymentCommand {
        DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: match kind {
                CommandKind::Deploy => "up-test".into(),
                CommandKind::Continuity => "continuity-test".into(),
            },
            kind,
            selector: "ghostlight".into(),
            requested_by: "operator".into(),
            requested_at_unix_millis: 100,
        }
    }

    /// Drives the replacement gate against the real `control.cc`. The point is
    /// not that the unit tests pass but that the record actually stuck on the
    /// host would now commit. Run with:
    ///   IDUNN_LIVE_CONTROL=/path/to/control.cc cargo test live_committing -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_committing_record_is_unstuck_by_the_replacement_gate() -> Result<()> {
        let Ok(path) = std::env::var("IDUNN_LIVE_CONTROL") else {
            eprintln!("IDUNN_LIVE_CONTROL unset");
            return Ok(());
        };
        let snapshot = ControlSnapshot::read(Path::new(&path))?;
        for stored in &snapshot.transactions {
            let value = &stored.value;
            if value.completion.is_some() {
                continue;
            }
            let (Some(ready), Some(latest)) = (
                value.ready.as_ref().and_then(ReadinessEvidence::odin),
                &value.latest_odin_observation,
            ) else {
                continue;
            };
            println!(
                "LIVE {} target={} phase={:?} ready_seq={} latest_seq={}",
                value.transaction_id,
                value.target,
                value.phase,
                ready.publisher_sequence,
                latest.publisher_sequence
            );
            println!(
                "  old gate (receipt == cursor): {}",
                if ready == latest { "PASS" } else { "REFUSE" }
            );
            let (latest_record, _) =
                OdinRuntimeTopologyCorrelationRecord::decode_canonical_signed_payload(
                    &latest.canonical_bytes,
                )?;
            println!(
                "  cursor says: projection={} instance={:?} ready={} present={} disagreements={}",
                latest_record.expected_projection_sha256,
                latest_record.runtime_instance_id,
                latest_record.ready,
                latest_record.present,
                latest_record.disagreements.len()
            );
            let (Some(expected), Some(activation)) = (&value.expected, &value.activation) else {
                println!("  transaction has not declared an incarnation yet");
                continue;
            };
            println!(
                "  declared:   projection={} instance={}",
                expected.canonical_sha256()?,
                activation.runtime_instance_id
            );
            let binds = latest_record.expected_projection_sha256 == expected.canonical_sha256()?
                && latest_record.runtime_instance_id.as_deref()
                    == Some(activation.runtime_instance_id.as_str());
            println!("  binds declared incarnation (what authentication enforces): {binds}");
        }
        Ok(())
    }

    /// The smallest real `Engine` the scheduler needs. Idunn had no
    /// Engine-level test at all, which is why "does one bad record kill the
    /// daemon" was arguable rather than measurable.
    struct EngineFixture {
        _temp: TempDir,
        engine: Engine,
        state_store: PathBuf,
        odin_signer: ServiceIdentitySigner<OdinTopologyIdentity>,
        root: PathBuf,
        /// The host programs every route driver of this Engine actuates
        /// through: logged stubs, so nothing touches the workstation's nginx.
        #[cfg(unix)]
        route_stubs: RouteStubs,
    }

    /// Stub nginx, systemd-run, systemctl and ufw that append each call to one
    /// log. `reload-fails` makes `systemctl reload` exit non-zero.
    #[cfg(unix)]
    struct RouteStubs {
        calls: PathBuf,
        reload_fails: PathBuf,
    }

    #[cfg(unix)]
    impl RouteStubs {
        fn new(root: &Path) -> Result<(RouteActuators, Self)> {
            use std::os::unix::fs::PermissionsExt;

            let dir = root.join("route-stubs");
            std::fs::create_dir_all(&dir)?;
            let calls = dir.join("calls");
            let reload_fails = dir.join("reload-fails");
            let program = |name: &str, refuse: &str| -> Result<PathBuf> {
                let path = dir.join(name);
                std::fs::write(
                    &path,
                    format!(
                        "#!/bin/sh\necho \"{name} $*\" >> '{}'\n{refuse}exit 0\n",
                        calls.display()
                    ),
                )?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
                Ok(path)
            };
            let actuators = RouteActuators {
                nginx: program("nginx", "")?,
                systemd_run: program("systemd-run", "")?,
                systemctl: program(
                    "systemctl",
                    &format!(
                        "if [ \"$1\" = reload ] && [ -e '{}' ]; then exit 1; fi\n",
                        reload_fails.display()
                    ),
                )?,
                ufw: program("ufw", "")?,
                preflight_root: root.join("route-preflight"),
            };
            Ok((actuators, Self { calls, reload_fails }))
        }

        fn refuse_reloads(&self) -> Result<()> {
            std::fs::write(&self.reload_fails, b"x")?;
            Ok(())
        }

        /// How many logged calls begin with `prefix`, e.g. `systemctl reload`.
        fn count(&self, prefix: &str) -> usize {
            std::fs::read_to_string(&self.calls)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with(prefix))
                .count()
        }
    }

    impl EngineFixture {
        fn new() -> Result<Self> {
            Self::build(None)
        }

        /// An Engine whose systemd workload port is `workload`, so the phase
        /// machine can run without systemd. This is the whole seam: nginx,
        /// systemctl and the network are never reached on an unrouted,
        /// stateless target, and the Odin evidence is signed by the key this
        /// fixture enrolled.
        fn with_workload(workload: Arc<dyn WorkloadPort>) -> Result<Self> {
            Self::build(Some(workload))
        }

        fn build(workload: Option<Arc<dyn WorkloadPort>>) -> Result<Self> {
            let temp = TempDir::new()?;
            let root = temp.path();
            std::fs::create_dir_all(root.join("identities"))?;
            let idunn = root.join("identities/idunn.cc");
            enroll_service_identity_at::<IdunnServiceIdentity>(&idunn)?;
            let odin_private = root.join("identities/odin.cc");
            let odin_signer = enroll_service_identity_at::<OdinTopologyIdentity>(&odin_private)?;
            let odin_anchor = root.join("identities/odin-anchor.cc");
            export_service_identity_trust_anchor(&odin_signer, &odin_anchor)?;

            let state_store = root.join("control.cc");
            #[cfg(unix)]
            let (route_actuators, route_stubs) = RouteStubs::new(root)?;
            let options = RuntimeOptions {
                state_store: state_store.clone(),
                bindings_dir: root.join("bindings"),
                source_root: root.join("sources"),
                staging_root: root.join("staging"),
                topology_store: root.join("topology.cc"),
                odin_correlation_store: root.join("odin-correlation.cc"),
                odin_trust_anchor: odin_anchor,
                idunn_identity_store: idunn,
                deployment_brake_operator_anchor: root.join("brake-anchor.cc"),
                #[cfg(unix)]
                route_actuators,
                ..RuntimeOptions::default()
            };
            let engine = match workload {
                Some(workload) => Engine::open_with_systemd_workload(options, workload)?,
                None => Engine::open(options)?,
            };
            Ok(Self {
                root: root.to_path_buf(),
                _temp: temp,
                engine,
                state_store,
                odin_signer,
                #[cfg(unix)]
                route_stubs,
            })
        }
    }

    fn terminal_transaction(target: &str) -> Result<DeploymentTransaction> {
        Ok(terminal_transaction_with_command(target)?.0)
    }

    fn terminal_transaction_with_command(
        target: &str,
    ) -> Result<(DeploymentTransaction, DeploymentCommand)> {
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: format!("up-{target}"),
            kind: CommandKind::Deploy,
            selector: target.into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        let mut transaction = DeploymentTransaction::new(&command, target.into(), 0, None, 100)?;
        transaction.phase = DeploymentPhase::Complete;
        transaction.completion = Some(TransactionCompletion::FailedBeforeFencing {
            error: "sealed source vanished".into(),
        });
        transaction.pre_fencing_abort = Some(PreFencingAbort {
            error: "sealed source vanished".into(),
            candidate_cleanup: CleanupEvidence::Skipped,
            topology_reconciliation: CleanupEvidence::Skipped,
            source_cleanup: CleanupEvidence::Complete,
        });
        assert!(transaction.is_terminal(), "fixture must be terminal");
        Ok((transaction, command))
    }

    #[test]
    fn a_finished_transaction_leaves_the_live_set_and_stays_observable() -> Result<()> {
        let world = EngineFixture::new()?;
        let history = history_store_path(&world.state_store);

        let (live, command) = terminal_transaction_with_command("ghostlight")?;
        // A transaction is only readable beside its immutable command.
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command.command_id.clone(),
                    current: None,
                }],
                &[command_envelope(
                    &command,
                    command.requested_at_unix_millis
                )?],
            )?
        );
        let mut opening = live.clone();
        opening.phase = DeploymentPhase::Sealing;
        opening.completion = None;
        opening.pre_fencing_abort = None;
        let opening_envelope = transaction_envelope(&opening, opening.updated_at_unix_millis)?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: opening.transaction_id.clone(),
                    current: None,
                }],
                std::slice::from_ref(&opening_envelope),
            )?
        );
        let stored = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .next()
            .context("seeded transaction")?;

        replace_transaction(&world.state_store, &stored, &live)?;

        // Gone from the set that still gates decisions...
        assert!(
            ControlSnapshot::read(&world.state_store)?
                .transactions
                .is_empty(),
            "a finished transaction stayed resident"
        );
        // ...and still answerable, which is what R11 refused to trade away.
        let archived = read_history_transactions(&world.state_store);
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].transaction_id, live.transaction_id);
        assert!(history.exists());
        // The command went with it. Left resident it would read as queued and
        // be frozen again next tick; consumed, it is history beside its
        // transaction and `status` still answers for it.
        assert!(
            ControlSnapshot::read(&world.state_store)?
                .commands
                .is_empty(),
            "a consumed command stayed resident"
        );
        let archived_commands = read_history_commands(&world.state_store);
        assert_eq!(archived_commands.len(), 1);
        assert_eq!(archived_commands[0].command_id, command.command_id);
        assert!(!world.engine.freeze_one_queued_command()?);
        Ok(())
    }

    #[test]
    fn a_resident_command_whose_transactions_are_history_is_retired_not_frozen() -> Result<()> {
        // Before commands travelled to history with their transactions, a
        // command outlived its retired transaction in the control store and
        // read as queued again. Freeze must retire it, never re-freeze it.
        let world = EngineFixture::new()?;
        let (finished, command) = terminal_transaction_with_command("ghostlight")?;
        SingleFileMessagePackBackingStore::new(&world.state_store).insert_entry_if_absent(
            command_envelope(&command, command.requested_at_unix_millis)?,
        )?;
        SingleFileMessagePackBackingStore::new(&history_store_path(&world.state_store))
            .insert_entry_if_absent(transaction_envelope(
                &finished,
                finished.updated_at_unix_millis,
            )?)?;
        assert_eq!(ControlSnapshot::read(&world.state_store)?.commands.len(), 1);

        assert!(world.engine.freeze_one_queued_command()?);

        assert!(
            ControlSnapshot::read(&world.state_store)?
                .commands
                .is_empty(),
            "consumed command stayed resident"
        );
        assert_eq!(read_history_commands(&world.state_store).len(), 1);
        assert!(
            ControlSnapshot::read(&world.state_store)?
                .transactions
                .is_empty(),
            "a consumed command was frozen again"
        );
        assert!(!world.engine.freeze_one_queued_command()?);
        Ok(())
    }

    #[test]
    fn a_resident_terminal_transaction_is_retired_by_the_tick_with_its_command() -> Result<()> {
        let world = EngineFixture::new()?;
        let (finished, command) = terminal_transaction_with_command("ghostlight")?;
        SingleFileMessagePackBackingStore::new(&world.state_store).insert_entry_if_absent(
            command_envelope(&command, command.requested_at_unix_millis)?,
        )?;
        SingleFileMessagePackBackingStore::new(&world.state_store).insert_entry_if_absent(
            transaction_envelope(&finished, finished.updated_at_unix_millis)?,
        )?;
        assert!(world.engine.retire_one_terminal_transaction()?);
        let live = ControlSnapshot::read(&world.state_store)?;
        assert!(live.transactions.is_empty() && live.commands.is_empty());
        assert_eq!(read_history_transactions(&world.state_store).len(), 1);
        assert_eq!(read_history_commands(&world.state_store).len(), 1);
        assert!(!world.engine.retire_one_terminal_transaction()?);
        Ok(())
    }

    #[test]
    fn re_archiving_after_a_crash_window_is_a_no_op() -> Result<()> {
        // History is written before the live copy is removed, so a crash
        // between the two leaves the record in both. The repeat must not
        // duplicate it or fail.
        let world = EngineFixture::new()?;
        let live = terminal_transaction("odin")?;
        let envelope = transaction_envelope(&live, live.updated_at_unix_millis)?;

        archive_terminal_transaction(&world.state_store, &envelope)?;
        archive_terminal_transaction(&world.state_store, &envelope)?;

        let archived = read_history_transactions(&world.state_store);
        assert_eq!(archived.len(), 1, "the repeat duplicated the record");
        Ok(())
    }

    #[test]
    fn an_unreadable_history_entry_is_skipped_rather_than_fatal() -> Result<()> {
        // The asymmetry that justifies two files: control.cc refuses a record
        // it cannot re-encode byte for byte, because those records still gate
        // decisions. History describes what already happened and must never be
        // able to refuse a read.
        let world = EngineFixture::new()?;
        let live = terminal_transaction("voidbot")?;
        let envelope = transaction_envelope(&live, live.updated_at_unix_millis)?;
        archive_terminal_transaction(&world.state_store, &envelope)?;

        let history = history_store_path(&world.state_store);
        assert!(
            SingleFileMessagePackBackingStore::new(&history).insert_entry_if_absent(
                CultCacheEnvelope {
                    key: "tx-corrupt".into(),
                    r#type: DeploymentTransaction::TYPE.into(),
                    payload: vec![0xc1],
                    stored_at: rfc3339_millis(1)?,
                    schema_id: Some(DEPLOYMENT_TRANSACTION_SCHEMA.into()),
                }
            )?
        );

        let archived = read_history_transactions(&world.state_store);
        assert_eq!(archived.len(), 1, "the readable record must survive");
        assert_eq!(archived[0].transaction_id, live.transaction_id);
        Ok(())
    }

    #[test]
    fn an_absent_brake_anchor_does_not_gate_an_empty_store() -> Result<()> {
        // `F:\Projects\CLAUDE.md`: a target brake "may never gate Idunn itself
        // or unrelated service lifecycle". Reading the operator anchor up front
        // meant a missing brake artifact refused startup against a store with
        // nothing in it -- the deployment authority held hostage by an artifact
        // describing one target's consent.
        let world = EngineFixture::new()?;
        assert!(
            !world
                .engine
                .options
                .deployment_brake_operator_anchor
                .exists(),
            "fixture must not have written a brake anchor"
        );
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        world.engine.validate_durable_authority(&snapshot)?;
        Ok(())
    }

    #[test]
    fn one_unreadable_record_faults_its_tick_without_touching_the_store() -> Result<()> {
        let world = EngineFixture::new()?;

        // An empty store schedules nothing and must not fault.
        assert!(!world.engine.run_scheduler_tick()?);

        // A record the control store cannot read. Idunn runs under
        // Restart=always, so before run_scheduler_tick existed this
        // propagated out of the loop and crashlooped the daemon -- taking
        // every unrelated target's continuity with it.
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: "tx-unreadable".into(),
                    current: None,
                }],
                &[CultCacheEnvelope {
                    key: "tx-unreadable".into(),
                    r#type: DeploymentTransaction::TYPE.into(),
                    // 0xc1 is msgpack's never-used byte: decodable by nothing.
                    payload: vec![0xc1],
                    stored_at: rfc3339_millis(1)?,
                    schema_id: Some(DEPLOYMENT_TRANSACTION_SCHEMA.into()),
                }],
            )?
        );
        let poisoned = std::fs::read(&world.state_store)?;

        for _ in 0..3 {
            assert!(
                world.engine.run_scheduler_tick().is_err(),
                "the tick must report the fault rather than swallow it"
            );
        }

        // Faulting is not the same as flailing: a tick that cannot read must
        // not rewrite, truncate or repair the store behind the operator.
        assert_eq!(
            std::fs::read(&world.state_store)?,
            poisoned,
            "a faulting tick rewrote the control store"
        );
        Ok(())
    }

    /// A real signing chain for topology gates. Until this existed no test in
    /// this file could construct a signed Odin correlation -- every topology
    /// fixture was `vec![byte]` -- so every gate between Idunn and Odin was
    /// asserted against bytes that could not be authenticated. That is how a
    /// comparison against Idunn's own wall clock survived in the admission
    /// path. Modelled on `odin-daemon`'s TestWorld, which already had to build
    /// this chain to test the other side of the same contract.
    struct TopologyFixture {
        _temp: TempDir,
        idunn_anchor: ServiceIdentityTrustAnchor,
        odin_signer: ServiceIdentitySigner<OdinTopologyIdentity>,
        provider_public_key: Vec<u8>,
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
    }

    impl TopologyFixture {
        fn new(target: &str) -> Result<Self> {
            let temp = TempDir::new()?;
            let root = temp.path().join("identities");
            std::fs::create_dir_all(&root)?;
            let idunn_signer =
                enroll_service_identity_at::<IdunnServiceIdentity>(&root.join("idunn.cc"))?;
            let idunn_anchor = idunn_signer.trust_anchor()?;
            let odin_signer =
                enroll_service_identity_at::<OdinTopologyIdentity>(&root.join("odin.cc"))?;
            let provider_signer = enroll_service_identity_at::<GameCultProviderHealthIdentity>(
                &root.join("provider.cc"),
            )?;

            let expected = IdunnExpectedIncarnationRecord {
                schema_version: cultnet_rs::IDUNN_EXPECTED_INCARNATION_SCHEMA.into(),
                target: target.into(),
                plan_id: sha256_id(b"plan"),
                incarnation_id: format!("{target}/generation-1"),
                sealed_release_id: sha256_id(b"release"),
                source_repository: format!("github.com/GameCult/{target}"),
                source_revision: "3".repeat(40),
                recipe_sha256: sha256_id(b"recipe"),
                runtime_id: format!("{target}-runtime"),
                expected_signer_identity_id: provider_signer.entry().identity_id.clone(),
                health_contract: format!("{target}.runtime-health.v1"),
                artifact_sha256: sha256_id(b"artifact"),
                state_schema_generation: None,
                state_contract_sha256: None,
                write_lease_required: false,
                route: None,
                capabilities: Vec::new(),
                dependencies: Vec::new(),
            };
            expected.validate()?;

            // Idunn-signed, exactly as the real activation path issues it: the
            // authority chain is what makes a correlation authenticable at all.
            let launch = IdunnRuntimeActivationLaunch::issue(
                &expected,
                sha256_id(b"artifact-witness"),
                NOW - 20,
                &idunn_signer,
            )?;
            let activation = launch.activation().clone();

            Ok(Self {
                _temp: temp,
                idunn_anchor,
                odin_signer,
                provider_public_key: provider_signer.entry().public_key.clone(),
                expected,
                activation,
            })
        }

        fn authority(&self) -> Result<cultnet_rs::VerifiedRuntimeAuthority> {
            verify_runtime_authority(
                &self.expected,
                &self.activation,
                &self.idunn_anchor,
                &self.provider_public_key,
            )
        }

        fn correlation(
            &self,
            sequence: u64,
            ready: bool,
        ) -> Result<OdinRuntimeTopologyCorrelationRecord> {
            Ok(OdinRuntimeTopologyCorrelationRecord {
                schema_version: cultnet_rs::ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into(),
                target: self.expected.target.clone(),
                expected_projection_sha256: self.expected.canonical_sha256()?,
                expected: true,
                current_activation_sha256: Some(self.activation.canonical_sha256()?),
                signed_presence_sha256: Some(sha256_id(b"presence")),
                observed_presence_state: Some("active".into()),
                observed_presence_publisher_sequence: Some(sequence),
                observed_write_lease_sha256: None,
                observed_capabilities: Vec::new(),
                runtime_id: self.expected.runtime_id.clone(),
                runtime_instance_id: Some(self.activation.runtime_instance_id.clone()),
                present: true,
                ready,
                dependencies: Vec::new(),
                disagreements: Vec::new(),
                signer_identity_id: self.odin_signer.entry().identity_id.clone(),
                publisher_sequence: sequence,
                observed_at_unix_millis: NOW,
                signature_algorithm: "ed25519".into(),
                signature: Vec::new(),
            })
        }

        fn sign(&self, record: &mut OdinRuntimeTopologyCorrelationRecord) -> Result<Vec<u8>> {
            record.signature = self
                .odin_signer
                .sign::<OdinRuntimeTopologyCorrelationPurpose>(
                    &record.unsigned_signature_payload()?,
                )
                .signature;
            record.canonical_bytes()
        }

        fn authenticate(
            &self,
            canonical: &[u8],
        ) -> Result<cultnet_rs::AuthenticatedOdinRuntimeTopologyCorrelation> {
            authenticate_odin_runtime_topology_correlation(
                canonical,
                &self.authority()?,
                None,
                &self.odin_signer.entry().public_key,
                OdinTopologyAuthenticationContext {
                    trusted_received_at_unix_millis: NOW,
                    maximum_age_millis: DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS,
                    maximum_future_skew_millis: DEFAULT_TOPOLOGY_MAXIMUM_FUTURE_SKEW_MILLIS,
                },
            )
        }
    }

    #[test]
    fn the_fixture_actually_authenticates() -> Result<()> {
        // First: prove the chain can REFUSE. Without this the tests below
        // establish nothing -- a fixture that authenticates anything would
        // make every gate look satisfied.
        let world = TopologyFixture::new("odin")?;
        let mut record = world.correlation(297_765, true)?;
        let canonical = world.sign(&mut record)?;
        world.authenticate(&canonical)?;

        // Re-encoded with the old signature over changed content.
        let mut forged = record.clone();
        forged.publisher_sequence = 297_766;
        assert!(world.authenticate(&forged.canonical_bytes()?).is_err());

        let mut unsigned = world.correlation(297_765, true)?;
        unsigned.signature = vec![0; 64];
        assert!(world.authenticate(&unsigned.canonical_bytes()?).is_err());

        // Stale beyond the trusted observation window.
        let mut old = world.correlation(297_765, true)?;
        old.observed_at_unix_millis = NOW - DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS - 1;
        let old_canonical = world.sign(&mut old)?;
        assert!(world.authenticate(&old_canonical).is_err());
        Ok(())
    }

    #[test]
    fn a_signed_observation_far_past_the_receipt_admits_this_incarnation() -> Result<()> {
        // The yggdrasil wedge, with a real signature rather than vec![byte]:
        // the cursor has run hundreds of thousands of sequences past the Ready
        // receipt and still describes the incarnation the transaction declared.
        let world = TopologyFixture::new("odin")?;
        let mut record = world.correlation(501_713, true)?;
        let canonical = world.sign(&mut record)?;
        let authenticated = world.authenticate(&canonical)?;

        // Authenticating at all is the incarnation check: the authority is
        // built from the declared expected and activation, and the correlation
        // had to bind both to get here. Nothing further to compare.
        assert!(is_semantic_ready(&authenticated));
        assert_eq!(authenticated.record().publisher_sequence, 501_713);
        Ok(())
    }

    #[test]
    fn authentication_binds_a_correlation_to_the_declared_incarnation() -> Result<()> {
        // The failure that matters: Odin's signature is valid and the record is
        // fresh, but the process it describes is not the one we activated.
        // Authentication already refuses this -- the authority is built from
        // this transaction's own expected and activation, and the correlation
        // must bind both. Pinned here because a redundant identity gate sat on
        // top of this for want of a test that could reach it.
        let world = TopologyFixture::new("odin")?;
        let mut restarted = world.correlation(501_713, true)?;
        restarted.runtime_instance_id = Some(sha256_id(b"another-instance"));
        let canonical = world.sign(&mut restarted)?;
        let error = world
            .authenticate(&canonical)
            .expect_err("correlation must bind the current activation");
        assert!(
            format!("{error:#}").contains("does not bind the current activation"),
            "{error:#}"
        );

        // Same for a correlation about another Expected projection: the
        // authority carries this transaction's declaration, so a correlation
        // that is not about it cannot authenticate against it.
        let mut reprojected = world.correlation(501_714, true)?;
        reprojected.expected_projection_sha256 = sha256_id(b"another-projection");
        let canonical = world.sign(&mut reprojected)?;
        assert!(world.authenticate(&canonical).is_err());
        Ok(())
    }

    #[test]
    fn a_signed_observation_that_is_not_ready_does_not_admit() -> Result<()> {
        let world = TopologyFixture::new("odin")?;
        let mut degraded = world.correlation(501_713, false)?;
        let canonical = world.sign(&mut degraded)?;
        assert!(!is_semantic_ready(&world.authenticate(&canonical)?));

        // Ready alongside a disagreement is not merely "not ready" -- the
        // correlation contract refuses to encode it at all, so a publisher
        // cannot assert readiness over its own dissent. Worth pinning: it is
        // why is_semantic_ready's disagreement clause is belt to the schema's
        // braces rather than the only thing standing there.
        let mut disputed = world.correlation(501_714, true)?;
        disputed.disagreements.push(OdinTopologyDisagreement {
            code: "route-membership-differs".into(),
            expected: Some("a".into()),
            observed: Some("b".into()),
        });
        assert!(world.sign(&mut disputed).is_err());
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Admission audit (soul/admission-audit). Each test below is a negative
    // check on one claim made by c09993c..8448f83; none asserts that the
    // happy path works, the tests above already do that.
    // ---------------------------------------------------------------------

    impl TopologyFixture {
        /// Authenticate with an explicit trusted time, against this fixture's
        /// own authority and Odin key. Mirrors `rehydrate_ready_token`'s two
        /// callers: `now` when require_current, `admitted_at` otherwise.
        fn authenticate_at(
            &self,
            canonical: &[u8],
            trusted_received_at_unix_millis: u64,
        ) -> Result<cultnet_rs::AuthenticatedOdinRuntimeTopologyCorrelation> {
            authenticate_odin_runtime_topology_correlation(
                canonical,
                &self.authority()?,
                None,
                &self.odin_signer.entry().public_key,
                OdinTopologyAuthenticationContext {
                    trusted_received_at_unix_millis,
                    maximum_age_millis: DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS,
                    maximum_future_skew_millis: DEFAULT_TOPOLOGY_MAXIMUM_FUTURE_SKEW_MILLIS,
                },
            )
        }
    }

    /// Claim 1: deleting `observation_describes_declared_incarnation` opened
    /// nothing because authentication binds the same identity. The strongest
    /// attack is a correlation that is internally perfect for ANOTHER
    /// incarnation of the same target -- its own projection, its own activation
    /// digest, its own instance id -- signed by OUR admitted Odin, so the only
    /// thing left to refuse it is the incarnation binding itself.
    #[test]
    fn a_perfect_correlation_about_another_incarnation_of_this_target_is_refused() -> Result<()> {
        let ours = TopologyFixture::new("odin")?;
        let theirs = TopologyFixture::new("odin")?;
        assert_ne!(
            ours.expected.canonical_sha256()?,
            theirs.expected.canonical_sha256()?,
            "fixtures must declare distinct incarnations for this test to mean anything"
        );
        // Sanity: it is a real, Ready correlation for the other incarnation.
        let mut genuine = theirs.correlation(501_713, true)?;
        let genuine = theirs.sign(&mut genuine)?;
        assert!(is_semantic_ready(&theirs.authenticate(&genuine)?));

        let mut record = theirs.correlation(501_713, true)?;
        let canonical = ours.sign(&mut record)?;
        let error = ours
            .authenticate(&canonical)
            .expect_err("another incarnation's correlation must not authenticate against ours");
        assert!(
            format!("{error:#}").contains("substitutes or omits Expected authority"),
            "{error:#}"
        );
        Ok(())
    }

    /// Claim 1, second face: same Expected, different activation. This is the
    /// case the deleted gate could not see at all -- it compared instance ids
    /// only -- and the one that matters when a process is re-activated under
    /// the same projection. Authentication compares the activation digest.
    #[test]
    fn a_correlation_bound_to_another_activation_of_the_same_expected_is_refused() -> Result<()> {
        let world = TopologyFixture::new("odin")?;
        let other = TopologyFixture::new("odin")?;

        let mut reactivated = world.correlation(501_713, true)?;
        reactivated.current_activation_sha256 = Some(other.activation.canonical_sha256()?);
        let canonical = world.sign(&mut reactivated)?;
        let error = world
            .authenticate(&canonical)
            .expect_err("activation digest must bind");
        assert!(
            format!("{error:#}").contains("does not bind the current activation"),
            "{error:#}"
        );

        // A Present record with no activation cannot even be encoded: the
        // schema refuses it upstream of authentication ("Present topology
        // state lacks an authenticated runtime session"). Either refusal is
        // the right answer; what must not happen is an absent field reading as
        // a match for ours.
        let mut anonymous = world.correlation(501_714, true)?;
        anonymous.current_activation_sha256 = None;
        let error = world
            .sign(&mut anonymous)
            .and_then(|canonical| world.authenticate(&canonical))
            .expect_err("absent activation must not read as ours");
        assert!(
            format!("{error:#}").contains("lacks an authenticated runtime session"),
            "{error:#}"
        );

        let mut nameless = world.correlation(501_715, true)?;
        nameless.runtime_instance_id = None;
        let error = world
            .sign(&mut nameless)
            .and_then(|canonical| world.authenticate(&canonical))
            .expect_err("absent instance id must not read as ours");
        // Likewise refused at encoding: a Present record must carry its
        // instance id ("runtime instance identity and observation evidence are
        // partial").
        assert!(format!("{error:#}").contains("are partial"), "{error:#}");
        Ok(())
    }

    /// Claim 1, third face: the deleted gate also compared `target`. The
    /// projection digest already covers the target, but a record can carry a
    /// foreign `target` field beside our projection digest; prove
    /// `validate_against_expected` refuses that on its own.
    #[test]
    fn a_correlation_naming_another_target_beside_our_projection_is_refused() -> Result<()> {
        let world = TopologyFixture::new("odin")?;
        let mut stranger = world.correlation(501_713, true)?;
        stranger.target = "ghostlight".into();
        let canonical = world.sign(&mut stranger)?;
        let error = world
            .authenticate(&canonical)
            .expect_err("target must bind");
        assert!(
            format!("{error:#}").contains("substitutes or omits Expected authority"),
            "{error:#}"
        );
        Ok(())
    }

    /// Claim 4: the fixture's chain can refuse at every link, not only at the
    /// correlation signature. If any of these passed, the tests above would be
    /// asserting against a rubber stamp.
    #[test]
    fn the_fixture_chain_refuses_at_every_link() -> Result<()> {
        let ours = TopologyFixture::new("odin")?;
        let theirs = TopologyFixture::new("odin")?;

        // Authority link: activation must bind this Expected ...
        assert!(
            verify_runtime_authority(
                &ours.expected,
                &theirs.activation,
                &ours.idunn_anchor,
                &ours.provider_public_key
            )
            .is_err(),
            "another incarnation's activation verified against our Expected"
        );
        // ... be signed by the Idunn we trust ...
        assert!(
            verify_runtime_authority(
                &ours.expected,
                &ours.activation,
                &theirs.idunn_anchor,
                &ours.provider_public_key
            )
            .is_err(),
            "activation verified against a foreign Idunn anchor"
        );
        // ... and name the provider key the Expected selected.
        assert!(
            verify_runtime_authority(
                &ours.expected,
                &ours.activation,
                &ours.idunn_anchor,
                &theirs.provider_public_key
            )
            .is_err(),
            "a foreign provider key was accepted as the Expected signer"
        );

        // Odin link: a correlation honestly signed by another Odin identity
        // (its own id, its own signature) is refused against our admitted key.
        let mut record = ours.correlation(501_713, true)?;
        record.signer_identity_id = theirs.odin_signer.entry().identity_id.clone();
        let canonical = theirs.sign(&mut record)?;
        let error = ours
            .authenticate(&canonical)
            .expect_err("foreign Odin signer accepted");
        assert!(
            format!("{error:#}").contains("names a different identity"),
            "{error:#}"
        );

        // Odin link: our identity id, their signature bytes.
        let mut record = ours.correlation(501_713, true)?;
        record.signature = theirs
            .odin_signer
            .sign::<OdinRuntimeTopologyCorrelationPurpose>(&record.unsigned_signature_payload()?)
            .signature;
        let error = ours
            .authenticate(&record.canonical_bytes()?)
            .expect_err("forged signature accepted");
        assert!(
            format!("{error:#}").contains("signature verification failed"),
            "{error:#}"
        );
        Ok(())
    }

    /// Claim 2: `rehydrate_ready_token(.., require_current = false)` replays the
    /// receipt's authentication at its own `admitted_at`, so the window it
    /// widens is exactly the one the receipt already passed at admission. Pin
    /// both halves: at `admitted_at` the receipt authenticates however old it
    /// is now; at `now` past the window it does not. An `admitted_at` earlier
    /// than the observation is refused by the future-skew bound, so the field
    /// cannot be pulled backwards to launder a later observation either.
    #[test]
    fn a_durable_receipt_replays_at_its_admission_time_and_not_at_now() -> Result<()> {
        let world = TopologyFixture::new("odin")?;
        let mut record = world.correlation(3_841, true)?;
        let canonical = world.sign(&mut record)?;
        let admitted_at = NOW + 5;

        world.authenticate_at(&canonical, admitted_at)?;
        let much_later = admitted_at + DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS + 1;
        assert!(
            world.authenticate_at(&canonical, much_later).is_err(),
            "require_current would refuse"
        );
        world.authenticate_at(&canonical, admitted_at)?;

        let before_observation = NOW - DEFAULT_TOPOLOGY_MAXIMUM_FUTURE_SKEW_MILLIS - 1;
        assert!(
            world
                .authenticate_at(&canonical, before_observation)
                .is_err(),
            "an admitted_at earlier than the observation must not authenticate"
        );
        Ok(())
    }

    /// Claim 5 companion: what the live store holds, not only whether the
    /// stuck record would commit. Run with:
    ///   IDUNN_LIVE_CONTROL=/path/to/control.cc cargo test live_store_inventory -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_store_inventory() -> Result<()> {
        let Ok(path) = std::env::var("IDUNN_LIVE_CONTROL") else {
            eprintln!("IDUNN_LIVE_CONTROL unset");
            return Ok(());
        };
        let snapshot = ControlSnapshot::read(Path::new(&path))?;
        println!(
            "INVENTORY commands={} transactions={} admitted={}",
            snapshot.commands.len(),
            snapshot.transactions.len(),
            snapshot.admitted.len()
        );
        for stored in &snapshot.transactions {
            let value = &stored.value;
            println!(
                "TX {} target={} kind={:?} phase={:?} complete={} ready_seq={:?} latest_seq={:?} incumbent={:?}",
                value.transaction_id,
                value.target,
                value.command_kind,
                value.phase,
                value.completion.is_some(),
                value
                    .ready
                    .as_ref()
                    .and_then(ReadinessEvidence::odin)
                    .map(|evidence| evidence.publisher_sequence),
                value
                    .latest_odin_observation
                    .as_ref()
                    .map(|evidence| evidence.publisher_sequence),
                value.incumbent_generation_id,
            );
        }
        for stored in &snapshot.admitted {
            let value = &stored.value;
            println!(
                "ADMITTED target={} generation={} tx={} admitted_at={} ready_seq={:?} ready_admitted_at={:?} latest_seq={:?} latest_admitted_at={:?} instance={} {}",
                value.target,
                value.generation_id,
                value.transaction_id,
                value.admitted_at_unix_millis,
                value.ready.odin().map(|evidence| evidence.publisher_sequence),
                value.ready.odin().map(|evidence| evidence.admitted_at_unix_millis),
                value
                    .latest_odin_observation
                    .as_ref()
                    .map(|evidence| evidence.publisher_sequence),
                value
                    .latest_odin_observation
                    .as_ref()
                    .map(|evidence| evidence.admitted_at_unix_millis),
                value.activation.runtime_instance_id,
                value.workload.describe(),
            );
        }
        Ok(())
    }

    fn topology(sequence: u64, byte: u8) -> TopologyEvidence {
        let canonical_bytes = vec![byte];
        TopologyEvidence {
            canonical_sha256: sha256_id(&canonical_bytes),
            canonical_bytes,
            signer_identity_id: "odin-signer".into(),
            publisher_sequence: sequence,
            admitted_at_unix_millis: 100,
        }
    }

    fn workload(uid: u32, pid_namespace_id: u64, mount_namespace_id: u64) -> WorkloadObservation {
        WorkloadObservation::Systemd(SystemdWorkloadObservation {
            unit: format!("idunn-{uid}.service"),
            unit_description: format!("Idunn test {uid}"),
            invocation_id: format!("invocation-{uid}"),
            exec_main_start_timestamp_monotonic: 1,
            service_type: "exec".into(),
            restart_policy: "always".into(),
            kill_mode: "control-group".into(),
            dynamic_user: true,
            systemd_user: format!("u{uid}"),
            systemd_group: format!("u{uid}"),
            supplementary_groups: String::new(),
            capability_bounding_set: String::new(),
            ambient_capabilities: String::new(),
            private_mounts: true,
            private_pids: true,
            protect_proc: "invisible".into(),
            proc_subset: "pid".into(),
            no_new_privileges: true,
            umask: "0077".into(),
            inaccessible_paths: String::new(),
            load_credential: String::new(),
            main_pid: uid,
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
            mount_namespace_id,
            pid_namespace_id,
            executable: PathBuf::from("/opt/test/bin/service"),
            executable_device: 1,
            executable_inode: 1,
            executable_sha256: sha256_id(&[1]),
            runtime_instance_id: sha256_id(&uid.to_be_bytes()),
            working_directory: PathBuf::from("/opt/test"),
            runtime_bundle: PathBuf::from("/run/test"),
            command_line_sha256: sha256_id(&[2]),
            environment_names: Vec::new(),
            environment_contract_sha256: sha256_id(&[3]),
            control_group: format!("/system.slice/idunn-{uid}.service"),
            credentials_directory: None,
            parent_only_file_descriptors: Vec::new(),
            activation_signer_identity_id: "activation".into(),
            activation_signer_public_key: vec![1; 32],
            service_credentials: Vec::new(),
        })
    }

    fn write_lease() -> IdunnProcessWriteLeaseRecord {
        IdunnProcessWriteLeaseRecord {
            schema_version: IDUNN_PROCESS_WRITE_LEASE_SCHEMA.into(),
            target: "ghostlight".into(),
            expected_projection_sha256: sha256_id(&[1]),
            plan_id: sha256_id(&[2]),
            incarnation_id: "incarnation-test".into(),
            sealed_release_id: sha256_id(&[3]),
            activation_witness_sha256: sha256_id(&[4]),
            state_schema_generation: "ghostlight-state-v1".into(),
            state_contract_sha256: sha256_id(&[5]),
            runtime_id: "ghostlight".into(),
            runtime_instance_id: sha256_id(&[6]),
            warming_presence_sha256: sha256_id(&[7]),
            lease_epoch: 1,
            issued_at_unix_millis: 100,
        }
    }

    #[test]
    fn validate_is_offline_and_declarative() {
        // It must accept a recipe alone, accept a recipe plus a binding, and
        // refuse to be handed anything executable. It opens no store and
        // actuates nothing, which is what makes it safe to run on a candidate
        // binding before that binding is installed.
        let parsed = parse(
            ["validate", "--recipe", "/tmp/recipe.toml"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert!(matches!(parsed, Command::Validate { binding: None, .. }));

        assert!(
            parse(
                ["validate", "--recipe", "/r.toml", "--binding", "/b.toml"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .is_ok()
        );
        assert!(parse(["validate"].into_iter().map(str::to_owned)).is_err());
        for rejected in [
            vec![
                "validate",
                "--recipe",
                "/r.toml",
                "--deploy-command",
                "sh -c bad",
            ],
            vec!["validate", "--recipe", "/r.toml", "--state-store", "/s.cc"],
        ] {
            assert!(parse(rejected.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn cli_exposes_only_declarative_commands() {
        for arguments in [
            vec!["up", "ghostlight", "--deploy-command", "sh -c bad"],
            vec!["serve", "--swarm-profile", "yggdrasil-local"],
            vec!["serve", "--restart-command", "bad"],
            vec!["validate-runtime-admission"],
        ] {
            assert!(parse(arguments.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn garden_path_accepts_service_and_profile_selectors() {
        for selector in ["ghostlight", "profile:aetheria", "profile:full-gamecult"] {
            let parsed =
                parse(["up", selector, "--no-wait"].into_iter().map(str::to_owned)).unwrap();
            let Command::Up {
                selector: actual, ..
            } = parsed
            else {
                panic!("expected up command")
            };
            assert_eq!(actual, selector);
        }
    }

    #[test]
    fn route_bindings_are_one_global_socket_and_candidate_authority_map() {
        let first = route_binding("first", 4103, 14103, 14111);
        let second = route_binding("second", 8831, 18831, 18839);
        validate_route_binding_set(&[("first", &first), ("second", &second)]).unwrap();

        let mut duplicate_id = second.clone();
        duplicate_id.route_id = first.route_id.clone();
        assert!(
            validate_route_binding_set(&[("first", &first), ("second", &duplicate_id)]).is_err()
        );

        let mut overlapping_candidates = second.clone();
        overlapping_candidates.private_port_start = 14111;
        overlapping_candidates.private_port_end = 14120;
        assert!(
            validate_route_binding_set(&[("first", &first), ("second", &overlapping_candidates),])
                .is_err()
        );

        let mut stable_inside_other_range = second;
        stable_inside_other_range.stable_endpoint = "tcp://127.0.0.1:14105".into();
        assert!(
            validate_route_binding_set(&[
                ("first", &first),
                ("second", &stable_inside_other_range),
            ])
            .is_err()
        );
    }

    #[test]
    fn deployment_command_is_immutable_positional_fact() -> Result<()> {
        let value = command(CommandKind::Deploy);
        value.validate()?;
        let encoded = rmp_serde::to_vec(&value)?;
        assert_eq!(encoded[0], 0x96);
        assert!(!encoded.windows(7).any(|window| window == b"running"));
        assert!(!encoded.windows(5).any(|window| window == b"owner"));
        Ok(())
    }

    #[test]
    fn phases_are_one_exact_forward_chain() {
        let phases = [
            DeploymentPhase::Sealing,
            DeploymentPhase::Starting,
            DeploymentPhase::Warming,
            DeploymentPhase::Fencing,
            DeploymentPhase::Leasing,
            DeploymentPhase::AwaitingReady,
            DeploymentPhase::Routing,
            DeploymentPhase::Committing,
            DeploymentPhase::Complete,
        ];
        assert!(
            phases
                .windows(2)
                .all(|pair| pair[1] as u8 == pair[0] as u8 + 1)
        );
    }

    #[test]
    fn only_live_stateless_deployments_can_be_cancelled_after_fencing() {
        let command = command(CommandKind::Deploy);
        let mut stateless =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100).unwrap();
        stateless.phase = DeploymentPhase::Routing;
        stateless.fencing = Some(FencingEvidence::SkippedStateless);
        assert!(cancel_is_safe_for_live_stateless(&stateless));

        let mut stateful = stateless.clone();
        stateful.fencing = Some(FencingEvidence::Revoked {
            incumbent_lease_sha256: None,
            candidate_lease_path_verified_empty: true,
        });
        assert!(!cancel_is_safe_for_live_stateless(&stateful));

        let mut unfenced = stateless.clone();
        unfenced.phase = DeploymentPhase::Sealing;
        assert!(!cancel_is_safe_for_live_stateless(&unfenced));

        let mut committed = stateless;
        committed.completion = Some(TransactionCompletion::Admitted {
            generation_id: "generation-test".into(),
        });
        assert!(!cancel_is_safe_for_live_stateless(&committed));
    }

    #[test]
    fn every_new_odin_sequence_must_advance_and_exact_retry_is_idempotent() -> Result<()> {
        let first = topology(7, 1);
        assert!(sequence_requires_admission(None, 6, &first)?);
        assert!(!sequence_requires_admission(
            Some(&first),
            7,
            &topology(7, 1)
        )?);
        assert!(sequence_requires_admission(
            Some(&first),
            7,
            &topology(8, 2)
        )?);
        assert!(sequence_requires_admission(Some(&first), 7, &topology(7, 2)).is_err());
        assert!(sequence_requires_admission(Some(&first), 7, &topology(6, 3)).is_err());
        Ok(())
    }

    #[test]
    fn odin_sequence_cursors_are_scoped_by_target_and_signer() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let mut ghostlight =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        ghostlight.latest_odin_observation = Some(topology(7, 1));
        ghostlight.odin_publisher_sequence_cursor = 7;
        let mut odin = DeploymentTransaction::new(&command, "odin".into(), 1, None, 100)?;
        odin.latest_odin_observation = Some(topology(41, 2));
        odin.odin_publisher_sequence_cursor = 41;
        let envelope = |key: &str| CultCacheEnvelope {
            key: key.into(),
            r#type: DeploymentTransaction::TYPE.into(),
            payload: Vec::new(),
            stored_at: "1970-01-01T00:00:00.100Z".into(),
            schema_id: Some(DEPLOYMENT_TRANSACTION_SCHEMA.into()),
        };
        let snapshot = ControlSnapshot {
            commands: Vec::new(),
            transactions: vec![
                Stored {
                    envelope: envelope("ghostlight"),
                    value: ghostlight,
                },
                Stored {
                    envelope: envelope("odin"),
                    value: odin,
                },
            ],
            admitted: Vec::new(),
            targets: Vec::new(),
        };

        assert_eq!(snapshot.max_odin_sequence("ghostlight", "odin-signer"), 7);
        assert_eq!(snapshot.max_odin_sequence("odin", "odin-signer"), 41);
        assert_eq!(snapshot.max_odin_sequence("ghostlight", "other-signer"), 0);

        // A transaction that failed was never built upon, so its readings must
        // not raise the bar for the next attempt. Counting them is how a target
        // that has failed once becomes a target that can never deploy.
        let mut abandoned = snapshot;
        abandoned.transactions[0].value.completion =
            Some(TransactionCompletion::FailedBeforeFencing {
                error: "sealed source was rejected".into(),
            });
        assert_eq!(abandoned.max_odin_sequence("ghostlight", "odin-signer"), 0);
        abandoned.transactions[1].value.completion =
            Some(TransactionCompletion::FailedAfterFencing {
                error: "candidate died after the fence".into(),
                recovery: TerminalRecovery::RestoreIncumbent,
            });
        assert_eq!(abandoned.max_odin_sequence("odin", "odin-signer"), 0);
        Ok(())
    }

    #[test]
    fn stale_odin_provider_receipt_cannot_gate_continuity_restart() -> Result<()> {
        let touched = std::cell::Cell::new(false);
        validate_live_providers_for_deploy(CommandKind::Continuity, || {
            touched.set(true);
            bail!("Odin provider receipt is stale")
        })?;
        assert!(!touched.get());
        assert!(
            validate_live_providers_for_deploy(CommandKind::Deploy, || {
                bail!("Odin provider receipt is stale")
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn prepared_write_lease_is_durable_but_not_admitted_authority() -> Result<()> {
        let lease = write_lease();
        lease.validate()?;
        let prepared = LeasingEvidence::Prepared {
            lease: lease.clone(),
            lease_sha256: lease.canonical_sha256()?,
        };
        let encoded = rmp_serde::to_vec(&prepared)?;
        let decoded: LeasingEvidence = rmp_serde::from_slice(&encoded)?;
        assert_eq!(decoded, prepared);
        assert!(decoded.lease().is_none());
        assert!(decoded.lease_sha256().is_none());
        assert_eq!(decoded.prepared_lease().unwrap().0, &lease);
        Ok(())
    }

    #[test]
    fn runtime_instance_identity_is_stable_for_activation_prepare_replay() -> Result<()> {
        let first = runtime_instance_id("tx-one")?;
        assert_eq!(runtime_instance_id("tx-one")?, first);
        assert_ne!(runtime_instance_id("tx-two")?, first);
        Ok(())
    }

    #[test]
    fn pre_fencing_abort_is_durable_before_cleanup_and_cannot_resume_as_success() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let mut transaction =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        transaction.pre_fencing_abort = Some(PreFencingAbort {
            error: "sealed source was rejected".into(),
            candidate_cleanup: CleanupEvidence::Skipped,
            topology_reconciliation: CleanupEvidence::Skipped,
            source_cleanup: CleanupEvidence::Pending,
        });
        transaction.validate()?;
        assert!(!transaction.is_terminal());

        transaction
            .pre_fencing_abort
            .as_mut()
            .unwrap()
            .source_cleanup = CleanupEvidence::Complete;
        transaction.phase = DeploymentPhase::Complete;
        transaction.last_error = Some("sealed source was rejected".into());
        transaction.completion = Some(TransactionCompletion::FailedBeforeFencing {
            error: "sealed source was rejected".into(),
        });
        transaction.validate()?;
        assert!(transaction.is_terminal());
        Ok(())
    }

    #[test]
    fn post_fencing_abort_is_terminal_only_when_every_cleanup_is_complete() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let mut transaction =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        transaction.phase = DeploymentPhase::Complete;
        transaction.post_fencing_abort = Some(PostFencingAbort {
            error: "candidate died after the fence".into(),
            route_restoration: CleanupEvidence::Skipped,
            lease_withdrawal: CleanupEvidence::Pending,
            candidate_cleanup: CleanupEvidence::Skipped,
            topology_reconciliation: CleanupEvidence::Skipped,
            source_cleanup: CleanupEvidence::Pending,
        });
        transaction.last_error = Some("candidate died after the fence".into());
        transaction.completion = Some(TransactionCompletion::FailedAfterFencing {
            error: "candidate died after the fence".into(),
            recovery: TerminalRecovery::RestoreIncumbent,
        });
        // The evidence exists so that a target is released only once the
        // candidate's route, lease, process and projection are actually gone.
        assert!(transaction.validate().is_err());
        assert!(!transaction.is_terminal());

        let abort = transaction.post_fencing_abort.as_mut().unwrap();
        abort.lease_withdrawal = CleanupEvidence::Complete;
        abort.source_cleanup = CleanupEvidence::Complete;
        transaction.validate()?;
        assert!(transaction.is_terminal());

        // The recorded error and the completion must name the same failure.
        transaction.completion = Some(TransactionCompletion::FailedAfterFencing {
            error: "a different story".into(),
            recovery: TerminalRecovery::RestoreIncumbent,
        });
        assert!(transaction.validate().is_err());
        Ok(())
    }

    #[test]
    fn a_pre_fence_transaction_cannot_carry_a_post_fencing_abort() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let mut transaction =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        assert!(transaction.phase < DeploymentPhase::Fencing);
        transaction.post_fencing_abort = Some(PostFencingAbort {
            error: "candidate died after the fence".into(),
            route_restoration: CleanupEvidence::Skipped,
            lease_withdrawal: CleanupEvidence::Skipped,
            candidate_cleanup: CleanupEvidence::Skipped,
            topology_reconciliation: CleanupEvidence::Skipped,
            source_cleanup: CleanupEvidence::Skipped,
        });
        assert!(transaction.validate().is_err());
        Ok(())
    }

    #[test]
    fn activation_without_workload_still_requires_durable_candidate_cleanup() {
        assert_eq!(
            candidate_cleanup_requirement(true, false),
            CleanupEvidence::Pending
        );
        assert_eq!(
            candidate_cleanup_requirement(true, true),
            CleanupEvidence::Pending
        );
        assert_eq!(
            candidate_cleanup_requirement(false, false),
            CleanupEvidence::Skipped
        );
    }

    #[test]
    fn bad_selector_becomes_one_terminal_refusal_record() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let transaction = DeploymentTransaction::rejected(
            &command,
            anyhow!("deployment selector is unknown"),
            100,
        )?;
        assert!(transaction.is_terminal());
        assert_eq!(transaction.target, command.selector);
        assert!(matches!(
            transaction.completion,
            Some(TransactionCompletion::FailedBeforeFencing { .. })
        ));
        Ok(())
    }

    #[test]
    fn post_commit_cleanup_keeps_exact_incumbent_work_owned_until_retired() -> Result<()> {
        let incumbent = workload(1001, 40, 50);
        let cleanup = PostCommitCleanup {
            incumbent: IncumbentCleanupEvidence::Pending {
                generation_id: "generation-old".into(),
                workload: incumbent.clone(),
            },
            source: SourceCleanupEvidence::Pending,
        };
        let encoded = rmp_serde::to_vec(&cleanup)?;
        let mut replayed: PostCommitCleanup = rmp_serde::from_slice(&encoded)?;
        assert_eq!(replayed, cleanup);
        assert!(!replayed.is_complete());
        replayed.incumbent = IncumbentCleanupEvidence::Complete {
            generation_id: "generation-old".into(),
        };
        assert!(!replayed.is_complete());
        replayed.source = SourceCleanupEvidence::Complete;
        assert!(replayed.is_complete());
        Ok(())
    }

    #[test]
    fn routed_incumbent_retirement_waits_for_the_declared_drain_deadline() -> Result<()> {
        assert_eq!(route_drain_deadline(1_000, 30)?, 31_000);
        assert!(route_drain_deadline(u64::MAX, 1).is_err());
        Ok(())
    }

    #[test]
    fn admitted_route_receipt_refreshes_when_stale_or_implausibly_future_dated() {
        assert!(route_observation_is_current(900, 1_000, 100, 10));
        assert!(route_observation_is_current(1_010, 1_000, 100, 10));
        assert!(!route_observation_is_current(899, 1_000, 100, 10));
        assert!(!route_observation_is_current(1_011, 1_000, 100, 10));
    }

    #[test]
    fn lease_warming_requires_both_new_odin_and_new_provider_evidence() {
        let old = digest('1');
        let fresh = digest('2');
        assert!(!provider_warming_advanced(7, &old, 7, &fresh));
        assert!(!provider_warming_advanced(7, &old, 8, &old));
        assert!(provider_warming_advanced(7, &old, 8, &fresh));
    }

    #[test]
    fn failed_route_proof_never_restores_any_fenced_incumbent() {
        assert!(!may_rollback_route_after_failed_proof(
            &FencingEvidence::Revoked {
                incumbent_lease_sha256: Some(digest('1')),
                candidate_lease_path_verified_empty: false,
            }
        ));
        assert!(!may_rollback_route_after_failed_proof(
            &FencingEvidence::Revoked {
                incumbent_lease_sha256: None,
                candidate_lease_path_verified_empty: true,
            }
        ));
        assert!(may_rollback_route_after_failed_proof(
            &FencingEvidence::SkippedStateless
        ));
    }

    #[test]
    fn fenced_writer_is_already_retired_before_post_commit_cleanup() {
        assert!(incumbent_was_stopped_during_fencing(
            &FencingEvidence::Revoked {
                incumbent_lease_sha256: Some(digest('1')),
                candidate_lease_path_verified_empty: false,
            }
        ));
        assert!(!incumbent_was_stopped_during_fencing(
            &FencingEvidence::Revoked {
                incumbent_lease_sha256: None,
                candidate_lease_path_verified_empty: true,
            }
        ));
        assert!(!incumbent_was_stopped_during_fencing(
            &FencingEvidence::SkippedStateless
        ));
    }

    #[test]
    fn warming_accepts_only_the_exact_projected_incumbent_lease_disagreement() {
        let expected_detail = format!("expected:{};activation:{}", digest('1'), digest('2'));
        let incumbent = digest('3');
        let exact = OdinTopologyDisagreement {
            code: "projected-write-lease".into(),
            expected: Some(expected_detail.clone()),
            observed: Some(incumbent.clone()),
        };

        assert!(warming_disagreements_match_incumbent(
            &expected_detail,
            Some(&incumbent),
            &[exact.clone()],
        ));
        assert!(!warming_disagreements_match_incumbent(
            &expected_detail,
            None,
            &[exact.clone()],
        ));

        let mut substituted = exact.clone();
        substituted.observed = Some(digest('4'));
        assert!(!warming_disagreements_match_incumbent(
            &expected_detail,
            Some(&incumbent),
            &[substituted],
        ));
        assert!(!warming_disagreements_match_incumbent(
            &expected_detail,
            Some(&incumbent),
            &[exact.clone(), exact],
        ));
    }

    #[test]
    fn candidate_and_incumbent_must_differ_in_all_three_native_boundaries() {
        let incumbent = workload(1001, 40, 50);
        let prove = |candidate: &WorkloadObservation| {
            WorkloadObservation::prove_isolation(candidate, Some(&incumbent))
        };
        assert!(prove(&workload(1002, 41, 51)).is_ok());
        assert!(prove(&workload(1001, 41, 51)).is_err());
        assert!(prove(&workload(1002, 40, 51)).is_err());
        assert!(prove(&workload(1002, 41, 50)).is_err());
    }

    #[test]
    fn exactly_one_inflight_transaction_may_own_a_target() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let one = DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        let two = DeploymentTransaction::new(&command, "ghostlight".into(), 1, None, 100)?;
        let snapshot = ControlSnapshot {
            commands: vec![Stored {
                envelope: command_envelope(&command, 100)?,
                value: command,
            }],
            transactions: vec![
                Stored {
                    envelope: transaction_envelope(&one, 100)?,
                    value: one,
                },
                Stored {
                    envelope: transaction_envelope(&two, 100)?,
                    value: two,
                },
            ],
            admitted: Vec::new(),
            targets: Vec::new(),
        };
        assert!(snapshot.validate_relations().is_err());
        Ok(())
    }

    #[test]
    fn command_ordinals_do_not_turn_one_target_wait_into_a_global_brake() -> Result<()> {
        let first_command = command(CommandKind::Deploy);
        let first = DeploymentTransaction::new(&first_command, "odin".into(), 0, None, 100)?;
        let second = DeploymentTransaction::new(&first_command, "ghostlight".into(), 1, None, 100)?;
        let mut unrelated_command = command(CommandKind::Deploy);
        unrelated_command.command_id = "up-unrelated".into();
        let unrelated =
            DeploymentTransaction::new(&unrelated_command, "huginn".into(), 0, None, 101)?;
        let snapshot = ControlSnapshot {
            commands: Vec::new(),
            transactions: [&first, &second, &unrelated]
                .into_iter()
                .map(|value| Stored {
                    envelope: transaction_envelope(value, 100).unwrap(),
                    value: value.clone(),
                })
                .collect(),
            admitted: Vec::new(),
            targets: Vec::new(),
        };
        assert!(!snapshot.has_earlier_authority_sibling(&first));
        assert!(snapshot.has_earlier_authority_sibling(&second));
        assert!(!snapshot.has_earlier_authority_sibling(&unrelated));
        Ok(())
    }

    #[test]
    fn complete_cleanup_remains_retryable_without_owning_the_current_incarnation() -> Result<()> {
        let command = command(CommandKind::Deploy);
        let mut transaction =
            DeploymentTransaction::new(&command, "ghostlight".into(), 0, None, 100)?;
        transaction.phase = DeploymentPhase::Complete;
        transaction.completion = Some(TransactionCompletion::Admitted {
            generation_id: format!("generation-{}", transaction.transaction_id),
        });
        transaction.post_commit_cleanup = Some(PostCommitCleanup {
            incumbent: IncumbentCleanupEvidence::SkippedNoIncumbent,
            source: SourceCleanupEvidence::Pending,
        });

        assert!(!transaction.is_terminal());
        assert!(!transaction.owns_target_authority());
        assert!(transaction.blocks_new_target_mutation());
        Ok(())
    }

    #[test]
    fn legacy_mutable_command_schema_is_rejected() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("control.cc");
        let legacy = CultCacheEnvelope {
            key: "up-legacy".into(),
            r#type: DeploymentCommand::TYPE.into(),
            payload: rmp_serde::to_vec(&(DEPLOYMENT_COMMAND_SCHEMA, "up-legacy", "running"))?,
            stored_at: rfc3339_millis(100)?,
            schema_id: Some("idunn.deployment_command.v1".into()),
        };
        assert!(
            SingleFileMessagePackBackingStore::new(&path).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: "up-legacy".into(),
                    current: None,
                }],
                &[legacy],
            )?
        );
        assert!(ControlSnapshot::read(&path).is_err());
        Ok(())
    }

    #[test]
    fn loadcredential_era_control_schemas_are_rejected_before_rehydration() -> Result<()> {
        for (record_type, key, schema) in [
            (
                DeploymentTransaction::TYPE,
                "tx-legacy-workload-observation",
                "idunn.deployment_transaction.v1",
            ),
            (
                AdmittedGeneration::TYPE,
                "legacy-admitted-target",
                "idunn.admitted_generation.v1",
            ),
        ] {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("control.cc");
            let legacy = CultCacheEnvelope {
                key: key.into(),
                r#type: record_type.into(),
                payload: Vec::new(),
                stored_at: rfc3339_millis(100)?,
                schema_id: Some(schema.into()),
            };
            assert!(
                SingleFileMessagePackBackingStore::new(&path).compare_exchange(
                    &[CultCacheExpectedEnvelope {
                        r#type: record_type.into(),
                        key: key.into(),
                        current: None,
                    }],
                    &[legacy],
                )?
            );
            assert!(ControlSnapshot::read(&path).is_err());
        }
        Ok(())
    }

    #[test]
    fn source_identity_is_explicit_and_atomic() {
        let partial = parse(
            ["serve", "--source-uid", "1001"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap_err()
        .to_string();
        assert!(partial.contains("supplied together"));
        let Command::Serve(options) = parse(
            ["serve", "--source-uid", "1001", "--source-gid", "1002"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap() else {
            panic!("expected serve")
        };
        assert_eq!(
            options.source_identity,
            Some(ProcessIdentity {
                uid: 1001,
                gid: 1002
            })
        );
    }

    // ---- v2/v3 control records lifted to the v3 generation / v4 transaction ----
    //
    // The bytes under tests/fixtures/idunn-control-legacy were written by the
    // encoder at Idunn 9001b58 (see the README there). Nothing below encodes a
    // legacy record: the legacy layout is decoded from those bytes, and the
    // lifted record is compared with what the decoded legacy record says.

    const FIXTURE_GENERATION: &str =
        include_str!("../tests/fixtures/idunn-control-legacy/generation-with-receipts.hex");
    const FIXTURE_GENERATION_REPAIRING: &str = include_str!(
        "../tests/fixtures/idunn-control-legacy/generation-route-repair-started.hex"
    );
    const FIXTURE_GENERATION_V3_ODIN: &str =
        include_str!("../tests/fixtures/idunn-control-legacy/generation-v3-odin.hex");
    const FIXTURE_GENERATION_V3_ROUTE_PROOF: &str =
        include_str!("../tests/fixtures/idunn-control-legacy/generation-v3-route-proof.hex");
    const FIXTURE_TRANSACTIONS: [(&str, &str); 6] = [
        (
            "fencing",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-fencing.hex"),
        ),
        (
            "leasing",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-leasing.hex"),
        ),
        (
            "awaiting-ready",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-awaiting-ready.hex"),
        ),
        (
            "routing",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-routing.hex"),
        ),
        (
            "committing",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-committing.hex"),
        ),
        (
            "failed-after-fencing",
            include_str!(
                "../tests/fixtures/idunn-control-legacy/transaction-failed-after-fencing.hex"
            ),
        ),
    ];
    const FIXTURE_TRANSACTION_V2: &str =
        include_str!("../tests/fixtures/idunn-control-legacy/transaction-committing-v2.hex");

    fn fixture_envelope(text: &str, record_type: &str) -> Result<CultCacheEnvelope> {
        let mut lines = text.lines();
        let schema = lines.next().context("fixture has no schema line")?;
        let hex = lines.next().context("fixture has no payload line")?.trim();
        let payload = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let key = if record_type == AdmittedGeneration::TYPE && schema == ADMITTED_GENERATION_SCHEMA_V3 {
            rmp_serde::from_slice::<LegacyAdmittedGenerationV3>(&payload)?.target
        } else if record_type == AdmittedGeneration::TYPE {
            rmp_serde::from_slice::<LegacyAdmittedGeneration>(&payload)?.target
        } else {
            rmp_serde::from_slice::<LegacyDeploymentTransaction>(&payload)?.transaction_id
        };
        Ok(CultCacheEnvelope {
            key,
            r#type: record_type.into(),
            payload,
            stored_at: rfc3339_millis(1_700_000_123_456)?,
            schema_id: Some(schema.into()),
        })
    }

    fn fixture_generation(text: &str) -> Result<(LegacyAdmittedGeneration, AdmittedGeneration)> {
        let envelope = fixture_envelope(text, AdmittedGeneration::TYPE)?;
        let legacy = rmp_serde::from_slice(&envelope.payload)?;
        Ok((legacy, read_generation_record(&envelope)?))
    }

    fn fixture_transaction(
        text: &str,
    ) -> Result<(LegacyDeploymentTransaction, DeploymentTransaction)> {
        let envelope = fixture_envelope(text, DeploymentTransaction::TYPE)?;
        let legacy = rmp_serde::from_slice(&envelope.payload)?;
        Ok((legacy, read_transaction_record(&envelope)?))
    }

    #[test]
    fn a_v2_generation_lifts_to_the_decisions_it_encoded() -> Result<()> {
        for text in [FIXTURE_GENERATION, FIXTURE_GENERATION_REPAIRING] {
            let (legacy, lifted) = fixture_generation(text)?;
            // Every receipt the old decisions read is the same receipt, now
            // tagged Odin-correlated.
            let odin = lifted.odin_receipts()?;
            assert_eq!(odin.ready, &legacy.ready);
            assert_eq!(odin.latest, &legacy.latest_odin_observation);
            assert_eq!(lifted.odin_authority.as_ref(), Some(&legacy.odin_authority));
            assert_eq!(
                lifted.odin_publisher_sequence_cursor,
                legacy.odin_publisher_sequence_cursor
            );
            assert_eq!(lifted.ready.voucher(), Voucher::Odin);
            assert_eq!(lifted.routing, legacy.routing);
            assert_eq!(lifted.leasing, legacy.leasing);
            assert_eq!(lifted.plan, legacy.plan);
            assert_eq!(lifted.expected, legacy.expected);
            assert_eq!(lifted.activation, legacy.activation);
            assert_eq!(lifted.workload, legacy.workload);
            assert_eq!(lifted.generation_id, legacy.generation_id);
            // The new state starts empty and the plan is still a v2 plan.
            assert_eq!(lifted.route_supervision, Some(RouteSupervisionState::default()));
            assert_eq!(lifted.last_error, None);
            assert_eq!(lifted.plan.schema, crate::deployment_plan::COMPILED_DEPLOYMENT_PLAN_SCHEMA_V2);
            assert_eq!(
                lifted.plan.phase_deadlines(),
                crate::deployment_plan::PhaseDeadlines::IDUNN_DEFAULTS
            );
        }
        Ok(())
    }

    #[test]
    fn v2_and_v3_transactions_lift_to_the_decisions_they_encoded() -> Result<()> {
        let mut phases = Vec::new();
        for (name, text) in FIXTURE_TRANSACTIONS {
            let (legacy, lifted) = fixture_transaction(text)?;
            phases.push((name, lifted.phase));
            assert_eq!(lifted.schema_version, DEPLOYMENT_TRANSACTION_SCHEMA);
            assert_eq!(lifted.phase, legacy.phase);
            assert_eq!(lifted.completion, legacy.completion);
            assert_eq!(lifted.plan, legacy.plan);
            assert_eq!(lifted.expected, legacy.expected);
            assert_eq!(lifted.leasing, legacy.leasing);
            assert_eq!(lifted.fencing, legacy.fencing);
            assert_eq!(lifted.routing, legacy.routing);
            assert_eq!(lifted.warming, legacy.warming);
            assert_eq!(lifted.latest_odin_observation, legacy.latest_odin_observation);
            assert_eq!(
                lifted.odin_publisher_sequence_cursor,
                legacy.odin_publisher_sequence_cursor
            );
            assert_eq!(
                lifted.ready.as_ref().and_then(ReadinessEvidence::odin),
                legacy.ready.as_ref()
            );
            assert_eq!(lifted.phase_deadline, None);
            assert_eq!(lifted.lease_adoption, None);
            assert_eq!(lifted.owns_target_authority(), legacy.phase != DeploymentPhase::Complete);
        }
        assert_eq!(
            phases.iter().map(|(_, phase)| *phase).collect::<Vec<_>>(),
            [
                DeploymentPhase::Fencing,
                DeploymentPhase::Leasing,
                DeploymentPhase::AwaitingReady,
                DeploymentPhase::Routing,
                DeploymentPhase::Committing,
                DeploymentPhase::Complete,
            ]
        );

        // The genuine v2 layout lifts to the same record as the v3 one.
        let (_, from_v2) = fixture_transaction(FIXTURE_TRANSACTION_V2)?;
        let (_, from_v3) = fixture_transaction(FIXTURE_TRANSACTIONS[4].1)?;
        assert_eq!(from_v2, from_v3);

        // A terminal failure predating recovery names the recovery it always did.
        let (_, failed) = fixture_transaction(FIXTURE_TRANSACTIONS[5].1)?;
        assert!(matches!(
            failed.completion,
            Some(TransactionCompletion::FailedAfterFencing {
                recovery: TerminalRecovery::RestoreIncumbent,
                ..
            })
        ));
        assert!(failed.is_terminal());
        Ok(())
    }

    #[test]
    fn the_boot_migration_rewrites_every_legacy_record_once() -> Result<()> {
        // The fixtures were cut from one build, so their records share keys
        // and each migrates in a store of its own.
        let mut fixtures = vec![
            fixture_envelope(FIXTURE_GENERATION, AdmittedGeneration::TYPE)?,
            fixture_envelope(FIXTURE_GENERATION_REPAIRING, AdmittedGeneration::TYPE)?,
            fixture_envelope(FIXTURE_TRANSACTION_V2, DeploymentTransaction::TYPE)?,
        ];
        for (_, text) in FIXTURE_TRANSACTIONS {
            fixtures.push(fixture_envelope(text, DeploymentTransaction::TYPE)?);
        }
        for envelope in fixtures {
            let temporary = tempfile::tempdir()?;
            let path = temporary.path().join("control.cc");
            let store = SingleFileMessagePackBackingStore::new(&path);
            assert!(store.compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: envelope.r#type.clone(),
                    key: envelope.key.clone(),
                    current: None,
                }],
                std::slice::from_ref(&envelope),
            )?);
            let debug_of = |envelope: &CultCacheEnvelope| -> Result<String> {
                Ok(if envelope.r#type == AdmittedGeneration::TYPE {
                    format!("{:?}", read_generation_record(envelope)?)
                } else {
                    format!("{:?}", read_transaction_record(envelope)?)
                })
            };
            let before = debug_of(&envelope)?;

            assert_eq!(migrate_control_store_to_current_schema(&path)?, 1);
            assert_eq!(migrate_control_store_to_current_schema(&path)?, 0);

            let after = store.pull_all_read_only_snapshot()?;
            assert_eq!(after.len(), 1);
            let expected_schema = if envelope.r#type == AdmittedGeneration::TYPE {
                ADMITTED_GENERATION_SCHEMA
            } else {
                DEPLOYMENT_TRANSACTION_SCHEMA
            };
            assert_eq!(after[0].schema_id.as_deref(), Some(expected_schema));
            assert_eq!(after[0].stored_at, envelope.stored_at);
            // The rewritten record is canonical and means what the legacy one meant.
            assert_eq!(debug_of(&after[0])?, before);
        }
        Ok(())
    }

    #[test]
    fn history_still_reads_legacy_transactions() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let state_store = temporary.path().join("control.cc");
        let envelope = fixture_envelope(FIXTURE_TRANSACTIONS[5].1, DeploymentTransaction::TYPE)?;
        SingleFileMessagePackBackingStore::new(&history_store_path(&state_store))
            .compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: envelope.key.clone(),
                    current: None,
                }],
                std::slice::from_ref(&envelope),
            )?;
        let archived = read_history_transactions(&state_store);
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].transaction_id, envelope.key);
        assert_eq!(archived[0].schema_version, DEPLOYMENT_TRANSACTION_SCHEMA);
        Ok(())
    }

    #[test]
    fn readiness_class_follows_the_expected_odin_dependency() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        let mut expected = generation.expected.clone();
        assert!(expected.dependencies.iter().any(|dependency| {
            dependency.kind == "shared-infrastructure"
                && dependency.capability == ODIN_RENDEZVOUS_CAPABILITY
        }));
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::OdinCorrelated));
        for dependency in &mut expected.dependencies {
            // Same capability, other kind: only the shared-infrastructure
            // declaration makes a target Odin-correlated.
            dependency.kind = "required".into();
        }
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::RouteProof));
        expected.dependencies.clear();
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::RouteProof));
        // Nothing to challenge and no Odin dependency: the recipe has declared
        // no way to prove readiness, and Idunn does not pick one.
        expected.route = None;
        assert_eq!(
            ReadinessClass::of(&expected),
            Err(UndeclaredReadiness {
                target: expected.target.clone()
            })
        );
        // Declaring the Odin dependency is the declaration, routed or not: a
        // routed target that only publishes to Odin is Odin-correlated.
        let odin_dependency = generation.expected.dependencies.clone();
        expected.dependencies = odin_dependency.clone();
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::OdinCorrelated));
        expected.route = generation.expected.route.clone();
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::OdinCorrelated));
        // Providing the rendezvous is being Odin, whatever else is declared.
        expected.dependencies.clear();
        expected.route = None;
        provide_odin(&mut expected);
        assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::OdinSelf));
        Ok(())
    }

    #[test]
    fn warming_evidence_is_shape_checked_whatever_its_source() {
        let good = |evidence: &ReadinessEvidence| match evidence {
            ReadinessEvidence::RouteProof { evidence } => evidence.clone(),
            ReadinessEvidence::OdinCorrelated { .. } => unreachable!("a route-proof fixture"),
        };
        let presence = good(&route_proof_evidence());
        for warming in [
            WarmingEvidence::FirstOdinDirect { evidence: presence.clone() },
            WarmingEvidence::RouteProofDirect { evidence: presence.clone() },
            WarmingEvidence::OdinTopology { evidence: topology(3, 1) },
        ] {
            warming.validate_shape().expect("well-formed evidence");
        }
        let mut damaged = presence;
        damaged.canonical_bytes.push(1);
        for warming in [
            WarmingEvidence::FirstOdinDirect { evidence: damaged.clone() },
            WarmingEvidence::RouteProofDirect { evidence: damaged },
            WarmingEvidence::OdinTopology {
                evidence: TopologyEvidence {
                    canonical_sha256: "sha256:not-the-digest".into(),
                    ..topology(3, 1)
                },
            },
        ] {
            assert!(warming.validate_shape().is_err(), "{warming:?}");
        }
    }

    #[test]
    fn a_presence_disagreement_names_each_disagreement() {
        let shortfall = PresenceDisagrees {
            disagreements: vec![
                OdinTopologyDisagreement {
                    code: "capacity-below-minimum".into(),
                    expected: Some("2".into()),
                    observed: Some("1".into()),
                },
                OdinTopologyDisagreement {
                    code: "state".into(),
                    expected: None,
                    observed: None,
                },
            ],
        };
        assert_eq!(
            shortfall.to_string(),
            "route proof answered with a runtime that disagrees with current authority: \
             capacity-below-minimum (expected 2, observed 1); state (expected none, observed none)"
        );
    }

    fn route_proof_evidence() -> ReadinessEvidence {
        let canonical_bytes = vec![7, 7, 7];
        ReadinessEvidence::RouteProof {
            evidence: RuntimePresenceEvidence {
                canonical_sha256: sha256_id(&canonical_bytes),
                canonical_bytes,
                message_id: "challenge-1".into(),
                challenged_at_unix_millis: 10,
                admitted_at_unix_millis: 11,
            },
        }
    }

    #[test]
    fn route_proof_readiness_is_refused_for_a_target_that_declares_odin() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        let mut proof = generation.clone();
        proof.ready = route_proof_evidence();
        proof.latest_odin_observation = None;
        proof.odin_authority = None;
        // The fixture's Expected declares Odin, so route proof is not its class.
        assert!(proof.validate().is_err());

        // Mixed shapes are refused whatever the class.
        let mut mixed = generation.clone();
        mixed.latest_odin_observation = None;
        assert!(mixed.validate().is_err());
        let mut mixed = generation;
        mixed.odin_authority = None;
        assert!(mixed.validate().is_err());

        let mut transaction = fixture_transaction(FIXTURE_TRANSACTIONS[3].1)?.1;
        transaction.validate()?;
        transaction.ready = Some(route_proof_evidence());
        assert!(transaction.validate().is_err());
        Ok(())
    }

    #[test]
    fn route_supervision_state_exists_exactly_for_a_promoted_route() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        let mut missing = generation.clone();
        missing.route_supervision = None;
        assert!(missing.validate().is_err());

        let mut unrouted = generation;
        unrouted.route_supervision = None;
        unrouted.routing = RoutingEvidence::SkippedUnrouted;
        // Unrouted while Expected still names a route: refused for that reason
        // too, so the test above pins the supervision rule on its own.
        assert!(unrouted.validate().is_err());
        Ok(())
    }

    #[test]
    fn a_new_generation_starts_unobserved_and_undegraded() -> Result<()> {
        let committing = fixture_transaction(FIXTURE_TRANSACTIONS[4].1)?.1;
        let (_, incumbent) = fixture_generation(FIXTURE_GENERATION)?;
        let authority = incumbent
            .odin_authority
            .clone()
            .context("fixture generation has no Odin authority")?;
        let next = AdmittedGeneration::from_transaction(
            &committing,
            Some(authority),
            1_700_000_200_000,
        )?;
        assert_eq!(next.route_supervision, Some(RouteSupervisionState::default()));
        assert_eq!(next.last_error, None);
        Ok(())
    }

    #[test]
    fn phase_deadlines_follow_the_plan_and_only_cover_post_fencing_phases() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        let plan = &generation.plan;
        let defaults = plan.phase_deadlines();
        for (phase, seconds) in [
            (DeploymentPhase::Fencing, defaults.fencing_seconds),
            (DeploymentPhase::Leasing, defaults.leasing_seconds),
            (DeploymentPhase::AwaitingReady, defaults.awaiting_ready_seconds),
            (DeploymentPhase::Routing, defaults.routing_seconds),
            (DeploymentPhase::Committing, defaults.committing_seconds),
        ] {
            let deadline = PhaseDeadline::entering(phase, plan, 1_000).context("no deadline")?;
            assert_eq!(deadline.phase, phase);
            assert_eq!(deadline.entered_at_unix_millis, 1_000);
            assert_eq!(deadline.deadline_at_unix_millis, 1_000 + u64::from(seconds) * 1000);
        }
        for phase in [
            DeploymentPhase::Sealing,
            DeploymentPhase::Starting,
            DeploymentPhase::Warming,
            DeploymentPhase::Complete,
        ] {
            assert!(PhaseDeadline::entering(phase, plan, 1_000).is_none());
        }
        Ok(())
    }

    #[test]
    fn transition_stamps_the_deadline_of_the_phase_it_enters() -> Result<()> {
        let world = EngineFixture::new()?;
        // Routing, already holding the route receipt Committing requires.
        let (_, mut transaction) = fixture_transaction(FIXTURE_TRANSACTIONS[3].1)?;
        assert_eq!(transaction.phase, DeploymentPhase::Routing);
        transaction.routing = fixture_transaction(FIXTURE_TRANSACTIONS[4].1)?.1.routing;
        let envelope = transaction_envelope(&transaction, transaction.updated_at_unix_millis)?;
        let command = command_envelope(&command(CommandKind::Continuity), 100)?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentCommand::TYPE.into(),
                        key: command.key.clone(),
                        current: None,
                    },
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: transaction.transaction_id.clone(),
                        current: None,
                    },
                ],
                &[command, envelope],
            )?
        );
        let stored = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .next()
            .context("stored transaction vanished")?;
        world.engine.transition(&stored, DeploymentPhase::Committing)?;
        let advanced = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .next()
            .context("advanced transaction vanished")?;
        let deadline = advanced.value.phase_deadline.context("no deadline stamped")?;
        assert_eq!(deadline.phase, DeploymentPhase::Committing);
        assert_eq!(
            deadline.deadline_at_unix_millis - deadline.entered_at_unix_millis,
            u64::from(advanced.value.plan.as_ref().unwrap().phase_deadlines().committing_seconds)
                * 1000
        );
        assert_eq!(deadline.entered_at_unix_millis, advanced.value.updated_at_unix_millis);
        Ok(())
    }

    #[test]
    fn a_deadline_must_describe_the_current_post_fencing_phase() -> Result<()> {
        let (_, mut transaction) = fixture_transaction(FIXTURE_TRANSACTIONS[2].1)?;
        let plan = transaction.plan.clone().unwrap();
        let now = transaction.updated_at_unix_millis;
        transaction.phase_deadline =
            PhaseDeadline::entering(DeploymentPhase::AwaitingReady, &plan, now);
        transaction.validate()?;
        let mut wrong_phase = transaction.clone();
        wrong_phase.phase_deadline =
            PhaseDeadline::entering(DeploymentPhase::Routing, &plan, now);
        assert!(wrong_phase.validate().is_err());
        let mut backwards = transaction.clone();
        backwards.phase_deadline.as_mut().unwrap().deadline_at_unix_millis = now;
        assert!(backwards.validate().is_err());
        // Entry is not ordered against the wall-clock stamp of later writes.
        let mut entered_after_the_stamp = transaction;
        entered_after_the_stamp.phase_deadline =
            PhaseDeadline::entering(DeploymentPhase::AwaitingReady, &plan, now + 1);
        entered_after_the_stamp.validate()?;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // The phase machine, driven by the real Engine past Fencing.
    // ---------------------------------------------------------------------

    /// A workload port with no systemd behind it: observing a workload
    /// returns what was observed, stopping and discarding succeed, and
    /// anything that would launch a process refuses.
    struct StillWorkload;

    impl WorkloadPort for StillWorkload {
        fn install(
            &self,
            _: &CompiledDeploymentPlan,
            _: &crate::drivers::MaterializedRelease,
        ) -> Result<crate::drivers::InstalledReleaseObservation> {
            bail!("StillWorkload launches nothing")
        }
        fn prepare_activation(
            &self,
            _: &CompiledDeploymentPlan,
            _: &IdunnExpectedIncarnationRecord,
            _: IdunnRuntimeActivationLaunch,
        ) -> Result<IdunnRuntimeActivationRecord> {
            bail!("StillWorkload launches nothing")
        }
        fn start_prepared(
            &self,
            _: &CompiledDeploymentPlan,
            _: &SealedRelease,
            _: &crate::drivers::InstalledReleaseObservation,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
        ) -> Result<WorkloadObservation> {
            bail!("StillWorkload launches nothing")
        }
        fn discard_prepared(
            &self,
            _: &CompiledDeploymentPlan,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
        ) -> Result<()> {
            Ok(())
        }
        fn observe(
            &self,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
            prior: &WorkloadObservation,
        ) -> Result<WorkloadObservation> {
            Ok(prior.clone())
        }
        fn stop(&self, _: &WorkloadObservation) -> Result<()> {
            Ok(())
        }
        fn is_permanently_stopped(&self, _: &WorkloadObservation) -> Result<bool> {
            Ok(false)
        }
    }

    /// A Continuity transaction for an unrouted, stateless target with no
    /// incumbent: the shape most Engine tests need.
    fn transaction_at(
        world: &EngineFixture,
        phase: DeploymentPhase,
    ) -> Result<DeploymentTransaction> {
        seeded_transaction(world, phase, CommandKind::Continuity, None)
    }

    /// The Odin publisher sequence a transaction seeded over `incumbent`
    /// warms at; its Ready is the next one. Above anything an earlier
    /// generation of the world admitted.
    fn seeded_sequence(incumbent: Option<&AdmittedGeneration>) -> u64 {
        if incumbent.is_some() { 14 } else { 4 }
    }

    /// A released deployment brake receipt, signed by an operator identity
    /// enrolled in the world: what a Deploy's `deployment_authorization` holds.
    fn deployment_authorization(
        world: &EngineFixture,
        expected: &IdunnExpectedIncarnationRecord,
        transaction_id: &str,
        now: u64,
    ) -> Result<DeploymentAuthorization> {
        use cultnet_rs::{
            IDUNN_DEPLOYMENT_BRAKE_AUTHORITY, IDUNN_DEPLOYMENT_BRAKE_ID,
            IDUNN_DEPLOYMENT_BRAKE_SCOPE, IDUNN_DEPLOYMENT_RELEASE_PURPOSE,
            IdunnDeploymentBrakeReleasePurpose, enroll_service_identity_at,
        };
        let signer = enroll_service_identity_at::<IdunnDeploymentBrakeOperatorIdentity>(
            &world.root.join("identities/brake-operator.cc"),
        )?;
        export_service_identity_trust_anchor(&signer, &world.root.join("brake-anchor.cc"))?;
        let mut record = IdunnDeploymentBrakeRecord {
            schema_version: IDUNN_DEPLOYMENT_BRAKE_SCHEMA.into(),
            brake_id: IDUNN_DEPLOYMENT_BRAKE_ID.into(),
            authority: IDUNN_DEPLOYMENT_BRAKE_AUTHORITY.into(),
            runtime_id: expected.runtime_id.clone(),
            status: "released".into(),
            scope: IDUNN_DEPLOYMENT_BRAKE_SCOPE.into(),
            reason: "test rollout".into(),
            observed_at_unix_millis: now,
            expires_at_unix_millis: Some(now + 600_000),
            authorization_id: Some("authorization-test".into()),
            authorization_purpose: Some(IDUNN_DEPLOYMENT_RELEASE_PURPOSE.into()),
            authorized_release_id: Some(expected.sealed_release_id.clone()),
            authorized_deployment_id: Some(transaction_id.to_owned()),
            authorized_by: Some(signer.trust_anchor()?.identity_id),
            authorization_issued_at_unix_millis: Some(now),
            authorization_expires_at_unix_millis: Some(now + 600_000),
            signature_algorithm: Some("ed25519".into()),
            signature: None,
            private_state_exposed: false,
            updated_by: "operator/test".into(),
        };
        record.signature = Some(
            signer
                .sign::<IdunnDeploymentBrakeReleasePurpose>(&rmp_serde::to_vec(&record)?)
                .signature,
        );
        record.validate()?;
        let canonical_brake_bytes = rmp_serde::to_vec(&record)?;
        Ok(DeploymentAuthorization {
            authorization_id: "authorization-test".into(),
            brake_sha256: sha256_id(&canonical_brake_bytes),
            canonical_brake_bytes,
            authorized_at_unix_millis: now,
        })
    }

    /// What makes a target Odin: it provides the rendezvous.
    fn provide_odin(expected: &mut IdunnExpectedIncarnationRecord) {
        expected.capabilities.push(cultnet_rs::IdunnExpectedCapability {
            capability: ODIN_RENDEZVOUS_CAPABILITY.into(),
            schema: "odin.verse-topology.v1".into(),
            compatibility: "v1".into(),
            minimum_capacity: 1,
        });
    }

    /// The declaration that makes an unrouted fixture target Odin-correlated.
    /// Idunn infers no way to prove readiness, and a target with no route has
    /// nothing for Idunn to challenge, so the world says outright that Odin
    /// reports on it. The plan selects no provider: these worlds never plan
    /// against Odin, they only read its correlation, which names the provider
    /// Expected does.
    fn declare_odin(expected: &mut IdunnExpectedIncarnationRecord) {
        expected.dependencies.push(cultnet_rs::IdunnExpectedDependency {
            kind: "shared-infrastructure".into(),
            capability: ODIN_RENDEZVOUS_CAPABILITY.into(),
            schema: "odin.verse-topology.v1".into(),
            compatibility: "v1".into(),
            minimum_capacity: 1,
            startup: "before-promotion".into(),
            provider_id: Some("odin".into()),
            provider_authority: Some("managed-incarnation".into()),
            provider_expected_projection_sha256: Some(sha256_id(b"odin expected projection")),
            provider_endpoint: None,
        });
    }

    /// A transaction for an unrouted, stateless target, sitting at `phase`
    /// (Warming or later) with every earlier phase's evidence in place. The
    /// plan, sealed release, Expected, activation and Warming evidence are the
    /// real constructions; only the workload and isolation observations are
    /// borrowed from a recorded transaction.
    ///
    /// `incumbent` is an admitted generation already in the world's store. A
    /// Continuity over it restarts that generation's own release, so it shares
    /// the incumbent's Expected key. A Deploy over it builds a second
    /// incarnation. A Deploy carries what phases past Fencing never read but
    /// validation requires: a three-field frozen-source receipt and a
    /// well-formed release receipt from the brake.
    fn seeded_transaction(
        world: &EngineFixture,
        phase: DeploymentPhase,
        kind: CommandKind,
        incumbent: Option<&AdmittedGeneration>,
    ) -> Result<DeploymentTransaction> {
        use crate::deployment_plan::tests::{
            BINDING, RECIPE, artifact_receipt, external_input_receipt, source,
        };
        let provider_path = world.root.join("identities/provider.cc");
        let provider_anchor = world.root.join("identities/provider-anchor.cc");
        let provider = if provider_path.exists() {
            open_service_identity_at::<GameCultProviderHealthIdentity>(&provider_path)?
        } else {
            let provider =
                enroll_service_identity_at::<GameCultProviderHealthIdentity>(&provider_path)?;
            export_service_identity_trust_anchor(&provider, &provider_anchor)?;
            provider
        };

        let (binding_head, binding_tail) = BINDING
            .split_once("[route]")
            .context("binding has no route table")?;
        let binding = format!(
            "{binding_head}[brakes]{}",
            binding_tail
                .split_once("[brakes]")
                .context("binding has no brakes table")?
                .1
        )
        .replace(
            "/etc/gamecult/trust/service.cc",
            &provider_anchor.display().to_string(),
        )
        .replace("service-runtime-signer", &provider.entry().identity_id);
        let recipe = RECIPE
            .split("[[dependencies]]")
            .next()
            .context("recipe is empty")?
            .replace("route_required = true", "route_required = false")
            .replace(
                r#"["GAMECULT_IDUNN_CANDIDATE_BIND", "GAMECULT_IDUNN_RUNTIME_BUNDLE"]"#,
                r#"["GAMECULT_IDUNN_RUNTIME_BUNDLE"]"#,
            );
        let (plan, release) = match (kind, incumbent) {
            (CommandKind::Continuity, Some(incumbent)) => (
                incumbent.plan.clone(),
                incumbent.sealed_release.clone(),
            ),
            _ => {
                let incarnation = if incumbent.is_some() {
                    "service-incarnation-2"
                } else {
                    "service-incarnation-1"
                };
                let plan = compile_deployment_plan(
                    recipe.as_bytes(),
                    binding.as_bytes(),
                    source(&recipe),
                    incarnation,
                    None,
                    110,
                    &[],
                )?;
                let release = SealedRelease::new(
                    &plan,
                    vec![artifact_receipt()],
                    vec![external_input_receipt()],
                    120,
                )?;
                (plan, release)
            }
        };
        let mut expected = release.expected_projection(&plan)?;
        assert!(!expected.write_lease_required && expected.route.is_none());
        declare_odin(&mut expected);

        let now = now_millis()?;
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: format!(
                "{}-service{}",
                match kind {
                    CommandKind::Deploy => "up",
                    CommandKind::Continuity => "continuity",
                },
                if incumbent.is_some() { "-again" } else { "" }
            ),
            kind,
            selector: "service".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        let mut transaction = match (kind, incumbent) {
            (CommandKind::Continuity, Some(incumbent)) => {
                DeploymentTransaction::from_continuity(&command, incumbent, now)?
            }
            _ => DeploymentTransaction::new(&command, "service".into(), 0, incumbent, now)?,
        };
        let activation = IdunnRuntimeActivationLaunch::issue(
            &expected,
            runtime_instance_id(&transaction.transaction_id)?,
            now,
            &world.engine.idunn_signer,
        )?
        .activation()
        .clone();
        let mut workload = fixture_transaction(FIXTURE_TRANSACTIONS[0].1)?
            .1
            .workload
            .context("recorded transaction has no workload")?;
        match kind {
            CommandKind::Continuity => {
                transaction.lifecycle_authorized_at_unix_millis = Some(now);
            }
            CommandKind::Deploy => {
                transaction.frozen_source = Some(FrozenSourceReceipt {
                    transaction_id: transaction.transaction_id.clone(),
                    plan_id: plan.plan_id.clone(),
                    snapshot_sha256: sha256_id(b"frozen source snapshot"),
                });
                transaction.deployment_authorization = Some(deployment_authorization(
                    world,
                    &expected,
                    &transaction.transaction_id,
                    now,
                )?);
            }
        }
        transaction.expected_publication_sha256 = Some(expected.canonical_sha256()?);
        transaction.activation_publication_sha256 = Some(activation.canonical_sha256()?);
        match &mut workload {
            WorkloadObservation::Systemd(observed) => {
                observed.runtime_instance_id = activation.runtime_instance_id.clone();
                observed.executable_sha256 = expected.artifact_sha256.clone();
            }
            WorkloadObservation::Host(_) => bail!("recorded workload is not a systemd unit"),
        }
        let recorded = fixture_transaction(FIXTURE_TRANSACTIONS[0].1)?.1;
        transaction.isolation = recorded.isolation;
        transaction.installed_release = Some(crate::drivers::InstalledReleaseObservation {
            sealed_release_id: release.sealed_release_id.clone(),
            root: PathBuf::from("/srv/service/releases/test"),
        });
        transaction.sealed_release = Some(release);
        transaction.expected = Some(expected);
        transaction.activation = Some(activation);
        transaction.workload = Some(workload);
        transaction.plan = Some(plan);
        // Odin's first word about the candidate, before it was Ready.
        let sequence = seeded_sequence(incumbent);
        let warming = signed_correlation(world, &transaction, sequence, false)?;
        let authenticated = world.engine.authenticate_topology_bytes(
            &ControlSnapshot::read(&world.state_store)?,
            &transaction,
            &warming,
            None,
            now,
        )?;
        transaction.warming = Some(WarmingEvidence::OdinTopology {
            evidence: TopologyEvidence::from_authenticated(&authenticated, now)?,
        });
        transaction.odin_publisher_sequence_cursor = sequence;
        transaction.enter_phase(phase, now);
        transaction.validate()?;

        let store = SingleFileMessagePackBackingStore::new(&world.state_store);
        assert!(store.compare_exchange(
            &[
                CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command.command_id.clone(),
                    current: None,
                },
                CultCacheExpectedEnvelope {
                    r#type: DeploymentTransaction::TYPE.into(),
                    key: transaction.transaction_id.clone(),
                    current: None,
                },
            ],
            &[
                command_envelope(&command, command.requested_at_unix_millis)?,
                transaction_envelope(&transaction, now)?,
            ],
        )?);
        Ok(transaction)
    }

    /// Odin's correlation for the transaction's incarnation, signed by the key
    /// the Engine trusts and stamped now.
    fn signed_correlation(
        world: &EngineFixture,
        transaction: &DeploymentTransaction,
        sequence: u64,
        ready: bool,
    ) -> Result<Vec<u8>> {
        let expected = transaction.expected.as_ref().context("no Expected")?;
        let activation = transaction.activation.as_ref().context("no activation")?;
        let mut record = OdinRuntimeTopologyCorrelationRecord {
            schema_version: cultnet_rs::ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into(),
            target: expected.target.clone(),
            expected_projection_sha256: expected.canonical_sha256()?,
            expected: true,
            current_activation_sha256: Some(activation.canonical_sha256()?),
            signed_presence_sha256: Some(sha256_id(b"presence")),
            observed_presence_state: Some("active".into()),
            observed_presence_publisher_sequence: Some(sequence),
            observed_write_lease_sha256: None,
            observed_capabilities: expected
                .capabilities
                .iter()
                .map(|capability| cultnet_rs::GameCultRuntimeCapability {
                    capability: capability.capability.clone(),
                    schema: capability.schema.clone(),
                    compatibility: capability.compatibility.clone(),
                    capacity: 1,
                })
                .collect(),
            runtime_id: expected.runtime_id.clone(),
            runtime_instance_id: Some(activation.runtime_instance_id.clone()),
            present: true,
            ready,
            dependencies: expected
                .dependencies
                .iter()
                .map(|dependency| cultnet_rs::OdinRuntimeDependencyEvidence {
                    kind: dependency.kind.clone(),
                    capability: dependency.capability.clone(),
                    schema: dependency.schema.clone(),
                    compatibility: dependency.compatibility.clone(),
                    provider_id: dependency.provider_id.clone(),
                    provider_authority: dependency.provider_authority.clone(),
                    provider_expected_projection_sha256: dependency
                        .provider_expected_projection_sha256
                        .clone(),
                    provider_endpoint: dependency.provider_endpoint.clone(),
                    observed_capacity: Some(dependency.minimum_capacity),
                    provider_evidence_sha256: Some(sha256_id(b"odin provider evidence")),
                    ready: true,
                })
                .collect(),
            disagreements: Vec::new(),
            signer_identity_id: world.odin_signer.entry().identity_id.clone(),
            publisher_sequence: sequence,
            observed_at_unix_millis: now_millis()?,
            signature_algorithm: "ed25519".into(),
            signature: Vec::new(),
        };
        record.signature = world
            .odin_signer
            .sign::<OdinRuntimeTopologyCorrelationPurpose>(&record.unsigned_signature_payload()?)
            .signature;
        record.canonical_bytes()
    }

    /// Odin publishes its Ready correlation for the transaction incarnation.
    fn odin_reports_ready(
        world: &EngineFixture,
        transaction: &DeploymentTransaction,
        sequence: u64,
    ) -> Result<()> {
        let expected = transaction.expected.as_ref().context("no Expected")?;
        SingleFileMessagePackBackingStore::new(&world.engine.options.odin_correlation_store)
            .insert_entry_if_absent(CultCacheEnvelope {
                key: crate::drivers::incarnation_key_of(
                    &expected.target,
                    &expected.canonical_sha256()?,
                ),
                r#type: OdinRuntimeTopologyCorrelationRecord::TYPE.into(),
                payload: signed_correlation(world, transaction, sequence, true)?,
                stored_at: rfc3339_millis(now_millis()?)?,
                schema_id: Some(cultnet_rs::ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into()),
            })?;
        Ok(())
    }

    fn resident(world: &EngineFixture) -> Result<Stored<DeploymentTransaction>> {
        ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .next()
            .context("the transaction left the live set")
    }

    /// The transaction wherever it now lives: live, or archived once terminal.
    fn latest(world: &EngineFixture) -> Result<DeploymentTransaction> {
        match resident(world) {
            Ok(stored) => Ok(stored.value),
            Err(_) => read_history_transactions(&world.state_store)
                .into_iter()
                .next()
                .context("the transaction is in neither the live set nor history"),
        }
    }

    /// One engine step at a time until `stop` holds, recording each phase
    /// entered and checking after every step that the deadline on the record
    /// is exactly the current post-fencing phase's, and absent outside one.
    fn drive(
        world: &EngineFixture,
        stop: impl Fn(&DeploymentTransaction) -> bool,
    ) -> Result<Vec<DeploymentPhase>> {
        let mut phases = Vec::new();
        for _ in 0..40 {
            let Ok(current) = resident(world) else {
                return Ok(phases);
            };
            if stop(&current.value) {
                return Ok(phases);
            }
            world.engine.advance_transaction(&current)?;
            let after = latest(world)?;
            match &after.phase_deadline {
                Some(deadline) => assert_eq!(deadline.phase, after.phase),
                None => assert!(
                    !(DeploymentPhase::Fencing..=DeploymentPhase::Committing)
                        .contains(&after.phase),
                    "a post-fencing record lost its deadline"
                ),
            }
            if phases.last() != Some(&after.phase) {
                phases.push(after.phase);
            }
        }
        bail!("the transaction did not reach the stop condition in 40 steps")
    }

    #[test]
    fn an_admission_runs_from_fencing_to_complete_and_commits() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let seeded = transaction_at(&world, DeploymentPhase::Fencing)?;
        odin_reports_ready(&world, &seeded, 5)?;

        let phases = drive(&world, |transaction| {
            transaction.phase == DeploymentPhase::Complete
        })?;
        assert_eq!(
            phases,
            [
                DeploymentPhase::Fencing,
                DeploymentPhase::Leasing,
                DeploymentPhase::AwaitingReady,
                DeploymentPhase::Routing,
                DeploymentPhase::Committing,
                DeploymentPhase::Complete,
            ]
        );
        let finished = latest(&world)?;
        assert_eq!(finished.phase_deadline, None);
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::Admitted { .. })
        ));
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let admitted = snapshot
            .admitted_for("service")
            .context("commit wrote no admitted generation")?;
        assert_eq!(admitted.value.transaction_id, finished.transaction_id);
        assert_eq!(Some(&admitted.value.expected), seeded.expected.as_ref());
        Ok(())
    }

    #[test]
    fn a_pre_fence_abort_runs_to_complete_without_a_deadline() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        transaction_at(&world, DeploymentPhase::Warming)?;
        let warming = resident(&world)?;
        assert_eq!(warming.value.phase_deadline, None);

        world
            .engine
            .begin_pre_fencing_abort(&warming, anyhow!("candidate refused"))?;
        drive(&world, |transaction| transaction.completion.is_some())?;
        let finished = latest(&world)?;
        assert_eq!(finished.phase, DeploymentPhase::Complete);
        assert_eq!(finished.phase_deadline, None);
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::FailedBeforeFencing { .. })
        ));
        assert!(finished.is_terminal());
        Ok(())
    }

    #[test]
    fn a_post_fence_abort_runs_to_complete() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let seeded = transaction_at(&world, DeploymentPhase::Fencing)?;
        odin_reports_ready(&world, &seeded, 5)?;
        drive(&world, |transaction| {
            transaction.phase == DeploymentPhase::Routing
        })?;
        let routing = resident(&world)?;
        assert!(routing.value.phase_deadline.is_some());

        world
            .engine
            .begin_post_fencing_abort(&routing, anyhow!("candidate refused"))?;
        drive(&world, |transaction| transaction.completion.is_some())?;
        let finished = latest(&world)?;
        assert_eq!(finished.phase, DeploymentPhase::Complete);
        assert_eq!(finished.phase_deadline, None);
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::FailedAfterFencing { .. })
        ));
        assert!(
            ControlSnapshot::read(&world.state_store)?
                .admitted_for("service")
                .is_none(),
            "an aborted candidate was admitted"
        );
        Ok(())
    }

    #[test]
    fn entering_a_phase_owns_the_deadline_and_a_stepped_clock_cannot_wedge_it() -> Result<()> {
        let (_, mut transaction) = fixture_transaction(FIXTURE_TRANSACTIONS[4].1)?;
        let plan = transaction.plan.clone().unwrap();
        let base = transaction.created_at_unix_millis;
        transaction.enter_phase(DeploymentPhase::Routing, base + 1_000);
        let deadline = transaction.phase_deadline.context("no deadline on entry")?;
        assert_eq!(deadline.phase, DeploymentPhase::Routing);
        assert_eq!(
            deadline.deadline_at_unix_millis,
            base + 1_000 + u64::from(plan.phase_deadlines().routing_seconds) * 1000
        );
        // A same-phase write after the wall clock stepped backwards is still a
        // valid record: the deadline is fixed at entry, not ordered against
        // later stamps, and neither is the creation time.
        transaction.updated_at_unix_millis = deadline.entered_at_unix_millis - 1;
        transaction.validate()?;
        transaction.updated_at_unix_millis = base - 1;
        transaction.validate()?;
        transaction.enter_phase(DeploymentPhase::Committing, base + 2_000);
        assert_eq!(
            transaction.phase_deadline.unwrap().phase,
            DeploymentPhase::Committing
        );
        transaction.enter_phase(DeploymentPhase::Complete, base + 3_000);
        assert_eq!(transaction.phase_deadline, None);
        Ok(())
    }

    #[test]
    fn an_undecodable_history_record_is_counted_and_never_dropped_silently() -> Result<()> {
        let (finished, _) = terminal_transaction_with_command("ghostlight")?;
        let good = transaction_envelope(&finished, finished.updated_at_unix_millis)?;
        let mut torn = good.clone();
        torn.key = "tx-torn".into();
        // 0xc1 is msgpack's never-used byte: decodable by nothing.
        torn.payload = vec![0xc1];
        let mut foreign = good.clone();
        foreign.key = "tx-foreign".into();
        foreign.schema_id = Some(DEPLOYMENT_TRANSACTION_SCHEMA_V3.into());
        foreign.payload = vec![0xc1];

        let (decoded, undecodable) =
            decode_history_transactions(vec![good, torn, foreign]);
        assert_eq!(decoded, [finished]);
        assert_eq!(
            undecodable.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            ["tx-torn", "tx-foreign"]
        );
        assert!(undecodable.iter().all(|(_, error)| !error.is_empty()));
        Ok(())
    }

    /// The layout `IsolationEvidence` had before c848459 turned it into an
    /// untagged Linux/Host enum. That commit claimed old records decode
    /// unchanged; this is the check with the old shape itself.
    #[test]
    fn isolation_evidence_from_before_the_host_variant_still_decodes() -> Result<()> {
        #[derive(Serialize)]
        struct BeforeTheHostVariant {
            candidate_uid: u32,
            candidate_pid_namespace_id: u64,
            candidate_mount_namespace_id: u64,
            incumbent_uid: Option<u32>,
            incumbent_pid_namespace_id: Option<u64>,
            incumbent_mount_namespace_id: Option<u64>,
        }
        for incumbent in [None, Some(7)] {
            let old = BeforeTheHostVariant {
                candidate_uid: 61_000,
                candidate_pid_namespace_id: 4_026_531_836,
                candidate_mount_namespace_id: 4_026_531_841,
                incumbent_uid: incumbent,
                incumbent_pid_namespace_id: incumbent.map(|_| 8),
                incumbent_mount_namespace_id: incumbent.map(|_| 9),
            };
            for bytes in [rmp_serde::to_vec(&old)?, rmp_serde::to_vec_named(&old)?] {
                let decoded: IsolationEvidence = rmp_serde::from_slice(&bytes)?;
                let IsolationEvidence::Linux(linux) = decoded else {
                    bail!("an old Linux record decoded as a host record");
                };
                assert_eq!(linux.candidate_uid, 61_000);
                assert_eq!(linux.incumbent_uid, incumbent);
            }
        }
        Ok(())
    }

    #[test]
    fn an_admitted_odin_without_odin_authority_is_refused_not_replaced_by_the_bootstrap_key() -> Result<()> {
        let world = EngineFixture::new()?;
        let empty = ControlSnapshot::read(&world.state_store)?;
        assert_eq!(
            world.engine.current_odin_authority(&empty)?,
            world.engine.bootstrap_odin_authority
        );

        let (_, mut odin) = fixture_generation(FIXTURE_GENERATION)?;
        odin.target = "odin".into();
        // Odin is the target that provides the rendezvous, whatever it is named.
        odin.expected.target = "odin".into();
        odin.expected.dependencies.clear();
        provide_odin(&mut odin.expected);
        odin.odin_authority = Some(AdmittedOdinAuthority::from_anchor(
            &world.odin_signer.trust_anchor()?,
        )?);
        let mut with_authority = ControlSnapshot::default();
        with_authority.admitted.push(Stored {
            value: odin.clone(),
            envelope: CultCacheEnvelope {
                key: "odin".into(),
                r#type: AdmittedGeneration::TYPE.into(),
                payload: Vec::new(),
                stored_at: "1970-01-01T00:00:00.100Z".into(),
                schema_id: Some(ADMITTED_GENERATION_SCHEMA.into()),
            },
        });
        assert_eq!(
            world.engine.current_odin_authority(&with_authority)?,
            odin.odin_authority.clone().unwrap()
        );

        odin.odin_authority = None;
        let mut without = ControlSnapshot::default();
        without.admitted.push(Stored {
            value: odin.clone(),
            envelope: CultCacheEnvelope {
                key: "odin".into(),
                r#type: AdmittedGeneration::TYPE.into(),
                payload: Vec::new(),
                stored_at: "1970-01-01T00:00:00.100Z".into(),
                schema_id: Some(ADMITTED_GENERATION_SCHEMA.into()),
            },
        });
        let error = world
            .engine
            .current_odin_authority(&without)
            .expect_err("a route-proof Odin must not fall back to the bootstrap key");
        assert!(format!("{error:#}").contains("no Odin authority"), "{error:#}");
        Ok(())
    }

    fn error_text(result: Result<()>) -> String {
        format!("{:#}", result.expect_err("the record must be refused"))
    }

    /// A route-proof generation from a real admission: the world's Expected
    /// declares no Odin dependency, so route proof is its class.
    fn route_proof_generation() -> Result<AdmittedGeneration> {
        route_proof::committed_generation()
    }

    #[test]
    fn a_route_proof_generation_is_admitted_only_in_its_exact_shape() -> Result<()> {
        let generation = route_proof_generation()?;
        assert_eq!(generation.ready.voucher(), Voucher::Candidate);
        assert!(generation.odin_authority.is_none());
        assert!(generation.latest_odin_observation.is_none());
        assert!(generation.odin_receipts().is_err());

        // Route proof carries no Odin receipt and no Odin authority.
        let mut with_receipt = generation.clone();
        with_receipt.latest_odin_observation = Some(topology(1, 1));
        assert!(with_receipt.validate().is_err());
        let mut with_authority = generation.clone();
        with_authority.odin_authority = Some(AdmittedOdinAuthority {
            signer_identity_id: "odin-signer".into(),
            signer_public_key: vec![1; 32],
        });
        assert!(with_authority.validate().is_err());

        // Its readiness evidence is checked by shape.
        let ReadinessEvidence::RouteProof { evidence } = &generation.ready else {
            bail!("not route proof");
        };
        let mut tampered = generation.clone();
        tampered.ready = ReadinessEvidence::RouteProof {
            evidence: RuntimePresenceEvidence {
                canonical_sha256: sha256_id(b"another"),
                ..evidence.clone()
            },
        };
        assert!(error_text(tampered.validate()).contains("runtime presence evidence"));
        let mut untimed = generation.clone();
        untimed.ready = ReadinessEvidence::RouteProof {
            evidence: RuntimePresenceEvidence {
                admitted_at_unix_millis: evidence.challenged_at_unix_millis - 1,
                ..evidence.clone()
            },
        };
        assert!(error_text(untimed.validate()).contains("timeline"));
        Ok(())
    }

    #[test]
    fn odin_correlated_readiness_evidence_is_checked_by_shape() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        generation.validate()?;
        let ReadinessEvidence::OdinCorrelated { evidence } = &generation.ready else {
            bail!("fixture is not Odin-correlated");
        };
        for broken in [
            TopologyEvidence {
                canonical_sha256: sha256_id(b"another"),
                ..evidence.clone()
            },
            TopologyEvidence {
                publisher_sequence: 0,
                ..evidence.clone()
            },
            TopologyEvidence {
                admitted_at_unix_millis: 0,
                ..evidence.clone()
            },
        ] {
            let mut tampered = generation.clone();
            tampered.ready = ReadinessEvidence::OdinCorrelated { evidence: broken };
            assert!(tampered.validate().is_err());
        }
        Ok(())
    }

    #[test]
    fn lease_adoption_evidence_is_checked_by_shape_on_the_transaction() -> Result<()> {
        let (_, transaction) = fixture_transaction(FIXTURE_TRANSACTIONS[2].1)?;
        let adoption = LeaseAdoptionEvidence {
            write_lease_sha256: sha256_id(b"lease"),
            signed_presence_sha256: sha256_id(b"presence"),
            source: AdoptionSource::Direct,
            observed_at_unix_millis: 100,
        };
        let refused = |change: fn(&mut LeaseAdoptionEvidence)| {
            let mut adopting = transaction.clone();
            let mut evidence = adoption.clone();
            change(&mut evidence);
            adopting.lease_adoption = Some(evidence);
            error_text(adopting.validate())
        };
        assert!(refused(|evidence| evidence.write_lease_sha256.clear())
            .contains("adopted lease digest"));
        assert!(refused(|evidence| evidence.signed_presence_sha256 = "no spaces allowed".into())
            .contains("presence digest"));
        assert!(refused(|evidence| evidence.observed_at_unix_millis = 0)
            .contains("no observation time"));
        // Well-formed but naming no granted lease: the record is refused for
        // that reason and no other.
        assert!(refused(|_| {}).contains("does not name the granted lease"));
        Ok(())
    }

    #[test]
    fn route_supervision_is_checked_on_the_generation_and_the_meters_on_their_own_record() -> Result<()> {
        let (_, generation) = fixture_generation(FIXTURE_GENERATION)?;
        generation.validate()?;
        let supervised = |change: fn(&mut RouteSupervisionState)| {
            let mut tampered = generation.clone();
            change(tampered.route_supervision.as_mut().unwrap());
            error_text(tampered.validate())
        };
        assert!(supervised(|state| state.degraded_since_unix_millis = Some(0))
            .contains("no start time"));

        let meters = |change: fn(&mut TargetSupervision)| {
            let mut meters = TargetSupervision::new("service");
            change(&mut meters);
            error_text(meters.validate())
        };
        TargetSupervision::new("service").validate()?;
        assert!(meters(|m| m.route_actuations = vec![0]).contains("ascending"));
        assert!(meters(|m| m.route_actuations = vec![9, 5]).contains("ascending"));
        assert!(
            meters(|m| m.route_actuations = (1..=13).collect()).contains("exceeds its bound")
        );
        assert!(meters(|m| m.continuity_restarts = (1..=7).collect()).contains("exceeds its bound"));
        assert!(meters(|m| m.continuity_deferred_until = Some(5)).contains("time or a reason"));
        assert!(meters(|m| m.continuity_deferral_reason = Some("x".into())).contains("time or a reason"));
        assert!(meters(|m| m.schema_version = "idunn.target_supervision.v0".into())
            .contains("unsupported"));
        Ok(())
    }

    #[test]
    fn max_odin_sequence_counts_admitted_generations_by_target_and_signer() -> Result<()> {
        let (_, mut generation) = fixture_generation(FIXTURE_GENERATION)?;
        let authority = generation.odin_authority.clone().context("no authority")?;
        generation.odin_publisher_sequence_cursor = 88;
        let target = generation.target.clone();
        let snapshot = ControlSnapshot {
            commands: Vec::new(),
            transactions: Vec::new(),
            admitted: vec![Stored {
                envelope: CultCacheEnvelope {
                    key: target.clone(),
                    r#type: AdmittedGeneration::TYPE.into(),
                    payload: Vec::new(),
                    stored_at: "1970-01-01T00:00:00.100Z".into(),
                    schema_id: Some(ADMITTED_GENERATION_SCHEMA.into()),
                },
                value: generation,
            }],
            targets: Vec::new(),
        };
        assert_eq!(snapshot.max_odin_sequence(&target, &authority.signer_identity_id), 88);
        assert_eq!(snapshot.max_odin_sequence(&target, "another-signer"), 0);
        assert_eq!(
            snapshot.max_odin_sequence("another-target", &authority.signer_identity_id),
            0
        );
        Ok(())
    }

    #[test]
    fn a_store_holding_a_lifted_legacy_generation_reads_and_keeps_its_key_honest() -> Result<()> {
        let temp = TempDir::new()?;
        let store = temp.path().join("control.cc");
        let envelope = fixture_envelope(FIXTURE_GENERATION, AdmittedGeneration::TYPE)?;
        let backing = SingleFileMessagePackBackingStore::new(&store);
        backing.insert_entry_if_absent(envelope.clone())?;

        let snapshot = ControlSnapshot::read(&store)?;
        assert_eq!(snapshot.admitted.len(), 1);
        let lifted = &snapshot.admitted[0].value;
        assert_eq!(lifted.target, envelope.key);
        assert_eq!(lifted.ready.voucher(), Voucher::Odin);
        assert!(snapshot.admitted_for(&envelope.key).is_some());

        // The lifted record is held to the same key rule as a current one.
        let other = temp.path().join("misfiled.cc");
        let mut misfiled = envelope;
        misfiled.key = "not-its-target".into();
        SingleFileMessagePackBackingStore::new(&other).insert_entry_if_absent(misfiled)?;
        assert!(error_text(ControlSnapshot::read(&other).map(|_| ()))
            .contains("admitted generation key is not its target"));
        Ok(())
    }

    /// Replace Odin's one correlation for the incarnation with a newer one.
    fn odin_republishes(
        world: &EngineFixture,
        transaction: &DeploymentTransaction,
        sequence: u64,
    ) -> Result<()> {
        let path = &world.engine.options.odin_correlation_store;
        let store = SingleFileMessagePackBackingStore::new(path);
        let existing = store.pull_all_read_only_snapshot()?;
        assert!(store.delete_batch_if_unchanged(&existing)?);
        odin_reports_ready(world, transaction, sequence)
    }

    fn ready_sequence(transaction: &DeploymentTransaction) -> Option<u64> {
        transaction
            .ready
            .as_ref()
            .and_then(ReadinessEvidence::odin)
            .map(|evidence| evidence.publisher_sequence)
    }

    #[test]
    fn a_newer_odin_reading_refreshes_the_receipt_before_awaiting_ready_or_routing_advance()
    -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let seeded = transaction_at(&world, DeploymentPhase::Fencing)?;
        odin_reports_ready(&world, &seeded, 5)?;

        // AwaitingReady: the receipt is written, then confirmed, then the
        // phase advances only on a receipt equal to the latest reading.
        drive(&world, |transaction| {
            transaction.phase == DeploymentPhase::AwaitingReady
                && ready_sequence(transaction) == Some(5)
        })?;
        odin_republishes(&world, &seeded, 6)?;
        world.engine.advance_transaction(&resident(&world)?)?; // admits 6
        let admitted = resident(&world)?.value;
        assert_eq!(admitted.phase, DeploymentPhase::AwaitingReady);
        assert_eq!(admitted.latest_odin_observation.as_ref().unwrap().publisher_sequence, 6);
        assert_eq!(ready_sequence(&admitted), Some(5));
        world.engine.advance_transaction(&resident(&world)?)?; // refreshes Ready to 6
        let refreshed = resident(&world)?.value;
        assert_eq!(refreshed.phase, DeploymentPhase::AwaitingReady);
        assert_eq!(ready_sequence(&refreshed), Some(6));
        world.engine.advance_transaction(&resident(&world)?)?;
        assert_eq!(resident(&world)?.value.phase, DeploymentPhase::Routing);

        // Routing: route admission demands a receipt equal to the latest
        // reading, so a newer one is adopted before anything is routed.
        odin_republishes(&world, &seeded, 7)?;
        world.engine.advance_transaction(&resident(&world)?)?; // admits 7
        assert_eq!(ready_sequence(&resident(&world)?.value), Some(6));
        world.engine.advance_transaction(&resident(&world)?)?; // refreshes Ready to 7
        let routing = resident(&world)?.value;
        assert_eq!(routing.phase, DeploymentPhase::Routing);
        assert_eq!(ready_sequence(&routing), Some(7));
        assert!(routing.routing.is_none());
        world.engine.advance_transaction(&resident(&world)?)?;
        assert!(resident(&world)?.value.routing.is_some());
        Ok(())
    }

    #[test]
    fn lease_adoption_names_only_the_granted_lease() -> Result<()> {
        let lease = write_lease();
        let lease_sha256 = lease.canonical_sha256()?;
        let adoption = LeaseAdoptionEvidence {
            write_lease_sha256: lease_sha256.clone(),
            signed_presence_sha256: sha256_id(b"adopting-presence"),
            source: AdoptionSource::Direct,
            observed_at_unix_millis: 100,
        };
        adoption.validate_shape()?;
        let granted = LeasingEvidence::Granted {
            lease: lease.clone(),
            lease_sha256: lease_sha256.clone(),
        };
        assert!(adoption.names(&granted));
        // Prepared is not granted, stateless has nothing to adopt, and another
        // lease's digest is not this lease.
        assert!(!adoption.names(&LeasingEvidence::Prepared {
            lease,
            lease_sha256,
        }));
        assert!(!adoption.names(&LeasingEvidence::SkippedStateless));
        let mut other = adoption.clone();
        other.write_lease_sha256 = sha256_id(b"another-lease");
        assert!(!other.names(&granted));

        // On a transaction, adoption without a granted lease is refused.
        let (_, mut transaction) = fixture_transaction(FIXTURE_TRANSACTIONS[2].1)?;
        transaction.lease_adoption = Some(adoption);
        assert!(transaction.validate().is_err());
        Ok(())
    }

    #[test]
    fn an_operator_required_recovery_must_say_why() -> Result<()> {
        let (_, mut failed) = fixture_transaction(FIXTURE_TRANSACTIONS[5].1)?;
        let Some(TransactionCompletion::FailedAfterFencing { recovery, .. }) =
            failed.completion.as_mut()
        else {
            panic!("fixture is not a post-fencing failure")
        };
        *recovery = TerminalRecovery::OperatorRequired {
            reason: "adopted-lease".into(),
        };
        failed.validate()?;
        assert!(failed.is_terminal());
        let Some(TransactionCompletion::FailedAfterFencing { recovery, .. }) =
            failed.completion.as_mut()
        else {
            unreachable!()
        };
        *recovery = TerminalRecovery::OperatorRequired {
            reason: String::new(),
        };
        assert!(failed.validate().is_err());
        Ok(())
    }
    /// The persisted projection a continuity restart leaves behind, read at
    /// the file the daemon writes.
    struct ContinuityProjection {
        _temp: TempDir,
        topology: CultCacheTopologyDriver,
        provider_anchor: ServiceIdentityTrustAnchor,
        expected: IdunnExpectedIncarnationRecord,
        admitted_activation: IdunnRuntimeActivationRecord,
        candidate_activation: IdunnRuntimeActivationRecord,
    }

    impl ContinuityProjection {
        /// The production sequence: the admitted incarnation dies and is
        /// demoted to Expected-only, then the continuity candidate publishes
        /// its own activation under the same key once it is observed.
        fn after_candidate_observed() -> Result<Self> {
            let temp = TempDir::new()?;
            let root = temp.path();
            let idunn = enroll_service_identity_at::<IdunnServiceIdentity>(&root.join("idunn.cc"))?;
            let provider = enroll_service_identity_at::<GameCultProviderHealthIdentity>(
                &root.join("provider.cc"),
            )?;
            let provider_anchor = provider.trust_anchor()?;
            let mut expected = TopologyFixture::new("ghostlight")?.expected;
            expected.expected_signer_identity_id = provider.entry().identity_id.clone();
            expected.artifact_sha256 = sha256_id(&[1]);
            expected.validate()?;
            let topology = CultCacheTopologyDriver {
                projection_store: root.join("topology.cc"),
                correlation_store: root.join("correlation.cc"),
            };
            let admitted = workload(1, 40, 50);
            let candidate = workload(2, 41, 51);
            let issue = |observation: &WorkloadObservation| -> Result<_> {
                Ok(IdunnRuntimeActivationLaunch::issue(
                    &expected,
                    observation.runtime_instance_id().to_owned(),
                    NOW,
                    &idunn,
                )?
                .activation()
                .clone())
            };
            let admitted_activation = issue(&admitted)?;
            let candidate_activation = issue(&candidate)?;
            assert_ne!(admitted_activation, candidate_activation);

            topology.publish_expected(&expected, &provider_anchor)?;
            topology.publish_observed_activation(&expected, &admitted_activation, &admitted)?;
            topology.demote_to_expected_only(
                &expected,
                &provider_anchor,
                &admitted_activation,
                None,
            )?;
            topology.publish_expected(&expected, &provider_anchor)?;
            topology.publish_observed_activation(&expected, &candidate_activation, &candidate)?;
            Ok(Self {
                _temp: temp,
                topology,
                provider_anchor,
                expected,
                admitted_activation,
                candidate_activation,
            })
        }

        fn continuity_transaction(&self) -> Result<DeploymentTransaction> {
            let mut transaction = DeploymentTransaction::new(
                &command(CommandKind::Continuity),
                "ghostlight".into(),
                0,
                None,
                100,
            )?;
            transaction.expected = Some(self.expected.clone());
            transaction.activation = Some(self.candidate_activation.clone());
            Ok(transaction)
        }

        /// Types of the records projected under the shared incarnation key.
        fn projected_types(&self) -> Result<Vec<String>> {
            let key = incarnation_key(&self.expected)?;
            Ok(
                SingleFileMessagePackBackingStore::new(&self.topology.projection_store)
                    .pull_all_read_only_snapshot()?
                    .into_iter()
                    .filter(|entry| entry.key == key)
                    .map(|entry| entry.r#type)
                    .collect(),
            )
        }
    }

    #[test]
    fn a_failed_continuity_demotes_its_own_activation_and_keeps_the_expected() -> Result<()> {
        let world = ContinuityProjection::after_candidate_observed()?;
        assert!(
            world
                .projected_types()?
                .contains(&IdunnRuntimeActivationRecord::TYPE.to_owned())
        );
        let transaction = world.continuity_transaction()?;
        assert_eq!(
            transaction.abort_topology_reconciliation(),
            CleanupEvidence::Pending
        );
        assert_eq!(
            post_fencing_abort_intent(&transaction, "candidate died").topology_reconciliation,
            CleanupEvidence::Pending
        );

        reconcile_failed_candidate_projection(
            &world.topology,
            &transaction,
            &world.provider_anchor,
        )?;

        assert_eq!(
            world.projected_types()?,
            vec![IdunnExpectedIncarnationRecord::TYPE.to_owned()],
            "the shared key must keep its Expected and nothing else"
        );
        Ok(())
    }

    #[test]
    fn a_continuity_that_issued_no_activation_owes_no_projection_change() -> Result<()> {
        let world = ContinuityProjection::after_candidate_observed()?;
        let mut transaction = world.continuity_transaction()?;
        transaction.activation = None;
        assert_eq!(
            transaction.abort_topology_reconciliation(),
            CleanupEvidence::Skipped
        );
        assert_eq!(
            post_fencing_abort_intent(&transaction, "candidate never started")
                .topology_reconciliation,
            CleanupEvidence::Skipped
        );
        Ok(())
    }

    #[test]
    fn a_deploy_fencing_after_a_failed_continuity_resolves_with_the_admitted_activation()
    -> Result<()> {
        let world = ContinuityProjection::after_candidate_observed()?;
        reconcile_failed_candidate_projection(
            &world.topology,
            &world.continuity_transaction()?,
            &world.provider_anchor,
        )?;
        let settled = std::fs::read(&world.topology.projection_store)?;

        // The deploy's post-fence rollback demotes the admitted generation's
        // exact activation. The failed continuity left nothing standing, so it
        // resolves instead of refusing as substituted.
        world.topology.demote_to_expected_only(
            &world.expected,
            &world.provider_anchor,
            &world.admitted_activation,
            None,
        )?;
        assert_eq!(std::fs::read(&world.topology.projection_store)?, settled);
        Ok(())
    }

    #[test]
    fn no_path_adopts_an_activation_the_transaction_did_not_issue() -> Result<()> {
        let world = ContinuityProjection::after_candidate_observed()?;
        let before = std::fs::read(&world.topology.projection_store)?;

        // The candidate's activation is still projected. The admitted
        // generation's exact activation is not what stands there, so its
        // demotion refuses rather than taking the newer one.
        assert!(
            world
                .topology
                .demote_to_expected_only(
                    &world.expected,
                    &world.provider_anchor,
                    &world.admitted_activation,
                    None,
                )
                .is_err()
        );

        // Nor may a continuity transaction demote an activation it did not
        // issue.
        let mut foreign = world.continuity_transaction()?;
        foreign.activation = Some(world.admitted_activation.clone());
        assert!(
            reconcile_failed_candidate_projection(
                &world.topology,
                &foreign,
                &world.provider_anchor
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&world.topology.projection_store)?, before);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // B1 fix batch: the transition, the precondition, the boot reconciliation,
    // history that fails closed, and the abort paths through the Engine.
    // ---------------------------------------------------------------------

    /// A workload port whose process can be killed: observing it succeeds
    /// until `kill`, then fails the way a dead unit does. Everything else is
    /// `StillWorkload`'s.
    struct SwitchWorkload {
        alive: std::sync::atomic::AtomicBool,
    }

    impl SwitchWorkload {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                alive: std::sync::atomic::AtomicBool::new(true),
            })
        }

        fn kill(&self) {
            self.alive
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl WorkloadPort for SwitchWorkload {
        fn install(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &crate::drivers::MaterializedRelease,
        ) -> Result<crate::drivers::InstalledReleaseObservation> {
            StillWorkload.install(plan, release)
        }
        fn prepare_activation(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            launch: IdunnRuntimeActivationLaunch,
        ) -> Result<IdunnRuntimeActivationRecord> {
            StillWorkload.prepare_activation(plan, expected, launch)
        }
        fn start_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &SealedRelease,
            installed: &crate::drivers::InstalledReleaseObservation,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<WorkloadObservation> {
            StillWorkload.start_prepared(plan, release, installed, expected, activation)
        }
        fn discard_prepared(
            &self,
            _: &CompiledDeploymentPlan,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
        ) -> Result<()> {
            Ok(())
        }
        fn observe(
            &self,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
            prior: &WorkloadObservation,
        ) -> Result<WorkloadObservation> {
            ensure!(
                self.alive.load(std::sync::atomic::Ordering::SeqCst),
                "the workload is gone"
            );
            Ok(prior.clone())
        }
        fn stop(&self, _: &WorkloadObservation) -> Result<()> {
            Ok(())
        }
        fn is_permanently_stopped(&self, _: &WorkloadObservation) -> Result<bool> {
            Ok(false)
        }
    }

    /// Admit a generation for "service" through the real phase machine and
    /// retire its transaction, leaving the world with an incumbent.
    fn admit_incumbent(world: &EngineFixture) -> Result<AdmittedGeneration> {
        let seeded = transaction_at(world, DeploymentPhase::Fencing)?;
        odin_reports_ready(world, &seeded, seeded_sequence(None) + 1)?;
        drive(world, |transaction| {
            transaction.phase == DeploymentPhase::Complete
        })?;
        assert!(world.engine.retire_one_terminal_transaction()?);
        Ok(ControlSnapshot::read(&world.state_store)?
            .admitted_for("service")
            .context("commit wrote no admitted generation")?
            .value
            .clone())
    }

    #[test]
    fn an_admitted_odin_correlated_generation_takes_a_newer_odin_reading_once() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let seeded = transaction_at(&world, DeploymentPhase::Fencing)?;
        let first = seeded_sequence(None) + 1;
        odin_reports_ready(&world, &seeded, first)?;
        drive(&world, |transaction| transaction.phase == DeploymentPhase::Complete)?;
        assert!(world.engine.retire_one_terminal_transaction()?);
        let admitted = ControlSnapshot::read(&world.state_store)?
            .admitted_for("service")
            .context("commit wrote no admitted generation")?
            .value
            .clone();
        assert_eq!(admitted.readiness(), Ok(ReadinessClass::OdinCorrelated));
        assert_eq!(admitted.odin_publisher_sequence_cursor, first);

        // Nothing newer than what is held: nothing is written.
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let current = snapshot.admitted_for("service").context("no generation")?;
        let envelope = current.envelope.clone();
        assert!(!world.engine.refresh_admitted_topology(&snapshot, current)?);
        assert_eq!(incumbent_envelope(&world)?, envelope);

        // A newer reading is adopted, once.
        odin_republishes(&world, &seeded, first + 4)?;
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let current = snapshot.admitted_for("service").context("no generation")?;
        assert!(world.engine.refresh_admitted_topology(&snapshot, current)?);
        let refreshed = ControlSnapshot::read(&world.state_store)?
            .admitted_for("service")
            .context("no generation")?
            .value
            .clone();
        assert_eq!(refreshed.odin_publisher_sequence_cursor, first + 4);
        assert_eq!(
            refreshed.latest_odin_observation.as_ref().map(|latest| latest.publisher_sequence),
            Some(first + 4)
        );
        assert_eq!(refreshed.ready.odin().map(|ready| ready.publisher_sequence), Some(first + 4));
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let current = snapshot.admitted_for("service").context("no generation")?;
        assert!(!world.engine.refresh_admitted_topology(&snapshot, current)?);
        Ok(())
    }

    /// Change the incumbent in the store, compare-and-swap.
    fn edit_incumbent(
        world: &EngineFixture,
        edit: impl FnOnce(&mut AdmittedGeneration),
    ) -> Result<AdmittedGeneration> {
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let stored = snapshot
            .admitted_for("service")
            .context("no admitted generation")?;
        let mut next = stored.value.clone();
        edit(&mut next);
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: AdmittedGeneration::TYPE.into(),
                    key: "service".into(),
                    current: Some(stored.envelope.clone()),
                }],
                &[admitted_envelope(&next, now_millis()?)?],
            )?
        );
        Ok(next)
    }

    /// Replace or create a target's meters in the store.
    fn set_meters(world: &EngineFixture, meters: &TargetSupervision) -> Result<()> {
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        world.engine.write_target_supervision(
            snapshot.supervision_for(&meters.target),
            meters,
            now_millis()?,
        )
    }

    fn meters_of(world: &EngineFixture, target: &str) -> Result<TargetSupervision> {
        Ok(ControlSnapshot::read(&world.state_store)?.supervision_or_new(target))
    }

    /// Meters for "service" whose restarts happened these many milliseconds ago.
    fn restarted_ago(ages: &[u64]) -> Result<TargetSupervision> {
        let now = now_millis()?;
        let mut meters = TargetSupervision::new("service");
        meters.continuity_restarts = ages.iter().map(|age| now - age).collect();
        meters.continuity_restarts.sort_unstable();
        Ok(meters)
    }

    /// Drop every live transaction and command: what a finished restart leaves.
    fn clear_live_transactions(world: &EngineFixture) -> Result<()> {
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        let envelopes = snapshot
            .transactions
            .iter()
            .map(|stored| stored.envelope.clone())
            .chain(snapshot.commands.iter().map(|stored| stored.envelope.clone()))
            .collect::<Vec<_>>();
        if envelopes.is_empty() {
            return Ok(());
        }
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store)
                .delete_batch_if_unchanged(&envelopes)?
        );
        Ok(())
    }

    fn incumbent_envelope(world: &EngineFixture) -> Result<CultCacheEnvelope> {
        Ok(ControlSnapshot::read(&world.state_store)?
            .admitted_for("service")
            .context("no admitted generation")?
            .envelope
            .clone())
    }

    /// Publish what a transaction's candidate projects: its Expected and,
    /// when asked, its observed activation.
    fn project_candidate(
        world: &EngineFixture,
        transaction: &DeploymentTransaction,
        with_activation: bool,
    ) -> Result<()> {
        let expected = transaction.expected.as_ref().context("no Expected")?;
        let anchor = world
            .engine
            .provider_anchor_for_plan(transaction.plan.as_ref().context("no plan")?)?;
        let topology = world.engine.topology();
        topology.publish_expected(expected, &anchor)?;
        if with_activation {
            // The observation must name the release the Expected declares;
            // the borrowed workload is patched to.
            let mut workload = transaction.workload.clone().context("no workload")?;
            if let WorkloadObservation::Systemd(observed) = &mut workload {
                observed.executable_sha256 = expected.artifact_sha256.clone();
            }
            topology.publish_observed_activation(
                expected,
                transaction.activation.as_ref().context("no activation")?,
                &workload,
            )?;
        }
        Ok(())
    }

    /// Publish what an admitted generation projects while it runs.
    fn project_admitted(world: &EngineFixture, generation: &AdmittedGeneration) -> Result<()> {
        let anchor = world.engine.provider_anchor_for_plan(&generation.plan)?;
        let topology = world.engine.topology();
        topology.publish_expected(&generation.expected, &anchor)?;
        topology.publish_observed_activation(
            &generation.expected,
            &generation.activation,
            &generation.workload,
        )?;
        Ok(())
    }

    /// The types of the records projected under one incarnation key, sorted.
    fn projected_under(
        world: &EngineFixture,
        expected: &IdunnExpectedIncarnationRecord,
    ) -> Result<Vec<String>> {
        let path = &world.engine.options.topology_store;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let key = incarnation_key(expected)?;
        let mut types = SingleFileMessagePackBackingStore::new(path)
            .pull_all_read_only_snapshot()?
            .into_iter()
            .filter(|entry| entry.key == key)
            .map(|entry| entry.r#type)
            .collect::<Vec<_>>();
        types.sort();
        Ok(types)
    }

    /// One transaction by id, wherever it now lives. A failed transaction is
    /// archived the moment it finishes, so it is in history, not resident.
    fn record_of(world: &EngineFixture, transaction_id: &str) -> Result<DeploymentTransaction> {
        if let Some(stored) = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .find(|stored| stored.value.transaction_id == transaction_id)
        {
            return Ok(stored.value);
        }
        read_history_transactions(&world.state_store)
            .into_iter()
            .find(|transaction| transaction.transaction_id == transaction_id)
            .context("the transaction is in neither the live set nor history")
    }

    fn expected_only() -> Vec<String> {
        vec![IdunnExpectedIncarnationRecord::TYPE.to_owned()]
    }

    // ---- the Engine's abort paths, Deploy and incumbent ----

    #[test]
    fn a_deploy_that_fences_commits_and_retires_the_incumbent_it_replaced() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        admit_incumbent(&world)?;
        let meters = restarted_ago(&[600_000, 60_000])?;
        set_meters(&world, &meters)?;
        let incumbent = edit_incumbent(&world, |_| {})?;

        let candidate = seeded_transaction(
            &world,
            DeploymentPhase::Fencing,
            CommandKind::Deploy,
            Some(&incumbent),
        )?;
        odin_reports_ready(&world, &candidate, seeded_sequence(Some(&incumbent)) + 1)?;
        drive(&world, |transaction| {
            transaction.phase == DeploymentPhase::Complete
        })?;

        let admitted = ControlSnapshot::read(&world.state_store)?
            .admitted_for("service")
            .context("the deploy admitted nothing")?
            .value
            .clone();
        assert_eq!(admitted.transaction_id, candidate.transaction_id);
        assert_ne!(admitted.generation_id, incumbent.generation_id);
        // The restart log belongs to the target: a commit must not reset it.
        assert_eq!(meters_of(&world, "service")?, meters);
        assert_eq!(admitted.route_supervision, incumbent.route_supervision);

        // The commit did not stop the incumbent: it is retired after.
        let committed = record_of(&world, &candidate.transaction_id)?;
        let cleanup = committed.post_commit_cleanup.clone().context("no cleanup")?;
        assert_eq!(
            cleanup.incumbent,
            IncumbentCleanupEvidence::Pending {
                generation_id: incumbent.generation_id.clone(),
                workload: incumbent.workload.clone(),
            }
        );
        assert_eq!(cleanup.source, SourceCleanupEvidence::Pending);
        let frozen = world
            .engine
            .options
            .staging_root
            .join("frozen-sources")
            .join(&candidate.transaction_id);
        std::fs::create_dir_all(&frozen)?;
        std::fs::write(frozen.join("tree"), b"frozen")?;

        drive(&world, |transaction| transaction.is_terminal())?;
        let retired = record_of(&world, &candidate.transaction_id)?
            .post_commit_cleanup
            .context("no cleanup")?;
        assert_eq!(
            retired.incumbent,
            IncumbentCleanupEvidence::Complete {
                generation_id: incumbent.generation_id
            }
        );
        assert_eq!(retired.source, SourceCleanupEvidence::Complete);
        assert!(!frozen.exists(), "the frozen source outlived its deploy");
        Ok(())
    }

    #[test]
    fn a_deploy_post_fence_abort_cleans_up_and_restores_its_incumbent_exactly() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let incumbent = admit_incumbent(&world)?;
        project_admitted(&world, &incumbent)?;
        let candidate = seeded_transaction(
            &world,
            DeploymentPhase::Fencing,
            CommandKind::Deploy,
            Some(&incumbent),
        )?;
        project_candidate(&world, &candidate, true)?;
        let candidate_expected = candidate.expected.clone().context("no Expected")?;
        assert_eq!(
            projected_under(&world, &candidate_expected)?.len(),
            2,
            "the candidate did not project"
        );
        let frozen = world
            .engine
            .options
            .staging_root
            .join("frozen-sources")
            .join(&candidate.transaction_id);
        std::fs::create_dir_all(&frozen)?;
        std::fs::write(frozen.join("tree"), b"frozen")?;
        let before = incumbent_envelope(&world)?;

        world
            .engine
            .begin_post_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        assert!(resident(&world)?.value.post_fencing_abort.is_some());
        drive(&world, |transaction| transaction.completion.is_some())?;

        let finished = record_of(&world, &candidate.transaction_id)?;
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::FailedAfterFencing {
                recovery: TerminalRecovery::RestoreIncumbent,
                ..
            })
        ));
        let abort = finished.post_fencing_abort.context("no abort evidence")?;
        assert_eq!(abort.topology_reconciliation, CleanupEvidence::Complete);
        assert_eq!(abort.source_cleanup, CleanupEvidence::Complete);
        assert!(!frozen.exists(), "the abandoned source was not cleaned");
        assert!(
            projected_under(&world, &candidate_expected)?.is_empty(),
            "the abandoned candidate is still projected"
        );
        // Fencing stopped the incumbent: it stands as Expected-only, with the
        // exact activation it ran under withdrawn, ready for continuity.
        assert_eq!(projected_under(&world, &incumbent.expected)?, expected_only());
        assert_eq!(incumbent_envelope(&world)?, before);
        Ok(())
    }

    #[test]
    fn a_deploy_pre_fence_abort_cleans_up_and_leaves_its_incumbent_running() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let incumbent = admit_incumbent(&world)?;
        project_admitted(&world, &incumbent)?;
        let candidate = seeded_transaction(
            &world,
            DeploymentPhase::Warming,
            CommandKind::Deploy,
            Some(&incumbent),
        )?;
        project_candidate(&world, &candidate, true)?;
        let candidate_expected = candidate.expected.clone().context("no Expected")?;
        let frozen = world
            .engine
            .options
            .staging_root
            .join("frozen-sources")
            .join(&candidate.transaction_id);
        std::fs::create_dir_all(&frozen)?;
        std::fs::write(frozen.join("tree"), b"frozen")?;
        let before = incumbent_envelope(&world)?;

        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        let intent = resident(&world)?.value.pre_fencing_abort.context("no intent")?;
        assert_eq!(intent.topology_reconciliation, CleanupEvidence::Pending);
        assert_eq!(intent.source_cleanup, CleanupEvidence::Pending);
        drive(&world, |transaction| transaction.completion.is_some())?;

        let finished = record_of(&world, &candidate.transaction_id)?;
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::FailedBeforeFencing { .. })
        ));
        assert!(finished.is_terminal());
        assert!(!frozen.exists(), "the abandoned source was not cleaned");
        assert!(projected_under(&world, &candidate_expected)?.is_empty());
        // Before fencing a deployment never touched its incumbent.
        assert_eq!(
            projected_under(&world, &incumbent.expected)?,
            {
                let mut types = vec![
                    IdunnExpectedIncarnationRecord::TYPE.to_owned(),
                    IdunnRuntimeActivationRecord::TYPE.to_owned(),
                ];
                types.sort();
                types
            }
        );
        assert_eq!(incumbent_envelope(&world)?, before);
        Ok(())
    }

    #[test]
    fn a_continuity_abort_over_its_incumbent_demotes_only_the_activation_it_issued() -> Result<()> {
        for post_fence in [false, true] {
            let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
            let incumbent = admit_incumbent(&world)?;
            let candidate = seeded_transaction(
                &world,
                if post_fence {
                    DeploymentPhase::Fencing
                } else {
                    DeploymentPhase::Warming
                },
                CommandKind::Continuity,
                Some(&incumbent),
            )?;
            // The candidate shares the incumbent's key: same Expected, its own
            // activation.
            assert_eq!(candidate.expected.as_ref(), Some(&incumbent.expected));
            assert_ne!(candidate.activation.as_ref(), Some(&incumbent.activation));
            project_candidate(&world, &candidate, true)?;
            let before = incumbent_envelope(&world)?;

            let stored = resident(&world)?;
            if post_fence {
                world
                    .engine
                    .begin_post_fencing_abort(&stored, anyhow!("candidate died"))?;
                assert_eq!(
                    resident(&world)?
                        .value
                        .post_fencing_abort
                        .context("no intent")?
                        .topology_reconciliation,
                    CleanupEvidence::Pending
                );
            } else {
                world
                    .engine
                    .begin_pre_fencing_abort(&stored, anyhow!("candidate died"))?;
                assert_eq!(
                    resident(&world)?
                        .value
                        .pre_fencing_abort
                        .context("no intent")?
                        .topology_reconciliation,
                    CleanupEvidence::Pending
                );
            }
            drive(&world, |transaction| transaction.completion.is_some())?;

            assert_eq!(
                projected_under(&world, &incumbent.expected)?,
                expected_only(),
                "post_fence={post_fence}: the shared key must keep its Expected and nothing else"
            );
            let finished = record_of(&world, &candidate.transaction_id)?;
            assert!(finished.is_terminal());
            assert!(matches!(
                finished.completion,
                Some(
                    TransactionCompletion::FailedBeforeFencing { .. }
                        | TransactionCompletion::FailedAfterFencing { .. }
                )
            ));
            assert_eq!(incumbent_envelope(&world)?, before);
        }
        Ok(())
    }

    // ---- fix 1: the legacy lift owns the transition ----

    const FIXTURE_PRE_B1_ABORTS: [(&str, &str); 3] = [
        (
            "pre-fence abort in flight",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-pre-fence-abort.hex"),
        ),
        (
            "pre-fence abort already terminal",
            include_str!(
                "../tests/fixtures/idunn-control-legacy/transaction-pre-fence-abort-terminal.hex"
            ),
        ),
        (
            "post-fence abort in flight",
            include_str!("../tests/fixtures/idunn-control-legacy/transaction-post-fence-abort.hex"),
        ),
    ];
    const FIXTURE_PROVIDER_ANCHOR: &str =
        include_str!("../tests/fixtures/idunn-control-legacy/provider-anchor.hex");
    /// Where the fixtures' plan binding names the provider trust anchor.
    const FIXTURE_ANCHOR_PATH: &str = "/tmp/idunn-b1-legacy/provider-anchor.cc";

    fn hex_bytes(text: &str) -> Vec<u8> {
        let hex = text.trim();
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("hex fixture"))
            .collect()
    }

    fn write_fixture_anchor() -> Result<()> {
        let path = Path::new(FIXTURE_ANCHOR_PATH);
        std::fs::create_dir_all(path.parent().context("anchor path has no parent")?)?;
        std::fs::write(path, hex_bytes(FIXTURE_PROVIDER_ANCHOR))?;
        Ok(())
    }

    fn abort_owed(transaction: &DeploymentTransaction) -> Option<CleanupEvidence> {
        transaction
            .pre_fencing_abort
            .as_ref()
            .map(|abort| abort.topology_reconciliation)
            .or_else(|| {
                transaction
                    .post_fencing_abort
                    .as_ref()
                    .map(|abort| abort.topology_reconciliation)
            })
    }

    /// Put a continuity's record and its command in the control store as they
    /// stood: `envelope` is written as given, in whatever schema it is in.
    fn make_resident(
        world: &EngineFixture,
        transaction: &DeploymentTransaction,
        envelope: CultCacheEnvelope,
    ) -> Result<()> {
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: transaction.command_id.clone(),
            kind: CommandKind::Continuity,
            selector: "service".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentCommand::TYPE.into(),
                        key: command.command_id.clone(),
                        current: None,
                    },
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: envelope.key.clone(),
                        current: None,
                    },
                ],
                &[command_envelope(&command, 100)?, envelope],
            )?
        );
        Ok(())
    }

    #[test]
    fn a_pre_b1_continuity_abort_lifts_without_reopening_and_boot_resolves_its_residue()
    -> Result<()> {
        write_fixture_anchor()?;
        for (name, text) in FIXTURE_PRE_B1_ABORTS {
            let envelope = fixture_envelope(text, DeploymentTransaction::TYPE)?;

            // The record is what the old rule wrote: an activation was issued
            // and the abort says it owes the projection nothing.
            let legacy: LegacyDeploymentTransaction = rmp_serde::from_slice(&envelope.payload)?;
            assert_eq!(legacy.command_kind, CommandKind::Continuity, "{name}");
            assert!(legacy.activation.is_some(), "{name}");

            // History describes what happened, so it lifts the record as written.
            let archived = lift_legacy_transaction(&envelope)?;
            assert_eq!(abort_owed(&archived), Some(CleanupEvidence::Skipped), "{name}");

            // The control store lifts it without reopening anything.
            let lifted = read_transaction_record(&envelope)?;
            let terminal = archived.completion.is_some();
            if terminal {
                assert!(
                    abort_owed(&lifted).is_some_and(CleanupEvidence::is_legacy_marker),
                    "{name}"
                );
                assert!(lifted.is_terminal(), "{name}: the lift reopened a finished record");
                assert_eq!(lifted.phase, archived.phase, "{name}");
                assert_eq!(lifted.completion, archived.completion, "{name}");
                assert!(!lifted.owns_target_authority(), "{name}");
            } else {
                assert_eq!(abort_owed(&lifted), Some(CleanupEvidence::Pending), "{name}");
                assert!(!lifted.is_terminal(), "{name}");
                assert_eq!(lifted.completion, None, "{name}");
            }

            // Boot: the store reads.
            let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
            make_resident(&world, &lifted, envelope.clone())?;
            assert_eq!(migrate_control_store_to_current_schema(&world.state_store)?, 1);
            let snapshot = ControlSnapshot::read(&world.state_store)?;
            assert_eq!(snapshot.transactions.len(), 1, "{name}");
            project_candidate(&world, &lifted, true)?;
            let expected = lifted.expected.clone().context("no Expected")?;
            assert_eq!(projected_under(&world, &expected)?.len(), 2, "{name}");

            if terminal {
                // A finished record owes nothing and is not touched: boot
                // reconciliation demotes the residue by its exact activation.
                let before = resident(&world)?.envelope;
                let outcome = world.engine.reconcile_failed_continuity_projections()?;
                assert_eq!(outcome.demoted, vec![lifted.transaction_id.clone()], "{name}");
                assert_eq!(resident(&world)?.envelope, before, "{name}");
            } else {
                // A record in flight resolves through its own abort.
                for _ in 0..12 {
                    let Ok(current) = resident(&world) else { break };
                    if current.value.completion.is_some() {
                        break;
                    }
                    world.engine.advance_transaction(&current)?;
                }
                assert!(
                    record_of(&world, &envelope.key)?.completion.is_some(),
                    "{name}: the abort never resolved"
                );
            }
            assert_eq!(
                projected_under(&world, &expected)?,
                expected_only(),
                "{name}: the projection was not demoted to Expected-only"
            );
        }
        Ok(())
    }

    #[test]
    fn resident_terminal_pre_b1_aborts_never_claim_target_authority() -> Result<()> {
        write_fixture_anchor()?;
        let envelope = fixture_envelope(FIXTURE_PRE_B1_ABORTS[1].1, DeploymentTransaction::TYPE)?;
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let live = seeded_transaction(&world, DeploymentPhase::Warming, CommandKind::Deploy, None)?;
        let first = read_transaction_record(&envelope)?;
        assert_eq!(first.target, live.target);
        make_resident(&world, &first, envelope)?;
        assert_eq!(migrate_control_store_to_current_schema(&world.state_store)?, 1);

        // A second resident abort of the same shape for the same target,
        // already written as the marker the migration persists.
        let mut second = first.clone();
        second.transaction_id = "tx-second-legacy".into();
        second.command_id = "continuity-second-legacy".into();
        make_resident(
            &world,
            &second,
            cleanup_evidence::last_error_envelope(&second, "second")?,
        )?;

        // The store reads: three records, one of them live, none of the
        // finished ones claiming the target.
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        assert_eq!(snapshot.transactions.len(), 3);
        let live_before = snapshot
            .transactions
            .iter()
            .find(|stored| stored.value.transaction_id == live.transaction_id)
            .context("live transaction")?
            .envelope
            .clone();

        // Reconciliation cleans the residue once, by exact activation.
        let expected = first.expected.clone().context("no Expected")?;
        project_candidate(&world, &first, true)?;
        assert_eq!(projected_under(&world, &expected)?.len(), 2);
        let outcome = world.engine.reconcile_failed_continuity_projections()?;
        assert_eq!(outcome.demoted.len(), 1);
        assert_eq!(projected_under(&world, &expected)?, expected_only());
        let after = ControlSnapshot::read(&world.state_store)?;
        assert_eq!(
            after
                .transactions
                .iter()
                .find(|stored| stored.value.transaction_id == live.transaction_id)
                .context("live transaction")?
                .envelope,
            live_before
        );
        Ok(())
    }

    #[test]
    fn boot_never_republishes_a_withdrawn_incarnation_and_history_keeps_one_copy() -> Result<()> {
        write_fixture_anchor()?;
        let envelope = fixture_envelope(FIXTURE_PRE_B1_ABORTS[1].1, DeploymentTransaction::TYPE)?;
        let lifted = read_transaction_record(&envelope)?;
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        make_resident(&world, &lifted, envelope.clone())?;
        // A crash between the two archive steps: history already holds it.
        assert!(
            SingleFileMessagePackBackingStore::new(&history_store_path(&world.state_store))
                .insert_entry_if_absent(envelope)?
        );
        assert_eq!(migrate_control_store_to_current_schema(&world.state_store)?, 1);

        // The incarnation was withdrawn: nothing is projected for it.
        let expected = lifted.expected.clone().context("no Expected")?;
        assert!(projected_under(&world, &expected)?.is_empty());
        for _ in 0..3 {
            world.engine.run_scheduler_tick()?;
        }
        assert!(
            world
                .engine
                .reconcile_failed_continuity_projections()?
                .demoted
                .is_empty()
        );
        assert!(
            projected_under(&world, &expected)?.is_empty(),
            "a withdrawn incarnation's Expected was published again"
        );

        // One copy in history, finished, and nothing resident.
        let history = read_history(&world.state_store)?;
        assert_eq!(history.transactions.len(), 1);
        assert!(history.transactions[0].is_terminal());
        assert!(ControlSnapshot::read(&world.state_store)?.transactions.is_empty());
        Ok(())
    }

    #[test]
    fn boot_runs_the_projection_reconciliation_serve_starts_from() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let incumbent = admit_incumbent(&world)?;
        let failed = seeded_transaction(
            &world,
            DeploymentPhase::Warming,
            CommandKind::Continuity,
            Some(&incumbent),
        )?;
        project_candidate(&world, &failed, true)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate died"))?;
        drive(&world, |transaction| transaction.completion.is_some())?;
        // A pre-B1 abort's residue: the failed candidate's activation stands.
        project_candidate(&world, &failed, true)?;
        assert_eq!(projected_under(&world, &incumbent.expected)?.len(), 2);

        let (_lock, _engine) = boot(world.engine.options.clone())?;
        assert_eq!(projected_under(&world, &incumbent.expected)?, expected_only());
        Ok(())
    }

    /// A workload whose candidate can neither be stopped nor recover: an abort
    /// that cannot finish its first step.
    struct WedgedWorkload;

    impl WorkloadPort for WedgedWorkload {
        fn install(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &crate::drivers::MaterializedRelease,
        ) -> Result<crate::drivers::InstalledReleaseObservation> {
            StillWorkload.install(plan, release)
        }
        fn prepare_activation(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            launch: IdunnRuntimeActivationLaunch,
        ) -> Result<IdunnRuntimeActivationRecord> {
            StillWorkload.prepare_activation(plan, expected, launch)
        }
        fn start_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &SealedRelease,
            installed: &crate::drivers::InstalledReleaseObservation,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<WorkloadObservation> {
            StillWorkload.start_prepared(plan, release, installed, expected, activation)
        }
        fn discard_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<()> {
            StillWorkload.discard_prepared(plan, expected, activation)
        }
        fn observe(
            &self,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
            prior: &WorkloadObservation,
        ) -> Result<WorkloadObservation> {
            StillWorkload.observe(expected, activation, prior)
        }
        fn stop(&self, _: &WorkloadObservation) -> Result<()> {
            bail!("the unit will not stop")
        }
        fn is_permanently_stopped(&self, _: &WorkloadObservation) -> Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn one_wedged_pre_fence_abort_does_not_stop_the_scheduler() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(WedgedWorkload))?;
        let wedged = transaction_at(&world, DeploymentPhase::Warming)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: "up-nowhere".into(),
            kind: CommandKind::Deploy,
            selector: "nowhere".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command.command_id.clone(),
                    current: None,
                }],
                &[command_envelope(&command, 100)?],
            )?
        );

        // Recording the error is not progress, and neither is repeating it.
        assert!(!world.engine.resume_one_transaction()?);
        assert!(!world.engine.resume_one_transaction()?);
        // Every tick answers: the wedged abort records its error and waits,
        // and the rest of the scheduler goes on to freeze the queued command.
        write_service_binding(&world)?;
        for _ in 0..6 {
            world.engine.run_scheduler_tick()?;
        }
        let stuck = record_of(&world, &wedged.transaction_id)?;
        assert!(!stuck.is_terminal());
        assert!(stuck.post_fencing_abort.is_none());
        assert!(stuck.last_error.is_some());
        assert!(
            read_history_transactions(&world.state_store)
                .iter()
                .any(|transaction| transaction.command_id == command.command_id),
            "the queued command was never frozen"
        );
        Ok(())
    }

    #[test]
    fn a_wedged_post_fence_abort_only_waits() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(WedgedWorkload))?;
        let dead = transaction_at(&world, DeploymentPhase::Fencing)?;
        world
            .engine
            .begin_post_fencing_abort(&resident(&world)?, anyhow!("candidate died"))?;

        // The abort cannot stop the unit. The abort is already durable, so
        // that is a resumable error, and the tick answers every time.
        for _ in 0..3 {
            world.engine.run_scheduler_tick()?;
        }
        let stuck = record_of(&world, &dead.transaction_id)?;
        assert!(!stuck.is_terminal());
        assert!(stuck.last_error.is_some());
        Ok(())
    }

    /// A wedged abort whose `stop()` error carries a changing counter, as
    /// systemctl output with a PID or timestamp would.
    struct ChangingErrorWorkload(std::sync::atomic::AtomicU32, std::sync::atomic::AtomicBool);

    impl WorkloadPort for ChangingErrorWorkload {
        fn install(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &crate::drivers::MaterializedRelease,
        ) -> Result<crate::drivers::InstalledReleaseObservation> {
            WedgedWorkload.install(plan, release)
        }
        fn prepare_activation(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            launch: IdunnRuntimeActivationLaunch,
        ) -> Result<IdunnRuntimeActivationRecord> {
            WedgedWorkload.prepare_activation(plan, expected, launch)
        }
        fn start_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &SealedRelease,
            installed: &crate::drivers::InstalledReleaseObservation,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<WorkloadObservation> {
            WedgedWorkload.start_prepared(plan, release, installed, expected, activation)
        }
        fn discard_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<()> {
            WedgedWorkload.discard_prepared(plan, expected, activation)
        }
        fn observe(
            &self,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
            prior: &WorkloadObservation,
        ) -> Result<WorkloadObservation> {
            WedgedWorkload.observe(expected, activation, prior)
        }
        fn stop(&self, _: &WorkloadObservation) -> Result<()> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
            bail!("the unit will not stop (main pid {n})")
        }
        fn is_permanently_stopped(&self, _: &WorkloadObservation) -> Result<bool> {
            Ok(true)
        }
    }

    fn queue_deploy_for_nowhere(world: &EngineFixture) -> Result<DeploymentCommand> {
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: "up-nowhere".into(),
            kind: CommandKind::Deploy,
            selector: "nowhere".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command.command_id.clone(),
                    current: None,
                }],
                &[command_envelope(&command, 100)?],
            )?
        );
        Ok(command)
    }

    #[test]
    fn a_wedge_whose_error_text_changes_every_tick_does_not_starve_other_targets() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(ChangingErrorWorkload(
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicBool::new(false),
        )))?;
        transaction_at(&world, DeploymentPhase::Warming)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        let command = queue_deploy_for_nowhere(&world)?;
        write_service_binding(&world)?;
        for _ in 0..20 {
            world.engine.run_scheduler_tick()?;
        }
        assert!(
            read_history_transactions(&world.state_store)
                .iter()
                .any(|transaction| transaction.command_id == command.command_id),
            "the queued command was starved by another target's wedge"
        );
        Ok(())
    }

    #[test]
    fn rewriting_last_error_is_not_progress() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let before = transaction_at(&world, DeploymentPhase::Warming)?;
        let mut noted = before.clone();
        noted.last_error = Some("the unit will not stop (main pid 7)".into());
        noted.updated_at_unix_millis += 1;
        assert!(!state_advanced(&before, &noted));
        let mut moved = noted.clone();
        moved.phase = DeploymentPhase::AwaitingReady;
        assert!(state_advanced(&before, &moved));
        Ok(())
    }

    #[test]
    fn a_failing_step_is_retried_after_a_backoff_not_every_tick() -> Result<()> {
        let workload = Arc::new(ChangingErrorWorkload(
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicBool::new(false),
        ));
        let world = EngineFixture::with_workload(workload.clone())?;
        transaction_at(&world, DeploymentPhase::Warming)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        for _ in 0..10 {
            world.engine.run_scheduler_tick()?;
        }
        assert_eq!(
            workload.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the wedged step was retried inside its backoff"
        );
        Ok(())
    }

    /// A candidate that fails to advance and is dead for good.
    struct DeadCandidateWorkload;

    impl WorkloadPort for DeadCandidateWorkload {
        fn install(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &crate::drivers::MaterializedRelease,
        ) -> Result<crate::drivers::InstalledReleaseObservation> {
            StillWorkload.install(plan, release)
        }
        fn prepare_activation(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            launch: IdunnRuntimeActivationLaunch,
        ) -> Result<IdunnRuntimeActivationRecord> {
            StillWorkload.prepare_activation(plan, expected, launch)
        }
        fn start_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            release: &SealedRelease,
            installed: &crate::drivers::InstalledReleaseObservation,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<WorkloadObservation> {
            StillWorkload.start_prepared(plan, release, installed, expected, activation)
        }
        fn discard_prepared(
            &self,
            plan: &CompiledDeploymentPlan,
            expected: &IdunnExpectedIncarnationRecord,
            activation: &IdunnRuntimeActivationRecord,
        ) -> Result<()> {
            StillWorkload.discard_prepared(plan, expected, activation)
        }
        fn observe(
            &self,
            _: &IdunnExpectedIncarnationRecord,
            _: &IdunnRuntimeActivationRecord,
            _: &WorkloadObservation,
        ) -> Result<WorkloadObservation> {
            bail!("the candidate is gone")
        }
        fn stop(&self, _: &WorkloadObservation) -> Result<()> {
            Ok(())
        }
        fn is_permanently_stopped(&self, _: &WorkloadObservation) -> Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn a_permanently_dead_candidate_past_the_fence_begins_its_post_fence_abort() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(DeadCandidateWorkload))?;
        let dead = transaction_at(&world, DeploymentPhase::Fencing)?;
        assert!(record_of(&world, &dead.transaction_id)?.post_fencing_abort.is_none());
        for _ in 0..8 {
            world.engine.resume_one_transaction()?;
        }
        assert!(
            record_of(&world, &dead.transaction_id)?
                .post_fencing_abort
                .is_some(),
            "a dead candidate past the fence was retried instead of aborted"
        );
        Ok(())
    }

    #[test]
    fn a_resume_fault_is_reported_once_and_left_in_the_record() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let live = transaction_at(&world, DeploymentPhase::Warming)?;
        let error = anyhow!("store hiccup 1");
        world
            .engine
            .note_fault("could not resume transaction", &live.transaction_id, &error);
        assert_eq!(
            record_of(&world, &live.transaction_id)?.last_error.as_deref(),
            Some("store hiccup 1")
        );
        let reports = world.engine.fault_reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[&live.transaction_id].offer(Some("store hiccup 1".into())).is_none());
        Ok(())
    }

    #[test]
    fn a_new_record_cannot_carry_the_legacy_marker() -> Result<()> {
        write_fixture_anchor()?;
        let envelope = fixture_envelope(FIXTURE_PRE_B1_ABORTS[1].1, DeploymentTransaction::TYPE)?;
        let lifted = read_transaction_record(&envelope)?;
        assert!(lifted.carries_legacy_marker());
        let error = error_text(transaction_envelope(&lifted, 1).map(|_| ()));
        assert!(error.contains("legacy lift"), "{error}");
        // The migration's own path still writes it, and it reads back.
        let (_, migrated) = cleanup_evidence::migrate_transaction_record(&envelope)?;
        assert!(read_transaction_record(&migrated)?.carries_legacy_marker());
        Ok(())
    }

    #[test]
    fn unreadable_history_does_not_skip_resident_residue_at_boot() -> Result<()> {
        write_fixture_anchor()?;
        let envelope = fixture_envelope(FIXTURE_PRE_B1_ABORTS[1].1, DeploymentTransaction::TYPE)?;
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let first = read_transaction_record(&envelope)?;
        make_resident(&world, &first, envelope)?;
        assert_eq!(migrate_control_store_to_current_schema(&world.state_store)?, 1);
        let expected = first.expected.clone().context("no Expected")?;
        project_candidate(&world, &first, true)?;
        assert_eq!(projected_under(&world, &expected)?.len(), 2);
        corrupt_history(&world)?;
        let outcome = world.engine.reconcile_failed_continuity_projections()?;
        assert_eq!(outcome.demoted.len(), 1);
        assert_eq!(projected_under(&world, &expected)?, expected_only());
        Ok(())
    }

    #[test]
    fn backoff_doubles_caps_and_waits_exactly() {
        assert_eq!(backoff_wait(500, 0), 500);
        assert_eq!(backoff_wait(500, 1), 1000);
        assert_eq!(backoff_wait(500, 2), 2000);
        assert_eq!(backoff_wait(500, 3), 4000);
        assert_eq!(backoff_wait(500, 30), RESUME_BACKOFF_CEILING_MILLIS);
        assert_eq!(backoff_wait(u64::MAX, 5), RESUME_BACKOFF_CEILING_MILLIS);
        assert!(is_waiting(9, 10, 5));
        assert!(!is_waiting(10, 10, 5));
        assert!(!is_waiting(11, 10, 5));
        // Due further ahead than the longest wait the owner can set: the clock
        // stepped back, so the attempt is due, not stalled for the step.
        assert!(is_waiting(5, 10, 5));
        assert!(!is_waiting(4, 10, 5));
    }

    #[test]
    fn a_resume_backoff_set_by_a_stepped_back_clock_is_due_and_a_real_one_waits() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let id = transaction_at(&world, DeploymentPhase::Warming)?.transaction_id;
        let not_before = |world: &EngineFixture| {
            world
                .engine
                .resume_backoff
                .lock()
                .unwrap()
                .get(&id)
                .map(|backoff| backoff.not_before_unix_millis)
        };
        let set = |world: &EngineFixture, not_before_unix_millis: u64| {
            world.engine.resume_backoff.lock().unwrap().insert(
                id.clone(),
                ResumeBackoff {
                    failures: 3,
                    not_before_unix_millis,
                },
            );
        };

        // Half a minute ahead is a wait the backoff can set: the step is left alone.
        let waiting = now_millis()? + 30_000;
        set(&world, waiting);
        let _ = world.engine.resume_candidate(&resident(&world)?);
        assert_eq!(not_before(&world), Some(waiting));

        // A day ahead is a clock that stepped back: the step is attempted, and
        // whatever it does, it replaces the backoff.
        let ahead = now_millis()? + 86_400_000;
        set(&world, ahead);
        let _ = world.engine.resume_candidate(&resident(&world)?);
        assert!(not_before(&world).is_none_or(|due| due < ahead));
        Ok(())
    }

    #[test]
    fn a_persistent_fault_is_reported_once_and_again_only_after_recovery() -> Result<()> {
        let workload = Arc::new(ChangingErrorWorkload(
            std::sync::atomic::AtomicU32::new(0),
            std::sync::atomic::AtomicBool::new(false),
        ));
        let world = EngineFixture::with_workload(workload.clone())?;
        let wedged = transaction_at(&world, DeploymentPhase::Warming)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate refused"))?;
        let id = wedged.transaction_id;
        let expire = || {
            world.engine.resume_backoff.lock().unwrap().clear();
        };
        let reported = || world.engine.fault_reports.lock().unwrap().contains_key(&id);
        // A fault already reported for this transaction (an Err from the
        // resume itself); a handled, still-failing step must not re-arm it.
        world
            .engine
            .note_fault("could not resume transaction", &id, &anyhow!("store hiccup"));
        assert!(reported());
        world.engine.resume_one_transaction()?;
        assert!(reported(), "a failing handled step re-armed the report");
        // Skipped inside the backoff: the fault is still the same fault.
        world.engine.resume_one_transaction()?;
        assert!(reported(), "a backoff skip re-armed the report");
        // Still failing after the backoff expires: still the same fault.
        expire();
        world.engine.resume_one_transaction()?;
        assert!(reported());
        // A real recovery clears it.
        workload.1.store(true, std::sync::atomic::Ordering::SeqCst);
        expire();
        world.engine.resume_one_transaction()?;
        assert!(!reported(), "a step that ran and succeeded left the fault armed");
        Ok(())
    }

    #[test]
    fn an_unretirable_legacy_marked_record_shows_its_fault_in_the_record() -> Result<()> {
        write_fixture_anchor()?;
        let envelope = fixture_envelope(FIXTURE_PRE_B1_ABORTS[1].1, DeploymentTransaction::TYPE)?;
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let first = read_transaction_record(&envelope)?;
        make_resident(&world, &first, envelope)?;
        assert_eq!(migrate_control_store_to_current_schema(&world.state_store)?, 1);
        assert!(record_of(&world, &first.transaction_id)?.carries_legacy_marker());
        corrupt_history(&world)?;
        assert!(!world.engine.retire_one_terminal_transaction()?);
        let noted = record_of(&world, &first.transaction_id)?;
        assert!(
            noted.last_error.is_some() && noted.last_error != first.last_error,
            "the retire fault never reached the record"
        );
        assert!(noted.carries_legacy_marker());
        Ok(())
    }

    #[test]
    fn a_tick_that_only_freezes_or_only_advances_reports_progress() -> Result<()> {
        // Only a freeze.
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        queue_deploy_for_nowhere(&world)?;
        write_service_binding(&world)?;
        assert!(world.engine.run_scheduler_tick()?, "a freeze-only tick reported no progress");

        // Only a phase advance.
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        transaction_at(&world, DeploymentPhase::Fencing)?;
        assert!(world.engine.resume_one_transaction()?, "a real advance reported none");
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        transaction_at(&world, DeploymentPhase::Fencing)?;
        assert!(world.engine.run_scheduler_tick()?);
        Ok(())
    }

    #[test]
    fn a_terminal_transaction_that_cannot_be_archived_does_not_stop_the_scheduler() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let live = transaction_at(&world, DeploymentPhase::Warming)?;
        odin_reports_ready(&world, &live, seeded_sequence(None) + 1)?;
        let (finished, command) = terminal_transaction_with_command("ghostlight")?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentCommand::TYPE.into(),
                        key: command.command_id.clone(),
                        current: None,
                    },
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: finished.transaction_id.clone(),
                        current: None,
                    },
                ],
                &[
                    command_envelope(&command, 100)?,
                    transaction_envelope(&finished, finished.updated_at_unix_millis)?,
                ],
            )?
        );
        corrupt_history(&world)?;
        let before = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .find(|stored| stored.value.transaction_id == live.transaction_id)
            .context("live transaction")?
            .envelope;

        // The finished record cannot be archived, and the live one still moves.
        world.engine.run_scheduler_tick()?;
        let after = ControlSnapshot::read(&world.state_store)?;
        assert!(
            after
                .transactions
                .iter()
                .any(|stored| stored.value.transaction_id == finished.transaction_id)
        );
        assert_ne!(
            after
                .transactions
                .iter()
                .find(|stored| stored.value.transaction_id == live.transaction_id)
                .context("live transaction")?
                .envelope,
            before,
            "the live transaction was not resumed"
        );
        Ok(())
    }

    // ---- fix 2: supervision owns the Expected-only precondition ----

    #[test]
    fn continuity_mints_no_restart_over_a_projection_it_cannot_demote() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        let incumbent = admit_incumbent(&world)?;

        // The projection names an activation that is not the incumbent's.
        let anchor = world.engine.provider_anchor_for_plan(&incumbent.plan)?;
        let topology = world.engine.topology();
        topology.publish_expected(&incumbent.expected, &anchor)?;
        let stranger = seeded_transaction(
            &world,
            DeploymentPhase::Warming,
            CommandKind::Continuity,
            Some(&incumbent),
        )?;
        topology.publish_observed_activation(
            &incumbent.expected,
            stranger.activation.as_ref().context("no activation")?,
            stranger.workload.as_ref().context("no workload")?,
        )?;
        // Remove the seeded stranger transaction: only the projection matters.
        let seeded = resident(&world)?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).delete_batch_if_unchanged(
                &[
                    seeded.envelope.clone(),
                    ControlSnapshot::read(&world.state_store)?
                        .commands
                        .into_iter()
                        .find(|stored| stored.value.command_id == seeded.value.command_id)
                        .context("seeded command")?
                        .envelope,
                ]
            )?
        );
        workload.kill();

        let before = now_millis()?;
        assert!(world.engine.supervise_one_admitted_generation()?);
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        assert!(
            snapshot.transactions.is_empty(),
            "continuity minted a restart over a projection it could not demote"
        );
        let recorded = snapshot
            .supervision_for("service")
            .context("the failed demotion was not recorded")?
            .value
            .clone();
        let deferred = recorded.continuity_deferred_until.context("no deferral time")?;
        assert!(recorded.continuity_deferral_reason.is_some());
        assert!(recorded.continuity_restarts.is_empty(), "a deferral spent an attempt");
        let after = now_millis()?;
        assert!(
            (before + CONTINUITY_DEFERRAL_MILLIS..=after + CONTINUITY_DEFERRAL_MILLIS)
                .contains(&deferred)
        );

        // Deferred: the next tick neither retries nor writes.
        assert!(!world.engine.supervise_one_admitted_generation()?);
        assert_eq!(meters_of(&world, "service")?, recorded);
        assert!(ControlSnapshot::read(&world.state_store)?.transactions.is_empty());

        // The projection is repaired and the deferral has run out: the restart
        // proceeds, from an Expected-only projection.
        topology.withdraw_stale_incarnation(&incumbent.expected, &anchor)?;
        project_admitted(&world, &incumbent)?;
        let mut ran_out = recorded.clone();
        ran_out.continuity_deferred_until = Some(1);
        set_meters(&world, &ran_out)?;
        assert!(world.engine.supervise_one_admitted_generation()?);
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        assert_eq!(snapshot.transactions.len(), 1);
        let minted = snapshot.supervision_for("service").context("no meters")?;
        assert_eq!(minted.value.continuity_restarts.len(), 1);
        assert_eq!(minted.value.continuity_deferred_until, None);
        assert_eq!(minted.value.continuity_deferral_reason, None);
        assert_eq!(snapshot.transactions[0].value.command_kind, CommandKind::Continuity);
        assert_eq!(projected_under(&world, &incumbent.expected)?, expected_only());
        Ok(())
    }

    // ---- fix 3: one-time boot reconciliation, by exact issuer ----

    #[test]
    fn boot_demotes_the_activation_a_failed_continuity_left_and_only_that() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let incumbent = admit_incumbent(&world)?;
        let failed = seeded_transaction(
            &world,
            DeploymentPhase::Warming,
            CommandKind::Continuity,
            Some(&incumbent),
        )?;
        project_candidate(&world, &failed, true)?;
        world
            .engine
            .begin_pre_fencing_abort(&resident(&world)?, anyhow!("candidate died"))?;
        drive(&world, |transaction| transaction.completion.is_some())?;
        // A failed transaction is archived the moment it finishes.
        let aborted = record_of(&world, &failed.transaction_id)?;
        assert!(aborted.is_terminal());
        assert!(ControlSnapshot::read(&world.state_store)?.transactions.is_empty());
        assert_eq!(projected_under(&world, &incumbent.expected)?, expected_only());

        // Drift a pre-B1 abort left: the failed candidate's activation stands.
        project_candidate(&world, &failed, true)?;
        assert_eq!(projected_under(&world, &incumbent.expected)?.len(), 2);

        let outcome = world.engine.reconcile_failed_continuity_projections()?;
        assert_eq!(
            outcome,
            ProjectionReconciliation {
                demoted: vec![failed.transaction_id.clone()],
                unexplained: Vec::new(),
            }
        );
        assert_eq!(projected_under(&world, &incumbent.expected)?, expected_only());

        // Once: nothing is left to demote.
        assert_eq!(
            world.engine.reconcile_failed_continuity_projections()?,
            ProjectionReconciliation::default()
        );
        Ok(())
    }

    #[test]
    fn boot_never_adopts_an_activation_no_failed_transaction_issued() -> Result<()> {
        let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let incumbent = admit_incumbent(&world)?;

        // The incumbent's own activation is accounted for.
        project_admitted(&world, &incumbent)?;
        assert_eq!(
            world.engine.reconcile_failed_continuity_projections()?,
            ProjectionReconciliation::default()
        );

        // Some other activation, issued by nothing in history, is reported and
        // left exactly as it is.
        let stranger = seeded_transaction(
            &world,
            DeploymentPhase::Warming,
            CommandKind::Continuity,
            Some(&incumbent),
        )?;
        let anchor = world.engine.provider_anchor_for_plan(&incumbent.plan)?;
        let topology = world.engine.topology();
        topology.withdraw_stale_incarnation(&incumbent.expected, &anchor)?;
        project_candidate(&world, &stranger, true)?;
        // A live transaction accounts for its own activation...
        assert_eq!(
            world.engine.reconcile_failed_continuity_projections()?,
            ProjectionReconciliation::default()
        );
        // ...and for nobody else's.
        topology.withdraw_stale_incarnation(&incumbent.expected, &anchor)?;
        let mut other = stranger.clone();
        let other_activation = IdunnRuntimeActivationLaunch::issue(
            &incumbent.expected,
            runtime_instance_id("tx-someone-else")?,
            now_millis()?,
            &world.engine.idunn_signer,
        )?
        .activation()
        .clone();
        if let Some(WorkloadObservation::Systemd(observed)) = other.workload.as_mut() {
            observed.runtime_instance_id = other_activation.runtime_instance_id.clone();
        }
        other.activation = Some(other_activation);
        project_candidate(&world, &other, true)?;
        assert_eq!(
            world.engine.reconcile_failed_continuity_projections()?.unexplained,
            vec!["service".to_owned()]
        );
        topology.withdraw_stale_incarnation(&incumbent.expected, &anchor)?;
        project_candidate(&world, &stranger, true)?;
        // ...and once it is gone, nothing does.
        let seeded = resident(&world)?;
        let command = ControlSnapshot::read(&world.state_store)?
            .commands
            .into_iter()
            .find(|stored| stored.value.command_id == seeded.value.command_id)
            .context("seeded command")?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store)
                .delete_batch_if_unchanged(&[seeded.envelope.clone(), command.envelope])?
        );
        let projection = std::fs::read(&world.engine.options.topology_store)?;
        let outcome = world.engine.reconcile_failed_continuity_projections()?;
        assert_eq!(
            outcome,
            ProjectionReconciliation {
                demoted: Vec::new(),
                unexplained: vec!["service".into()],
            }
        );
        assert_eq!(std::fs::read(&world.engine.options.topology_store)?, projection);
        Ok(())
    }

    // ---- fix 4: history that cannot be read stops actuation ----

    /// An operator binding for the target "service", so the command queue can
    /// resolve selectors.
    fn write_service_binding(world: &EngineFixture) -> Result<()> {
        use crate::deployment_plan::tests::BINDING;
        let (head, tail) = BINDING
            .split_once("[route]")
            .context("binding has no route table")?;
        let binding = format!(
            "{head}[brakes]{}",
            tail.split_once("[brakes]")
                .context("binding has no brakes table")?
                .1
        );
        std::fs::create_dir_all(&world.engine.options.bindings_dir)?;
        std::fs::write(world.engine.options.bindings_dir.join("service.toml"), binding)?;
        Ok(())
    }

    fn corrupt_history(world: &EngineFixture) -> Result<()> {
        std::fs::write(history_store_path(&world.state_store), b"\xc1\xc1 torn")?;
        Ok(())
    }

    #[test]
    fn history_that_cannot_be_read_is_an_error_and_missing_history_is_empty() -> Result<()> {
        let world = EngineFixture::new()?;
        let history = read_history(&world.state_store)?;
        assert!(history.transactions.is_empty() && history.report().is_none());

        corrupt_history(&world)?;
        let error = read_history(&world.state_store).err().context("torn history read")?;
        assert!(format!("{error:#}").contains("history"), "{error:#}");
        assert!(world.engine.history_for_decision().is_none());
        // The display path still answers, with nothing.
        assert!(read_history_transactions(&world.state_store).is_empty());
        Ok(())
    }

    #[test]
    fn undecodable_history_names_what_it_is_missing() -> Result<()> {
        let (finished, _) = terminal_transaction_with_command("ghostlight")?;
        let good = transaction_envelope(&finished, finished.updated_at_unix_millis)?;
        let mut torn = good.clone();
        torn.key = "tx-torn".into();
        torn.payload = vec![0xc1];
        let (transactions, undecodable) = decode_history_transactions(vec![good, torn]);
        let history = HistoryRead {
            transactions,
            undecodable,
        };
        let report = history.report().context("nothing reported")?;
        assert!(report.contains("1 archived transaction"), "{report}");
        assert!(report.contains("tx-torn"), "{report}");

        let complete = HistoryRead {
            transactions: history.transactions,
            undecodable: Vec::new(),
        };
        assert_eq!(complete.report(), None);
        Ok(())
    }

    #[test]
    fn a_fault_is_said_once_while_it_lasts() {
        let once = ReportOnce::default();
        assert_eq!(once.offer(None), None);
        assert_eq!(once.offer(Some("torn".into())), Some("torn".into()));
        assert_eq!(once.offer(Some("torn".into())), None);
        assert_eq!(once.offer(Some("torn".into())), None);
        assert_eq!(once.offer(Some("worse".into())), Some("worse".into()));
        assert_eq!(once.offer(None), None);
        assert_eq!(once.offer(Some("worse".into())), Some("worse".into()));
    }

    #[test]
    fn unreadable_history_does_not_stop_continuity() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        admit_incumbent(&world)?;
        workload.kill();
        corrupt_history(&world)?;

        // Crash recovery reads the target's own log, never history: the dead
        // workload is restarted, and the restart is counted there.
        assert!(world.engine.supervise_one_admitted_generation()?);
        assert_eq!(ControlSnapshot::read(&world.state_store)?.transactions.len(), 1);
        assert_eq!(meters_of(&world, "service")?.continuity_restarts.len(), 1);
        Ok(())
    }

    #[test]
    fn unreadable_history_stops_command_retirement_and_freezing() -> Result<()> {
        let world = EngineFixture::new()?;
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: "up-nowhere".into(),
            kind: CommandKind::Deploy,
            selector: "nowhere".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[CultCacheExpectedEnvelope {
                    r#type: DeploymentCommand::TYPE.into(),
                    key: command.command_id.clone(),
                    current: None,
                }],
                &[command_envelope(&command, 100)?],
            )?
        );
        write_service_binding(&world)?;
        corrupt_history(&world)?;

        // Whether the command was consumed is unknown, so it is not acted on.
        assert!(!world.engine.freeze_one_queued_command()?);
        assert!(ControlSnapshot::read(&world.state_store)?.transactions.is_empty());

        // Readable, it is acted on: the unknown selector is refused.
        std::fs::remove_file(history_store_path(&world.state_store))?;
        assert!(world.engine.freeze_one_queued_command()?);
        assert_eq!(ControlSnapshot::read(&world.state_store)?.transactions.len(), 1);
        Ok(())
    }

    #[test]
    fn the_legacy_lift_owes_only_a_continuity_that_issued_an_activation() -> Result<()> {
        let envelope = fixture_envelope(
            FIXTURE_PRE_B1_ABORTS[0].1,
            DeploymentTransaction::TYPE,
        )?;
        let written = lift_legacy_transaction(&envelope)?;
        let owed = |transaction: &DeploymentTransaction| {
            transaction
                .pre_fencing_abort
                .as_ref()
                .map(|abort| abort.topology_reconciliation)
        };
        assert_eq!(owed(&written), Some(CleanupEvidence::Skipped));

        let mut lifted = written.clone();
        cleanup_evidence::owe_legacy_continuity_projection(&mut lifted);
        assert_eq!(owed(&lifted), Some(CleanupEvidence::Pending));

        // A deployment published its own key and recorded what it owed.
        let mut deploy = written.clone();
        deploy.command_kind = CommandKind::Deploy;
        cleanup_evidence::owe_legacy_continuity_projection(&mut deploy);
        assert_eq!(owed(&deploy), Some(CleanupEvidence::Skipped));

        // A continuity that issued nothing owes nothing.
        let mut unissued = written;
        unissued.activation = None;
        cleanup_evidence::owe_legacy_continuity_projection(&mut unissued);
        assert_eq!(owed(&unissued), Some(CleanupEvidence::Skipped));
        Ok(())
    }

    #[test]
    fn continuity_stops_when_the_targets_own_log_has_spent_the_window() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        admit_incumbent(&world)?;
        workload.kill();
        // Six restarts inside the window, the last long enough ago that its
        // doubling wait (160 s) is over: only the ceiling holds.
        set_meters(
            &world,
            &restarted_ago(&[3_000_000, 2_400_000, 1_800_000, 1_200_000, 600_000, 200_000])?,
        )?;
        assert!(!world.engine.supervise_one_admitted_generation()?);
        assert!(ControlSnapshot::read(&world.state_store)?.transactions.is_empty());
        assert!(
            world
                .engine
                .fault_reports
                .lock()
                .unwrap()
                .contains_key("continuity:service"),
            "exhaustion was not reported"
        );

        // The oldest leaves the window: five remain, so one more is allowed.
        set_meters(
            &world,
            &restarted_ago(&[3_700_000, 2_400_000, 1_800_000, 1_200_000, 600_000, 200_000])?,
        )?;
        assert!(world.engine.supervise_one_admitted_generation()?);
        assert_eq!(ControlSnapshot::read(&world.state_store)?.transactions.len(), 1);
        Ok(())
    }

    #[test]
    fn continuity_restarts_are_spaced_by_a_doubling_wait_and_counted_in_the_mint() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        admit_incumbent(&world)?;
        workload.kill();
        let restarts = |world: &EngineFixture| -> Result<bool> {
            let progressed = world.engine.supervise_one_admitted_generation()?;
            let minted = !ControlSnapshot::read(&world.state_store)?.transactions.is_empty();
            assert_eq!(progressed, minted);
            clear_live_transactions(world)?;
            Ok(minted)
        };

        // The first restart is minted, and the same write counts it.
        assert!(restarts(&world)?);
        assert_eq!(meters_of(&world, "service")?.continuity_restarts.len(), 1);
        // At once again: the 5 s wait has not passed.
        assert!(!restarts(&world)?);
        // The second wait is 10 s and the third 20 s: each is the last one doubled.
        set_meters(&world, &restarted_ago(&[100_000, 6_000])?)?;
        assert!(!restarts(&world)?);
        set_meters(&world, &restarted_ago(&[100_000, 11_000])?)?;
        assert!(restarts(&world)?);
        assert_eq!(meters_of(&world, "service")?.continuity_restarts.len(), 3);
        set_meters(&world, &restarted_ago(&[200_000, 100_000, 15_000])?)?;
        assert!(!restarts(&world)?);
        set_meters(&world, &restarted_ago(&[200_000, 100_000, 21_000])?)?;
        assert!(restarts(&world)?);
        Ok(())
    }

    #[test]
    fn a_deployment_yields_to_a_restart_only_while_the_target_has_restarts_left() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        let incumbent = admit_incumbent(&world)?;
        let redeploy =
            seeded_transaction(&world, DeploymentPhase::Warming, CommandKind::Deploy, Some(&incumbent))?;
        workload.kill();
        // The target's restarts are spent: nothing is left to yield to, and
        // aborting the deployment would close the last door.
        set_meters(
            &world,
            &restarted_ago(&[3_000_000, 2_400_000, 1_800_000, 1_200_000, 600_000, 200_000])?,
        )?;
        world.engine.supervise_one_admitted_generation()?;
        let after = resident(&world)?.value;
        assert_eq!(after.transaction_id, redeploy.transaction_id);
        assert!(after.pre_fencing_abort.is_none(), "yielded with no restarts left");

        set_meters(&world, &restarted_ago(&[200_000])?)?;
        world.engine.supervise_one_admitted_generation()?;
        assert!(resident(&world)?.value.pre_fencing_abort.is_some());
        Ok(())
    }

    /// A source that resolves one fixed recipe and never freezes anything.
    struct FixedSource {
        recipe: String,
    }

    impl SourcePort for FixedSource {
        fn resolve(
            &self,
            _binding: &OperatorBinding,
            _resolution_id: &str,
            _selected_at_unix_millis: u64,
        ) -> Result<crate::drivers::ResolvedSource> {
            Ok(crate::drivers::ResolvedSource {
                facts: crate::deployment_plan::tests::source(&self.recipe),
                recipe_bytes: self.recipe.clone().into_bytes(),
            })
        }

        fn freeze(&self, _: &str, _: &CompiledDeploymentPlan) -> Result<FrozenSourceReceipt> {
            bail!("this source freezes nothing")
        }

        fn observe_frozen(
            &self,
            _: &CompiledDeploymentPlan,
            _: &FrozenSourceReceipt,
        ) -> Result<crate::drivers::FrozenSource> {
            bail!("this source freezes nothing")
        }

        fn cleanup(&self, _: &str, _: Option<&FrozenSourceReceipt>) -> Result<()> {
            Ok(())
        }
    }

    /// A Deploy transaction for an unrouted target, still Sealing, whose
    /// source resolves `recipe`. One scheduler tick admits its plan or refuses it.
    fn sealing_world(recipe: &str) -> Result<EngineFixture> {
        use crate::deployment_plan::tests::BINDING;
        let mut world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
        let (binding_head, binding_tail) = BINDING
            .split_once("[route]")
            .context("binding has no route table")?;
        let binding = format!(
            "{binding_head}[brakes]{}",
            binding_tail
                .split_once("[brakes]")
                .context("binding has no brakes table")?
                .1
        );
        std::fs::create_dir_all(world.root.join("bindings"))?;
        std::fs::write(world.root.join("bindings/service.toml"), binding)?;
        world.engine.source = Arc::new(FixedSource {
            recipe: recipe.to_owned(),
        });
        let now = now_millis()?;
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: "up-service".into(),
            kind: CommandKind::Deploy,
            selector: "service".into(),
            requested_by: "test".into(),
            requested_at_unix_millis: 100,
        };
        let transaction = DeploymentTransaction::new(&command, "service".into(), 0, None, now)?;
        assert!(
            SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentCommand::TYPE.into(),
                        key: command.command_id.clone(),
                        current: None,
                    },
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: transaction.transaction_id.clone(),
                        current: None,
                    },
                ],
                &[
                    command_envelope(&command, command.requested_at_unix_millis)?,
                    transaction_envelope(&transaction, now)?,
                ],
            )?
        );
        Ok(world)
    }

    fn unrouted_recipe() -> String {
        crate::deployment_plan::tests::RECIPE
            .split("[[dependencies]]")
            .next()
            .expect("the recipe has a body")
            .replace("route_required = true", "route_required = false")
            .replace(
                r#"["GAMECULT_IDUNN_CANDIDATE_BIND", "GAMECULT_IDUNN_RUNTIME_BUNDLE"]"#,
                r#"["GAMECULT_IDUNN_RUNTIME_BUNDLE"]"#,
            )
    }

    #[test]
    fn admission_refuses_a_recipe_that_declares_no_way_to_prove_readiness() -> Result<()> {
        let recipe = unrouted_recipe();
        let world = sealing_world(&recipe)?;
        world.engine.run_scheduler_tick()?;
        let refused = latest(&world)?;
        assert!(refused.plan.is_none(), "the refused recipe was planned");
        assert!(refused.frozen_source.is_none(), "the refused recipe was frozen");
        let abort = refused
            .pre_fencing_abort
            .context("a recipe with no declared proof of readiness must be refused")?;
        assert!(
            abort.error.contains("target service declares no way to prove readiness"),
            "{}",
            abort.error
        );
        Ok(())
    }

    #[test]
    fn admission_takes_a_recipe_that_declares_its_proof() -> Result<()> {
        let recipe = unrouted_recipe().replace(
            "capability = \"service.runtime\"",
            &format!("capability = \"{ODIN_RENDEZVOUS_CAPABILITY}\""),
        );
        let world = sealing_world(&recipe)?;
        world.engine.run_scheduler_tick()?;
        let planned = latest(&world)?;
        assert!(planned.pre_fencing_abort.is_none(), "{:?}", planned.pre_fencing_abort);
        assert_eq!(
            planned.plan.as_ref().map(CompiledDeploymentPlan::readiness_class).transpose()?,
            Some(ReadinessClass::OdinSelf)
        );
        Ok(())
    }

    // ---------------------------------------------------------------------
    // A held record is reported and never decided for. Supervision mints no
    // continuity over a held generation, and nothing yields to a held record.
    // ---------------------------------------------------------------------

    /// Rewrite the world's admitted generation into what a pre-B3 binary left
    /// behind: no Odin declaration, admitted on Odin receipts.
    fn undeclare_incumbent(world: &EngineFixture) -> Result<AdmittedGeneration> {
        let now = now_millis()?;
        let mut failure = None;
        let next = edit_incumbent(world, |generation| {
            generation.expected.dependencies.clear();
            match IdunnRuntimeActivationLaunch::issue(
                &generation.expected,
                generation.activation.runtime_instance_id.clone(),
                now,
                &world.engine.idunn_signer,
            ) {
                Ok(launch) => generation.activation = launch.activation().clone(),
                Err(error) => failure = Some(error),
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        ControlSnapshot::read(&world.state_store).context("store with the undeclared generation")?;
        assert!(matches!(next.readiness(), Err(ReadinessDisagreement::Undeclared(_))));
        Ok(next)
    }

    /// Queue the operator's declaring redeploy of `service`.
    fn queue_declaring_redeploy(world: &EngineFixture, command_id: &str) -> Result<()> {
        use crate::deployment_plan::tests::BINDING;
        let (binding_head, binding_tail) = BINDING.split_once("[route]").context("route")?;
        let binding = format!(
            "{binding_head}[brakes]{}",
            binding_tail.split_once("[brakes]").context("brakes")?.1
        );
        std::fs::create_dir_all(world.root.join("bindings"))?;
        std::fs::write(world.root.join("bindings/service.toml"), binding)?;
        let command = DeploymentCommand {
            schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
            command_id: command_id.into(),
            kind: CommandKind::Deploy,
            selector: "service".into(),
            requested_by: "operator".into(),
            requested_at_unix_millis: now_millis()?,
        };
        assert!(SingleFileMessagePackBackingStore::new(&world.state_store).compare_exchange(
            &[CultCacheExpectedEnvelope {
                r#type: DeploymentCommand::TYPE.into(),
                key: command.command_id.clone(),
                current: None,
            }],
            &[command_envelope(&command, command.requested_at_unix_millis)?],
        )?);
        Ok(())
    }

    /// Every transaction the store knows: live, then archived.
    fn every_transaction(world: &EngineFixture) -> Result<Vec<DeploymentTransaction>> {
        let mut all = ControlSnapshot::read(&world.state_store)?
            .transactions
            .into_iter()
            .map(|stored| stored.value)
            .collect::<Vec<_>>();
        all.extend(read_history_transactions(&world.state_store));
        Ok(all)
    }

    fn is_continuity_over(transaction: &DeploymentTransaction, held: &AdmittedGeneration) -> bool {
        transaction.command_kind == CommandKind::Continuity
            && transaction.incumbent_generation_id.as_deref() == Some(held.generation_id.as_str())
    }

    #[test]
    fn a_held_generation_that_dies_mints_no_continuity_and_leaves_its_target_free() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        admit_incumbent(&world)?;
        let held = undeclare_incumbent(&world)?;
        world.engine.validate_durable_authority(&ControlSnapshot::read(&world.state_store)?)?;

        workload.kill();
        for _ in 0..4 {
            assert!(!world.engine.supervise_one_admitted_generation()?);
        }
        assert!(
            ControlSnapshot::read(&world.state_store)?.transactions.is_empty(),
            "supervision minted a transaction over a held generation"
        );
        // Reported, once per state change: the same text is not offered again.
        let reports = world.engine.fault_reports.lock().unwrap();
        let down = reports.get("generation-down:service").context("the dead hold is not reported")?;
        let last = down.last.lock().unwrap().clone();
        assert!(last.is_some());
        assert!(down.offer(last).is_none());
        drop(reports);

        // The target is free: the declaring redeploy freezes and is not
        // displaced by a continuity.
        queue_declaring_redeploy(&world, "up-service-declared")?;
        for _ in 0..8 {
            let _ = world.engine.run_scheduler_tick();
        }
        let all = every_transaction(&world)?;
        assert!(
            all.iter().all(|transaction| !is_continuity_over(transaction, &held)),
            "a continuity was minted over the held generation"
        );
        assert!(
            all.iter().any(|transaction| transaction.command_id == "up-service-declared"),
            "the declaring redeploy never froze"
        );
        Ok(())
    }

    #[test]
    fn a_declaring_redeploy_is_not_yielded_when_the_held_incumbent_dies() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        admit_incumbent(&world)?;
        let held = undeclare_incumbent(&world)?;
        let redeploy =
            seeded_transaction(&world, DeploymentPhase::Warming, CommandKind::Deploy, Some(&held))?;
        assert_eq!(redeploy.held_disagreement(), None);
        workload.kill();
        for _ in 0..4 {
            let _ = world.engine.supervise_one_admitted_generation()?;
        }
        let all = every_transaction(&world)?;
        assert!(
            all.iter().all(|transaction| !is_continuity_over(transaction, &held)),
            "a continuity was minted over the held incumbent"
        );
        let after = resident(&world)?.value;
        assert_eq!(after.transaction_id, redeploy.transaction_id);
        assert!(
            after.pre_fencing_abort.is_none(),
            "the redeploy yielded: {:?}",
            after.pre_fencing_abort
        );
        Ok(())
    }

    /// The control for the test above: the same redeploy over a dead incumbent
    /// that is not held is yielded to continuity.
    #[test]
    fn a_declaring_redeploy_still_yields_to_continuity_when_the_incumbent_is_not_held() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        let incumbent = admit_incumbent(&world)?;
        assert!(incumbent.readiness().is_ok());
        let redeploy =
            seeded_transaction(&world, DeploymentPhase::Warming, CommandKind::Deploy, Some(&incumbent))?;
        workload.kill();
        world.engine.supervise_one_admitted_generation()?;
        let after = resident(&world)?.value;
        assert_eq!(after.transaction_id, redeploy.transaction_id);
        let abort = after.pre_fencing_abort.context("the redeploy was not yielded")?;
        assert!(abort.error.contains("yielded to continuity"), "{}", abort.error);
        Ok(())
    }

    /// `seeded` with its own Expected undeclared, so Idunn holds it.
    fn undeclared(world: &EngineFixture, seeded: &DeploymentTransaction) -> Result<DeploymentTransaction> {
        let mut bare = seeded.clone();
        // Undeclare the Expected and re-derive what is bound to its digest.
        let expected = bare.expected.as_mut().context("no expected")?;
        expected.dependencies.clear();
        bare.expected_publication_sha256 = Some(expected.canonical_sha256()?);
        let activation = IdunnRuntimeActivationLaunch::issue(
            expected,
            seeded.activation.as_ref().context("no activation")?.runtime_instance_id.clone(),
            now_millis()?,
            &world.engine.idunn_signer,
        )?
        .activation()
        .clone();
        bare.activation_publication_sha256 = Some(activation.canonical_sha256()?);
        bare.activation = Some(activation);
        bare.validate()?;
        assert!(bare.held_disagreement().is_some());
        Ok(bare)
    }

    /// A pre-fence Deploy whose own Expected is undeclared, stored in place.
    fn held_predeploy(
        world: &EngineFixture,
        incumbent: &AdmittedGeneration,
    ) -> Result<DeploymentTransaction> {
        let seeded =
            seeded_transaction(world, DeploymentPhase::Warming, CommandKind::Deploy, Some(incumbent))?;
        let bare = undeclared(world, &seeded)?;
        replace_transaction(&world.state_store, &resident(world)?, &bare)?;
        Ok(bare)
    }

    #[test]
    fn a_held_predeploy_transaction_is_not_aborted_to_yield_to_continuity() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        let incumbent = admit_incumbent(&world)?;
        // The incumbent is declared; the Deploy in front of it is the held one.
        let held = held_predeploy(&world, &incumbent)?;

        workload.kill();
        for _ in 0..4 {
            let _ = world.engine.supervise_one_admitted_generation()?;
        }
        let after = resident(&world)?.value;
        assert_eq!(after.transaction_id, held.transaction_id);
        assert!(after.pre_fencing_abort.is_none(), "Idunn aborted a held record");
        assert!(after.completion.is_none());
        Ok(())
    }

    #[test]
    fn cancel_withdraws_a_held_predeploy_and_leaves_the_incumbent_untouched() -> Result<()> {
        let workload = SwitchWorkload::new();
        let world = EngineFixture::with_workload(workload.clone())?;
        let incumbent = admit_incumbent(&world)?;
        let held = held_predeploy(&world, &incumbent)?;
        let before = incumbent_envelope(&world)?;

        cancel(&world.state_store, &held.command_id, "operator")?;
        let cancelling = resident(&world)?.value;
        assert_eq!(
            cancelling.pre_fencing_abort.as_ref().map(|abort| abort.error.as_str()),
            Some("cancelled by operator")
        );
        assert!(cancelling.post_fencing_abort.is_none());

        drive(&world, |transaction| transaction.completion.is_some())?;
        // History also holds the incumbent's own admission: find ours by id.
        let finished = every_transaction(&world)?
            .into_iter()
            .find(|transaction| transaction.transaction_id == held.transaction_id)
            .context("the cancelled redeploy is nowhere")?;
        assert!(matches!(
            finished.completion,
            Some(TransactionCompletion::FailedBeforeFencing { .. })
        ));
        assert!(finished.is_terminal());
        // The target is released and nothing of the incumbent was touched.
        assert!(!finished.blocks_new_target_mutation());
        assert_eq!(incumbent_envelope(&world)?, before);
        assert!(
            every_transaction(&world)?
                .iter()
                .all(|transaction| !is_continuity_over(transaction, &incumbent)),
            "cancelling the redeploy minted a continuity"
        );
        Ok(())
    }

    #[test]
    fn cancel_refuses_a_predeploy_that_is_not_held_and_a_held_one_past_the_fence() -> Result<()> {
        let world = EngineFixture::with_workload(SwitchWorkload::new())?;
        let incumbent = admit_incumbent(&world)?;
        let seeded =
            seeded_transaction(&world, DeploymentPhase::Warming, CommandKind::Deploy, Some(&incumbent))?;
        assert!(seeded.held_disagreement().is_none());
        assert!(cancel(&world.state_store, &seeded.command_id, "operator").is_err());
        assert!(resident(&world)?.value.pre_fencing_abort.is_none());

        let mut past_the_fence = undeclared(&world, &seeded)?;
        past_the_fence.enter_phase(DeploymentPhase::Fencing, now_millis()?);
        assert!(past_the_fence.held_disagreement().is_some());
        assert!(!cancel_is_safe_for_held_prefence(&past_the_fence));
        Ok(())
    }

    // ---------------------------------------------------------------------
    // B2: the meters, the v4 lift, and what `status` shows.
    // ---------------------------------------------------------------------

    #[test]
    fn the_actuation_log_slides() -> Result<()> {
        let window = ROUTE_ACTUATION_WINDOW_MILLIS;
        let t = 1_000_000_000_u64;
        let mut meters = TargetSupervision::new("service");
        for offset in 0..12 {
            meters.charge_route(t + offset, RouteActuation::Forward)?;
        }
        let refused = meters.charge_route(t + window - 1, RouteActuation::Forward).unwrap_err();
        assert_eq!((refused.used, refused.reopens_at_unix_millis), (12, t + window));
        // Only the first entry has left the window: exactly one slot reopens.
        meters.charge_route(t + window, RouteActuation::Forward)?;
        let refused = meters.charge_route(t + window, RouteActuation::Forward).unwrap_err();
        assert_eq!((refused.used, refused.reopens_at_unix_millis), (12, t + 1 + window));
        meters.validate()?;
        Ok(())
    }

    #[test]
    fn a_survival_actuation_is_counted_past_the_limit_and_never_refused() -> Result<()> {
        let t = 1_000_000_000_u64;
        let mut meters = TargetSupervision::new("service");
        for offset in 0..12 {
            meters.charge_route(t + offset, RouteActuation::Forward)?;
        }
        meters.charge_route(t + 20, RouteActuation::Survival)?;
        assert_eq!(meters.route_actuations.len(), ROUTE_ACTUATION_CEILING);
        assert_eq!(meters.route_actuations.last(), Some(&(t + 20)));
        // The survival took a slot: the forward ceiling is still exact, and
        // reopens when the oldest surviving entry leaves.
        let refused = meters.charge_route(t + 21, RouteActuation::Forward).unwrap_err();
        assert_eq!(refused.reopens_at_unix_millis, t + 1 + ROUTE_ACTUATION_WINDOW_MILLIS);
        for _ in 0..20 {
            meters.charge_route(t + 22, RouteActuation::Survival)?;
        }
        meters.validate()?;
        Ok(())
    }

    #[test]
    fn a_clock_stepped_back_stalls_nothing_past_one_window() -> Result<()> {
        let t = 5_000_000_000_u64;
        let now = t - 86_400_000;
        let mut meters = TargetSupervision::new("service");
        meters.route_actuations = vec![t; ROUTE_ACTUATION_CEILING];
        meters.continuity_restarts = vec![t; CONTINUITY_RESTART_ATTEMPTS];
        meters.continuity_deferred_until = Some(t);
        meters.continuity_deferral_reason = Some("demotion failed".into());
        meters.validate()?;

        // Entries from the future count as now: the meters hold for one window.
        let read_only = meters.clone();
        assert!(read_only.restarts_exhausted(now));
        let refused = meters.charge_route(now, RouteActuation::Forward).unwrap_err();
        assert!(refused.reopens_at_unix_millis <= now + ROUTE_ACTUATION_WINDOW_MILLIS);
        // The clamp is written, so the window runs from the step and no longer.
        assert_eq!(meters.route_actuations, vec![now; ROUTE_ACTUATION_CEILING]);
        assert_eq!(meters.continuity_restarts, vec![now; CONTINUITY_RESTART_ATTEMPTS]);
        assert!(!meters.restarts_exhausted(now + CONTINUITY_RESTART_WINDOW_MILLIS));
        meters.charge_route(now + ROUTE_ACTUATION_WINDOW_MILLIS, RouteActuation::Forward)?;

        // Waits set for the future are due, not stalled for the length of the step.
        assert!(!read_only.continuity_is_waiting(now));
        let route = RouteSupervisionState {
            next_challenge_at_unix_millis: Some(t),
            ..RouteSupervisionState::default()
        };
        assert!(!route.is_waiting(now, DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS));
        assert!(!is_waiting(now, t, RESUME_BACKOFF_CEILING_MILLIS));
        // The same waits, set for a moment ahead, still wait.
        let ahead = RouteSupervisionState {
            next_challenge_at_unix_millis: Some(now + 5_000),
            ..RouteSupervisionState::default()
        };
        assert!(ahead.is_waiting(now, DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS));
        Ok(())
    }

    #[test]
    fn a_route_challenge_wait_widens_to_a_cap_and_a_proof_clears_it() {
        let age = DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS;
        let mut state = RouteSupervisionState::default();
        let mut waits = Vec::new();
        for _ in 0..8 {
            state.record_failed_challenge(1_000, age);
            waits.push(state.next_challenge_at_unix_millis.unwrap() - 1_000);
        }
        assert_eq!(&waits[..3], &[age, age * 2, age * 4]);
        assert_eq!(waits[7], ROUTE_CHALLENGE_BACKOFF_CAP_MILLIS);
        assert_eq!(state.degraded_since_unix_millis, Some(1_000));
        state.record_proved_challenge(9_000);
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.next_challenge_at_unix_millis, None);
        assert_eq!(state.degraded_since_unix_millis, None);
    }

    #[test]
    fn a_v3_generation_lifts_to_v4_once_and_reencodes_canonically() -> Result<()> {
        for text in [FIXTURE_GENERATION_V3_ODIN, FIXTURE_GENERATION_V3_ROUTE_PROOF] {
            let temp = TempDir::new()?;
            let store = temp.path().join("control.cc");
            let envelope = fixture_envelope(text, AdmittedGeneration::TYPE)?;
            let legacy: LegacyAdmittedGenerationV3 = rmp_serde::from_slice(&envelope.payload)?;
            SingleFileMessagePackBackingStore::new(&store).insert_entry_if_absent(envelope.clone())?;
            // The v3 bytes are not v4 bytes: only the typed lift may read them.
            assert!(decode_record::<AdmittedGeneration>(&envelope).is_err());

            assert_eq!(migrate_control_store_to_current_schema(&store)?, 1);
            assert_eq!(migrate_control_store_to_current_schema(&store)?, 0);
            let snapshot = ControlSnapshot::read(&store)?;
            let migrated = snapshot.admitted.first().context("the lift lost the record")?;
            assert_eq!(migrated.envelope.schema_id.as_deref(), Some(ADMITTED_GENERATION_SCHEMA));
            // After the lift the canonical re-encode accepts its own bytes.
            let lifted: AdmittedGeneration = decode_record(&migrated.envelope)?;
            assert_eq!(lifted, migrated.value);

            // The decisions are the ones the v3 record encoded; only the retired
            // fields are gone, and no meters were invented.
            assert_eq!(lifted.target, legacy.target);
            assert_eq!(lifted.generation_id, legacy.generation_id);
            assert_eq!(lifted.plan, legacy.plan);
            assert_eq!(lifted.expected, legacy.expected);
            assert_eq!(lifted.activation, legacy.activation);
            assert_eq!(lifted.workload, legacy.workload);
            assert_eq!(lifted.leasing, legacy.leasing);
            assert_eq!(lifted.ready, legacy.ready);
            assert_eq!(lifted.latest_odin_observation, legacy.latest_odin_observation);
            assert_eq!(lifted.routing, legacy.routing);
            assert_eq!(lifted.odin_authority, legacy.odin_authority);
            assert_eq!(
                lifted.odin_publisher_sequence_cursor,
                legacy.odin_publisher_sequence_cursor
            );
            assert_eq!(
                lifted.route_supervision,
                legacy.route_supervision.map(|state| RouteSupervisionState {
                    last_challenge_at_unix_millis: state.last_challenge_at_unix_millis,
                    consecutive_failures: state.consecutive_failures,
                    next_challenge_at_unix_millis: state.next_challenge_at_unix_millis,
                    degraded_since_unix_millis: state.degraded_since_unix_millis,
                })
            );
            assert_eq!(lifted.last_error, None);
            assert!(snapshot.targets.is_empty());
        }
        Ok(())
    }

    #[test]
    fn a_v3_generation_that_shows_written_meters_is_refused_not_dropped() -> Result<()> {
        let envelope = fixture_envelope(FIXTURE_GENERATION_V3_ROUTE_PROOF, AdmittedGeneration::TYPE)?;
        let legacy: LegacyAdmittedGenerationV3 = rmp_serde::from_slice(&envelope.payload)?;
        assert!(legacy.route_supervision.is_some(), "the fixture is a routed generation");
        let refused = |change: fn(&mut LegacyAdmittedGenerationV3)| -> Result<String> {
            let mut edited = legacy.clone();
            change(&mut edited);
            let mut tampered = envelope.clone();
            tampered.payload = rmp_serde::to_vec(&edited)?;
            Ok(error_text(read_generation_record(&tampered).map(|_| ())))
        };
        let restarts = refused(|edited| edited.continuity_backoff.attempts = 2)?;
        assert!(restarts.contains("refusing to drop"), "{restarts}");
        assert!(restarts.contains(&legacy.target), "the error names the record: {restarts}");
        let actuations =
            refused(|edited| edited.route_supervision.as_mut().unwrap().actuations.count = 1)?;
        assert!(actuations.contains("refusing to drop"), "{actuations}");

        // A whole store carrying one refuses to boot, naming the record.
        let temp = TempDir::new()?;
        let store = temp.path().join("control.cc");
        let mut edited = legacy.clone();
        edited.continuity_backoff.attempts = 2;
        let mut tampered = envelope.clone();
        tampered.payload = rmp_serde::to_vec(&edited)?;
        SingleFileMessagePackBackingStore::new(&store).insert_entry_if_absent(tampered)?;
        assert!(migrate_control_store_to_current_schema(&store).is_err());
        Ok(())
    }

    #[test]
    fn a_v3_generation_lifts_each_route_supervision_field_to_its_own_place() -> Result<()> {
        let envelope = fixture_envelope(FIXTURE_GENERATION_V3_ROUTE_PROOF, AdmittedGeneration::TYPE)?;
        let mut legacy: LegacyAdmittedGenerationV3 = rmp_serde::from_slice(&envelope.payload)?;
        // Four distinct non-default values, so a swap cannot land on itself.
        legacy.route_supervision = Some(LegacyRouteSupervisionStateV3 {
            last_challenge_at_unix_millis: Some(1_111),
            consecutive_failures: 3,
            next_challenge_at_unix_millis: Some(2_222),
            degraded_since_unix_millis: Some(3_333),
            actuations: LegacyActuationWindowV3::default(),
        });
        let mut edited = envelope.clone();
        edited.payload = rmp_serde::to_vec(&legacy)?;
        let lifted = read_generation_record(&edited)?;
        assert_eq!(
            lifted.route_supervision,
            Some(RouteSupervisionState {
                last_challenge_at_unix_millis: Some(1_111),
                consecutive_failures: 3,
                next_challenge_at_unix_millis: Some(2_222),
                degraded_since_unix_millis: Some(3_333),
            })
        );
        Ok(())
    }

    #[test]
    fn status_covers_the_admitted_targets_and_the_metered_ones() -> Result<()> {
        // "service" is admitted with no meters at all; "metered" has meters and
        // no generation. Neither set contains the other.
        let world = EngineFixture::with_workload(SwitchWorkload::new())?;
        admit_incumbent(&world)?;
        assert!(ControlSnapshot::read(&world.state_store)?.targets.is_empty());
        set_meters(&world, &TargetSupervision::new("metered"))?;
        let snapshot = ControlSnapshot::read(&world.state_store)?;
        assert_eq!(
            supervised_targets(&snapshot).into_iter().collect::<Vec<_>>(),
            ["metered", "service"]
        );
        Ok(())
    }

    #[test]
    fn target_supervision_reads_back_and_its_key_must_be_its_target() -> Result<()> {
        let world = EngineFixture::new()?;
        let meters = restarted_ago(&[5_000])?;
        set_meters(&world, &meters)?;
        assert_eq!(meters_of(&world, "service")?, meters);
        assert_eq!(ControlSnapshot::read(&world.state_store)?.targets.len(), 1);

        let mut elsewhere = target_supervision_envelope(&meters, 5)?;
        elsewhere.key = "other".into();
        SingleFileMessagePackBackingStore::new(&world.state_store).insert_entry_if_absent(elsewhere)?;
        let error = error_text(ControlSnapshot::read(&world.state_store).map(|_| ()));
        assert!(error.contains("not its target"), "{error}");
        Ok(())
    }

    #[test]
    fn status_shows_each_targets_meters_including_a_target_with_no_generation() -> Result<()> {
        let now = 10_000_000_u64;
        let mut meters = TargetSupervision::new("service");
        meters.route_actuations = (0..12).map(|offset| now - 5_000 + offset).collect();
        meters.continuity_restarts = vec![now - 9_000, now - 4_000];
        meters.continuity_deferred_until = Some(now + 500);
        meters.continuity_deferral_reason = Some("projection would not demote".into());

        let (_, mut generation) = fixture_generation(FIXTURE_GENERATION)?;
        generation.last_error = Some("held".into());
        let lines = render_supervision("service", Some(&generation), Some(&meters), now).join("\n");
        assert!(lines.contains(&format!("target service {}", generation.generation_id)), "{lines}");
        assert!(lines.contains("held: held"), "{lines}");
        assert!(lines.contains(&format!(
            "continuity restarts 2/6 next-restart-at {}",
            now - 4_000 + 10_000
        )), "{lines}");
        assert!(lines.contains("continuity deferred until 10000500: projection would not demote"), "{lines}");
        assert!(lines.contains(&format!(
            "route actuations 12/12 in window, reopens-at {}",
            now - 5_000 + ROUTE_ACTUATION_WINDOW_MILLIS
        )), "{lines}");
        assert!(lines.contains("route healthy consecutive-failures 0"), "{lines}");

        // A first deployment has meters and no generation: it shows them alone.
        let mut first = TargetSupervision::new("fresh");
        first.route_actuations = vec![now - 1];
        let lines = render_supervision("fresh", None, Some(&first), now).join("\n");
        assert!(lines.contains("target fresh no-admitted-generation"), "{lines}");
        assert!(lines.contains("route actuations 1/12 in window, reopens-at none"), "{lines}");
        // An unrouted, unmetered target shows no route line at all.
        let lines = render_supervision("plain", Some(&unrouted_generation()?), None, now).join("\n");
        assert!(!lines.contains("route actuations"), "{lines}");
        Ok(())
    }

    fn unrouted_generation() -> Result<AdmittedGeneration> {
        let (_, mut generation) = fixture_generation(FIXTURE_GENERATION)?;
        generation.route_supervision = None;
        Ok(generation)
    }

    // ---------------------------------------------------------------------
    // Route-proof readiness: a routed target that declares no Odin dependency
    // is admitted by Idunn's own challenges. Odin is never read.
    // ---------------------------------------------------------------------
    mod route_proof {
        use super::*;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
        use std::time::Duration;

        /// The runtime behind both endpoints, answering Idunn's snapshot
        /// challenge the way the TypeScript signer does: locally, in the
        /// reported health, with `route-observation:<id>` as the detail.
        struct RuntimeStub {
            expected: IdunnExpectedIncarnationRecord,
            activation: IdunnRuntimeActivationRecord,
            provider: cultnet_rs::ServiceIdentitySigner<GameCultProviderHealthIdentity>,
            activation_signer: cultnet_rs::IdunnRuntimeActivationSigner,
            state: Mutex<&'static str>,
            capacity: AtomicU32,
            sequence: AtomicU64,
            candidate_hits: AtomicUsize,
            stable_hits: AtomicUsize,
            stop: AtomicBool,
            /// Accept the connection and close it without a word: a process
            /// that is up but not answering. Unlike a closed port, no other
            /// test's listener can be handed the address.
            hang_up: AtomicBool,
            /// The write lease its presence claims to hold: what a candidate
            /// that picked up Idunn's grant reports.
            lease: Mutex<Option<String>>,
            reply: Mutex<Reply>,
        }

        /// How the runtime answers a well-formed challenge.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Reply {
            Honest,
            /// An answer to some other challenge.
            ForeignChallengeId,
            /// An HTTP error where the snapshot should be.
            HttpFailure,
            /// A first Odin candidate answering its Warming challenge: the
            /// detail names the challenge as `idunn-warming:<id>`.
            FirstOdinWarming,
        }

        impl RuntimeStub {
            fn answer(&self, message_id: &str) -> Result<Vec<u8>> {
                let expected = &self.expected;
                let mut presence = cultnet_rs::GameCultRuntimePresenceHealthRecord {
                    schema_version: cultnet_rs::GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into(),
                    target: expected.target.clone(),
                    expected_projection_sha256: self.activation.expected_projection_sha256.clone(),
                    plan_id: expected.plan_id.clone(),
                    incarnation_id: expected.incarnation_id.clone(),
                    sealed_release_id: expected.sealed_release_id.clone(),
                    activation_witness_sha256: self.activation.canonical_sha256()?,
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
                        .map(|capability| cultnet_rs::GameCultRuntimeCapability {
                            capability: capability.capability.clone(),
                            schema: capability.schema.clone(),
                            compatibility: capability.compatibility.clone(),
                            capacity: self.capacity.load(Ordering::SeqCst),
                        })
                        .collect(),
                    health_contract: expected.health_contract.clone(),
                    state: (*self.state.lock().unwrap()).into(),
                    detail: match *self.reply.lock().unwrap() {
                        Reply::FirstOdinWarming => format!("idunn-warming:{message_id}"),
                        _ => format!("route-observation:{message_id}"),
                    },
                    write_lease_sha256: self.lease.lock().unwrap().clone(),
                    signer_identity_id: self.provider.entry().identity_id.clone(),
                    publisher_sequence: self.sequence.fetch_add(1, Ordering::SeqCst) + 1,
                    observed_at_unix_millis: now_millis()?,
                    signature_algorithm: "ed25519".into(),
                    signature: Vec::new(),
                    activation_signer_identity_id: self.activation_signer.identity_id(),
                    activation_signature: Vec::new(),
                };
                let proof = presence.canonical_proof_payload()?;
                presence.signature = self
                    .provider
                    .sign::<cultnet_rs::GameCultRuntimePresenceHealthPurpose>(&proof)
                    .signature;
                presence.activation_signature =
                    self.activation_signer.sign_presence_proof(&presence)?;
                Ok(rmp_serde::to_vec(&presence)?)
            }

            fn set_state(&self, state: &'static str) {
                *self.state.lock().unwrap() = state;
            }

            fn serve_one(&self, mut stream: TcpStream) -> Result<()> {
                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                let (header_end, length) = loop {
                    let read = stream.read(&mut chunk)?;
                    ensure!(read > 0, "stub client closed early");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..index]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .context("stub request has no Content-Length")?
                            .trim()
                            .parse::<usize>()?;
                        if request.len() >= index + 4 + length {
                            break (index + 4, length);
                        }
                    }
                };
                let message = cultnet_rs::decode_cultnet_message_from_slice(
                    &request[header_end..header_end + length],
                    cultnet_rs::CultNetWireContract::CultNetSchemaV0,
                )?;
                let cultnet_rs::CultNetMessage::SnapshotRequest { message_id, .. } = message
                else {
                    bail!("stub received something other than a snapshot request");
                };
                let reply = *self.reply.lock().unwrap();
                if reply == Reply::HttpFailure {
                    stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )?;
                    return Ok(());
                }
                let response = cultnet_rs::CultNetMessage::SnapshotResponseRaw {
                    message_id: if reply == Reply::ForeignChallengeId {
                        "candidate-foreign".to_owned()
                    } else {
                        message_id.clone()
                    },
                    documents: vec![cultnet_rs::CultNetRawDocumentRecord {
                        schema_id: cultnet_rs::GAMECULT_RUNTIME_PRESENCE_HEALTH_SCHEMA.into(),
                        record_key: self.expected.target.clone(),
                        stored_at: rfc3339_millis(now_millis()?)?,
                        payload_encoding: cultnet_rs::CultNetRawPayloadEncoding::Messagepack,
                        payload: self.answer(&message_id)?,
                        source_runtime_id: None,
                        source_agent_id: None,
                        source_role: None,
                        tags: None,
                    }],
                };
                let body = cultnet_rs::encode_cultnet_message_to_vec(
                    &response,
                    cultnet_rs::CultNetWireContract::CultNetSchemaV0,
                )?;
                stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/msgpack\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )?;
                stream.write_all(&body)?;
                Ok(())
            }

            fn listen(self: &Arc<Self>, listener: TcpListener, candidate: bool) {
                listener.set_nonblocking(true).expect("nonblocking listener");
                let stub = Arc::clone(self);
                std::thread::spawn(move || {
                    while !stub.stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).ok();
                                let hits = if candidate {
                                    &stub.candidate_hits
                                } else {
                                    &stub.stable_hits
                                };
                                hits.fetch_add(1, Ordering::SeqCst);
                                if !stub.hang_up.load(Ordering::SeqCst) {
                                    let _ = stub.serve_one(stream);
                                }
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                });
            }
        }

        impl Drop for RuntimeStub {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
            }
        }

        /// Odin, as the world it runs in sees it.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Odin {
            /// Every read of the correlation store fails.
            Unreachable,
            /// Odin publishes a correlation that says the target is not Ready.
            NotReady,
        }

        struct RoutedWorld {
            world: EngineFixture,
            workload: Arc<SwitchWorkload>,
            stub: Arc<RuntimeStub>,
            odin: Odin,
            poisoned: Vec<u8>,
        }

        /// A stateless, routed target that declares no Odin dependency, over
        /// `minimum_capacity`, seeded at `phase` (Warming or Fencing): its
        /// Warming evidence, when the phase needs it, is the candidate's own
        /// answer to a real challenge. Listeners stand at both endpoints.
        fn routed_world(
            odin: Odin,
            minimum_capacity: u32,
            phase: DeploymentPhase,
            provides_odin: bool,
        ) -> Result<RoutedWorld> {
            build_routed_world(
                odin,
                minimum_capacity,
                phase,
                provides_odin,
                false,
                CommandKind::Continuity,
            )
        }

        /// The same world owned by a deployment command instead of a
        /// continuity: its route actuations are `Forward` and can be refused.
        fn routed_deploy_world(
            odin: Odin,
            minimum_capacity: u32,
            phase: DeploymentPhase,
        ) -> Result<RoutedWorld> {
            build_routed_world(odin, minimum_capacity, phase, false, false, CommandKind::Deploy)
        }

        /// The same world for a target with process-writable state: its
        /// candidate is granted a write lease and Ready must be the candidate
        /// reporting exactly that lease.
        fn stateful_routed_world(odin: Odin, phase: DeploymentPhase) -> Result<RoutedWorld> {
            build_routed_world(odin, 1, phase, false, true, CommandKind::Continuity)
        }

        fn build_routed_world(
            odin: Odin,
            minimum_capacity: u32,
            phase: DeploymentPhase,
            provides_odin: bool,
            stateful: bool,
            kind: CommandKind,
        ) -> Result<RoutedWorld> {
            use crate::deployment_plan::tests::{
                BINDING, RECIPE, artifact_receipt, external_input_receipt, source,
            };
            let switch = SwitchWorkload::new();
            let world = EngineFixture::with_workload(switch.clone())?;
            let candidate_listener = TcpListener::bind("127.0.0.1:0")?;
            let stable_listener = TcpListener::bind("127.0.0.1:0")?;
            let candidate_port = candidate_listener.local_addr()?.port();
            let stable_port = stable_listener.local_addr()?.port();

            let provider_path = world.root.join("identities/provider.cc");
            let provider_anchor = world.root.join("identities/provider-anchor.cc");
            let provider =
                enroll_service_identity_at::<GameCultProviderHealthIdentity>(&provider_path)?;
            export_service_identity_trust_anchor(&provider, &provider_anchor)?;
            let config = world.root.join("service.conf");
            let binding = BINDING
                .replace(
                    "/etc/gamecult/trust/service.cc",
                    &provider_anchor.display().to_string(),
                )
                .replace("service-runtime-signer", &provider.entry().identity_id)
                .replace("127.0.0.1:17999", &format!("127.0.0.1:{stable_port}"))
                .replace(
                    "private_port_start = 18000",
                    &format!("private_port_start = {candidate_port}"),
                )
                .replace(
                    "private_port_end = 18009",
                    &format!("private_port_end = {}", candidate_port + 1),
                )
                .replace(
                    "/etc/nginx/idunn-stream-routes/service.conf",
                    &config.display().to_string().replace('\\', "/"),
                );
            let (binding, recipe_source) = if stateful {
                std::fs::create_dir_all(world.root.join("lease"))?;
                (
                    binding.replace(
                        "[brakes]",
                        &format!(
                            "[process_write_lease]\nrecord_path = \"{}\"\n\n[brakes]",
                            world.root.join("lease/process-write-lease.cc").display()
                        ),
                    ),
                    RECIPE
                        .replace(
                            "writer = \"none\"\nrecovery = \"rebuildable\"\nstartup = \"open-at-start\"",
                            "writer = \"process-bound-single-writer\"\nrecovery = \"preserve\"\nstartup = \"create-or-open-after-write-lease\"",
                        )
                        .replace(
                            "required_environment = [\"GAMECULT_IDUNN_CANDIDATE_BIND\", \"GAMECULT_IDUNN_RUNTIME_BUNDLE\"]",
                            "required_environment = [\"GAMECULT_IDUNN_CANDIDATE_BIND\", \"GAMECULT_IDUNN_PROCESS_WRITE_LEASE\", \"GAMECULT_IDUNN_RUNTIME_BUNDLE\"]",
                        ),
                )
            } else {
                (binding, RECIPE.to_owned())
            };
            let recipe = recipe_source
                .split("[[dependencies]]")
                .next()
                .context("recipe is empty")?
                .replace(
                    "capability = \"service.runtime\"",
                    &format!(
                        "capability = \"{}\"\ncapacity = {minimum_capacity}",
                        if provides_odin {
                            ODIN_RENDEZVOUS_CAPABILITY
                        } else {
                            "service.runtime"
                        }
                    ),
                );
            let plan = compile_deployment_plan(
                recipe.as_bytes(),
                binding.as_bytes(),
                source(&recipe),
                "service-incarnation-1",
                Some(candidate_port),
                110,
                &[],
            )?;
            let release = SealedRelease::new(
                &plan,
                vec![artifact_receipt()],
                vec![external_input_receipt()],
                120,
            )?;
            let expected = release.expected_projection(&plan)?;
            assert!(expected.route.is_some() && expected.dependencies.is_empty());
            assert_eq!(expected.write_lease_required, stateful);
            assert_eq!(
                ReadinessClass::of(&expected),
                Ok(if provides_odin {
                    ReadinessClass::OdinSelf
                } else {
                    ReadinessClass::RouteProof
                })
            );

            let now = now_millis()?;
            let command = DeploymentCommand {
                schema_version: DEPLOYMENT_COMMAND_SCHEMA.into(),
                command_id: match kind {
                    CommandKind::Deploy => "up-service",
                    CommandKind::Continuity => "continuity-service",
                }
                .into(),
                kind,
                selector: "service".into(),
                requested_by: "test".into(),
                requested_at_unix_millis: 100,
            };
            let mut transaction =
                DeploymentTransaction::new(&command, "service".into(), 0, None, now)?;
            match kind {
                CommandKind::Continuity => {
                    transaction.lifecycle_authorized_at_unix_millis = Some(now);
                }
                CommandKind::Deploy => {
                    transaction.frozen_source = Some(FrozenSourceReceipt {
                        transaction_id: transaction.transaction_id.clone(),
                        plan_id: plan.plan_id.clone(),
                        snapshot_sha256: sha256_id(b"frozen source snapshot"),
                    });
                    transaction.deployment_authorization = Some(deployment_authorization(
                        &world,
                        &expected,
                        &transaction.transaction_id,
                        now,
                    )?);
                }
            }
            let launch = IdunnRuntimeActivationLaunch::issue(
                &expected,
                runtime_instance_id(&transaction.transaction_id)?,
                now,
                &world.engine.idunn_signer,
            )?;
            let mut seed = Vec::new();
            let activation = launch.write_credential(&mut seed)?;
            let recorded = fixture_transaction(FIXTURE_TRANSACTIONS[0].1)?.1;
            let mut workload = recorded.workload.clone().context("no recorded workload")?;
            match &mut workload {
                WorkloadObservation::Systemd(observed) => {
                    observed.runtime_instance_id = activation.runtime_instance_id.clone();
                    observed.executable_sha256 = expected.artifact_sha256.clone();
                }
                WorkloadObservation::Host(_) => bail!("recorded workload is not a systemd unit"),
            }
            transaction.isolation = recorded.isolation;
            transaction.installed_release = Some(crate::drivers::InstalledReleaseObservation {
                sealed_release_id: release.sealed_release_id.clone(),
                root: PathBuf::from("/srv/service/releases/test"),
            });
            transaction.expected_publication_sha256 = Some(expected.canonical_sha256()?);
            transaction.activation_publication_sha256 = Some(activation.canonical_sha256()?);
            transaction.sealed_release = Some(release);
            transaction.expected = Some(expected.clone());
            transaction.activation = Some(activation.clone());
            transaction.workload = Some(workload);
            transaction.plan = Some(plan);

            let stub = Arc::new(RuntimeStub {
                expected,
                activation,
                provider: open_service_identity_at::<GameCultProviderHealthIdentity>(
                    &provider_path,
                )?,
                activation_signer: cultnet_rs::IdunnRuntimeActivationSigner::from_credential_reader(
                    &seed[..],
                )?,
                state: Mutex::new("warming"),
                capacity: AtomicU32::new(minimum_capacity),
                sequence: AtomicU64::new(0),
                candidate_hits: AtomicUsize::new(0),
                stable_hits: AtomicUsize::new(0),
                stop: AtomicBool::new(false),
                hang_up: AtomicBool::new(false),
                lease: Mutex::new(None),
                reply: Mutex::new(Reply::Honest),
            });
            stub.listen(candidate_listener, true);
            stub.listen(stable_listener, false);

            if phase >= DeploymentPhase::Fencing && provides_odin {
                // Odin's own first word about the candidate, as for any
                // Odin-correlated target.
                let warming = signed_correlation(&world, &transaction, 4, false)?;
                let authenticated = world.engine.authenticate_topology_bytes(
                    &ControlSnapshot::read(&world.state_store)?,
                    &transaction,
                    &warming,
                    None,
                    now,
                )?;
                transaction.warming = Some(WarmingEvidence::OdinTopology {
                    evidence: TopologyEvidence::from_authenticated(&authenticated, now)?,
                });
                transaction.odin_publisher_sequence_cursor = 4;
            } else if phase >= DeploymentPhase::Fencing {
                let CandidateAnswer::Answered { evidence, .. } =
                    world
                        .engine
                        .challenge_candidate(&transaction, &["warming"], None)?
                else {
                    bail!("the stub candidate did not answer");
                };
                transaction.warming = Some(WarmingEvidence::RouteProofDirect { evidence });
            }
            if phase >= DeploymentPhase::Fencing {
                transaction.route_preflight = Some(candidate_preflight(&transaction)?);
            }
            if stateful {
                // The lease driver reads the Expected and the observed
                // activation from the topology store, as Starting left them.
                let plan = transaction.plan.as_ref().unwrap();
                let anchor = world.engine.provider_anchor_for_plan(plan)?;
                let topology = world.engine.topology();
                topology.publish_expected(transaction.expected.as_ref().unwrap(), &anchor)?;
                topology.publish_observed_activation(
                    transaction.expected.as_ref().unwrap(),
                    transaction.activation.as_ref().unwrap(),
                    transaction.workload.as_ref().unwrap(),
                )?;
            }
            transaction.enter_phase(phase, now);
            transaction.validate()?;
            let store = SingleFileMessagePackBackingStore::new(&world.state_store);
            assert!(store.compare_exchange(
                &[
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentCommand::TYPE.into(),
                        key: command.command_id.clone(),
                        current: None,
                    },
                    CultCacheExpectedEnvelope {
                        r#type: DeploymentTransaction::TYPE.into(),
                        key: transaction.transaction_id.clone(),
                        current: None,
                    },
                ],
                &[
                    command_envelope(&command, command.requested_at_unix_millis)?,
                    transaction_envelope(&transaction, now)?,
                ],
            )?);

            // Odin's world. Unreachable: the correlation store is bytes no
            // reader can decode, so any read of it is an error. NotReady: a
            // valid correlation that says the target is not Ready.
            let store = &world.engine.options.odin_correlation_store;
            let poisoned = match odin {
                _ if provides_odin => {
                    odin_reports_ready(&world, &transaction, 5)?;
                    Vec::new()
                }
                Odin::Unreachable => {
                    let garbage = b"odin is not a cache".to_vec();
                    std::fs::write(store, &garbage)?;
                    garbage
                }
                Odin::NotReady => {
                    let wrongly = signed_correlation(&world, &transaction, 9, false)?;
                    SingleFileMessagePackBackingStore::new(store).insert_entry_if_absent(
                        CultCacheEnvelope {
                            key: crate::drivers::incarnation_key_of(
                                &transaction.target,
                                &transaction.expected.as_ref().unwrap().canonical_sha256()?,
                            ),
                            r#type: OdinRuntimeTopologyCorrelationRecord::TYPE.into(),
                            payload: wrongly,
                            stored_at: rfc3339_millis(now_millis()?)?,
                            schema_id: Some(
                                cultnet_rs::ODIN_RUNTIME_TOPOLOGY_CORRELATION_SCHEMA.into(),
                            ),
                        },
                    )?;
                    Vec::new()
                }
            };
            Ok(RoutedWorld {
                world,
                workload: switch,
                stub,
                odin,
                poisoned,
            })
        }

        /// The preflight receipt of a candidate whose route was never
        /// installed: what Warming records before Fencing, without running the
        /// host's nginx.
        fn candidate_preflight(
            transaction: &DeploymentTransaction,
        ) -> Result<crate::drivers::RoutePreflightReceipt> {
            let rendered = NginxRouteDriver::new(
                transaction
                    .plan
                    .as_ref()
                    .unwrap()
                    .parsed_inputs()?
                    .1
                    .route
                    .context("no route binding")?,
            )
            .render(transaction.expected.as_ref().unwrap())?;
            Ok(crate::drivers::RoutePreflightReceipt {
                route_id: "service".into(),
                candidate_runtime_instance_id: transaction
                    .activation
                    .as_ref()
                    .unwrap()
                    .runtime_instance_id
                    .clone(),
                candidate_membership_sha256: sha256_id(&rendered),
                incumbent_runtime_instance_id: None,
                incumbent_membership_sha256: None,
                incumbent_configuration: None,
            })
        }

        impl RoutedWorld {
            fn step(&self) -> Result<()> {
                self.world.engine.advance_transaction(&resident(&self.world)?)
            }

            fn run_to_routing(&self) -> Result<()> {
                for _ in 0..12 {
                    if self.transaction()?.phase == DeploymentPhase::Routing {
                        return Ok(());
                    }
                    self.step()?;
                }
                bail!("the transaction never reached Routing")
            }

            fn transaction(&self) -> Result<DeploymentTransaction> {
                Ok(resident(&self.world)?.value)
            }

            /// Odin was never read: the poisoned store is byte for byte what
            /// it was, and no step failed on it.
            fn assert_odin_untouched(&self) -> Result<()> {
                if self.odin == Odin::Unreachable {
                    assert_eq!(
                        std::fs::read(&self.world.engine.options.odin_correlation_store)?,
                        self.poisoned
                    );
                }
                Ok(())
            }

            /// Routing runs for real: it installs the fragment through the
            /// world's stub host programs and proves the stable route against
            /// the stub runtime. The world only drives the steps until the
            /// transaction enters Committing.
            fn promote(&self) -> Result<()> {
                assert_eq!(self.transaction()?.phase, DeploymentPhase::Routing);
                for _ in 0..4 {
                    if self.transaction()?.phase == DeploymentPhase::Committing {
                        return Ok(());
                    }
                    self.step()?;
                }
                bail!("the transaction never reached Committing")
            }
        }

        /// A route-proof generation from a real admission.
        pub(super) fn committed_generation() -> Result<AdmittedGeneration> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            routed.promote()?;
            routed.step()?;
            admitted(&routed)
        }

        fn admitted(world: &RoutedWorld) -> Result<AdmittedGeneration> {
            Ok(ControlSnapshot::read(&world.world.state_store)?
                .admitted
                .into_iter()
                .next()
                .context("nothing was admitted")?
                .value)
        }

        #[test]
        fn a_route_proof_target_admits_with_odin_unreachable_and_never_reads_it() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            // Fencing, Leasing, AwaitingReady: the candidate is still warming.
            for _ in 0..12 {
                routed.step()?;
                if routed.transaction()?.last_error.is_some() {
                    break;
                }
            }
            let waiting = routed.transaction()?;
            assert_eq!(waiting.phase, DeploymentPhase::AwaitingReady);
            assert!(waiting.ready.is_none());
            assert_eq!(
                waiting.last_error.as_deref(),
                Some("candidate is still warming")
            );
            // The candidate finishes warming: Ready is its own Active answer.
            routed.stub.set_state("active");
            routed.step()?;
            assert!(matches!(
                routed.transaction()?.ready,
                Some(ReadinessEvidence::RouteProof { .. })
            ));
            routed.step()?;
            assert_eq!(routed.transaction()?.phase, DeploymentPhase::Routing);
            routed.promote()?;
            routed.step()?;

            let generation = admitted(&routed)?;
            generation.validate()?;
            assert_eq!(generation.ready.voucher(), Voucher::Candidate);
            assert!(generation.odin_authority.is_none());
            assert!(generation.latest_odin_observation.is_none());
            assert!(generation.route_supervision.is_some());
            // Idunn's own challenges carried every proof: the seed's warming,
            // two Ready polls; the stable endpoint proved the route twice.
            assert_eq!(routed.stub.candidate_hits.load(Ordering::SeqCst), 3);
            assert!(routed.stub.stable_hits.load(Ordering::SeqCst) >= 2);
            routed.assert_odin_untouched()?;

            // Boot and supervision read no Odin either: the topology refresh
            // returns before it opens Odin's store, and durable authority
            // skips a route-proof generation's receipts.
            let stored = ControlSnapshot::read(&routed.world.state_store)?;
            let current = stored.admitted.first().context("no admitted generation")?;
            assert!(!routed.world.engine.refresh_admitted_topology(&stored, current)?);
            routed.world.engine.validate_durable_authority(&stored)?;
            routed.assert_odin_untouched()?;
            Ok(())
        }

        #[test]
        fn an_odin_correlation_about_a_route_proof_target_decides_nothing() -> Result<()> {
            let routed = routed_world(Odin::NotReady, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            routed.promote()?;
            routed.step()?;
            let generation = admitted(&routed)?;
            assert_eq!(generation.ready.voucher(), Voucher::Candidate);
            // The valid, not-Ready correlation was neither admitted as a
            // receipt nor consulted: the record carries no Odin cursor.
            assert_eq!(generation.odin_publisher_sequence_cursor, 0);
            Ok(())
        }

        #[test]
        fn a_route_proof_candidate_that_has_not_answered_is_waited_on_not_aborted() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            routed.step()?;
            let warmed = routed.transaction()?;
            assert_eq!(warmed.phase, DeploymentPhase::Warming);
            let Some(WarmingEvidence::RouteProofDirect { evidence }) = &warmed.warming else {
                bail!("Warming evidence is not the candidate's own presence");
            };
            assert!(evidence.message_id.starts_with("candidate-"));
            assert_eq!(routed.stub.candidate_hits.load(Ordering::SeqCst), 1);
            routed.assert_odin_untouched()?;

            // Nothing answers: the same step waits, and says why.
            let silent = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            silent.stub.hang_up.store(true, Ordering::SeqCst);
            silent.step()?;
            let waiting = silent.transaction()?;
            assert!(waiting.warming.is_none() && waiting.pre_fencing_abort.is_none());
            assert!(
                waiting
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("candidate endpoint did not answer")),
                "{:?}",
                waiting.last_error
            );
            Ok(())
        }

        #[test]
        fn a_capacity_below_the_expected_minimum_is_named_on_the_route_path() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 2, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            routed.promote()?;
            routed.stub.capacity.store(1, Ordering::SeqCst);
            let failure = routed.step().expect_err("a shortfall must be refused");
            // The shortfall is a typed value, not a sentence to search.
            let disagrees = failure
                .downcast_ref::<PresenceDisagrees>()
                .with_context(|| format!("the refusal is not typed: {failure:#}"))?;
            let shortfall = disagrees
                .disagreements
                .iter()
                .find(|disagreement| disagreement.code == "expected-capability-000-capacity")
                .context("the capacity shortfall is not named")?;
            assert!(
                shortfall.expected.as_deref().is_some_and(|text| text.ends_with("capacity>=2")),
                "{shortfall:?}"
            );
            assert!(shortfall.observed.is_some(), "{shortfall:?}");
            assert!(admitted(&routed).is_err());
            Ok(())
        }

        #[test]
        fn odin_itself_is_never_route_proof() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let mut expected = routed.stub.expected.clone();
            assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::RouteProof));
            // Odin provides the rendezvous capability and declares no dependency.
            expected.capabilities[0].capability = ODIN_RENDEZVOUS_CAPABILITY.into();
            assert!(expected.dependencies.is_empty());
            assert_eq!(ReadinessClass::of(&expected), Ok(ReadinessClass::OdinSelf));

            // Route-proof evidence for it is refused: its admitted generation
            // is what names the Odin authority, so it must carry one.
            let (_, mut odin) = fixture_generation(FIXTURE_GENERATION)?;
            odin.target = "odin".into();
            odin.expected = expected;
            odin.ready = route_proof_evidence();
            odin.latest_odin_observation = None;
            odin.odin_authority = None;
            assert!(error_text(odin.validate()).contains("not route-proof"));
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn route_admission_installs_through_the_host_programs_without_reading_odin() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            // One real Routing step: the fragment goes in through the world's
            // stub nginx, ufw and systemctl, and the stable listener proves it.
            routed.step()?;
            assert!(matches!(
                routed.transaction()?.routing,
                Some(RoutingEvidence::Promoted { .. })
            ));
            assert_eq!(routed.world.route_stubs.count("systemctl reload"), 1);
            assert_eq!(routed.world.route_stubs.count("ufw allow"), 1);
            routed.assert_odin_untouched()?;
            Ok(())
        }

        #[test]
        fn a_stateless_candidate_that_finished_warming_first_still_warms() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            routed.stub.set_state("active");
            routed.step()?;
            assert!(matches!(
                routed.transaction()?.warming,
                Some(WarmingEvidence::RouteProofDirect { .. })
            ));
            Ok(())
        }

        #[test]
        fn the_lease_refresh_asks_the_candidate_for_a_new_warming_presence() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            let current = resident(&routed.world)?;
            let prior = routed
                .world
                .engine
                .rehydrate_warming_token(&current.value, now_millis()?, false)?;
            let (_, evidence, token) = routed
                .world
                .engine
                .fresh_warming_for_lease(&current, now_millis()?)?
                .context("a warming candidate must refresh")?;
            assert!(matches!(evidence, WarmingEvidence::RouteProofDirect { .. }));
            assert_ne!(token.signed_presence_sha256(), prior.signed_presence_sha256());
            // A candidate that went Active is not warming: nothing to bind.
            routed.stub.set_state("active");
            let failure = routed
                .world
                .engine
                .fresh_warming_for_lease(&current, now_millis()?)
                .map(|_| ())
                .expect_err("an active candidate is not a warming one");
            assert!(format!("{failure:#}").contains("not warming"), "{failure:#}");
            routed.assert_odin_untouched()?;
            Ok(())
        }

        #[test]
        fn boot_reauthenticates_a_live_route_proof_transactions_evidence() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            routed.world.engine.validate_durable_authority(&snapshot)?;

            // Warming evidence that does not answer the challenge it names.
            let current = resident(&routed.world)?;
            let mut forged = current.value.clone();
            let Some(WarmingEvidence::RouteProofDirect { evidence }) = &mut forged.warming else {
                bail!("no route-proof Warming evidence");
            };
            evidence.message_id = "candidate-another".into();
            replace_transaction(&routed.world.state_store, &current, &forged)?;
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            let failure = routed
                .world
                .engine
                .validate_durable_authority(&snapshot)
                .expect_err("Warming evidence for another challenge");
            assert!(format!("{failure:#}").contains("exact challenge"), "{failure:#}");

            // A Ready label carried by a presence that was only warming.
            let current = resident(&routed.world)?;
            let CandidateAnswer::Answered { evidence, .. } = routed
                .world
                .engine
                .challenge_candidate(&current.value, &["warming"], None)?
            else {
                bail!("the stub candidate did not answer");
            };
            let mut forged = current.value.clone();
            forged.warming = Some(WarmingEvidence::RouteProofDirect {
                evidence: evidence.clone(),
            });
            forged.ready = Some(ReadinessEvidence::RouteProof { evidence });
            replace_transaction(&routed.world.state_store, &current, &forged)?;
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            let failure = routed
                .world
                .engine
                .validate_durable_authority(&snapshot)
                .expect_err("Ready carried by a warming presence");
            assert!(format!("{failure:#}").contains("not active"), "{failure:#}");
            routed.assert_odin_untouched()?;
            Ok(())
        }

        #[test]
        fn a_target_providing_the_rendezvous_commits_on_odin_evidence_and_carries_the_authority()
        -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, true)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            let ready = routed.transaction()?;
            assert!(ready.ready.as_ref().and_then(ReadinessEvidence::odin).is_some());
            routed.promote()?;
            routed.step()?;
            let generation = admitted(&routed)?;
            generation.validate()?;
            assert_eq!(generation.ready.voucher(), Voucher::Odin);
            assert_eq!(
                generation.odin_authority,
                Some(routed.world.engine.bootstrap_odin_authority.clone())
            );
            assert!(generation.latest_odin_observation.is_some());
            // Boot re-authenticates every Odin receipt this generation carries.
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            routed.world.engine.validate_durable_authority(&snapshot)?;
            Ok(())
        }

        #[test]
        fn a_store_of_route_proof_generations_boots_and_resolves_the_bootstrap_authority()
        -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            routed.promote()?;
            routed.step()?;
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            routed.world.engine.validate_durable_authority(&snapshot)?;
            assert_eq!(
                routed.world.engine.current_odin_authority(&snapshot)?,
                routed.world.engine.bootstrap_odin_authority
            );
            Ok(())
        }

        impl RoutedWorld {
            /// Boot would accept this store, and Odin was never read.
            fn assert_boots(&self) -> Result<()> {
                let snapshot = ControlSnapshot::read(&self.world.state_store)?;
                self.world.engine.validate_durable_authority(&snapshot)?;
                self.assert_odin_untouched()
            }

            /// Step until `done`, checking after every step that boot would
            /// still accept the store.
            fn drive_until(&self, done: impl Fn(&DeploymentTransaction) -> bool) -> Result<()> {
                for _ in 0..20 {
                    if done(&self.transaction()?) {
                        return Ok(());
                    }
                    self.step()?;
                    self.assert_boots()?;
                }
                bail!("the transaction never reached the awaited state")
            }

            fn answer_now(&self) -> Result<(RuntimePresenceEvidence, cultnet_rs::VerifiedRuntimePresence)> {
                match self.world.engine.challenge_candidate(
                    &self.transaction()?,
                    &["warming", "active"],
                    None,
                )? {
                    CandidateAnswer::Answered { evidence, present } => Ok((evidence, present)),
                    CandidateAnswer::Silent(reason) => bail!("the candidate was silent: {reason}"),
                }
            }
        }

        // -----------------------------------------------------------------
        // The stateful path, end to end through the Engine.
        // -----------------------------------------------------------------

        #[test]
        fn a_stateful_route_proof_target_holds_its_lease_from_fencing_to_commit() -> Result<()> {
            let routed = stateful_routed_world(Odin::Unreachable, DeploymentPhase::Fencing)?;
            routed.assert_boots()?;

            routed.drive_until(|t| matches!(t.leasing, Some(LeasingEvidence::Prepared { .. })))?;
            assert_eq!(routed.transaction()?.phase, DeploymentPhase::Leasing);
            routed.drive_until(|t| matches!(t.leasing, Some(LeasingEvidence::Granted { .. })))?;
            routed.drive_until(|t| t.phase == DeploymentPhase::AwaitingReady)?;
            let lease_sha256 = routed
                .transaction()?
                .leasing
                .as_ref()
                .and_then(LeasingEvidence::lease_sha256)
                .context("no granted lease")?
                .to_owned();

            // The candidate has not picked the lease up: still warming.
            routed.step()?;
            assert_eq!(
                routed.transaction()?.last_error.as_deref(),
                Some("candidate is still warming")
            );
            assert!(routed.transaction()?.ready.is_none());
            // Active without the lease Idunn granted is not Ready.
            routed.stub.set_state("active");
            let refusal = routed
                .step()
                .expect_err("an active candidate without the granted lease is refused");
            assert!(
                format!("{refusal:#}").contains("exact current process write lease"),
                "{refusal:#}"
            );
            assert!(routed.transaction()?.ready.is_none());
            // Active holding exactly that lease is.
            *routed.stub.lease.lock().unwrap() = Some(lease_sha256.clone());
            routed.step()?;
            assert!(matches!(
                routed.transaction()?.ready,
                Some(ReadinessEvidence::RouteProof { .. })
            ));
            routed.assert_boots()?;

            routed.drive_until(|t| t.phase == DeploymentPhase::Routing)?;
            routed.promote()?;
            routed.assert_boots()?;
            routed.step()?;

            let generation = admitted(&routed)?;
            generation.validate()?;
            assert_eq!(generation.ready.voucher(), Voucher::Candidate);
            assert!(generation.expected.write_lease_required);
            assert_eq!(generation.leasing.lease_sha256(), Some(lease_sha256.as_str()));
            assert!(generation.odin_authority.is_none());
            routed.assert_boots()?;
            Ok(())
        }

        // -----------------------------------------------------------------
        // Warming is one challenge, then the phase moves.
        // -----------------------------------------------------------------

        #[test]
        fn warming_challenges_once_and_then_advances_on_every_tick() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            routed.step()?;
            let recorded = routed
                .transaction()?
                .warming
                .context("the first step recorded no Warming evidence")?;
            assert_eq!(routed.stub.candidate_hits.load(Ordering::SeqCst), 1);

            // The route preflight needs the host's nginx; its receipt is recorded
            // here so the phase can move on.
            let current = resident(&routed.world)?;
            let mut next = current.value.clone();
            next.route_preflight = Some(candidate_preflight(&next)?);
            replace_transaction(&routed.world.state_store, &current, &next)?;

            routed.drive_until(|t| t.phase == DeploymentPhase::Fencing)?;
            assert_eq!(
                routed.stub.candidate_hits.load(Ordering::SeqCst),
                1,
                "a recorded Warming answer is never asked for again"
            );
            assert_eq!(routed.transaction()?.warming, Some(recorded));
            Ok(())
        }

        #[test]
        fn a_stateful_candidate_may_not_answer_its_warming_challenge_active() {
            let mut expected = fixture_generation(FIXTURE_GENERATION).unwrap().1.expected;
            expected.write_lease_required = true;
            assert_eq!(route_proof_warming_states(&expected), ["warming"]);
            expected.write_lease_required = false;
            assert_eq!(route_proof_warming_states(&expected), ["warming", "active"]);
        }

        #[test]
        fn a_stateful_candidate_that_answers_warming_active_is_refused() -> Result<()> {
            let routed = stateful_routed_world(Odin::Unreachable, DeploymentPhase::Warming)?;
            routed.stub.set_state("active");
            let refusal = routed
                .step()
                .expect_err("a stateful candidate cannot be active before it holds a lease");
            assert!(format!("{refusal:#}").contains("not warming"), "{refusal:#}");
            assert!(routed.transaction()?.warming.is_none());
            Ok(())
        }

        // -----------------------------------------------------------------
        // What a challenged presence must prove.
        // -----------------------------------------------------------------

        #[test]
        fn a_presence_holds_the_write_lease_exactly_when_it_is_not_warming() -> Result<()> {
            let routed = stateful_routed_world(Odin::Unreachable, DeploymentPhase::Warming)?;
            let authority = routed
                .world
                .engine
                .runtime_authority(&routed.transaction()?)?;
            let lease = sha256_id(b"the granted lease");
            let other = sha256_id(b"some other lease");
            let judge = |state: &'static str,
                         held: Option<&str>,
                         current: Option<&str>,
                         states: &[&str]|
             -> Result<()> {
                let challenged = now_millis()?;
                routed.stub.set_state(state);
                *routed.stub.lease.lock().unwrap() = held.map(str::to_owned);
                let bytes = routed.stub.answer("candidate-probe")?;
                routed
                    .world
                    .engine
                    .authenticate_challenged_presence(
                        &authority,
                        states,
                        current,
                        "candidate-probe",
                        challenged,
                        now_millis()?,
                        &bytes,
                    )
                    .map(|_| ())
            };
            let either = ["warming", "active"];
            judge("warming", None, None, &either)?;
            judge("active", Some(&lease), Some(&lease), &either)?;
            for (state, held, current, named) in [
                // Warming holds nothing, whatever Idunn has granted.
                ("warming", Some(lease.as_str()), None, "write lease"),
                ("warming", Some(lease.as_str()), Some(lease.as_str()), "write lease"),
                // Anything else holds exactly the current grant.
                ("active", None, Some(lease.as_str()), "exact current process write lease"),
                ("active", Some(lease.as_str()), None, "exact current process write lease"),
                (
                    "active",
                    Some(other.as_str()),
                    Some(lease.as_str()),
                    "exact current process write lease",
                ),
            ] {
                let error = judge(state, held, current, &either)
                    .expect_err("a presence with the wrong lease is refused");
                assert!(
                    format!("{error:#}").contains(named),
                    "{state} {held:?} {current:?}: {error:#}"
                );
            }
            Ok(())
        }

        #[test]
        fn a_presence_minted_before_its_challenge_is_refused() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let authority = routed
                .world
                .engine
                .runtime_authority(&routed.transaction()?)?;
            let bytes = routed.stub.answer("candidate-probe")?;
            let minted = now_millis()?;
            // Answered before the challenge that claims it: an old presence
            // replayed for a new question.
            let replay = routed
                .world
                .engine
                .authenticate_challenged_presence(
                    &authority,
                    &["warming"],
                    None,
                    "candidate-probe",
                    minted + 1_000,
                    minted + 1_000,
                    &bytes,
                )
                .expect_err("a presence minted before its challenge");
            assert!(
                format!("{replay:#}").contains("minted before its challenge"),
                "{replay:#}"
            );
            let early = routed
                .world
                .engine
                .authenticate_challenged_presence(
                    &authority,
                    &["warming"],
                    None,
                    "candidate-probe",
                    minted + 1_000,
                    minted,
                    &bytes,
                )
                .expect_err("a receipt before its challenge");
            assert!(format!("{early:#}").contains("predates its challenge"), "{early:#}");
            Ok(())
        }

        #[test]
        fn direct_warming_evidence_is_bound_to_the_bytes_it_authenticated() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let id = routed.transaction()?.transaction_id;
            let direct = |evidence: RuntimePresenceEvidence| WarmingEvidence::RouteProofDirect { evidence };

            let (evidence, present) = routed.answer_now()?;
            SequenceAdmittedWarming::from_direct_presence(id.clone(), direct(evidence), present)?;

            // The digest is the authenticated presence's, but the bytes are not.
            let (mut evidence, present) = routed.answer_now()?;
            evidence.canonical_bytes.push(0);
            let error = SequenceAdmittedWarming::from_direct_presence(id.clone(), direct(evidence), present)
                .expect_err("bytes that are not the authenticated presence");
            assert!(format!("{error:#}").contains("differs from its authenticated"), "{error:#}");

            // The bytes are the authenticated presence's, but the digest is not.
            let (mut evidence, present) = routed.answer_now()?;
            evidence.canonical_sha256 = sha256_id(b"another presence");
            let error = SequenceAdmittedWarming::from_direct_presence(id.clone(), direct(evidence), present)
                .expect_err("a digest that is not the authenticated presence's");
            assert!(format!("{error:#}").contains("differs from its authenticated"), "{error:#}");

            // Evidence about another answer altogether.
            let (other, _) = routed.answer_now()?;
            let (_, present) = routed.answer_now()?;
            SequenceAdmittedWarming::from_direct_presence(id, direct(other), present)
                .expect_err("evidence about another answer");
            Ok(())
        }

        // -----------------------------------------------------------------
        // Validation: candidate-vouched evidence belongs to route-proof targets.
        // -----------------------------------------------------------------

        #[test]
        fn candidate_vouched_evidence_is_refused_on_a_target_odin_reports_on() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let (evidence, _) = routed.answer_now()?;
            let world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
            let odin_reported = transaction_at(&world, DeploymentPhase::Warming)?;
            assert_eq!(
                ReadinessClass::of(odin_reported.expected.as_ref().unwrap()),
                Ok(ReadinessClass::OdinCorrelated)
            );

            let mut warming = odin_reported.clone();
            warming.warming = Some(WarmingEvidence::RouteProofDirect {
                evidence: evidence.clone(),
            });
            assert!(error_text(warming.validate()).contains("not route-proof"));

            let mut ready = odin_reported.clone();
            ready.ready = Some(ReadinessEvidence::RouteProof {
                evidence: evidence.clone(),
            });
            assert!(error_text(ready.validate()).contains("not route-proof"));

            // Direct Odin warming is Odin's alone, by class: a route-proof
            // target may not claim it, and a target that provides the
            // rendezvous may, whatever it is named.
            let mut direct = routed.transaction()?;
            direct.warming = Some(WarmingEvidence::FirstOdinDirect {
                evidence: evidence.clone(),
            });
            assert!(error_text(direct.validate()).contains("reserved for Odin"));
            let odin = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, true)?;
            let mut direct = odin.transaction()?;
            assert_eq!(direct.target, "service");
            direct.warming = Some(WarmingEvidence::FirstOdinDirect { evidence });
            direct.validate()?;
            Ok(())
        }

        #[test]
        fn a_route_proof_transaction_past_awaiting_ready_needs_the_candidates_ready_proof() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            let routing = routed.transaction()?;
            routing.validate()?;
            let mut bare = routing.clone();
            bare.ready = None;
            assert!(error_text(bare.validate()).contains("lacks Ready evidence"));
            routed.promote()?;
            let mut bare = routed.transaction()?;
            assert_eq!(bare.phase, DeploymentPhase::Committing);
            bare.ready = None;
            assert!(error_text(bare.validate()).contains("lacks Ready evidence"));
            Ok(())
        }

        // -----------------------------------------------------------------
        // The class is the Expected's alone. Evidence collected under another
        // class is held and reported, never stepped, never repaired.
        // -----------------------------------------------------------------

        /// Odin's Ready for the world's target, as pre-B3 code recorded it for
        /// every routed target.
        fn odin_receipt_for(
            routed: &RoutedWorld,
            transaction: &DeploymentTransaction,
            sequence: u64,
        ) -> Result<TopologyEvidence> {
            odin_receipt_with(routed, transaction, sequence, true)
        }

        fn odin_receipt_with(
            routed: &RoutedWorld,
            transaction: &DeploymentTransaction,
            sequence: u64,
            ready: bool,
        ) -> Result<TopologyEvidence> {
            let now = now_millis()?;
            let bytes = signed_correlation(&routed.world, transaction, sequence, ready)?;
            let authenticated = routed.world.engine.authenticate_topology_bytes(
                &ControlSnapshot::read(&routed.world.state_store)?,
                transaction,
                &bytes,
                transaction
                    .leasing
                    .as_ref()
                    .and_then(LeasingEvidence::lease_sha256),
                now,
            )?;
            TopologyEvidence::from_authenticated(&authenticated, now)
        }

        fn stored(generation: &AdmittedGeneration) -> Stored<AdmittedGeneration> {
            Stored {
                value: generation.clone(),
                envelope: CultCacheEnvelope {
                    key: generation.target.clone(),
                    r#type: AdmittedGeneration::TYPE.into(),
                    payload: Vec::new(),
                    stored_at: "1970-01-01T00:00:00.100Z".into(),
                    schema_id: Some(ADMITTED_GENERATION_SCHEMA.into()),
                },
            }
        }

        #[test]
        fn a_pre_b3_transaction_over_a_route_proof_target_is_held_and_reported_once() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;

            // Ready as pre-B3 Idunn wrote it: an Odin receipt.
            let current = resident(&routed.world)?;
            let mut legacy = current.value.clone();
            let receipt = odin_receipt_for(&routed, &legacy, 9)?;
            legacy.latest_odin_observation = Some(receipt.clone());
            legacy.odin_publisher_sequence_cursor = 9;
            legacy.ready = Some(ReadinessEvidence::OdinCorrelated { evidence: receipt });
            replace_transaction(&routed.world.state_store, &current, &legacy)?;

            let disagreement = ReadinessDisagreement::WrongVoucher {
                target: "service".into(),
                required: ReadinessClass::RouteProof,
                collected: Voucher::Odin,
            };
            assert_eq!(legacy.readiness_disagreement(), Some(disagreement.clone()));
            // The record is readable and boot accepts it: held, not refused.
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            routed.world.engine.validate_durable_authority(&snapshot)?;

            let hits = (
                routed.stub.candidate_hits.load(Ordering::SeqCst),
                routed.stub.stable_hits.load(Ordering::SeqCst),
            );
            for _ in 0..4 {
                assert!(!routed.world.engine.run_scheduler_tick()?);
            }
            let held = resident(&routed.world)?;
            assert_eq!(held.value.phase, DeploymentPhase::Routing);
            assert!(held.value.pre_fencing_abort.is_none());
            assert!(held.value.post_fencing_abort.is_none());
            assert_eq!(held.value.ready, legacy.ready, "nothing re-derived the evidence");
            assert_eq!(
                held.value.last_error.as_deref(),
                Some(disagreement.to_string().as_str())
            );
            // Reported once: later ticks leave the record alone.
            routed.world.engine.run_scheduler_tick()?;
            assert_eq!(resident(&routed.world)?.envelope, held.envelope);
            assert_eq!(
                hits,
                (
                    routed.stub.candidate_hits.load(Ordering::SeqCst),
                    routed.stub.stable_hits.load(Ordering::SeqCst)
                ),
                "a held transaction is asked nothing"
            );
            routed.assert_odin_untouched()?;

            // An abort already under way is not held: it never reads the class.
            let mut aborting = held.value.clone();
            aborting.post_fencing_abort = Some(post_fencing_abort_intent(&aborting, "operator abort"));
            replace_transaction(&routed.world.state_store, &held, &aborting)?;
            let after = resident(&routed.world)?;
            let _ = routed.world.engine.resume_candidate(&after);
            assert_ne!(
                resident(&routed.world)?.value.last_error.as_deref(),
                Some(disagreement.to_string().as_str()),
                "an abort in progress runs instead of being held"
            );
            Ok(())
        }

        #[test]
        fn a_pre_b3_generation_over_a_route_proof_target_boots_and_is_reported_not_refreshed()
        -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            let at_routing = routed.transaction()?;
            routed.promote()?;
            routed.step()?;

            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            let current = snapshot.admitted.first().context("nothing was admitted")?;
            // Receipts that would not survive being re-proved: authentic, but
            // not Ready. Boot re-proves against the current Odin authority and
            // requires exact semantic Ready, never what a generation stores, so
            // this record is only bootable if boot does not re-prove it.
            let receipt = odin_receipt_with(&routed, &at_routing, 9, false)?;
            let mut legacy = current.value.clone();
            legacy.ready = ReadinessEvidence::OdinCorrelated {
                evidence: receipt.clone(),
            };
            legacy.latest_odin_observation = Some(receipt);
            // The stored authority is present because an Odin-tagged record
            // carries one; it is not what boot reads.
            let stranger = cultnet_rs::enroll_service_identity_at::<OdinTopologyIdentity>(
                &routed.world.root.join("identities/stranger.cc"),
            )?;
            legacy.odin_authority = Some(AdmittedOdinAuthority::from_anchor(&stranger.trust_anchor()?)?);
            legacy.odin_publisher_sequence_cursor = 9;
            assert!(
                SingleFileMessagePackBackingStore::new(&routed.world.state_store)
                    .compare_exchange(
                        &[CultCacheExpectedEnvelope {
                            r#type: AdmittedGeneration::TYPE.into(),
                            key: legacy.target.clone(),
                            current: Some(current.envelope.clone()),
                        }],
                        &[admitted_envelope(&legacy, now_millis()?)?],
                    )?
            );

            assert_eq!(
                legacy.readiness(),
                Err(ReadinessDisagreement::WrongVoucher {
                    target: "service".into(),
                    required: ReadinessClass::RouteProof,
                    collected: Voucher::Odin,
                })
            );
            let snapshot = ControlSnapshot::read(&routed.world.state_store)?;
            let current = snapshot.admitted.first().context("the generation is gone")?;
            // The class chooses the path: no Odin receipt is re-proved at boot
            // and none is refreshed, though the evidence is an Odin receipt.
            routed.world.engine.validate_durable_authority(&snapshot)?;
            assert!(!routed.world.engine.refresh_admitted_topology(&snapshot, current)?);
            routed.assert_odin_untouched()?;

            routed.world.engine.supervise_one_admitted_generation()?;
            assert!(
                routed
                    .world
                    .engine
                    .fault_reports
                    .lock()
                    .unwrap()
                    .contains_key("generation:service"),
                "supervision reports the disagreement"
            );
            routed.assert_odin_untouched()?;
            Ok(())
        }

        #[test]
        fn only_a_transaction_that_still_reads_its_class_is_held_for_its_evidence() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            let at_routing = routed.transaction()?;
            // What pre-B3 Idunn would have collected: an Odin receipt.
            let mut legacy = at_routing.clone();
            legacy.ready = Some(ReadinessEvidence::OdinCorrelated {
                evidence: odin_receipt_for(&routed, &at_routing, 9)?,
            });
            let disagreement = legacy.readiness_disagreement();
            assert!(disagreement.is_some());
            assert_eq!(legacy.held_disagreement(), disagreement);

            // Finished work, and an abort under way, never read the class.
            let mut completed = legacy.clone();
            completed.completion = Some(TransactionCompletion::Admitted {
                generation_id: "generation-1".into(),
            });
            assert_eq!(completed.held_disagreement(), None);
            let mut aborting = legacy.clone();
            aborting.pre_fencing_abort = Some(PreFencingAbort {
                error: "operator abort".into(),
                candidate_cleanup: CleanupEvidence::Pending,
                topology_reconciliation: CleanupEvidence::Pending,
                source_cleanup: CleanupEvidence::Pending,
            });
            assert_eq!(aborting.held_disagreement(), None);
            let mut aborting = legacy;
            aborting.post_fencing_abort = Some(post_fencing_abort_intent(&aborting, "operator abort"));
            assert_eq!(aborting.held_disagreement(), None);
            Ok(())
        }

        #[test]
        fn a_stored_record_that_declares_no_way_to_prove_readiness_is_held() -> Result<()> {
            let world = EngineFixture::new()?;
            // Odin's store is unreadable: any read of it is an error.
            std::fs::write(&world.engine.options.odin_correlation_store, b"odin is not a cache")?;

            let (_, mut generation) = fixture_generation(FIXTURE_GENERATION)?;
            let target = generation.expected.target.clone();
            assert_eq!(generation.readiness(), Ok(ReadinessClass::OdinCorrelated));
            generation.expected.dependencies.clear();
            generation.expected.route = None;
            let undeclared = UndeclaredReadiness { target: target.clone() };
            assert_eq!(
                generation.readiness(),
                Err(ReadinessDisagreement::Undeclared(undeclared.clone()))
            );
            assert!(
                !world
                    .engine
                    .refresh_admitted_topology(&ControlSnapshot::default(), &stored(&generation))?
            );

            // The mirror: Odin declared, but the receipt is the candidate's.
            let (_, mut odin_class) = fixture_generation(FIXTURE_GENERATION)?;
            odin_class.ready = route_proof_evidence();
            assert_eq!(
                odin_class.readiness(),
                Err(ReadinessDisagreement::WrongVoucher {
                    target: target.clone(),
                    required: ReadinessClass::OdinCorrelated,
                    collected: Voucher::Candidate,
                })
            );

            // A transaction says the same about what it has collected.
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let (evidence, _) = routed.answer_now()?;
            let odin_world = EngineFixture::with_workload(Arc::new(StillWorkload))?;
            let seeded = transaction_at(&odin_world, DeploymentPhase::Warming)?;
            assert_eq!(seeded.readiness_disagreement(), None);
            let mut collected = seeded.clone();
            collected.warming = Some(WarmingEvidence::RouteProofDirect { evidence });
            assert_eq!(
                collected.readiness_disagreement(),
                Some(ReadinessDisagreement::WrongVoucher {
                    target: "service".into(),
                    required: ReadinessClass::OdinCorrelated,
                    collected: Voucher::Candidate,
                })
            );
            let mut bare = seeded.clone();
            bare.expected.as_mut().unwrap().dependencies.clear();
            assert_eq!(
                bare.readiness_disagreement(),
                Some(ReadinessDisagreement::Undeclared(UndeclaredReadiness {
                    target: "service".into()
                }))
            );
            let mut no_expected = seeded;
            no_expected.expected = None;
            assert_eq!(no_expected.readiness_disagreement(), None);
            Ok(())
        }

        // -----------------------------------------------------------------
        // Odin is whichever target provides the rendezvous.
        // -----------------------------------------------------------------

        #[test]
        fn odin_is_the_target_that_provides_the_rendezvous_not_the_target_named_odin() -> Result<()> {
            let world = EngineFixture::new()?;
            let (_, fixture) = fixture_generation(FIXTURE_GENERATION)?;

            // A target named "odin" that merely depends on Odin is not Odin.
            let mut named = fixture.clone();
            named.target = "odin".into();
            named.expected.target = "odin".into();
            let mut snapshot = ControlSnapshot::default();
            snapshot.admitted.push(stored(&named));
            assert!(snapshot.admitted_odin().is_none());
            assert_eq!(
                world.engine.current_odin_authority(&snapshot)?,
                world.engine.bootstrap_odin_authority
            );

            // The target that provides the rendezvous is, whatever it is called.
            let mut provider = fixture.clone();
            provider.target = "verse".into();
            provider.expected.target = "verse".into();
            provider.expected.dependencies.clear();
            provide_odin(&mut provider.expected);
            let authority = AdmittedOdinAuthority::from_anchor(&world.odin_signer.trust_anchor()?)?;
            provider.odin_authority = Some(authority.clone());
            snapshot.admitted.push(stored(&provider));
            assert_eq!(
                snapshot.admitted_odin().map(|found| found.value.target.as_str()),
                Some("verse")
            );
            assert_eq!(
                snapshot.admitted_odin().and_then(|found| found.value.odin_authority.clone()),
                Some(authority)
            );
            Ok(())
        }

        #[test]
        fn a_target_that_provides_the_rendezvous_warms_as_odin_whatever_it_is_named() -> Result<()> {
            // Nothing else has been admitted that could observe it, so Odin
            // observes itself directly -- and only a stateful incarnation may.
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, true)?;
            assert_eq!(routed.transaction()?.target, "service");
            let refusal = routed
                .step()
                .expect_err("a stateless Odin cannot be observed directly");
            assert!(
                format!("{refusal:#}").contains("direct Odin warming must be a stateful incarnation"),
                "{refusal:#}"
            );
            Ok(())
        }

        #[test]
        fn direct_odin_warming_is_reserved_to_odin_by_class_not_by_name() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            let mut named = routed.transaction()?;
            named.target = "odin".into();
            let error = routed
                .world
                .engine
                .authenticate_first_odin_warming_presence(&named, "warming-probe", 1, 2, &[1])
                .expect_err("a target named odin that is not Odin");
            assert!(
                format!("{error:#}").contains("reserved for a stateful Odin incarnation"),
                "{error:#}"
            );
            Ok(())
        }

        #[test]
        fn a_stateful_target_that_provides_the_rendezvous_is_observed_directly_whatever_it_is_named()
        -> Result<()> {
            let provider = build_routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, true, true, CommandKind::Continuity)?;
            let transaction = provider.transaction()?;
            assert_eq!(transaction.target, "service");
            assert!(transaction.expected.as_ref().unwrap().write_lease_required);
            provider.stub.set_state("warming");
            *provider.stub.reply.lock().unwrap() = Reply::FirstOdinWarming;
            let challenged_at = now_millis()?;
            let answer = provider.stub.answer("warming-probe")?;
            provider.world.engine.authenticate_first_odin_warming_presence(
                &transaction,
                "warming-probe",
                challenged_at,
                now_millis()?,
                &answer,
            )?;

            // The reverse: named odin, stateful, but nothing provides the
            // rendezvous.
            let stateful = stateful_routed_world(Odin::Unreachable, DeploymentPhase::Warming)?;
            let mut named = stateful.transaction()?;
            assert!(named.expected.as_ref().unwrap().write_lease_required);
            named.target = "odin".into();
            let error = stateful
                .world
                .engine
                .authenticate_first_odin_warming_presence(&named, "warming-probe", 1, 2, &[1])
                .expect_err("a stateful target named odin that does not provide the rendezvous");
            assert!(
                format!("{error:#}").contains("reserved for a stateful Odin incarnation"),
                "{error:#}"
            );
            Ok(())
        }

        // -----------------------------------------------------------------
        // A bad answer is an error. Only silence waits.
        // -----------------------------------------------------------------

        #[test]
        fn a_bad_answer_to_a_challenge_is_an_error_and_only_silence_waits() -> Result<()> {
            for reply in [Reply::ForeignChallengeId, Reply::HttpFailure] {
                let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
                *routed.stub.reply.lock().unwrap() = reply;
                let error = match routed.world.engine.challenge_candidate(
                    &routed.transaction()?,
                    &["warming"],
                    None,
                ) {
                    Err(error) => error,
                    Ok(_) => bail!("a bad answer was taken for silence"),
                };
                assert!(
                    matches!(
                        error.downcast_ref::<ChallengeFailure>(),
                        Some(ChallengeFailure::Refused(_))
                    ),
                    "{error:#}"
                );
                // Before the fence a candidate that answers wrongly is aborted,
                // not waited on.
                routed.world.engine.run_scheduler_tick()?;
                let after = routed.transaction()?;
                assert!(after.pre_fencing_abort.is_some(), "the bad answer aborted it");
                assert!(after.warming.is_none());
            }

            let silent = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            silent.stub.hang_up.store(true, Ordering::SeqCst);
            assert!(matches!(
                silent.world.engine.challenge_candidate(&silent.transaction()?, &["warming"], None)?,
                CandidateAnswer::Silent(_)
            ));
            Ok(())
        }

        // -----------------------------------------------------------------
        // B2: the route gate, the meters and the supervision pass, through
        // the Engine, over the world's stub host programs.
        // -----------------------------------------------------------------

        impl RoutedWorld {
            /// Run a fresh world to an admitted generation whose transaction
            /// is retired: what admitted-route supervision looks at.
            fn admit(&self) -> Result<()> {
                self.stub.set_state("active");
                self.run_to_routing()?;
                self.promote()?;
                self.drive_until(|transaction| transaction.is_terminal())?;
                assert!(self.world.engine.retire_one_terminal_transaction()?);
                Ok(())
            }

            fn fragment_path(&self) -> PathBuf {
                self.world.root.join("service.conf")
            }

            /// The target's route ledger holds `entries` recent actuations.
            fn ledger(&self, entries: usize) -> Result<()> {
                let now = now_millis()?;
                let mut meters = TargetSupervision::new("service");
                meters.route_actuations = (0..entries as u64).map(|offset| now - 1_000 + offset).collect();
                set_meters(&self.world, &meters)
            }

            fn reloads(&self) -> usize {
                self.world.route_stubs.count("systemctl reload")
            }

            /// Something else rewrote the fragment: the admitted membership drifted.
            fn drift(&self) -> Result<()> {
                std::fs::write(self.fragment_path(), b"drifted bytes\n")?;
                Ok(())
            }

            fn tick(&self) -> Result<bool> {
                self.world.engine.supervise_one_admitted_generation()
            }
        }

        #[cfg(unix)]
        #[test]
        fn a_full_ledger_still_restores_the_admitted_and_incumbent_route() -> Result<()> {
            let full = ROUTE_ACTUATION_CEILING;
            let newest = |world: &EngineFixture| -> Result<u64> {
                let meters = meters_of(world, "service")?;
                assert_eq!(meters.route_actuations.len(), ROUTE_ACTUATION_CEILING);
                Ok(*meters.route_actuations.last().unwrap())
            };

            // (i) A post-fence abort withdraws the candidate's membership.
            let aborting = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            aborting.ledger(full)?;
            let before = newest(&aborting.world)?;
            aborting
                .world
                .engine
                .begin_post_fencing_abort(&resident(&aborting.world)?, anyhow!("test"))?;
            aborting.step()?;
            assert_eq!(aborting.reloads(), 1, "the withdrawal did not run");
            assert!(newest(&aborting.world)? > before, "the withdrawal was not counted");

            // (ii) Supervision repairs a drifted admitted fragment.
            let supervised = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            supervised.admit()?;
            supervised.ledger(full)?;
            let (before, reloads) = (newest(&supervised.world)?, supervised.reloads());
            supervised.drift()?;
            assert!(supervised.tick()?);
            assert_eq!(supervised.reloads(), reloads + 1, "the repair did not run");
            assert!(newest(&supervised.world)? > before, "the repair was not counted");
            assert!(admitted(&supervised)?.route_supervision.unwrap().degraded_since_unix_millis.is_none());

            // (iii) A failed proof at Routing rolls the route back, though the
            // install that preceded it took the last free slot.
            let failing = routed_deploy_world(Odin::Unreachable, 1, DeploymentPhase::Fencing)?;
            failing.stub.set_state("active");
            failing.run_to_routing()?;
            failing.ledger(full - 1)?;
            failing.stub.hang_up.store(true, Ordering::SeqCst);
            let before = now_millis()?;
            assert!(failing.step().is_err(), "an unanswered stable route was admitted");
            assert_eq!(failing.reloads(), 2, "install then rollback");
            assert!(newest(&failing.world)? >= before, "the rollback was not counted");
            assert!(!failing.fragment_path().exists(), "the rollback left the candidate's route");
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_continuity_restart_is_never_refused_by_a_full_ledger() -> Result<()> {
            // Warming: the preflight of a continuity runs and is counted.
            let warming = routed_world(Odin::Unreachable, 1, DeploymentPhase::Warming, false)?;
            warming.step()?;
            assert!(warming.transaction()?.warming.is_some());
            warming.ledger(ROUTE_ACTUATION_CEILING)?;
            let before = *meters_of(&warming.world, "service")?.route_actuations.last().unwrap();
            warming.step()?;
            assert!(warming.transaction()?.route_preflight.is_some(), "the preflight was refused");
            assert!(*meters_of(&warming.world, "service")?.route_actuations.last().unwrap() > before);

            // Routing: its install runs and is counted.
            let routing = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routing.stub.set_state("active");
            routing.run_to_routing()?;
            routing.ledger(ROUTE_ACTUATION_CEILING)?;
            let before = *meters_of(&routing.world, "service")?.route_actuations.last().unwrap();
            routing.step()?;
            assert!(routing.reloads() >= 1, "the install did not run");
            assert!(routing.fragment_path().exists());
            assert!(*meters_of(&routing.world, "service")?.route_actuations.last().unwrap() > before);
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_survival_that_cannot_be_counted_runs_and_a_forward_is_refused() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            let transaction = routed.transaction()?;
            let preflight = transaction.route_preflight.clone().context("no preflight")?;
            let driver = routed.world.engine.route_driver(
                transaction
                    .plan
                    .as_ref()
                    .unwrap()
                    .parsed_inputs()?
                    .1
                    .route
                    .context("no route binding")?,
            );
            // The ledger cannot be read, so nothing can be counted.
            std::fs::write(&routed.world.state_store, b"unreadable")?;
            let engine = &routed.world.engine;

            let refused = engine
                .route_gate("service", CommandKind::Deploy)
                .admit(RouteActuation::Forward)
                .unwrap_err();
            assert!(refused.downcast_ref::<RouteActuationRefused>().is_none(), "{refused:#}");
            assert!(engine.charge_route_actuation("service", RouteActuation::Forward).is_err());

            engine.charge_route_actuation("service", RouteActuation::Survival)?;
            driver.withdraw_candidate_membership(
                &preflight,
                &engine.route_gate("service", CommandKind::Deploy),
            )?;
            assert_eq!(routed.reloads(), 1, "the uncounted withdrawal did not run");
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_second_failed_challenge_waits_twice_as_long_before_the_next_repair() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            routed.stub.hang_up.store(true, Ordering::SeqCst);
            let elapse = |routed: &RoutedWorld| {
                edit_incumbent(&routed.world, |generation| {
                    generation.route_supervision.as_mut().unwrap().next_challenge_at_unix_millis =
                        Some(1);
                })
            };

            routed.drift()?;
            assert!(routed.tick()?);
            elapse(&routed)?;
            routed.drift()?;
            assert!(routed.tick()?);
            let state = admitted(&routed)?.route_supervision.context("no route state")?;
            assert_eq!(state.consecutive_failures, 2);
            assert_eq!(
                state.next_challenge_at_unix_millis.unwrap() - state.last_challenge_at_unix_millis.unwrap(),
                2 * DEFAULT_TOPOLOGY_MAXIMUM_AGE_MILLIS
            );

            // The wait is longer than one observation age: no challenge, so no
            // repair and no reload, however often the pass runs.
            let reloads = routed.reloads();
            for _ in 0..20 {
                routed.drift()?;
                routed.tick()?;
            }
            assert_eq!(routed.reloads(), reloads, "the route was repaired inside its wait");
            assert_eq!(
                admitted(&routed)?.route_supervision.unwrap().consecutive_failures,
                2
            );
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_proved_challenge_that_cannot_be_recorded_ends_the_pass_without_failing_it() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            let old = ControlSnapshot::read(&routed.world.state_store)?;
            let stale = old.admitted_for("service").context("no generation")?;
            // Something else writes the generation after the pass read it.
            edit_incumbent(&routed.world, |generation| {
                generation.route_supervision.as_mut().unwrap().last_challenge_at_unix_millis = Some(5);
            })?;
            routed.drift()?;
            assert!(routed.world.engine.supervise_admitted_route(stale)?);
            assert_eq!(
                admitted(&routed)?.route_supervision.unwrap().last_challenge_at_unix_millis,
                Some(5),
                "the stale write replaced the newer generation"
            );
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_first_routed_deploy_is_metered_and_refused_at_the_ceiling() -> Result<()> {
            let metered = routed_deploy_world(Odin::Unreachable, 1, DeploymentPhase::Fencing)?;
            metered.stub.set_state("active");
            metered.run_to_routing()?;
            assert!(ControlSnapshot::read(&metered.world.state_store)?.targets.is_empty());
            metered.step()?;
            assert_eq!(meters_of(&metered.world, "service")?.route_actuations.len(), 1);

            let refused = routed_deploy_world(Odin::Unreachable, 1, DeploymentPhase::Fencing)?;
            refused.stub.set_state("active");
            refused.run_to_routing()?;
            refused.ledger(ROUTE_ACTUATION_CEILING)?;
            let error = refused.step().unwrap_err();
            assert!(error.downcast_ref::<RouteActuationRefused>().is_some(), "{error:#}");
            assert_eq!(refused.world.route_stubs.count(""), 0, "a refused install ran a program");
            assert!(!refused.fragment_path().exists());

            // Past the fence the refusal is resumable, not an abort: the resume
            // backoff spaces it and no actuation runs while it waits.
            assert!(!refused.world.engine.resume_one_transaction()?);
            let waiting = refused.transaction()?;
            assert_eq!(waiting.phase, DeploymentPhase::Routing);
            assert!(waiting.post_fencing_abort.is_none());
            assert!(waiting.last_error.as_deref().is_some_and(|text| text.contains("ceiling")));
            assert_eq!(refused.world.route_stubs.count(""), 0);
            Ok(())
        }

        #[test]
        fn a_stepped_back_clock_is_settled_where_the_decision_is_made() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            let ahead = now_millis()? + 86_400_000;

            // A supervision pass that would decide from the restart log settles
            // it first: the write ends the pass, and nothing is minted from
            // entries that only look recent because the clock stepped back.
            let mut restarts = TargetSupervision::new("service");
            restarts.continuity_restarts = vec![ahead; CONTINUITY_RESTART_ATTEMPTS];
            set_meters(&routed.world, &restarts)?;
            routed.workload.kill();
            assert!(routed.tick()?);
            assert!(ControlSnapshot::read(&routed.world.state_store)?.transactions.is_empty());
            let settled = meters_of(&routed.world, "service")?;
            assert!(settled.continuity_restarts.iter().all(|&at| at < ahead));

            // A refused forward actuation writes the clamp, so the window runs
            // from now and not from the day the clock stood on.
            let mut ledger = TargetSupervision::new("service");
            ledger.route_actuations = vec![ahead; ROUTE_ACTUATION_CEILING];
            set_meters(&routed.world, &ledger)?;
            let refused = routed
                .world
                .engine
                .charge_route_actuation("service", RouteActuation::Forward)
                .unwrap_err();
            assert!(refused.downcast_ref::<RouteActuationRefused>().is_some());
            let settled = meters_of(&routed.world, "service")?;
            assert!(settled.route_actuations.iter().all(|&at| at < ahead));
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_refused_preflight_starts_no_private_mount_unit_and_fails_at_once_with_the_reopen_time()
        -> Result<()> {
            let routed = routed_deploy_world(Odin::Unreachable, 1, DeploymentPhase::Warming)?;
            routed.step()?;
            assert!(routed.transaction()?.warming.is_some());
            routed.ledger(ROUTE_ACTUATION_CEILING)?;
            let reopens_at = meters_of(&routed.world, "service")?
                .route_reopens_at(now_millis()?)
                .context("the ledger is not full")?;

            // The refusal is an error of its own phase, before the fence: the
            // deployment fails now, and its error carries the reopen time.
            let id = routed.transaction()?.transaction_id;
            routed.world.engine.resume_one_transaction()?;
            let abort = routed
                .transaction()?
                .pre_fencing_abort
                .context("the refusal did not abort the deployment")?;
            assert!(abort.error.contains(&format!("reopens at unix ms {reopens_at}")), "{}", abort.error);
            for _ in 0..8 {
                if record_of(&routed.world, &id)?.completion.is_some() {
                    break;
                }
                routed.step()?;
            }
            let Some(TransactionCompletion::FailedBeforeFencing { error }) =
                record_of(&routed.world, &id)?.completion
            else {
                let record = record_of(&routed.world, &id)?;
                bail!(
                    "the deployment did not fail before the fence: phase {:?} completion {:?} abort {:?} error {:?}",
                    record.phase, record.completion, record.pre_fencing_abort, record.last_error
                );
            };
            assert!(error.contains(&format!("reopens at unix ms {reopens_at}")), "{error}");
            assert_eq!(routed.world.route_stubs.count(""), 0, "a program ran");
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_failed_challenge_and_a_dead_unit_in_one_tick_do_not_lose_a_cas() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            routed.workload.kill();
            routed.stub.hang_up.store(true, Ordering::SeqCst);
            routed.drift()?;

            // Tick 1: the repair charges the target's meters and the proof
            // fails. Both are writes, so they end the pass: nothing else is
            // decided from the snapshot they made stale.
            assert!(routed.tick()?);
            let state = admitted(&routed)?.route_supervision.context("no route state")?;
            assert_eq!(state.consecutive_failures, 1);
            assert!(state.degraded_since_unix_millis.is_some());
            assert!(ControlSnapshot::read(&routed.world.state_store)?.transactions.is_empty());
            assert_eq!(meters_of(&routed.world, "service")?.route_actuations.len(), 2);

            // Tick 2: the route waits, and the dead unit is restarted from a
            // fresh read, counted in the same write that mints it.
            assert!(routed.tick()?);
            assert_eq!(ControlSnapshot::read(&routed.world.state_store)?.transactions.len(), 1);
            assert_eq!(meters_of(&routed.world, "service")?.continuity_restarts.len(), 1);
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_proved_challenge_clears_the_degradation() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            routed.stub.hang_up.store(true, Ordering::SeqCst);
            routed.drift()?;
            assert!(routed.tick()?);
            let degraded = admitted(&routed)?.route_supervision.context("no route state")?;
            assert_eq!(degraded.consecutive_failures, 1);
            assert!(degraded.degraded_since_unix_millis.is_some());
            assert!(degraded.next_challenge_at_unix_millis.is_some());

            routed.stub.hang_up.store(false, Ordering::SeqCst);
            edit_incumbent(&routed.world, |generation| {
                generation.route_supervision.as_mut().unwrap().next_challenge_at_unix_millis = Some(1);
            })?;
            routed.drift()?;
            assert!(routed.tick()?);
            let healed = admitted(&routed)?.route_supervision.context("no route state")?;
            assert_eq!(healed.consecutive_failures, 0);
            assert_eq!(healed.next_challenge_at_unix_millis, None);
            assert_eq!(healed.degraded_since_unix_millis, None);
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_failing_reload_that_deletes_the_fragment_backs_the_route_off() -> Result<()> {
            let routed = routed_world(Odin::Unreachable, 1, DeploymentPhase::Fencing, false)?;
            routed.admit()?;
            routed.world.route_stubs.refuse_reloads()?;
            let reloads = routed.reloads();
            routed.drift()?;
            for _ in 0..200 {
                routed.tick()?;
            }
            assert_eq!(routed.reloads() - reloads, 1, "the route was reloaded again and again");
            assert!(!routed.fragment_path().exists());
            assert!(admitted(&routed)?.route_supervision.unwrap().degraded_since_unix_millis.is_some());
            Ok(())
        }

        #[cfg(unix)]
        #[test]
        fn a_candidate_whose_route_proof_fails_reloads_at_most_the_ceiling() -> Result<()> {
            let routed = routed_deploy_world(Odin::Unreachable, 1, DeploymentPhase::Fencing)?;
            routed.stub.set_state("active");
            routed.run_to_routing()?;
            routed.stub.hang_up.store(true, Ordering::SeqCst);
            let mut last = None;
            for _ in 0..30 {
                last = Some(routed.step().unwrap_err());
            }
            // Every install is followed by its rollback and both are counted, so
            // twelve reloads spend the window; the rest are refused.
            assert_eq!(routed.reloads(), ROUTE_ACTUATION_CEILING);
            let last = last.unwrap();
            assert!(last.downcast_ref::<RouteActuationRefused>().is_some(), "{last:#}");
            Ok(())
        }
    }
}
