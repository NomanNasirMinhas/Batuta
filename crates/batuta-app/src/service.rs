//! Running the daemon as a Windows service, and installing it.
//!
//! A bare executable registered with `sc create` does not work: the SCM
//! expects the process to connect to it and report status, and kills anything
//! that does not. `windows-service` provides that plumbing.
//!
//! Startup is reported honestly. Loading a 260 MB snapshot — or building one
//! from scratch on first run — takes longer than the SCM's patience for a
//! service that claims to be starting without saying so, so the service sits
//! in `StartPending` until the pipe is actually accepting, and only then
//! reports `Running`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::config::Config;
use crate::daemon;

pub const SERVICE_NAME: &str = "Batuta";
pub const DISPLAY_NAME: &str = "Batuta File Index";
const DESCRIPTION: &str =
    "Keeps the Batuta file index current by following the NTFS change journal.";

const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Entry point when the SCM starts us. Called by `batuta service-run`.
pub fn run() -> Result<()> {
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("connecting to the service control manager")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = serve() {
        eprintln!("service failed: {e:#}");
    }
}

fn serve() -> Result<()> {
    let stop = daemon::Stop::new();
    let stop_for_handler = stop.clone();

    let handler = move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        // Shutdown is the machine going down; treat it exactly like Stop so
        // the index is checkpointed either way.
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop_for_handler.trigger();
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

    let report = |state: ServiceState, accept: ServiceControlAccept, wait: Duration| {
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: wait,
            process_id: None,
        });
    };

    // The first scan can take seconds; say so rather than letting the SCM
    // conclude we hung.
    report(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(120),
    );

    let cfg = Config::load(&Config::default_path()).unwrap_or_default();
    let result = daemon::serve_with(&cfg, false, stop, || {
        report(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            Duration::default(),
        );
    });

    report(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::default(),
    );
    result
}

// ------------------------------------------------------------------ installer

fn manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .context("opening the service control manager (needs Administrator)")
}

/// Register the service, pointed at `exe`.
pub fn install(exe: &Path) -> Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        // `windows-service` passes the path as a separate field and quotes it
        // itself, which is what keeps an install directory containing a space
        // from being read as two arguments.
        executable_path: PathBuf::from(exe),
        launch_arguments: vec![OsString::from("service-run")],
        dependencies: vec![],
        account_name: None, // LocalSystem: required to open a raw volume handle
        account_password: None,
    };

    let service = match m.create_service(&info, ServiceAccess::CHANGE_CONFIG) {
        Ok(s) => s,
        Err(e) => {
            // Already there: reconfigure rather than failing, so re-running
            // setup after moving the install directory does the right thing.
            let existing = m
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .with_context(|| {
                    format!("creating the service failed ({e}), and it already exists")
                })?;
            existing
                .change_config(&info)
                .context("updating the existing service")?;
            existing
        }
    };
    let _ = service.set_description(DESCRIPTION);
    Ok(())
}

/// Start the service and wait for it to report running.
pub fn start(timeout: Duration) -> Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let service = m.open_service(
        SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;

    if service.query_status()?.current_state != ServiceState::Running {
        service.start::<&str>(&[]).context("starting the service")?;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let state = service.query_status()?.current_state;
        match state {
            ServiceState::Running => return Ok(()),
            ServiceState::Stopped if Instant::now() > deadline => {
                bail!("the service stopped immediately after starting")
            }
            _ if Instant::now() > deadline => {
                bail!("the service did not reach running within {timeout:?} (state: {state:?})")
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Stop the service if it is running, and wait for it.
pub fn stop(timeout: Duration) -> Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) = m.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    ) else {
        return Ok(()); // not installed
    };

    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    let _ = service.stop();

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if service.query_status()?.current_state == ServiceState::Stopped {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!("the service did not stop within {timeout:?}")
}

/// Remove the service. Stopping first, so deletion takes effect immediately
/// rather than being deferred until the process exits.
pub fn uninstall() -> Result<bool> {
    let _ = stop(Duration::from_secs(20));
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let Ok(service) = m.open_service(SERVICE_NAME, ServiceAccess::DELETE) else {
        return Ok(false);
    };
    service.delete().context("deleting the service")?;
    Ok(true)
}

/// Is the service installed?
pub fn is_installed() -> bool {
    manager(ServiceManagerAccess::CONNECT)
        .and_then(|m| {
            m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)
                .map_err(Into::into)
        })
        .is_ok()
}

/// Prove the service is actually serving, not merely running.
///
/// A service can report `Running` while having failed to open its pipe — the
/// state alone says the process started, not that it works. Only a round trip
/// over the pipe shows it is answering.
pub fn verify(timeout: Duration) -> Result<VerifyReport> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let service = m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS)?;
    let state = service.query_status()?.current_state;

    let deadline = Instant::now() + timeout;
    let mut answered = None;
    while Instant::now() < deadline {
        if let Some(batuta_ipc::Response::Status {
            nodes, watching, ..
        }) = daemon::try_daemon(&batuta_ipc::Request::Status)
        {
            answered = Some((nodes, watching));
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    Ok(VerifyReport {
        running: state == ServiceState::Running,
        answered,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct VerifyReport {
    pub running: bool,
    /// Nodes indexed and whether it is watching, if the pipe answered.
    pub answered: Option<(u64, bool)>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.running && self.answered.is_some()
    }

    pub fn describe(&self) -> String {
        match (self.running, self.answered) {
            (true, Some((nodes, watching))) => format!(
                "running, answering on the pipe, {} entries indexed, watching: {}",
                crate::fmt::count(nodes),
                if watching { "yes" } else { "no" }
            ),
            (true, None) => {
                "running, but not answering on the pipe — check the Windows event log".into()
            }
            (false, Some(_)) => {
                "not reported running, though something is answering the pipe".into()
            }
            (false, None) => "not running".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_service_that_never_answers_is_not_ok() {
        // The case the state alone would miss: the process started but the
        // pipe never opened, so nothing can actually query it.
        let r = VerifyReport {
            running: true,
            answered: None,
        };
        assert!(!r.ok());
        assert!(r.describe().contains("not answering"), "{}", r.describe());
    }

    #[test]
    fn only_running_plus_answering_counts_as_working() {
        let r = VerifyReport {
            running: true,
            answered: Some((3_401_288, true)),
        };
        assert!(r.ok());
        let d = r.describe();
        assert!(d.contains("3,401,288"), "{d}");
        assert!(d.contains("watching: yes"), "{d}");
    }

    #[test]
    fn a_stopped_service_is_reported_plainly() {
        let r = VerifyReport {
            running: false,
            answered: None,
        };
        assert!(!r.ok());
        assert_eq!(r.describe(), "not running");
    }

    #[test]
    fn the_service_launches_itself_with_the_hidden_subcommand() {
        // The name must match the clap subcommand, or the SCM would start a
        // process that immediately exits with a usage error.
        assert_eq!(SERVICE_NAME, "Batuta");
        assert!(!DISPLAY_NAME.is_empty());
    }
}
