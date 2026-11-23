mod gamma;
mod guard;
mod input;
mod locker;

use base::config::keys::OPTION_ALLOW_AUTO_PRIVACY_SCREEN;
use hbb_common::{
    config::Config,
    log,
    tokio::{
        self,
        io::{AsyncBufReadExt, BufReader},
        process::{ChildStdin, Command},
        sync::watch,
        time::timeout,
    },
    ResultType,
};
use std::{
    ffi::OsStr,
    os::unix::process::CommandExt,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

const GUARD_ARG: &str = "--x11-privacy-screen-guard";
const RECOVER_ARG: &str = "--x11-privacy-screen-recover";
const COVERED: &str = "covered";
const INPUT_BLOCK_FAILED: &str = "input-block-failed";
const STARTUP_ROLLED_BACK: &str = "rolled-back ";
const START_TIMEOUT: Duration = Duration::from_secs(6);
const COVERAGE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) const LOST_MESSAGE: &str = "Privacy screen failed during the session";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Busy,
    Unsupported,
    InputBlock,
    Start,
    Lost,
}

impl Failure {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Busy => "Privacy screen unavailable while another connection uses the desktop",
            Self::Unsupported => "Privacy screen requires a local X11 session with screen locking",
            Self::InputBlock => "Failed to block local keyboard and mouse for the privacy screen",
            Self::Start => "Failed to cover all displays for the privacy screen",
            Self::Lost => LOST_MESSAGE,
        }
    }
}

struct DesktopConnections {
    ordinary: usize,
    protected: bool,
}

static DESKTOP_CONNECTIONS: Mutex<DesktopConnections> = Mutex::new(DesktopConnections {
    ordinary: 0,
    protected: false,
});

struct DesktopSlot {
    protected: bool,
}

impl DesktopSlot {
    fn reserve(protected: bool) -> Result<Self, Failure> {
        let mut connections = DESKTOP_CONNECTIONS.lock().unwrap();
        if connections.protected || (protected && connections.ordinary != 0) {
            return Err(Failure::Busy);
        }
        if protected {
            connections.protected = true;
        } else {
            connections.ordinary += 1;
        }
        Ok(Self { protected })
    }
}

impl Drop for DesktopSlot {
    fn drop(&mut self) {
        let mut connections = DESKTOP_CONNECTIONS.lock().unwrap();
        if self.protected {
            connections.protected = false;
        } else {
            connections.ordinary -= 1;
        }
    }
}

#[derive(Clone)]
pub(crate) struct InputGate(Arc<Mutex<Option<Arc<ChildStdin>>>>);

impl InputGate {
    // Keep the pipe open for the lifetime of an input operation. Once stopped, a queued
    // operation cannot get a permit and the helper sees EOF after the last operation ends.
    pub(crate) fn permit(&self) -> Option<Arc<ChildStdin>> {
        self.0.lock().unwrap().clone()
    }

    fn stop(&self) -> bool {
        self.0.lock().unwrap().take().is_some()
    }
}

enum Status {
    Starting,
    Covered,
    Failed(Failure),
}

struct ScreenProcess {
    input: InputGate,
    status: watch::Receiver<Status>,
    _slot: Arc<DesktopSlot>,
}

fn guard_command(arg: &str) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    // The service's stop command matches "rustdesk --" for the server and tray.
    command.as_std_mut().arg0("rustdesk-x11-privacy-screen");
    command.arg(arg).process_group(0);
    Ok(command)
}

async fn recover_screen() -> ResultType<()> {
    let mut recovery = guard_command(RECOVER_ARG)?
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()?;
    let status = recovery.wait().await?;
    if !status.success() {
        hbb_common::bail!("X11 privacy screen recovery helper exited with {status}");
    }
    Ok(())
}

impl ScreenProcess {
    fn launch(slot: DesktopSlot, on_lost: impl FnOnce() + Send + 'static) -> ResultType<Self> {
        let child = guard_command(GUARD_ARG)
            .and_then(|mut command| command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn());
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = guard::request_desktop_lock() {
                        hbb_common::throttled_log!(
                            Duration::from_secs(5),
                            error,
                            "Could not lock the desktop after privacy screen launch failed: {error}"
                        );
                    }
                    drop(slot);
                });
                return Err(error.into());
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            hbb_common::bail!("Could not open the X11 privacy screen helper pipes");
        };
        let input = InputGate(Arc::new(Mutex::new(Some(Arc::new(stdin)))));
        let input_for_task = input.clone();
        let (tx, status) = watch::channel(Status::Starting);
        // Keep the exclusive slot until both the old connection and the helper have closed.
        let slot = Arc::new(slot);
        let slot_for_task = slot.clone();
        tokio::spawn(async move {
            let mut reports = BufReader::new(stdout).lines();
            let (failure, rolled_back) = match timeout(START_TIMEOUT, reports.next_line()).await {
                Ok(Ok(Some(report))) if report == COVERED => {
                    tx.send_replace(Status::Covered);
                    while matches!(
                        timeout(COVERAGE_TIMEOUT, reports.next_line()).await,
                        Ok(Ok(Some(report))) if report == COVERED
                    ) {}
                    (Failure::Lost, false)
                }
                Ok(Ok(Some(report))) => {
                    let restored = report.strip_prefix(STARTUP_ROLLED_BACK);
                    let failure = match restored.unwrap_or(&report) {
                        "busy" => Failure::Busy,
                        "unsupported" => Failure::Unsupported,
                        INPUT_BLOCK_FAILED => Failure::InputBlock,
                        _ => Failure::Start,
                    };
                    (failure, restored.is_some())
                }
                _ => (Failure::Start, false),
            };
            let was_running = input_for_task.stop();
            drop(reports);
            if rolled_back {
                if let Err(error) = child.wait().await {
                    hbb_common::throttled_log!(
                        Duration::from_secs(5),
                        warn,
                        "Could not reap the X11 privacy screen helper after rollback: {error}"
                    );
                }
                // Show the startup error once the helper has freed the desktop for retry.
                drop(slot_for_task);
                tx.send_replace(Status::Failed(failure));
                return;
            }
            tx.send_replace(Status::Failed(failure));
            if was_running && failure == Failure::Lost {
                log::warn!("{}", LOST_MESSAGE);
                on_lost();
            }
            let status = child.wait().await;
            if !matches!(failure, Failure::Busy | Failure::Unsupported)
                && !matches!(&status, Ok(status) if status.success())
            {
                log::warn!("X11 privacy screen helper ended unexpectedly: {status:?}");
                if let Err(error) = recover_screen().await {
                    log::error!("Could not recover the X11 privacy screen: {error:#}");
                    let lock = tokio::task::spawn_blocking(guard::request_desktop_lock).await;
                    if let Err(error) = lock
                        .map_err(hbb_common::anyhow::Error::from)
                        .and_then(|v| v)
                    {
                        log::error!(
                            "Could not request a lock after X11 privacy failure: {error:#}"
                        );
                    }
                }
            }
            drop(slot_for_task);
        });
        Ok(Self {
            input,
            status,
            _slot: slot,
        })
    }
}

