//! The host actuator contract, and the Idunn-side drivers that speak it.
//!
//! A managed host that Idunn cannot actuate locally runs one `idunn-host`
//! process in the session the workload needs. That process dials Idunn over
//! CultNet RUDP and executes the consequences of the runner and workload
//! ports for its host. It decides nothing: every request is signed by Idunn's
//! service identity, every report by the host's, and both ends refuse the
//! other's message unless the signature, the session nonce and the sequence
//! all hold. The host opens no inbound port and holds no credential for the
//! Idunn host beyond Idunn's public anchor.
//!
//! Both binaries are built from this module, so the wire records are the
//! Rust types and nothing is hand-encoded on either side.

use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use cultnet_rs::{
    CultNetRudpServerEvent, CultNetRudpServerHub, CultNetRudpServerHubOptions,
    CultNetRudpServerSessionContext, IdunnExpectedIncarnationRecord, IdunnRuntimeActivationLaunch,
    IdunnRuntimeActivationRecord, IdunnServiceIdentity, ServiceIdentityProfile,
    ServiceIdentitySignature, ServiceIdentitySigner, ServiceIdentityTrustAnchor,
    ServiceSignaturePurpose, verify_service_identity_signature,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::deployment_plan::{CompiledDeploymentPlan, SealedRelease};
use crate::drivers::{
    FrozenSource, HostWorkloadObservation, InstalledReleaseObservation, MaterializedRelease,
    RunnerPort, WorkloadObservation, WorkloadPort,
};

pub const HOST_ACTUATOR_PROTOCOL: &str = "gamecult.idunn.host_actuator.v1";
/// The reliable, ordered CultNet channel. Every request and report is a
/// document that must arrive exactly once and in order.
pub const HOST_ACTUATOR_CHANNEL: &str = "schema";
/// A session whose peer has been silent this long is gone. The actuator pings
/// at a third of it.
pub const HOST_ACTUATOR_SESSION_TIMEOUT_MS: u64 = 15_000;
pub const HOST_ACTUATOR_PING_MS: u64 = 5_000;
/// Every request carries the connection id CultNet uses to fence a stale
/// endpoint. One value for the whole protocol; the session generation and the
/// nonce carry the real identity.
pub const HOST_ACTUATOR_CONNECTION_ID: u32 = 0x4944_4e48; // "IDNH"

const REQUEST_POLL: Duration = Duration::from_millis(20);
const MATERIALIZE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const START_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// The service identity a host actuator signs its reports with. One per host,
/// enrolled on the host; its public anchor is installed on the Idunn host and
/// named by the workload binding.
pub enum IdunnHostActuatorIdentity {}

impl ServiceIdentityProfile for IdunnHostActuatorIdentity {
    const PRIVATE_TYPE: &'static str = "idunn.host_actuator_identity.private.v1";
    const PRIVATE_SCHEMA: &'static str = "idunn.host_actuator_identity.private.v1";
    const PRIVATE_KEY: &'static str = "idunn-host-actuator-identity";
    const TRUST_ANCHOR_TYPE: &'static str = "idunn.host_actuator_identity.trust_anchor.v1";
    const TRUST_ANCHOR_SCHEMA: &'static str = "idunn.host_actuator_identity.trust_anchor.v1";
    const TRUST_ANCHOR_KEY: &'static str = "idunn-host-actuator-identity-public";
    const ID_DOMAIN: &'static [u8] = b"idunn.host-actuator-identity.id.v1\0";
    const SIGNATURE_DOMAIN: &'static [u8] = b"idunn.host-actuator-identity.signature.v1\0";
    const PROTECTOR_CONTEXT: &'static str = "idunn-host-actuator-identity-v1";
}

pub struct HostActuatorRequestPurpose;

impl ServiceSignaturePurpose<IdunnServiceIdentity> for HostActuatorRequestPurpose {
    const PURPOSE: &'static [u8] = b"idunn.host_actuator.request.v1";
}

pub struct HostActuatorReportPurpose;

impl ServiceSignaturePurpose<IdunnHostActuatorIdentity> for HostActuatorReportPurpose {
    const PURPOSE: &'static [u8] = b"idunn.host_actuator.report.v1";
}

/// What Idunn asks a host to do. Each carries everything the actuator needs;
/// the actuator keeps no transaction state between requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostActuatorRequest {
    /// The first request on a session, answering the actuator's Hello with
    /// the nonce every later message on this session must carry.
    Welcome,
    Materialize {
        transaction_id: String,
        plan: CompiledDeploymentPlan,
        sealed_at_unix_millis: u64,
    },
    Install {
        plan: CompiledDeploymentPlan,
        release: SealedRelease,
        root: String,
    },
    PrepareActivation {
        plan: CompiledDeploymentPlan,
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
        credential: Vec<u8>,
    },
    Start {
        plan: CompiledDeploymentPlan,
        release: SealedRelease,
        installed: InstalledReleaseObservation,
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
    },
    Observe {
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
        prior: HostWorkloadObservation,
    },
    Stop {
        observation: HostWorkloadObservation,
    },
    /// Is the prior process still running? Answered without touching it.
    Probe { prior: HostWorkloadObservation },
    Discard {
        plan: CompiledDeploymentPlan,
        expected: IdunnExpectedIncarnationRecord,
        activation: IdunnRuntimeActivationRecord,
    },
}

