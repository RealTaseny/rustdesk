use super::{gamma::GammaBackup, input::LocalInput, locker::Locker};
use dbus::{
    blocking::{stdintf::org_freedesktop_dbus::Properties, Connection as Bus, Proxy},
    Path,
};
use hbb_common::{log, ResultType};
use nix::{
    errno::Errno,
    poll::{poll, PollFd, PollFlags, PollTimeout},
};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    os::{
        fd::AsFd,
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    },
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::{
            ConnectionExt as _, Crtc, GetCrtcGammaReply, GetCrtcInfoReply,
            GetScreenResourcesCurrentReply, Mode, Output, Rotation, SetConfig,
        },
        xfixes::ConnectionExt as _,
        xproto::{ConnectionExt as _, ScreenSaver},
    },
    rust_connection::RustConnection,
};

const LOGIN_SERVICE: &str = "org.freedesktop.login1";
const LOGIN_SESSION: &str = "org.freedesktop.login1.Session";
const DBUS_TIMEOUT: Duration = Duration::from_secs(1);
const VERIFY_INTERVAL_MS: u16 = 200;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(3);
const LOG_INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn run(recovery: bool) -> ResultType<()> {
    if !recovery && parent_released(PollTimeout::ZERO)? {
        return Ok(());
    }
    let desktop = match Desktop::current() {
        Ok(desktop) => desktop,
        Err(error) => {
            report("unsupported")?;
            return Err(error);
        }
    };
    let _ownership = match desktop.claim() {
        Ok(socket) => socket,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            report("busy")?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    let display = match desktop.x11_display() {
        Ok(display) => display,
        Err(error) => {
            report("unsupported")?;
            return Err(error);
        }
    };
    let mut blackout = match Blackout::new(&display, &desktop.id) {
        Ok(blackout) => blackout,
        Err(error) => {
            if let Err(lock_error) = desktop.request_lock() {
                log::warn!("X11 privacy screen could not request the desktop lock: {lock_error}");
            }
            report("failed")?;
            return Err(error);
        }
    };
    if recovery && blackout.backup.is_empty() {
        return desktop.request_lock();
    }
    if !recovery {
        if let Err(error) = desktop.locker(&blackout.x11) {
            report("unsupported")?;
            return Err(error);
        }
    }
    let mut local_input = None;
    if !recovery {
        let mut lock_required = !blackout.backup.is_empty();
        let mut failure = "failed";
        let result = (|| {
            let layout = blackout.cover()?;
            let input = LocalInput::grab(&display).map_err(|error| {
                failure = super::INPUT_BLOCK_FAILED;
                error
            })?;
            local_input = Some(input);
            if lock_required {
                desktop.wait_for_lock(&display, &mut blackout, &mut local_input)?;
            }
            blackout.monitor(&layout, local_input.as_ref(), &mut lock_required)
        })();
        if let Err(error) = result {
            log::warn!("X11 privacy screen coverage failed: {error:#}");
            if !lock_required {
                // A new session cannot access the desktop before the first coverage report.
                blackout.restore()?;
                drop(local_input);
                if let Err(error) = report(&format!("{}{failure}", super::STARTUP_ROLLED_BACK)) {
                    log::trace!("Could not report privacy screen rollback to the host: {error}");
                }
                return Ok(());
            }
            if let Err(error) = report(failure) {
                log::debug!("Could not report privacy screen failure to the host: {error}");
            }
        }
        parent_released(PollTimeout::NONE)?;
    }

    desktop.wait_for_lock(&display, &mut blackout, &mut local_input)?;
    blackout.restore()?;
    drop(local_input);
    Ok(())
}

pub(super) fn request_desktop_lock() -> ResultType<()> {
    let desktop = Desktop::current()?;
    let _ownership = desktop.claim()?;
    desktop.request_lock()
}

fn report(status: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{status}")?;
    stdout.flush()
}

fn parent_released(timeout: PollTimeout) -> ResultType<bool> {
    let stdin = io::stdin();
    let mut poll_fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    loop {
        match poll(&mut poll_fds, timeout) {
            Ok(ready) => return Ok(ready != 0),
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

struct Desktop {
    bus: Bus,
    id: String,
    path: Path<'static>,
    uid: u32,
}

impl Desktop {
    fn current() -> ResultType<Self> {
        let bus = Bus::new_system()?;
        let (id, path): (String, Path<'static>) = bus
            .with_proxy(
                LOGIN_SERVICE,
                "/org/freedesktop/login1/seat/seat0",
                DBUS_TIMEOUT,
            )
            .get("org.freedesktop.login1.Seat", "ActiveSession")?;
        if id.is_empty() {
            hbb_common::bail!("No active local desktop session");
        }
        let session = bus.with_proxy(LOGIN_SERVICE, &path, DBUS_TIMEOUT);
        let (uid, _): (u32, Path<'static>) = session.get(LOGIN_SESSION, "User")?;
        let current_uid = hbb_common::users::get_current_uid();
        let class: String = session.get(LOGIN_SESSION, "Class")?;
        if (current_uid != 0 && current_uid != uid)
            || class != "user" && class != "user-early"
            || session.get::<bool>(LOGIN_SESSION, "Remote")?
            || !session.get::<bool>(LOGIN_SESSION, "Active")?
        {
            hbb_common::bail!("The active session is not the local user desktop");
        }
        session.get::<bool>(LOGIN_SESSION, "LockedHint")?;
        Ok(Self { bus, id, path, uid })
    }

    fn proxy(&self) -> Proxy<'_, &Bus> {
        self.bus.with_proxy(LOGIN_SERVICE, &self.path, DBUS_TIMEOUT)
    }

    fn claim(&self) -> io::Result<UnixDatagram> {
        // A replacement server may start while the old helper is still waiting for the lock.
        let address = SocketAddr::from_abstract_name(format!("rustdesk-x11-privacy-{}", self.id))?;
        UnixDatagram::bind_addr(&address)
    }

    fn x11_display(&self) -> ResultType<String> {
        let session = self.proxy();
        let session_type: String = session.get(LOGIN_SESSION, "Type")?;
        let session_display: String = session.get(LOGIN_SESSION, "Display")?;
        let display = std::env::var("DISPLAY")?;
        let number = display_number(&display);
        if session_type != "x11"
            || number.is_none()
            || (!session_display.is_empty() && display_number(&session_display) != number)
        {
            hbb_common::bail!("The active desktop is not the X11 display used by RustDesk");
        }
        Ok(display)
    }

    fn locker(&self, x11: &RustConnection) -> ResultType<Locker> {
        Locker::discover(&self.bus, self.uid, x11)
    }

    fn request_lock(&self) -> ResultType<()> {
        self.proxy()
            .method_call::<(), _, _, _>(LOGIN_SESSION, "Lock", ())?;
        Ok(())
    }

    fn wait_for_lock(
        &self,
        display: &str,
        blackout: &mut Blackout,
        local_input: &mut Option<LocalInput>,
    ) -> ResultType<()> {
        let session = self.proxy();
        let started = Instant::now();
        let mut next_request = started;
        let mut pending = None;
        loop {
            blackout.keep_covered();
            if let Some(input) = local_input.as_ref() {
                if let Err(error) = input.refresh() {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "X11 privacy screen could not block local input while locking: {error:#}"
                    );
                }
            }
            let locked_hint: Option<bool> = match session.get(LOGIN_SESSION, "LockedHint") {
                Ok(locked) => Some(locked),
                Err(error)
                    if matches!(
                        error.name(),
                        Some(
                            "org.freedesktop.DBus.Error.UnknownObject"
                                | "org.freedesktop.login1.NoSuchSession"
                        )
                    ) =>
                {
                    return Err(error.into());
                }
                Err(error) => {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "X11 privacy screen could not check the desktop lock: {error}"
                    );
                    None
                }
            };
            if Instant::now() >= next_request {
                next_request = Instant::now() + LOCK_RETRY_INTERVAL;
                if local_input.is_none() {
                    match LocalInput::grab(display) {
                        Ok(input) => *local_input = Some(input),
                        Err(error) => hbb_common::throttled_log!(
                            LOG_INTERVAL,
                            warn,
                            "X11 privacy screen could not grab local input while locking: {error:#}"
                        ),
                    }
                }
                let result = if pending.is_none() {
                    self.locker(&blackout.x11).and_then(|locker| {
                        let request =
                            locker.request(&blackout.x11, locked_hint, || self.request_lock())?;
                        pending = Some((locker, request));
                        Ok(())
                    })
                } else {
                    // Keep the original desktop call pending. GNOME waits for the
                    // curtain and a second call would register an extra callback.
                    self.request_lock()
                };
                if let Err(error) = result {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "X11 privacy screen could not request the desktop lock: {error:#}"
                    );
                    if pending.is_none() {
                        if let Err(error) = self.request_lock() {
                            hbb_common::throttled_log!(
                                LOG_INTERVAL,
                                warn,
                                "X11 privacy screen could not request the logind lock: {error:#}"
                            );
                        }
                    }
                }
            }
            if let Some((locker, request)) = pending.as_mut() {
                match locker.confirmed(request, &blackout.x11, locked_hint == Some(true)) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => {
                        hbb_common::throttled_log!(
                            LOG_INTERVAL,
                            warn,
                            "X11 privacy screen could not confirm the desktop lock: {error:#}"
                        );
                        pending = None;
                    }
                }
            }
            if started.elapsed() >= LOCK_RETRY_INTERVAL {
                hbb_common::throttled_log!(
                    LOG_INTERVAL,
                    warn,
                    "X11 privacy screen is keeping the displays covered while waiting for the desktop lock"
                );
            }
            std::thread::sleep(Duration::from_millis(VERIFY_INTERVAL_MS.into()));
        }
    }
}

fn display_number(display: &str) -> Option<u16> {
    let (host, number_and_screen) = display.rsplit_once(':')?;
    if !matches!(host, "" | "unix" | "unix/") {
        return None;
    }
    number_and_screen.split('.').next()?.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
struct ActiveCrtc {
    last_set: u32,
    mode: Mode,
    outputs: Vec<Output>,
    position: (i16, i16),
    rotation: Rotation,
}

impl ActiveCrtc {
    fn from_info(resources: &GetScreenResourcesCurrentReply, info: GetCrtcInfoReply) -> Self {
        Self {
            // The configuration timestamp also advances when available modes change.
            last_set: resources.timestamp,
            mode: info.mode,
            outputs: info.outputs,
            position: (info.x, info.y),
            rotation: info.rotation,
        }
    }
}

struct Blackout {
    x11: RustConnection,
    backup: GammaBackup,
}

impl Blackout {
    fn new(display: &str, session: &str) -> ResultType<Self> {
        let (x11, _) = x11rb::connect(Some(display))?;
        let randr = x11.randr_query_version(1, 3)?.reply()?;
        if (randr.major_version, randr.minor_version) < (1, 3) {
            hbb_common::bail!("RandR 1.3 is required");
        }
        let xfixes = x11.xfixes_query_version(4, 0)?.reply()?;
        if xfixes.major_version < 4 {
            hbb_common::bail!("XFixes 4 is required to hide the hardware cursor");
        }
        let backup = GammaBackup::load(&x11, session)?;
        Ok(Self { x11, backup })
    }

    fn layout(&self) -> ResultType<BTreeMap<Crtc, ActiveCrtc>> {
        let mut displays = BTreeMap::new();
        for screen in &self.x11.setup().roots {
            let resources = self
                .x11
                .randr_get_screen_resources_current(screen.root)?
                .reply()?;
            for &crtc in &resources.crtcs {
                let info = self
                    .x11
                    .randr_get_crtc_info(crtc, resources.config_timestamp)?
                    .reply()?;
                if info.status != SetConfig::SUCCESS {
                    hbb_common::bail!("RandR changed while reading the active displays");
                }
                if info.mode != 0 && !info.outputs.is_empty() {
                    displays.insert(crtc, ActiveCrtc::from_info(&resources, info));
                }
            }
        }
        Ok(displays)
    }

    fn cover(&mut self) -> ResultType<BTreeMap<Crtc, ActiveCrtc>> {
        let layout = self.layout()?;
        if layout.is_empty() {
            hbb_common::bail!("No active RandR display outputs");
        }
        let originals = layout
            .keys()
            .map(|&crtc| Ok((crtc, self.read_gamma(crtc)?)))
            .collect::<ResultType<Vec<_>>>()?;
        for (crtc, gamma) in originals {
            self.blacken(crtc, gamma)?;
        }
        // Some drivers scan out the cursor independently of the desktop color table.
        for screen in &self.x11.setup().roots {
            self.x11.xfixes_hide_cursor(screen.root)?.check()?;
        }
        Ok(layout)
    }

    fn read_gamma(&self, crtc: Crtc) -> ResultType<GetCrtcGammaReply> {
        let gamma = self.x11.randr_get_crtc_gamma(crtc)?.reply()?;
        if gamma.red.is_empty() {
            hbb_common::bail!("No color table for RandR CRTC {crtc}");
        }
        Ok(gamma)
    }

    fn blacken(&mut self, crtc: Crtc, gamma: GetCrtcGammaReply) -> ResultType<()> {
        if !is_black(&gamma) {
            let black = vec![0; gamma.red.len()];
            self.backup.remember(&self.x11, crtc, gamma)?;
            self.x11
                .randr_set_crtc_gamma(crtc, &black, &black, &black)?
                .check()?;
        }
        Ok(())
    }

    fn monitor(
        &self,
        expected: &BTreeMap<Crtc, ActiveCrtc>,
        input: Option<&LocalInput>,
        lock_required: &mut bool,
    ) -> ResultType<()> {
        let Some(input) = input else {
            hbb_common::bail!("Local input is unprotected");
        };
        loop {
            input.refresh()?;
            if &self.layout()? != expected {
                hbb_common::bail!("RandR display configuration changed");
            }
            for &crtc in expected.keys() {
                if !is_black(&self.read_gamma(crtc)?) {
                    hbb_common::bail!("RandR color table changed for CRTC {crtc}");
                }
            }
            *lock_required = true;
            report(super::COVERED)?;
            if parent_released(VERIFY_INTERVAL_MS.into())? {
                return Ok(());
            }
        }
    }

    fn keep_covered(&mut self) {
        let result = self.layout().map(|layout| {
            for crtc in layout.into_keys() {
                if let Err(error) = self
                    .read_gamma(crtc)
                    .and_then(|gamma| self.blacken(crtc, gamma))
                {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "X11 privacy screen could not cover CRTC {crtc} while locking: {error:#}"
                    );
                }
            }
        });
        if let Err(error) = result {
            hbb_common::throttled_log!(
                LOG_INTERVAL,
                warn,
                "X11 privacy screen could not read displays while locking: {error:#}"
            );
        }
    }

    fn restore(self) -> ResultType<()> {
        let mut available = Vec::new();
        for screen in &self.x11.setup().roots {
            available.extend(
                self.x11
                    .randr_get_screen_resources_current(screen.root)?
                    .reply()?
                    .crtcs,
            );
        }
        let result = self.backup.restore(&self.x11, &available);
        // GNOME keeps the lock screen black until activity even after LockedHint is true.
        let wake: ResultType<()> = (|| {
            self.x11.force_screen_saver(ScreenSaver::RESET)?.check()?;
            Ok(())
        })();
        if let Err(error) = wake {
            log::warn!("X11 privacy screen could not wake the locked display: {error:#}");
        }
        result
    }
}

fn is_black(gamma: &GetCrtcGammaReply) -> bool {
    gamma
        .red
        .iter()
        .chain(&gamma.green)
        .chain(&gamma.blue)
        .all(|&entry| entry == 0)
}

#[cfg(test)]
mod tests {
    use super::{ActiveCrtc, GetCrtcInfoReply, GetScreenResourcesCurrentReply, Rotation};

    #[test]
    fn available_mode_changes_preserve_the_two_display_layout() {
        let mut resources = GetScreenResourcesCurrentReply {
            timestamp: 20,
            config_timestamp: 10,
            ..Default::default()
        };
        let layout = |resources: &GetScreenResourcesCurrentReply| {
            [(0, 4), (800, 5)].map(|(x, output)| {
                ActiveCrtc::from_info(
                    resources,
                    GetCrtcInfoReply {
                        mode: 3,
                        x,
                        rotation: Rotation::ROTATE0,
                        outputs: vec![output],
                        ..Default::default()
                    },
                )
            })
        };
        let covered = layout(&resources);
        resources.config_timestamp = 30;
        assert_eq!(covered, layout(&resources));

        resources.timestamp = 40;
        assert_ne!(covered, layout(&resources));
    }
}
