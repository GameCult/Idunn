//! The unit's process for an upstream daemon: parse the recipe's argv, read
//! what Idunn set, run the launcher, and map the outcome to an exit code.
#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use std::sync::atomic::{AtomicBool, Ordering};

    use anyhow::Context;
    use idunn_daemon::deployment::{
        IDUNN_PROCESS_WRITE_LEASE_ENVIRONMENT, IDUNN_RUNTIME_BUNDLE_ENVIRONMENT,
        IDUNN_RUNTIME_CANDIDATE_BIND_ENVIRONMENT, IDUNN_RUNTIME_MEMBER_BIND_ENVIRONMENT,
    };
    use idunn_daemon::launcher::{self, *};

    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    fn launch() -> anyhow::Result<Outcome> {
        let release_root = std::env::current_exe()?
            .parent()
            .context("the launcher is not inside a release")?
            .to_owned();
        let contract = LaunchContract::parse(release_root, std::env::args().skip(1))?;
        let bundle = std::env::var_os(IDUNN_RUNTIME_BUNDLE_ENVIRONMENT)
            .context("the Idunn runtime bundle is not set")?;
        let bind = std::env::var(IDUNN_RUNTIME_CANDIDATE_BIND_ENVIRONMENT)
            .context("the Idunn candidate bind is not set")?;
        let member = launcher::parse_member_bind(
            &std::env::var(IDUNN_RUNTIME_MEMBER_BIND_ENVIRONMENT)
                .context("the Idunn member bind is not set")?,
            &bind,
        )?;
        let (activation, provider) = launcher::inherited_authority_files()?;
        let authority = RuntimeAuthority::open(bundle.as_ref(), activation, provider)?;
        authority.require_candidate_bind(&bind)?;
        let lease_path = std::env::var_os(IDUNN_PROCESS_WRITE_LEASE_ENVIRONMENT);
        anyhow::ensure!(
            lease_path.is_some() == authority.expected().write_lease_required,
            "the process write lease binding differs from Expected"
        );
        let route = std::net::TcpListener::bind(&bind).context("binding the candidate endpoint")?;
        let sink = OdinRudpSink::from_expected(authority.expected())?;
        let lease = DriverLeaseReader::new(
            &authority.expected().target,
            lease_path.as_deref().unwrap_or_default().as_ref(),
        );
        let health_path = contract.health_path.clone();
        let ports = Ports {
            credentials: std::sync::Arc::new(CredentialsDirectory::from_environment()),
            clock: std::sync::Arc::new(SystemClock),
            sink: std::sync::Arc::new(sink),
            lease: std::sync::Arc::new(lease),
            spawner: Box::new(ProcessSpawner),
            health: Box::new(move |member| {
                std::sync::Arc::new(HttpHealthProbe {
                    member,
                    path: health_path.clone(),
                })
            }),
        };
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        }
        Ok(launcher::run(
            &contract,
            authority,
            route,
            member,
            &std::env::temp_dir(),
            ports,
            &STOP,
        ))
    }

    match launch() {
        Ok(outcome) => {
            if let Outcome::Failed(code) = &outcome {
                eprintln!("idunn-launch ended: {code}");
            }
            std::process::ExitCode::from(outcome.exit_code() as u8)
        }
        Err(error) => {
            eprintln!("idunn-launch refused: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("idunn-launch runs only on Unix");
    std::process::exit(2);
}