/// What a host reports back. `Failed` is the only report that may answer any
/// request; every other one answers exactly the request of its kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostActuatorReport {
    Hello {
        platform: String,
        actuator_version: String,
    },
    Welcomed,
    Materialized {
        release: SealedRelease,
        root: String,
    },
    Installed {
        installed: InstalledReleaseObservation,
    },
    Prepared {
        activation: IdunnRuntimeActivationRecord,
    },
    Started {
        observation: HostWorkloadObservation,
    },
    Observed {
        observation: HostWorkloadObservation,
    },
    Stopped,
    Probed {
        running: bool,
    },
    Discarded,
    Failed {
        error: String,
    },
}

/// The signed frame both directions use. `session_nonce` is empty only in the
/// Hello that opens a session; Idunn mints it in Welcome and every later
/// message on the session must carry it. `sequence` is the request's number;
/// a report echoes the sequence of the request it answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostActuatorEnvelope {
    pub protocol: String,
    pub host: String,
    pub session_nonce: Vec<u8>,
    pub sequence: u64,
    pub body: Vec<u8>,
    pub signer_identity_id: String,
    pub signature: Vec<u8>,
}

impl HostActuatorEnvelope {
    fn signed_payload(&self) -> Result<Vec<u8>> {
        Ok(rmp_serde::to_vec(&(
            &self.protocol,
            &self.host,
            &self.session_nonce,
            self.sequence,
            &self.body,
        ))?)
    }

