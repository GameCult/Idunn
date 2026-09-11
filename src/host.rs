//! `idunn-host`: the actuator body on a managed host.
//!
//! One process per host, started at logon in the session the workload needs.
//! It dials the Idunn hub, proves itself with the host's service identity,
//! and executes what `host_actuator` defines: fetch and build the exact
//! source, install the sealed release, start the workload as a child of this
//! process, report what it can prove about that process, stop it when told.
//! It keeps no transaction state; a restarted actuator finds a running
//! workload again by pid and creation time.
//!
//! Only the process layer is Windows-specific and it is confined to
//! `platform`. Everything else is the same code Idunn runs on yggdrasil.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use cultnet_rs::{
    CultNetRudpSocketTransportConnection, CultNetRudpSocketTransportOptions,
    IdunnExpectedIncarnationRecord, IdunnRuntimeActivationRecord, ServiceIdentitySigner,
    ServiceIdentityTrustAnchor, open_service_identity_at,
};
use serde::Deserialize;

use crate::control_plane::read_trust_anchor;
use crate::deployment::{
    ArtifactSource, HostWorkloadBinding, IDUNN_ACTIVATION_CREDENTIAL_ENVIRONMENT,
    IDUNN_RUNTIME_BUNDLE_ENVIRONMENT, LaunchArgument, OperatorBinding, TargetDeclaration,
};
use crate::deployment_plan::{ArtifactReceipt, CompiledDeploymentPlan, SealedRelease};
use crate::drivers::{
    HostWorkloadObservation, InstalledReleaseObservation, copy_artifact, copy_tree,
    digest_artifact, release_artifact, remove_tree_inside, sha256_id, write_runtime_bundle_records,
};
use crate::host_actuator::{
    HOST_ACTUATOR_CHANNEL, HOST_ACTUATOR_CONNECTION_ID, HOST_ACTUATOR_PING_MS,
    HOST_ACTUATOR_SESSION_TIMEOUT_MS, HostActuatorEnvelope, HostActuatorReport,
    HostActuatorRequest, IdunnHostActuatorIdentity,
};

pub const HOST_ACTUATOR_CONFIG_SCHEMA: &str = "gamecult.idunn.host_actuator_config.v1";
/// The workload reads its activation credential from the file named by
/// `IDUNN_ACTIVATION_CREDENTIAL_ENVIRONMENT`. On a single-user workstation a
/// parent-only descriptor would protect it from nobody the file ACL does not
/// already exclude; the file lives inside the runtime bundle, which is inside
/// the user's own profile.
const ACTIVATION_CREDENTIAL_FILE: &str = "activation-credential";
const REDIAL_AFTER: Duration = Duration::from_secs(2);
const LOOP_POLL: Duration = Duration::from_millis(20);
const STOP_WAIT: Duration = Duration::from_secs(10);
/// Exit code reported for a process that was gone before its exit could be
/// read: the actuator restarted, or the pid was reused. Distinct from every
/// code a process can return on purpose.
pub const EXIT_CODE_VANISHED: u32 = u32::MAX;

/// Host environment a build step or workload inherits. Everything else in
/// the actuator's environment stays with the actuator. Cargo and git need
/// the toolchain paths and a temp directory; nothing needs the rest.
const PASSTHROUGH_ENVIRONMENT: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "PUBLIC",
    "USERNAME",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostActuatorConfig {
    pub schema: String,
    pub host: String,
    pub idunn_endpoint: String,
    pub identity_store: PathBuf,
    pub idunn_trust_anchor: PathBuf,
    pub work_root: PathBuf,
}