impl Drop for ScreenProcess {
    fn drop(&mut self) {
        self.input.stop();
    }
}

pub(crate) type WorkerInput = Arc<OnceLock<InputGate>>;

#[derive(Default)]
pub(crate) struct Desktop {
    _ordinary_slot: Option<DesktopSlot>,
    screen: Option<ScreenProcess>,
    input: WorkerInput,
}

impl Desktop {
    pub(crate) async fn admit(
        &mut self,
        on_lost: impl FnOnce() + Send + 'static,
    ) -> Result<bool, Failure> {
        let enabled = Config::get_bool_option(OPTION_ALLOW_AUTO_PRIVACY_SCREEN);
        let slot = DesktopSlot::reserve(enabled)?;
        if !enabled {
            self._ordinary_slot = Some(slot);
            return Ok(false);
        }
        let mut screen = ScreenProcess::launch(slot, on_lost).map_err(|error| {
            hbb_common::throttled_log!(
                Duration::from_secs(5),
                error,
                "Failed to launch the X11 privacy screen helper: {error}"
            );
            Failure::Start
        })?;
        let status = screen
            .status
            .wait_for(|status| !matches!(status, Status::Starting))
            .await
            .map_err(|_| Failure::Start)?;
        if let Status::Failed(failure) = *status {
            return Err(failure);
        }
        drop(status);
        self.input
            .set(screen.input.clone())
            .map_err(|_| Failure::Start)?;
        self.screen = Some(screen);
        Ok(true)
    }

    pub(crate) fn worker_input(&self) -> WorkerInput {
        self.input.clone()
    }

    pub(crate) fn coverage(&self) -> Option<bool> {
        self.screen.as_ref().map(|screen| {
            matches!(*screen.status.borrow(), Status::Covered) && screen.input.permit().is_some()
        })
    }

    pub(crate) fn stop_privacy(&self) -> bool {
        if let Some(screen) = &self.screen {
            screen.input.stop();
            return true;
        }
        false
    }
}

pub(crate) fn dispatch_guard() {
    let recovery = match std::env::args_os().nth(1).as_deref() {
        Some(arg) if arg == OsStr::new(GUARD_ARG) => false,
        Some(arg) if arg == OsStr::new(RECOVER_ARG) => true,
        _ => return,
    };
    hbb_common::init_log(false, "x11-privacy-screen");
    let code = match guard::run(recovery) {
        Ok(()) => 0,
        Err(error) => {
            log::error!("X11 privacy screen helper: {error:#}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::{DesktopSlot, Failure, InputGate};
    use hbb_common::tokio::{self, process::Command, time::timeout};
    use std::{
        process::Stdio,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[test]
    fn protected_desktop_cannot_share_or_displace_an_existing_session() {
        let ordinary = DesktopSlot::reserve(false).unwrap();
        let another_ordinary = DesktopSlot::reserve(false).unwrap();
        assert_eq!(DesktopSlot::reserve(true).err(), Some(Failure::Busy));
        drop(ordinary);
        drop(another_ordinary);

        let protected = DesktopSlot::reserve(true).unwrap();
        assert_eq!(DesktopSlot::reserve(false).err(), Some(Failure::Busy));
        assert_eq!(DesktopSlot::reserve(true).err(), Some(Failure::Busy));
        drop(protected);

        assert!(DesktopSlot::reserve(true).is_ok());
    }

    #[tokio::test]
    async fn stopping_privacy_discards_queued_input_before_releasing_the_helper() {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = InputGate(Arc::new(Mutex::new(child.stdin.take().map(Arc::new))));
        let in_flight = input.permit().unwrap();

        assert!(input.stop());
        assert!(input.permit().is_none());
        assert!(timeout(Duration::from_millis(50), child.wait())
            .await
            .is_err());

        drop(in_flight);
        assert!(timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
    }
}