    fn validate_shape(&self, host: &str) -> Result<()> {
        ensure!(
            self.protocol == HOST_ACTUATOR_PROTOCOL,
            "host actuator envelope speaks {:?}, not {HOST_ACTUATOR_PROTOCOL}",
            self.protocol
        );
        ensure!(
            self.host == host,
            "host actuator envelope is for host {:?}, not {host:?}",
            self.host
        );
        ensure!(!self.body.is_empty(), "host actuator envelope has no body");
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(rmp_serde::to_vec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(bytes).context("decoding host actuator envelope")
    }

    /// Sign a request as Idunn.
    pub fn request(
        host: &str,
        session_nonce: &[u8],
        sequence: u64,
        request: &HostActuatorRequest,
        signer: &ServiceIdentitySigner<IdunnServiceIdentity>,
    ) -> Result<Self> {
        let mut envelope = Self {
            protocol: HOST_ACTUATOR_PROTOCOL.into(),
            host: host.into(),
            session_nonce: session_nonce.to_vec(),
            sequence,
            body: rmp_serde::to_vec(request)?,
            signer_identity_id: signer.entry().identity_id.clone(),
            signature: Vec::new(),
        };
        let proof = signer.sign::<HostActuatorRequestPurpose>(&envelope.signed_payload()?);
        envelope.signature = proof.signature;
        Ok(envelope)
    }

    /// Sign a report as the host.
    pub fn report(
        host: &str,
        session_nonce: &[u8],
        sequence: u64,
        report: &HostActuatorReport,
        signer: &ServiceIdentitySigner<IdunnHostActuatorIdentity>,
    ) -> Result<Self> {
        let mut envelope = Self {
            protocol: HOST_ACTUATOR_PROTOCOL.into(),
            host: host.into(),
            session_nonce: session_nonce.to_vec(),
            sequence,
            body: rmp_serde::to_vec(report)?,
            signer_identity_id: signer.entry().identity_id.clone(),
            signature: Vec::new(),
        };
        let proof = signer.sign::<HostActuatorReportPurpose>(&envelope.signed_payload()?);
        envelope.signature = proof.signature;
        Ok(envelope)
    }

    /// Verify and open a request as the host.
    pub fn open_request(
        &self,
        host: &str,
        idunn_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<HostActuatorRequest> {
        self.validate_shape(host)?;
        verify_service_identity_signature::<IdunnServiceIdentity, HostActuatorRequestPurpose>(
            idunn_anchor,
            &self.signed_payload()?,
            &ServiceIdentitySignature {
                identity_id: self.signer_identity_id.clone(),
                signature: self.signature.clone(),
            },
        )
        .context("host actuator request is not signed by the admitted Idunn")?;
        rmp_serde::from_slice(&self.body).context("decoding host actuator request")
    }

    /// Verify and open a report as Idunn.
    pub fn open_report(
        &self,
        host: &str,
        host_anchor: &ServiceIdentityTrustAnchor,
    ) -> Result<HostActuatorReport> {
        self.validate_shape(host)?;
        verify_service_identity_signature::<IdunnHostActuatorIdentity, HostActuatorReportPurpose>(
            host_anchor,
            &self.signed_payload()?,
            &ServiceIdentitySignature {
                identity_id: self.signer_identity_id.clone(),
                signature: self.signature.clone(),
            },
        )
        .with_context(|| format!("host actuator report is not signed by the admitted {host}"))?;
        rmp_serde::from_slice(&self.body).context("decoding host actuator report")
    }
}

/// One attached host: the session that authenticated as it and the nonce
/// its messages carry. A host that connects again replaces this wholesale.
struct AttachedHost {
    session: CultNetRudpServerSessionContext,
    nonce: Vec<u8>,
    next_sequence: u64,
    reports: VecDeque<(u64, HostActuatorReport)>,
}

/// The Idunn-side listener for host actuators. Owns the socket and the
/// attached sessions; is serviced once per scheduler tick and while a request
/// waits for its report.
pub struct HostActuatorHub {
    hub: CultNetRudpServerHub,
    attached: BTreeMap<String, AttachedHost>,
}

impl HostActuatorHub {
    /// Bind the hub and, on a host with ufw, admit its endpoint on the host
    /// firewall the way the route driver admits a stable endpoint: owned by
    /// the code that listens, never opened by hand. The bind address is
    /// expected to be the mesh address, so the allow is mesh-only by
    /// construction.
    pub fn bind(bind: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(bind)
            .with_context(|| format!("binding the host actuator hub on {bind}"))?;
        socket.set_nonblocking(true)?;
        admit_hub_endpoint(bind)?;
        let options = CultNetRudpServerHubOptions::new(
            "idunn-host-actuator-hub",
            socket,
            HOST_ACTUATOR_CONNECTION_ID,
        );
        Ok(Self {
            hub: CultNetRudpServerHub::new(options)?,
            attached: BTreeMap::new(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.hub.local_addr()
    }

    pub fn attached_hosts(&self) -> Vec<String> {
        self.attached.keys().cloned().collect()
    }

    pub fn is_attached(&self, host: &str) -> bool {
        self.attached.contains_key(host)
    }

    /// Drain the socket: attach hosts whose Hello verifies against the anchor
    /// bound for them, queue reports, drop sessions that went silent.
    pub fn service(
        &mut self,
        anchors: &BTreeMap<String, ServiceIdentityTrustAnchor>,
        signer: &ServiceIdentitySigner<IdunnServiceIdentity>,
    ) -> Result<()> {
        for gone in self
            .hub
            .remove_timed_out_sessions(HOST_ACTUATOR_SESSION_TIMEOUT_MS)
        {
            self.detach_session(&gone, "timed out");
        }
        self.hub.poll_resends()?;
        while let Some(event) = self.hub.receive_event_once()? {
            match event {
                CultNetRudpServerEvent::Connected { session } => {
                    if let Err(error) = self.attach(session.clone(), anchors, signer) {
                        eprintln!(
                            "Idunn refused a host actuator from {}: {error:#}",
                            session.remote_addr
                        );
                        let _ = self.hub.disconnect(&session, b"refused".to_vec());
                    }
                }
                CultNetRudpServerEvent::Frame { session, frame } => {
                    if frame.channel_id != HOST_ACTUATOR_CHANNEL {
                        continue;
                    }
                    if let Err(error) = self.accept_report(&session, &frame.payload, anchors) {
                        eprintln!(
                            "Idunn dropped a host actuator frame from {}: {error:#}",
                            session.remote_addr
                        );
                    }
                }
                CultNetRudpServerEvent::Pong { .. } => {}
                CultNetRudpServerEvent::Disconnected { session, .. } => {
                    self.detach_session(&session, "disconnected");
                }
            }
        }
        Ok(())
    }

    fn detach_session(&mut self, session: &CultNetRudpServerSessionContext, why: &str) {
        let hosts: Vec<String> = self
            .attached
            .iter()
            .filter(|(_, attached)| {
                attached.session.remote_addr == session.remote_addr
                    && attached.session.session_generation == session.session_generation
            })
            .map(|(host, _)| host.clone())
            .collect();
        for host in hosts {
            eprintln!("Idunn host actuator {host} {why}");
            self.attached.remove(&host);
        }
    }

    fn attach(
        &mut self,
        session: CultNetRudpServerSessionContext,
        anchors: &BTreeMap<String, ServiceIdentityTrustAnchor>,
        signer: &ServiceIdentitySigner<IdunnServiceIdentity>,
    ) -> Result<()> {
        let hello = HostActuatorEnvelope::decode(&session.connect_payload)?;
        let host = hello.host.clone();
        let anchor = anchors
            .get(&host)
            .with_context(|| format!("no workload binding names host {host:?}"))?;
        ensure!(
            hello.session_nonce.is_empty() && hello.sequence == 0,
            "host actuator Hello carries a session it does not have yet"
        );
        let HostActuatorReport::Hello {
            platform,
            actuator_version,
        } = hello.open_report(&host, anchor)?
        else {
            bail!("host actuator opened its session with something other than Hello");
        };
        let nonce = Uuid::new_v4().as_bytes().to_vec();
        let welcome =
            HostActuatorEnvelope::request(&host, &nonce, 1, &HostActuatorRequest::Welcome, signer)?;
        self.hub
            .send(&session, HOST_ACTUATOR_CHANNEL, welcome.encode()?)?;
        if let Some(previous) = self.attached.insert(
            host.clone(),
            AttachedHost {
                session,
                nonce,
                next_sequence: 2,
                reports: VecDeque::new(),
            },
        ) {
            let _ = self
                .hub
                .disconnect(&previous.session, b"replaced by a newer session".to_vec());
        }
        eprintln!(
            "Idunn host actuator {host} attached ({platform}, idunn-host {actuator_version})"
        );
        Ok(())
    }

    fn accept_report(
        &mut self,
        session: &CultNetRudpServerSessionContext,
        payload: &[u8],
        anchors: &BTreeMap<String, ServiceIdentityTrustAnchor>,
    ) -> Result<()> {
        let envelope = HostActuatorEnvelope::decode(payload)?;
        let host = envelope.host.clone();
        let attached = self
            .attached
            .get_mut(&host)
            .with_context(|| format!("host {host:?} is not attached"))?;
        ensure!(
            attached.session.remote_addr == session.remote_addr
                && attached.session.session_generation == session.session_generation,
            "report arrived on a session that is not the attached one for {host:?}"
        );
        ensure!(
            envelope.session_nonce == attached.nonce,
            "report carries another session's nonce"
        );
        let anchor = anchors
            .get(&host)
            .with_context(|| format!("no workload binding names host {host:?}"))?;
        let report = envelope.open_report(&host, anchor)?;
        if envelope.sequence == 1 {
            ensure!(
                matches!(report, HostActuatorReport::Welcomed),
                "sequence 1 must be the Welcome acknowledgement"
            );
            return Ok(());
        }
        attached.reports.push_back((envelope.sequence, report));
        Ok(())
    }

    /// Send one request and wait for its report, servicing the socket the
    /// whole time. The actuator answers every request; a session that drops
    /// before it does is the error.
    pub fn request(
        &mut self,
        host: &str,
        request: &HostActuatorRequest,
        timeout: Duration,
        anchors: &BTreeMap<String, ServiceIdentityTrustAnchor>,
        signer: &ServiceIdentitySigner<IdunnServiceIdentity>,
    ) -> Result<HostActuatorReport> {
        self.service(anchors, signer)?;
        let (session, nonce, sequence) = {
            let attached = self
                .attached
                .get_mut(host)
                .with_context(|| format!("host actuator {host} is not attached"))?;
            let sequence = attached.next_sequence;
            attached.next_sequence += 1;
            (attached.session.clone(), attached.nonce.clone(), sequence)
        };
        let envelope = HostActuatorEnvelope::request(host, &nonce, sequence, request, signer)?;
        self.hub
            .send(&session, HOST_ACTUATOR_CHANNEL, envelope.encode()?)?;
        let deadline = Instant::now() + timeout;
        loop {
            self.service(anchors, signer)?;
            let Some(attached) = self.attached.get_mut(host) else {
                bail!("host actuator {host} detached while a request was outstanding");
            };
            ensure!(
                attached.session.session_generation == session.session_generation,
                "host actuator {host} reconnected while a request was outstanding"
            );
            if let Some(index) = attached
                .reports
                .iter()
                .position(|(answered, _)| *answered == sequence)
            {
                let (_, report) = attached.reports.remove(index).expect("indexed report");
                return match report {
                    HostActuatorReport::Failed { error } => {
                        bail!("host actuator {host} failed: {error}")
                    }
                    report => Ok(report),
                };
            }
            attached
                .reports
                .retain(|(answered, _)| *answered > sequence);
            ensure!(
                Instant::now() < deadline,
                "host actuator {host} did not answer within {timeout:?}"
            );
            thread::sleep(REQUEST_POLL);
        }
    }
}

#[cfg(unix)]
fn admit_hub_endpoint(bind: SocketAddr) -> Result<()> {
    let ufw = Path::new("/usr/sbin/ufw");
    if !ufw.is_file() {
        return Ok(());
    }
    ensure!(
        !bind.ip().is_unspecified(),
        "the host actuator hub must bind one address so its firewall allow names it; {bind} is unspecified"
    );
    let output = std::process::Command::new(ufw)
        .args([
            "allow",
            "in",
            "to",
            &bind.ip().to_string(),
            "port",
            &bind.port().to_string(),
            "proto",
            "udp",
            "comment",
            "Idunn host actuator hub",
        ])
        .stdin(std::process::Stdio::null())
        .env_clear()
        .env("LANG", "C.UTF-8")
        .output()
        .context("admitting the host actuator hub on the host firewall")?;
    ensure!(
        output.status.success(),
        "ufw refused the host actuator hub allow: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(not(unix))]
fn admit_hub_endpoint(_bind: SocketAddr) -> Result<()> {
    Ok(())
}

/// The hub shared between the scheduler and its service thread.
pub type SharedHostActuatorHub = Arc<Mutex<HostActuatorHub>>;

/// Service the hub on its own thread so a scheduler tick that blocks on a
/// build elsewhere does not starve every attached host into a timeout.
/// The thread carries its own copy of Idunn's signer, opened from the same
/// store, and reloads the bound anchors through `anchors`.
pub fn spawn_hub_service(
    hub: SharedHostActuatorHub,
    signer: ServiceIdentitySigner<IdunnServiceIdentity>,
    anchors: impl Fn() -> BTreeMap<String, ServiceIdentityTrustAnchor> + Send + 'static,
) {
    thread::Builder::new()
        .name("idunn-host-actuator-hub".into())
        .spawn(move || {
            let mut anchors_refreshed = Instant::now();
            let mut current = anchors();
            loop {
                if anchors_refreshed.elapsed() >= Duration::from_secs(5) {
                    current = anchors();
                    anchors_refreshed = Instant::now();
                }
                if let Err(error) = hub.lock().expect("hub mutex").service(&current, &signer) {
                    eprintln!("Idunn host actuator hub fault: {error:#}");
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("spawning the host actuator hub thread");
}

/// What the engine hands the host drivers: the hub, the anchors currently
/// bound, and Idunn's signer.
pub struct HostActuatorAccess<'a> {
    pub hub: SharedHostActuatorHub,
    pub anchors: BTreeMap<String, ServiceIdentityTrustAnchor>,
    pub signer: &'a ServiceIdentitySigner<IdunnServiceIdentity>,
}

impl HostActuatorAccess<'_> {
    fn request(
        &self,
        host: &str,
        request: &HostActuatorRequest,
        timeout: Duration,
    ) -> Result<HostActuatorReport> {
        // The lock is held for the whole exchange. The service thread waits
        // it out, which is fine: `request` services the socket itself while
        // it waits, so nothing attached goes unserviced meanwhile.
        self.hub.lock().expect("hub mutex").request(
            host,
            request,
            timeout,
            &self.anchors,
            self.signer,
        )
    }
}

fn plan_host(plan: &CompiledDeploymentPlan) -> Result<String> {
    let (_, binding) = plan.parsed_inputs()?;
    Ok(binding.workload.host()?.host.clone())
}

/// Builds on the managed host. The frozen source on the Idunn host is the
/// evidence; the host fetches the same exact revision itself and proves the
/// recipe bytes match the plan before running a step.
pub struct HostActuatorRunnerDriver<'a> {
    pub access: HostActuatorAccess<'a>,
}

impl RunnerPort for HostActuatorRunnerDriver<'_> {
    fn materialize(
        &self,
        source: &FrozenSource,
        plan: &CompiledDeploymentPlan,
        _staging_parent: &Path,
        sealed_at_unix_millis: u64,
    ) -> Result<MaterializedRelease> {
        plan.validate()?;
        source.receipt().validate_against(plan)?;
        let host = plan_host(plan)?;
        let report = self.access.request(
            &host,
            &HostActuatorRequest::Materialize {
                transaction_id: source.receipt().transaction_id.clone(),
                plan: plan.clone(),
                sealed_at_unix_millis,
            },
            MATERIALIZE_TIMEOUT,
        )?;
        let HostActuatorReport::Materialized { release, root } = report else {
            bail!("host actuator {host} answered Materialize with {report:?}");
        };
        release.validate_against(plan)?;
        ensure!(
            release.sealed_at_unix_millis == sealed_at_unix_millis,
            "host actuator sealed the release at another time than asked"
        );
        ensure!(!root.is_empty(), "host actuator reported no staging root");
        Ok(MaterializedRelease {
            release,
            root: PathBuf::from(root),
        })
    }
}

/// Runs the workload on the managed host and relays its observations.
pub struct HostActuatorWorkloadDriver<'a> {
    pub access: HostActuatorAccess<'a>,
}

impl HostActuatorWorkloadDriver<'_> {
    fn check_observation(
        &self,
        host: &str,
        observation: &HostWorkloadObservation,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        ensure!(observation.host == host, "observation names another host");
        ensure!(
            observation.runtime_instance_id == activation.runtime_instance_id
                && observation.activation_signer_identity_id
                    == activation.activation_signer_identity_id
                && observation.activation_signer_public_key
                    == activation.activation_signer_public_key,
            "observation belongs to another activation"
        );
        ensure!(
            observation.executable_sha256 == expected.artifact_sha256,
            "host workload executable differs from Expected"
        );
        ensure!(observation.process_id > 0, "host workload has no process");
        ensure!(
            observation.process_creation_time > 0,
            "host workload has no creation time"
        );
        Ok(())
    }
}

impl WorkloadPort for HostActuatorWorkloadDriver<'_> {
    fn install(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &MaterializedRelease,
    ) -> Result<InstalledReleaseObservation> {
        plan.validate()?;
        release.release.validate_against(plan)?;
        let host = plan_host(plan)?;
        let report = self.access.request(
            &host,
            &HostActuatorRequest::Install {
                plan: plan.clone(),
                release: release.release.clone(),
                root: release.root.display().to_string(),
            },
            INSTALL_TIMEOUT,
        )?;
        let HostActuatorReport::Installed { installed } = report else {
            bail!("host actuator {host} answered Install with {report:?}");
        };
        ensure!(
            installed.sealed_release_id == release.release.sealed_release_id,
            "host actuator installed another release"
        );
        Ok(installed)
    }

    fn prepare_activation(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        launch: IdunnRuntimeActivationLaunch,
    ) -> Result<IdunnRuntimeActivationRecord> {
        plan.validate()?;
        expected.validate()?;
        let host = plan_host(plan)?;
        let mut credential = Vec::new();
        let activation = launch.write_credential(&mut credential)?;
        let report = self.access.request(
            &host,
            &HostActuatorRequest::PrepareActivation {
                plan: plan.clone(),
                expected: expected.clone(),
                activation: activation.clone(),
                credential,
            },
            START_TIMEOUT,
        )?;
        let HostActuatorReport::Prepared {
            activation: prepared,
        } = report
        else {
            bail!("host actuator {host} answered PrepareActivation with {report:?}");
        };
        ensure!(
            prepared == activation,
            "host actuator prepared another activation than the one issued"
        );
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
        expected.validate()?;
        activation.validate()?;
        let host = plan_host(plan)?;
        let report = self.access.request(
            &host,
            &HostActuatorRequest::Start {
                plan: plan.clone(),
                release: release.clone(),
                installed: installed.clone(),
                expected: expected.clone(),
                activation: activation.clone(),
            },
            START_TIMEOUT,
        )?;
        let HostActuatorReport::Started { observation } = report else {
            bail!("host actuator {host} answered Start with {report:?}");
        };
        self.check_observation(&host, &observation, expected, activation)?;
        ensure!(
            observation.exit_code.is_none(),
            "host workload exited during start"
        );
        Ok(WorkloadObservation::Host(observation))
    }

    fn discard_prepared(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        let host = plan_host(plan)?;
        let report = self.access.request(
            &host,
            &HostActuatorRequest::Discard {
                plan: plan.clone(),
                expected: expected.clone(),
                activation: activation.clone(),
            },
            STOP_TIMEOUT,
        )?;
        ensure!(
            matches!(report, HostActuatorReport::Discarded),
            "host actuator {host} answered Discard with {report:?}"
        );
        Ok(())
    }

    fn observe(
        &self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        prior: &WorkloadObservation,
    ) -> Result<WorkloadObservation> {
        let prior = prior.host()?;
        let host = prior.host.clone();
        let report = self.access.request(
            &host,
            &HostActuatorRequest::Observe {
                expected: expected.clone(),
                activation: activation.clone(),
                prior: prior.clone(),
            },
            OBSERVE_TIMEOUT,
        )?;
        let HostActuatorReport::Observed { observation } = report else {
            bail!("host actuator {host} answered Observe with {report:?}");
        };
        self.check_observation(&host, &observation, expected, activation)?;
        ensure!(
            observation.process_id == prior.process_id
                && observation.process_creation_time == prior.process_creation_time
                && observation.executable == prior.executable,
            "host workload identity changed after observation"
        );
        if let Some(code) = observation.exit_code {
            bail!(
                "host workload {} pid {} exited with code {code}",
                prior.runtime_instance_id,
                prior.process_id
            );
        }
        Ok(WorkloadObservation::Host(observation))
    }

    fn stop(&self, observation: &WorkloadObservation) -> Result<()> {
        let observation = observation.host()?;
        let report = self.access.request(
            &observation.host,
            &HostActuatorRequest::Stop {
                observation: observation.clone(),
            },
            STOP_TIMEOUT,
        )?;
        ensure!(
            matches!(report, HostActuatorReport::Stopped),
            "host actuator {} answered Stop with {report:?}",
            observation.host
        );
        Ok(())
    }

    /// Nothing on a managed host restarts a workload: the process either
    /// runs or has exited. An actuator that cannot be asked is not evidence
    /// either way and is an error, which the caller treats as resumable.
    fn is_permanently_stopped(&self, observation: &WorkloadObservation) -> Result<bool> {
        let prior = observation.host()?;
        let report = self.access.request(
            &prior.host,
            &HostActuatorRequest::Probe {
                prior: prior.clone(),
            },
            OBSERVE_TIMEOUT,
        )?;
        let HostActuatorReport::Probed { running } = report else {
            bail!(
                "host actuator {} answered Probe with {report:?}",
                prior.host
            );
        };
        Ok(!running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cultnet_rs::enroll_service_identity_at;
    use tempfile::TempDir;

    fn identities() -> Result<(
        TempDir,
        ServiceIdentitySigner<IdunnServiceIdentity>,
        ServiceIdentitySigner<IdunnHostActuatorIdentity>,
    )> {
        let temp = TempDir::new()?;
        let idunn = enroll_service_identity_at::<IdunnServiceIdentity>(&temp.path().join("i.cc"))?;
        let host =
            enroll_service_identity_at::<IdunnHostActuatorIdentity>(&temp.path().join("h.cc"))?;
        Ok((temp, idunn, host))
    }

    #[test]
    fn a_request_opens_only_under_idunns_anchor_and_only_for_its_host() -> Result<()> {
        let (_temp, idunn, host) = identities()?;
        let nonce = vec![7; 16];
        let envelope = HostActuatorEnvelope::request(
            "raven",
            &nonce,
            3,
            &HostActuatorRequest::Welcome,
            &idunn,
        )?;
        let decoded = HostActuatorEnvelope::decode(&envelope.encode()?)?;
        assert_eq!(
            decoded.open_request("raven", &idunn.trust_anchor()?)?,
            HostActuatorRequest::Welcome
        );
        assert!(
            decoded
                .open_request("nightwing", &idunn.trust_anchor()?)
                .is_err()
        );
        // The host's own anchor is not Idunn's.
        let host_anchor = host.trust_anchor()?;
        let forged = ServiceIdentityTrustAnchor {
            identity_id: decoded.signer_identity_id.clone(),
            ..host_anchor
        };
        assert!(decoded.open_request("raven", &forged).is_err());
        let mut tampered = decoded.clone();
        tampered.sequence = 4;
        assert!(
            tampered
                .open_request("raven", &idunn.trust_anchor()?)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn a_report_opens_only_under_the_hosts_anchor() -> Result<()> {
        let (_temp, idunn, host) = identities()?;
        let report = HostActuatorReport::Hello {
            platform: "windows".into(),
            actuator_version: "0.1.0".into(),
        };
        let envelope = HostActuatorEnvelope::report("raven", &[], 0, &report, &host)?;
        assert_eq!(
            envelope.open_report("raven", &host.trust_anchor()?)?,
            report
        );
        let other =
            enroll_service_identity_at::<IdunnHostActuatorIdentity>(&_temp.path().join("o.cc"))?;
        assert!(
            envelope
                .open_report("raven", &other.trust_anchor()?)
                .is_err()
        );
        let _ = idunn;
        Ok(())
    }

    /// A real hub and a real client socket on loopback: Hello attaches under
    /// the bound anchor, a wrong host is refused, a request is answered.
    #[test]
    fn a_hub_attaches_a_hello_it_can_verify_and_answers_a_request() -> Result<()> {
        let (_temp, idunn, host) = identities()?;
        let mut hub = HostActuatorHub::bind("127.0.0.1:0".parse()?)?;
        let hub_addr = hub.local_addr()?;
        let anchors: BTreeMap<String, ServiceIdentityTrustAnchor> =
            [("raven".to_string(), host.trust_anchor()?)].into();

        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.set_nonblocking(true)?;
        let mut client = cultnet_rs::CultNetRudpSocketTransportConnection::new(
            cultnet_rs::CultNetRudpSocketTransportOptions::client(
                "test-actuator",
                socket,
                hub_addr,
                HOST_ACTUATOR_CONNECTION_ID,
            ),
        )?;
        let hello = HostActuatorEnvelope::report(
            "raven",
            &[],
            0,
            &HostActuatorReport::Hello {
                platform: "test".into(),
                actuator_version: "0".into(),
            },
            &host,
        )?;
        client.connect(hello.encode()?)?;

        // Pump both ends until the Welcome arrives.
        let mut nonce = None;
        for _ in 0..200 {
            hub.service(&anchors, &idunn)?;
            client.poll_resends()?;
            while let Some(frame) = client.receive_once()? {
                let envelope = HostActuatorEnvelope::decode(&frame.payload)?;
                let request = envelope.open_request("raven", &idunn.trust_anchor()?)?;
                assert_eq!(request, HostActuatorRequest::Welcome);
                nonce = Some(envelope.session_nonce.clone());
                let ack = HostActuatorEnvelope::report(
                    "raven",
                    &envelope.session_nonce,
                    envelope.sequence,
                    &HostActuatorReport::Welcomed,
                    &host,
                )?;
                client.send(HOST_ACTUATOR_CHANNEL, ack.encode()?)?;
            }
            if nonce.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let nonce = nonce.expect("welcome arrived");
        assert!(hub.is_attached("raven"));

        // A request is answered by the client in a thread that keeps pumping.
        let idunn_anchor = idunn.trust_anchor()?;
        let answerer = thread::spawn(move || -> Result<()> {
            for _ in 0..500 {
                client.poll_resends()?;
                while let Some(frame) = client.receive_once()? {
                    let envelope = HostActuatorEnvelope::decode(&frame.payload)?;
                    let request = envelope.open_request("raven", &idunn_anchor)?;
                    assert_eq!(envelope.session_nonce, nonce);
                    let report = match request {
                        HostActuatorRequest::Probe { .. } => {
                            HostActuatorReport::Probed { running: false }
                        }
                        other => HostActuatorReport::Failed {
                            error: format!("unexpected {other:?}"),
                        },
                    };
                    let answer = HostActuatorEnvelope::report(
                        "raven",
                        &nonce,
                        envelope.sequence,
                        &report,
                        &host,
                    )?;
                    client.send(HOST_ACTUATOR_CHANNEL, answer.encode()?)?;
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(5));
            }
            bail!("no request arrived")
        });
        let probe = HostActuatorRequest::Probe {
            prior: HostWorkloadObservation {
                host: "raven".into(),
                actuator_identity_id: String::new(),
                process_id: 0,
                process_creation_time: 0,
                session_id: 0,
                user_sid: String::new(),
                executable: String::new(),
                executable_sha256: String::new(),
                command_line_sha256: String::new(),
                environment_names: Vec::new(),
                environment_contract_sha256: String::new(),
                runtime_bundle: String::new(),
                runtime_instance_id: String::new(),
                activation_signer_identity_id: String::new(),
                activation_signer_public_key: Vec::new(),
                exit_code: None,
            },
        };
        let report = hub.request("raven", &probe, Duration::from_secs(5), &anchors, &idunn)?;
        assert_eq!(report, HostActuatorReport::Probed { running: false });
        answerer.join().expect("answerer thread")?;

        // An unknown host is refused at Hello.
        assert!(
            hub.request(
                "nightwing",
                &probe,
                Duration::from_millis(50),
                &anchors,
                &idunn
            )
            .is_err()
        );
        Ok(())
    }
}