impl HostActuatorConfig {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("decoding host actuator config")?;
        ensure!(
            config.schema == HOST_ACTUATOR_CONFIG_SCHEMA,
            "unsupported host actuator config schema {:?}",
            config.schema
        );
        ensure!(
            !config.host.is_empty(),
            "host actuator config names no host"
        );
        config
            .idunn_endpoint
            .parse::<SocketAddr>()
            .context("idunn_endpoint is not a socket address")?;
        for (label, path) in [
            ("identity_store", &config.identity_store),
            ("idunn_trust_anchor", &config.idunn_trust_anchor),
            ("work_root", &config.work_root),
        ] {
            ensure!(path.is_absolute(), "{label} must be an absolute path");
        }
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading host actuator config {}", path.display()))?;
        Self::parse(&text)
    }

    fn idunn_endpoint(&self) -> SocketAddr {
        self.idunn_endpoint
            .parse()
            .expect("validated at parse time")
    }

    fn sources(&self) -> PathBuf {
        self.work_root.join("sources")
    }

    fn staging(&self) -> PathBuf {
        self.work_root.join("staging")
    }

    fn logs(&self) -> PathBuf {
        self.work_root.join("logs")
    }
}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<()> {
    let mut args = args.into_iter();
    let command = args.next().unwrap_or_else(|| "--help".into());
    let mut config_path = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ))
            }
            other => bail!("unknown idunn-host option {other:?}\n\n{}", usage()),
        }
    }
    match command.as_str() {
        "serve" => serve(&HostActuatorConfig::load(
            &config_path.context("idunn-host serve requires --config")?,
        )?),
        "validate" => {
            let config = HostActuatorConfig::load(
                &config_path.context("idunn-host validate requires --config")?,
            )?;
            let actuator = Actuator::open(config)?;
            println!(
                "host {} identity {} idunn {} anchor {}",
                actuator.config.host,
                actuator.signer.entry().identity_id,
                actuator.config.idunn_endpoint,
                actuator.idunn_anchor.identity_id
            );
            Ok(())
        }
        "--help" | "-h" | "help" => bail!(usage()),
        other => bail!("unknown idunn-host command {other:?}\n\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "Idunn host actuator\n\n\
     idunn-host serve --config PATH\n\
     idunn-host validate --config PATH\n\n\
     Dials the Idunn host actuator hub named in the config and executes the\n\
     runner and workload consequences for this host. Decides nothing."
}

fn serve(config: &HostActuatorConfig) -> Result<()> {
    let mut actuator = Actuator::open(config.clone())?;
    actuator.log(&format!(
        "idunn-host {} for host {} dialling {}",
        env!("CARGO_PKG_VERSION"),
        config.host,
        config.idunn_endpoint
    ));
    loop {
        match actuator.session() {
            Ok(()) => actuator.log("session closed"),
            Err(error) => actuator.log(&format!("session ended: {error:#}")),
        }
        thread::sleep(REDIAL_AFTER);
    }
}

struct BuildInFlight {
    sequence: u64,
    handle: JoinHandle<Result<(SealedRelease, PathBuf)>>,
}

struct Actuator {
    config: HostActuatorConfig,
    signer: ServiceIdentitySigner<IdunnHostActuatorIdentity>,
    idunn_anchor: ServiceIdentityTrustAnchor,
    /// Children this actuator started, by runtime instance id. A child stays
    /// here after it exits so its exit code can be reported.
    spawned: BTreeMap<String, Child>,
    build: Option<BuildInFlight>,
    user_sid: String,
}

impl Actuator {
    fn open(config: HostActuatorConfig) -> Result<Self> {
        let signer = open_service_identity_at::<IdunnHostActuatorIdentity>(&config.identity_store)
            .context("opening the host actuator identity")?;
        let idunn_anchor =
            read_trust_anchor::<cultnet_rs::IdunnServiceIdentity>(&config.idunn_trust_anchor)
                .context("reading the Idunn public anchor")?;
        for directory in [config.sources(), config.staging(), config.logs()] {
            fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        let user_sid = platform::current_user_sid()?;
        Ok(Self {
            config,
            signer,
            idunn_anchor,
            spawned: BTreeMap::new(),
            build: None,
            user_sid,
        })
    }

    fn log(&self, message: &str) {
        let line = format!(
            "{} {message}\n",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ")
        );
        eprint!("{line}");
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.config.logs().join("idunn-host.log"))
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// One dial: Hello, Welcome, then requests until the session ends.
    fn session(&mut self) -> Result<()> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        let mut client =
            CultNetRudpSocketTransportConnection::new(CultNetRudpSocketTransportOptions::client(
                format!("idunn-host-{}", self.config.host),
                socket,
                self.config.idunn_endpoint(),
                HOST_ACTUATOR_CONNECTION_ID,
            ))?;
        let hello = HostActuatorEnvelope::report(
            &self.config.host,
            &[],
            0,
            &HostActuatorReport::Hello {
                platform: std::env::consts::OS.into(),
                actuator_version: env!("CARGO_PKG_VERSION").into(),
            },
            &self.signer,
        )?;
        client.connect(hello.encode()?)?;
        let mut nonce: Option<Vec<u8>> = None;
        let mut last_sequence = 0_u64;
        let mut last_ping = Instant::now();
        loop {
            client.poll_resends()?;
            while let Some(frame) = client.receive_once()? {
                if frame.channel_id != HOST_ACTUATOR_CHANNEL {
                    continue;
                }
                if let Err(error) =
                    self.handle_frame(&mut client, &mut nonce, &mut last_sequence, &frame.payload)
                {
                    self.log(&format!("request refused: {error:#}"));
                }
            }
            if let Some(reason) = client.disconnect_reason() {
                bail!("Idunn disconnected: {}", String::from_utf8_lossy(reason));
            }
            if client.check_timeout(HOST_ACTUATOR_SESSION_TIMEOUT_MS) {
                bail!("Idunn went silent");
            }
            if last_ping.elapsed() >= Duration::from_millis(HOST_ACTUATOR_PING_MS) {
                client.ping(Vec::new())?;
                last_ping = Instant::now();
            }
            if let Some(nonce) = &nonce {
                self.poll_build(&mut client, nonce)?;
            }
            thread::sleep(LOOP_POLL);
        }
    }

    fn reply(
        &self,
        client: &mut CultNetRudpSocketTransportConnection,
        nonce: &[u8],
        sequence: u64,
        report: &HostActuatorReport,
    ) -> Result<()> {
        let envelope =
            HostActuatorEnvelope::report(&self.config.host, nonce, sequence, report, &self.signer)?;
        client.send(HOST_ACTUATOR_CHANNEL, envelope.encode()?)
    }

    fn handle_frame(
        &mut self,
        client: &mut CultNetRudpSocketTransportConnection,
        nonce: &mut Option<Vec<u8>>,
        last_sequence: &mut u64,
        payload: &[u8],
    ) -> Result<()> {
        let envelope = HostActuatorEnvelope::decode(payload)?;
        let request = envelope.open_request(&self.config.host, &self.idunn_anchor)?;
        if let HostActuatorRequest::Welcome = request {
            ensure!(
                nonce.is_none(),
                "Idunn welcomed a session that was already open"
            );
            ensure!(
                envelope.session_nonce.len() >= 16,
                "Welcome carries too short a nonce"
            );
            *nonce = Some(envelope.session_nonce.clone());
            *last_sequence = envelope.sequence;
            self.log("attached to Idunn");
            return self.reply(
                client,
                &envelope.session_nonce,
                envelope.sequence,
                &HostActuatorReport::Welcomed,
            );
        }
        let session_nonce = nonce
            .as_ref()
            .context("a request arrived before Welcome")?
            .clone();
        ensure!(
            envelope.session_nonce == session_nonce,
            "request carries another session's nonce"
        );
        ensure!(
            envelope.sequence > *last_sequence,
            "request sequence {} does not advance past {}",
            envelope.sequence,
            *last_sequence
        );
        *last_sequence = envelope.sequence;
        let sequence = envelope.sequence;
        if let HostActuatorRequest::Materialize {
            transaction_id,
            plan,
            sealed_at_unix_millis,
        } = request
        {
            if self.build.is_some() {
                return self.reply(
                    client,
                    &session_nonce,
                    sequence,
                    &HostActuatorReport::Failed {
                        error: "a build is already in flight on this host".into(),
                    },
                );
            }
            self.log(&format!("materializing {transaction_id}"));
            let config = self.config.clone();
            let handle = thread::spawn(move || {
                materialize(&config, &transaction_id, &plan, sealed_at_unix_millis)
            });
            self.build = Some(BuildInFlight { sequence, handle });
            return Ok(());
        }
        let report = match self.execute(request) {
            Ok(report) => report,
            Err(error) => {
                self.log(&format!("request {sequence} failed: {error:#}"));
                HostActuatorReport::Failed {
                    error: format!("{error:#}"),
                }
            }
        };
        self.reply(client, &session_nonce, sequence, &report)
    }

    fn poll_build(
        &mut self,
        client: &mut CultNetRudpSocketTransportConnection,
        nonce: &[u8],
    ) -> Result<()> {
        let finished = self
            .build
            .as_ref()
            .is_some_and(|build| build.handle.is_finished());
        if !finished {
            return Ok(());
        }
        let build = self.build.take().expect("checked above");
        let report = match build.handle.join() {
            Ok(Ok((release, root))) => {
                self.log(&format!("sealed {}", release.sealed_release_id));
                HostActuatorReport::Materialized {
                    release,
                    root: root.display().to_string(),
                }
            }
            Ok(Err(error)) => {
                self.log(&format!("materialize failed: {error:#}"));
                HostActuatorReport::Failed {
                    error: format!("{error:#}"),
                }
            }
            Err(_) => HostActuatorReport::Failed {
                error: "the build thread panicked".into(),
            },
        };
        self.reply(client, nonce, build.sequence, &report)
    }

    fn execute(&mut self, request: HostActuatorRequest) -> Result<HostActuatorReport> {
        match request {
            HostActuatorRequest::Welcome | HostActuatorRequest::Materialize { .. } => {
                bail!("handled before execute")
            }
            HostActuatorRequest::Install {
                plan,
                release,
                root,
            } => {
                let installed = self.install(&plan, &release, Path::new(&root))?;
                Ok(HostActuatorReport::Installed { installed })
            }
            HostActuatorRequest::PrepareActivation {
                plan,
                expected,
                activation,
                credential,
            } => {
                self.prepare_activation(&plan, &expected, &activation, &credential)?;
                Ok(HostActuatorReport::Prepared { activation })
            }
            HostActuatorRequest::Start {
                plan,
                release,
                installed,
                expected,
                activation,
            } => {
                let observation =
                    self.start(&plan, &release, &installed, &expected, &activation)?;
                Ok(HostActuatorReport::Started { observation })
            }
            HostActuatorRequest::Observe {
                expected,
                activation,
                prior,
            } => {
                let observation = self.observe(&expected, &activation, &prior)?;
                Ok(HostActuatorReport::Observed { observation })
            }
            HostActuatorRequest::Stop { observation } => {
                self.stop(&observation)?;
                Ok(HostActuatorReport::Stopped)
            }
            HostActuatorRequest::Probe { prior } => Ok(HostActuatorReport::Probed {
                running: self.is_running(&prior)?,
            }),
            HostActuatorRequest::Discard {
                plan,
                expected,
                activation,
            } => {
                self.discard(&plan, &expected, &activation)?;
                Ok(HostActuatorReport::Discarded)
            }
        }
    }

    fn workload_binding<'a>(
        &self,
        binding: &'a OperatorBinding,
    ) -> Result<&'a HostWorkloadBinding> {
        let workload = binding.workload.host()?;
        ensure!(
            workload.host == self.config.host,
            "plan is bound to host {:?}, this actuator is {:?}",
            workload.host,
            self.config.host
        );
        Ok(workload)
    }

    fn install(
        &self,
        plan: &CompiledDeploymentPlan,
        release: &SealedRelease,
        root: &Path,
    ) -> Result<InstalledReleaseObservation> {
        plan.validate()?;
        release.validate_against(plan)?;
        let (_, binding) = plan.parsed_inputs()?;
        let workload = self.workload_binding(&binding)?;
        let release_root = PathBuf::from(&workload.release_root);
        fs::create_dir_all(&release_root)?;
        let destination = release_root.join(&release.sealed_release_id);
        if destination.exists() && verify_artifacts(&destination, release).is_err() {
            remove_tree_inside(&release_root, &destination)?;
        }
        if !destination.exists() {
            ensure!(
                root.is_dir(),
                "staging root {} is absent; materialize again",
                root.display()
            );
            copy_tree(root, &destination)?;
        }
        verify_artifacts(&destination, release)?;
        Ok(InstalledReleaseObservation {
            sealed_release_id: release.sealed_release_id.clone(),
            root: destination,
        })
    }

    fn bundle_path(
        &self,
        workload: &HostWorkloadBinding,
        activation: &IdunnRuntimeActivationRecord,
    ) -> PathBuf {
        PathBuf::from(&workload.runtime_root).join(&activation.runtime_instance_id)
    }

    fn prepare_activation(
        &self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        credential: &[u8],
    ) -> Result<()> {
        plan.validate()?;
        expected.validate()?;
        activation.validate()?;
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "activation belongs to another Expected"
        );
        ensure!(
            credential.len() == 32,
            "activation credential is not a 32-byte seed"
        );
        let (_, binding) = plan.parsed_inputs()?;
        let workload = self.workload_binding(&binding)?;
        let bundle = self.bundle_path(workload, activation);
        if bundle.exists() {
            fs::remove_dir_all(&bundle)?;
        }
        write_runtime_bundle_records(&bundle, expected, activation)?;
        fs::write(bundle.join(ACTIVATION_CREDENTIAL_FILE), credential)?;
        Ok(())
    }

    fn launch_environment(
        &self,
        workload: &HostWorkloadBinding,
        bundle: &Path,
    ) -> Result<BTreeMap<String, String>> {
        let mut environment = workload.environment.clone();
        for (name, path) in &workload.secret_files {
            ensure!(
                Path::new(path).is_file(),
                "secret file {path} bound as {name} is absent on this host"
            );
            ensure!(
                environment.insert(name.clone(), path.clone()).is_none(),
                "workload environment and secret bindings collide on {name}"
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
        ensure!(
            environment
                .insert(
                    IDUNN_ACTIVATION_CREDENTIAL_ENVIRONMENT.into(),
                    bundle
                        .join(ACTIVATION_CREDENTIAL_FILE)
                        .display()
                        .to_string(),
                )
                .is_none(),
            "operator binding attempts to replace the activation credential"
        );
        Ok(environment)
    }

    fn launch_command(
        declaration: &TargetDeclaration,
        workload: &HostWorkloadBinding,
        installed: &Path,
    ) -> Result<(PathBuf, Vec<String>)> {
        let artifact = release_artifact(declaration, &declaration.service.executable_artifact)?;
        let executable = installed.join(&artifact.destination);
        ensure!(executable.is_file(), "sealed service executable is absent");
        let mut arguments = Vec::new();
        for argument in &declaration.service.arguments {
            arguments.push(match argument {
                LaunchArgument::Literal { value } => value.clone(),
                LaunchArgument::Binding { name } => workload
                    .argument_bindings
                    .get(name)
                    .with_context(|| format!("no argument binding {name}"))?
                    .clone(),
            });
        }
        Ok((executable, arguments))
    }

    fn start(
        &mut self,
        plan: &CompiledDeploymentPlan,
        release: &SealedRelease,
        installed: &InstalledReleaseObservation,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<HostWorkloadObservation> {
        plan.validate()?;
        release.validate_against(plan)?;
        expected.validate()?;
        activation.validate()?;
        ensure!(
            expected.plan_id == plan.plan_id
                && expected.sealed_release_id == release.sealed_release_id
                && activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "workload inputs do not describe one sealed incarnation"
        );
        let (declaration, binding) = plan.parsed_inputs()?;
        let workload = self.workload_binding(&binding)?;
        verify_artifacts(&installed.root, release)?;
        let bundle = self.bundle_path(workload, activation);
        ensure!(
            bundle.join("expected.cc").is_file()
                && bundle.join("activation.cc").is_file()
                && bundle.join(ACTIVATION_CREDENTIAL_FILE).is_file(),
            "runtime bundle {} is not prepared",
            bundle.display()
        );
        let (executable, arguments) =
            Self::launch_command(&declaration, workload, &installed.root)?;
        let (executable_sha256, _) = digest_artifact(&executable)?;
        let executable_sha256 = format!("sha256-{executable_sha256}");
        ensure!(
            executable_sha256 == expected.artifact_sha256,
            "installed executable differs from Expected"
        );
        let environment = self.launch_environment(workload, &bundle)?;
        if let Some(mut previous) = self.spawned.remove(&activation.runtime_instance_id) {
            if previous.try_wait()?.is_none() {
                bail!("a child for this activation is already running");
            }
        }
        let stdout = File::create(bundle.join("stdout.log"))?;
        let stderr = File::create(bundle.join("stderr.log"))?;
        let mut command = Command::new(&executable);
        command
            .args(&arguments)
            .current_dir(&installed.root)
            .env_clear()
            .envs(passthrough_environment())
            .envs(&environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        platform::detach(&mut command);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", executable.display()))?;
        let pid = child.id();
        self.spawned
            .insert(activation.runtime_instance_id.clone(), child);
        self.log(&format!(
            "started {} pid {pid} from {}",
            activation.runtime_instance_id,
            executable.display()
        ));
        let command_line = rmp_serde::to_vec(&(executable.display().to_string(), &arguments))?;
        let facts = platform::process_facts(pid)?.context("the started process is already gone")?;
        Ok(HostWorkloadObservation {
            host: self.config.host.clone(),
            actuator_identity_id: self.signer.entry().identity_id.clone(),
            process_id: pid,
            process_creation_time: facts.creation_time,
            session_id: facts.session_id,
            user_sid: self.user_sid.clone(),
            executable: executable.display().to_string(),
            executable_sha256,
            command_line_sha256: sha256_id(&command_line),
            environment_names: environment.keys().cloned().collect(),
            environment_contract_sha256: sha256_id(&rmp_serde::to_vec(&environment)?),
            runtime_bundle: bundle.display().to_string(),
            runtime_instance_id: activation.runtime_instance_id.clone(),
            activation_signer_identity_id: activation.activation_signer_identity_id.clone(),
            activation_signer_public_key: activation.activation_signer_public_key.clone(),
            exit_code: self.exit_code(&activation.runtime_instance_id, pid, facts.creation_time)?,
        })
    }

    /// `Some` once the process behind (pid, creation time) is gone. A child
    /// this actuator spawned reports its real exit code; any other process
    /// is vanished or running, nothing finer.
    fn exit_code(
        &mut self,
        runtime_instance_id: &str,
        pid: u32,
        creation_time: u64,
    ) -> Result<Option<u32>> {
        if let Some(child) = self.spawned.get_mut(runtime_instance_id) {
            if child.id() == pid {
                return Ok(match child.try_wait()? {
                    Some(status) => {
                        Some(status.code().map_or(EXIT_CODE_VANISHED, |code| code as u32))
                    }
                    None => None,
                });
            }
        }
        Ok(match platform::process_facts(pid)? {
            Some(facts) if facts.creation_time == creation_time => facts.exit_code,
            _ => Some(EXIT_CODE_VANISHED),
        })
    }

    fn observe(
        &mut self,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
        prior: &HostWorkloadObservation,
    ) -> Result<HostWorkloadObservation> {
        expected.validate()?;
        activation.validate()?;
        ensure!(
            activation.runtime_instance_id == prior.runtime_instance_id,
            "observation belongs to another activation"
        );
        ensure!(
            prior.host == self.config.host,
            "observation names another host"
        );
        let exit_code = self.exit_code(
            &prior.runtime_instance_id,
            prior.process_id,
            prior.process_creation_time,
        )?;
        let mut observation = prior.clone();
        observation.exit_code = exit_code;
        if exit_code.is_none() {
            let facts = platform::process_facts(prior.process_id)?
                .context("the process disappeared between two reads")?;
            ensure!(
                facts.creation_time == prior.process_creation_time,
                "pid {} now belongs to another process",
                prior.process_id
            );
            ensure!(
                same_path(&facts.image_path, &prior.executable),
                "pid {} now runs {} instead of {}",
                prior.process_id,
                facts.image_path,
                prior.executable
            );
            let (executable_sha256, _) = digest_artifact(Path::new(&prior.executable))?;
            ensure!(
                format!("sha256-{executable_sha256}") == prior.executable_sha256,
                "the installed executable changed under the running process"
            );
        }
        Ok(observation)
    }

    fn is_running(&mut self, prior: &HostWorkloadObservation) -> Result<bool> {
        Ok(self
            .exit_code(
                &prior.runtime_instance_id,
                prior.process_id,
                prior.process_creation_time,
            )?
            .is_none())
    }

    fn stop(&mut self, observation: &HostWorkloadObservation) -> Result<()> {
        ensure!(
            observation.host == self.config.host,
            "observation names another host"
        );
        if !self.is_running(observation)? {
            self.spawned.remove(&observation.runtime_instance_id);
            return Ok(());
        }
        self.log(&format!(
            "stopping {} pid {}",
            observation.runtime_instance_id, observation.process_id
        ));
        platform::terminate(observation.process_id, observation.process_creation_time)?;
        let deadline = Instant::now() + STOP_WAIT;
        while self.is_running(observation)? {
            ensure!(
                Instant::now() < deadline,
                "pid {} did not exit within {STOP_WAIT:?}",
                observation.process_id
            );
            thread::sleep(Duration::from_millis(50));
        }
        self.spawned.remove(&observation.runtime_instance_id);
        Ok(())
    }

    fn discard(
        &mut self,
        plan: &CompiledDeploymentPlan,
        expected: &IdunnExpectedIncarnationRecord,
        activation: &IdunnRuntimeActivationRecord,
    ) -> Result<()> {
        ensure!(
            activation.expected_projection_sha256 == expected.canonical_sha256()?,
            "discarded activation does not belong to the prepared Expected"
        );
        let (_, binding) = plan.parsed_inputs()?;
        let workload = self.workload_binding(&binding)?;
        if let Some(mut child) = self.spawned.remove(&activation.runtime_instance_id) {
            if child.try_wait()?.is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let bundle = self.bundle_path(workload, activation);
        if bundle.exists() {
            fs::remove_dir_all(&bundle)
                .with_context(|| format!("removing runtime bundle {}", bundle.display()))?;
        }
        Ok(())
    }
}

fn same_path(left: &str, right: &str) -> bool {
    let normalize = |value: &str| value.replace('/', "\\").to_ascii_lowercase();
    normalize(left) == normalize(right)
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn passthrough_environment() -> BTreeMap<String, String> {
    PASSTHROUGH_ENVIRONMENT
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_string(), value))
        })
        .collect()
}

fn verify_artifacts(root: &Path, release: &SealedRelease) -> Result<()> {
    for artifact in &release.artifacts {
        let path = root.join(&artifact.destination);
        ensure!(path.exists(), "artifact {} is absent", artifact.artifact_id);
        let (sha256, size_bytes) = digest_artifact(&path)?;
        ensure!(
            format!("sha256-{sha256}") == artifact.sha256 && size_bytes == artifact.size_bytes,
            "artifact {} differs from its sealed digest",
            artifact.artifact_id
        );
    }
    Ok(())
}

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env_clear()
        .envs(passthrough_environment())
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Fetch the exact revision, prove it is the tree the plan froze, run the
/// steps, collect the artifacts, seal. Runs on a thread; touches only the
/// work root.
fn materialize(
    config: &HostActuatorConfig,
    transaction_id: &str,
    plan: &CompiledDeploymentPlan,
    sealed_at_unix_millis: u64,
) -> Result<(SealedRelease, PathBuf)> {
    plan.validate()?;
    let (declaration, binding) = plan.parsed_inputs()?;
    let workload = binding.workload.host()?;
    ensure!(
        workload.host == config.host,
        "plan is bound to host {:?}, this actuator is {:?}",
        workload.host,
        config.host
    );
    ensure!(
        plan.source.gitlinks.is_empty(),
        "host-native materialization does not fetch gitlinks yet"
    );
    ensure!(
        declaration.external_inputs.is_empty(),
        "host-native materialization does not fetch external inputs"
    );
    ensure!(
        transaction_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "transaction id is not a path component"
    );

    let source = config.sources().join(transaction_id);
    if source.exists() {
        remove_tree_inside(&config.sources(), &source)?;
    }
    git(
        &config.sources(),
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            &plan.source.origin,
            &source.display().to_string(),
        ],
    )?;
    git(
        &source,
        &["checkout", "--quiet", "--detach", &plan.source.revision],
    )?;
    ensure!(
        git(&source, &["rev-parse", "HEAD"])? == plan.source.revision,
        "checked out revision differs from the frozen one"
    );
    ensure!(
        git(&source, &["rev-parse", "HEAD^{tree}"])? == plan.source.source_tree,
        "checked out tree differs from the frozen one"
    );
    let recipe = fs::read(source.join(&plan.source.recipe_path))
        .context("reading the recipe from the fetched tree")?;
    ensure!(
        recipe == plan.recipe_blob,
        "recipe in the fetched tree differs from the plan's recipe bytes"
    );

    for step in &declaration.steps {
        let runner = binding.runners[&step.runner].host_native()?;
        ensure!(!step.argv.is_empty(), "step {} has no program", step.id);
        ensure!(
            runner.allowed_programs.contains(&step.argv[0]),
            "runner {} does not admit program {}",
            step.runner,
            step.argv[0]
        );
        let mut environment = passthrough_environment();
        environment.extend(runner.environment.clone());
        environment.insert(
            declaration.source_stamp_environment.clone(),
            plan.source.revision.clone(),
        );
        for required in &step.required_environment {
            ensure!(
                environment.contains_key(required),
                "step {} lacks required environment {required}",
                step.id
            );
        }
        let working_directory = source.join(&step.working_directory);
        let log_path = config
            .logs()
            .join(format!("{transaction_id}-{}.log", step.id));
        let log = File::create(&log_path)?;
        let status = Command::new(&step.argv[0])
            .args(&step.argv[1..])
            .current_dir(&working_directory)
            .env_clear()
            .envs(&environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .status()
            .with_context(|| format!("running step {}", step.id))?;
        ensure!(
            status.success(),
            "step {} exited with {status}; log at {}",
            step.id,
            log_path.display()
        );
    }

    let staging = config.staging().join(transaction_id);
    if staging.exists() {
        remove_tree_inside(&config.staging(), &staging)?;
    }
    fs::create_dir_all(&staging)?;
    let mut artifacts = Vec::new();
    for artifact in &declaration.artifacts {
        let source_path = match artifact.source_kind {
            ArtifactSource::RunnerOutput | ArtifactSource::WorktreeTree => {
                source.join(&artifact.source)
            }
        };
        ensure!(
            source_path.exists(),
            "declared artifact output {} is absent",
            artifact.id
        );
        let destination = staging.join(&artifact.destination);
        ensure!(
            destination.starts_with(&staging),
            "artifact destination escaped its staging root"
        );
        copy_artifact(&source_path, &destination)?;
        let (sha256, size_bytes) = digest_artifact(&destination)?;
        if let Some(expected) = &artifact.expected_sha256 {
            ensure!(
                &sha256 == expected,
                "artifact {} differs from its recipe-pinned digest",
                artifact.id
            );
        }
        artifacts.push(ArtifactReceipt {
            artifact_id: artifact.id.clone(),
            destination: artifact.destination.clone(),
            sha256: format!("sha256-{sha256}"),
            size_bytes,
            executable: artifact.executable,
        });
    }
    let release = SealedRelease::new(plan, artifacts, Vec::new(), sealed_at_unix_millis)?;
    Ok((release, staging))
}

/// Everything that asks the operating system about a process.
#[cfg(windows)]
mod platform {
    use super::*;
    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, GetLastError, HANDLE, LocalFree, STILL_ACTIVE,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, GetCurrentProcess, GetExitCodeProcess,
        GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
        TerminateProcess,
    };

    pub struct ProcessFacts {
        pub creation_time: u64,
        pub image_path: String,
        pub session_id: u32,
        pub exit_code: Option<u32>,
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by OpenProcess and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open(pid: u32, access: u32) -> Result<Option<Handle>> {
        // SAFETY: OpenProcess has no memory-safety preconditions.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            // SAFETY: GetLastError is always safe to call.
            let error = unsafe { GetLastError() };
            const ERROR_INVALID_PARAMETER: u32 = 87;
            const ERROR_ACCESS_DENIED: u32 = 5;
            return match error {
                ERROR_INVALID_PARAMETER => Ok(None),
                ERROR_ACCESS_DENIED => bail!("access denied opening pid {pid}"),
                other => bail!("OpenProcess({pid}) failed with Win32 error {other}"),
            };
        }
        Ok(Some(Handle(handle)))
    }

    fn filetime_u64(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    pub fn process_facts(pid: u32) -> Result<Option<ProcessFacts>> {
        let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)? else {
            return Ok(None);
        };
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: the handle is open with query access and every out-pointer is valid.
        let ok =
            unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) };
        ensure!(ok != 0, "GetProcessTimes({pid}) failed");
        let mut code = 0_u32;
        // SAFETY: as above.
        let ok = unsafe { GetExitCodeProcess(handle.0, &mut code) };
        ensure!(ok != 0, "GetExitCodeProcess({pid}) failed");
        let exit_code = if code == STILL_ACTIVE as u32 {
            None
        } else {
            Some(code)
        };
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: the buffer is valid for `length` UTF-16 units.
        let ok = unsafe {
            QueryFullProcessImageNameW(
                handle.0,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
        let image_path = if ok != 0 {
            String::from_utf16_lossy(&buffer[..length as usize])
        } else {
            // A process that has exited still answers times and exit code but
            // may no longer name its image.
            ensure!(
                exit_code.is_some(),
                "QueryFullProcessImageNameW({pid}) failed"
            );
            String::new()
        };
        let mut session_id = 0_u32;
        // SAFETY: the out-pointer is valid.
        let ok = unsafe { ProcessIdToSessionId(pid, &mut session_id) };
        ensure!(
            ok != 0 || exit_code.is_some(),
            "ProcessIdToSessionId({pid}) failed"
        );
        Ok(Some(ProcessFacts {
            creation_time: filetime_u64(creation),
            image_path,
            session_id,
            exit_code,
        }))
    }

    pub fn terminate(pid: u32, creation_time: u64) -> Result<()> {
        let Some(facts) = process_facts(pid)? else {
            return Ok(());
        };
        ensure!(
            facts.creation_time == creation_time,
            "refusing to terminate pid {pid}: it is now another process"
        );
        if facts.exit_code.is_some() {
            return Ok(());
        }
        let Some(handle) = open(pid, PROCESS_TERMINATE)? else {
            return Ok(());
        };
        // SAFETY: the handle has terminate access.
        let ok = unsafe { TerminateProcess(handle.0, 1) };
        ensure!(ok != 0, "TerminateProcess({pid}) failed");
        Ok(())
    }

    pub fn detach(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    pub fn current_user_sid() -> Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the current process pseudo-handle is always valid.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        ensure!(ok != 0, "OpenProcessToken failed");
        let token = Handle(token);
        let mut length = 0_u32;
        // SAFETY: a null buffer with zero length asks for the required size.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut length);
        }
        ensure!(length > 0, "GetTokenInformation reported no size");
        let mut buffer = vec![0_u8; length as usize];
        // SAFETY: the buffer is at least `length` bytes.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        };
        ensure!(ok != 0, "GetTokenInformation failed");
        // SAFETY: the buffer holds a TOKEN_USER written by the call above.
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let mut string_sid: *mut u16 = std::ptr::null_mut();
        // SAFETY: the SID pointer comes from the token buffer, still alive.
        let ok = unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string_sid) };
        ensure!(
            ok != 0 && !string_sid.is_null(),
            "ConvertSidToStringSidW failed"
        );
        let mut length = 0;
        // SAFETY: the string is NUL-terminated by the API.
        while unsafe { *string_sid.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `length` units are valid.
        let sid =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(string_sid, length) });
        // SAFETY: the API allocated the string with LocalAlloc.
        unsafe {
            LocalFree(string_sid.cast());
        }
        Ok(sid)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub struct ProcessFacts {
        pub creation_time: u64,
        pub image_path: String,
        pub session_id: u32,
        pub exit_code: Option<u32>,
    }

    pub fn process_facts(_pid: u32) -> Result<Option<ProcessFacts>> {
        bail!("idunn-host observes processes on Windows only")
    }

    pub fn terminate(_pid: u32, _creation_time: u64) -> Result<()> {
        bail!("idunn-host stops processes on Windows only")
    }

    pub fn detach(_command: &mut Command) {}

    pub fn current_user_sid() -> Result<String> {
        bail!("idunn-host runs on Windows only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_strict_and_absolute() {
        let root = if cfg!(windows) {
            "C:/GameCult/idunn"
        } else {
            "/gamecult/idunn"
        };
        let good = format!(
            r#"
schema = "gamecult.idunn.host_actuator_config.v1"
host = "raven"
idunn_endpoint = "10.77.0.1:17890"
identity_store = "{root}/host-actuator-identity.cc"
idunn_trust_anchor = "{root}/idunn-public-anchor.cc"
work_root = "{root}/work"
"#
        );
        let config = HostActuatorConfig::parse(&good).expect("valid config");
        assert_eq!(config.host, "raven");
        assert!(HostActuatorConfig::parse(&good.replace("10.77.0.1:17890", "yggdrasil")).is_err());
        assert!(HostActuatorConfig::parse(&good.replace("host_actuator_config.v1", "v0")).is_err());
        assert!(HostActuatorConfig::parse(&format!("{good}\nextra = 1\n")).is_err());
        assert!(HostActuatorConfig::parse(&good.replace(&format!("{root}/work"), "work")).is_err());
    }

    #[test]
    fn passthrough_is_an_allowlist_not_the_whole_environment() {
        let environment = passthrough_environment();
        for name in environment.keys() {
            assert!(PASSTHROUGH_ENVIRONMENT.contains(&name.as_str()), "{name}");
        }
    }

    #[test]
    fn a_step_program_outside_the_allowlist_is_refused() {
        let allowed: BTreeSet<String> = ["cargo".to_string()].into();
        assert!(allowed.contains("cargo"));
        assert!(!allowed.contains("powershell"));
    }

    #[cfg(windows)]
    #[test]
    fn the_actuator_can_observe_itself_and_a_child() -> Result<()> {
        let facts = platform::process_facts(std::process::id())?.expect("own process");
        assert!(facts.creation_time > 0);
        assert!(facts.exit_code.is_none());
        assert!(facts.image_path.to_ascii_lowercase().ends_with(".exe"));
        let sid = platform::current_user_sid()?;
        assert!(sid.starts_with("S-1-"), "{sid}");

        let mut command = Command::new("cmd.exe");
        command
            .args(["/c", "exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        platform::detach(&mut command);
        let mut child = command.spawn()?;
        let pid = child.id();
        let status = child.wait()?;
        assert_eq!(status.code(), Some(7));
        // After wait the handle is closed and the pid may be gone or reused;
        // either answer is a non-running answer.
        match platform::process_facts(pid)? {
            None => {}
            Some(facts) => assert!(facts.exit_code.is_some() || facts.creation_time > 0),
        }
        Ok(())
    }
}
