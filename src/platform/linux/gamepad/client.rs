use super::{
    axis_neutral, channel, gamepad_axis, gamepad_button, input, joystick_index, uapi, valid_device,
    ERROR_LOG_INTERVAL, MAX_DEVICES, MAX_EVENTS, MAX_RUMBLE_SLOTS, VIRTUAL_PHYS,
};
use crate::client::LoginConfigHandler;
use base::message_proto::{
    gamepad_attached, gamepad_channel::Union, gamepad_device::Axis, gamepad_input::Kind,
    gamepad_rumble::Action, GamepadChannel, GamepadDevice, GamepadFrame, GamepadInput,
    GamepadRumble,
};
use evdev::Device;
use hbb_common::{
    anyhow::Context,
    futures::future::select_all,
    libc, log,
    tokio::{
        self,
        io::unix::AsyncFd,
        sync::{mpsc, watch},
        task::JoinHandle,
        time::{self, Duration, MissedTickBehavior},
    },
    ResultType,
};
use std::{
    collections::{BTreeMap, VecDeque},
    fs, future, io, mem,
    os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, RwLock, Weak,
    },
};
use uuid::Uuid;

const HOTPLUG_INTERVAL: Duration = Duration::from_millis(500);
const FRAME_QUEUE: usize = 32;
pub(crate) const ROUTE_UNAVAILABLE: &str = "Gamepad forwarding is unavailable";
static LAST_DEVICE_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
struct Focus {
    owner: Option<(Uuid, Weak<RwLock<LoginConfigHandler>>)>,
    epoch: u32,
}

impl Focus {
    fn epoch_for(&self, session: &Arc<RwLock<LoginConfigHandler>>) -> u32 {
        if self
            .owner
            .as_ref()
            .is_some_and(|(_, owner)| owner.as_ptr() == Arc::as_ptr(session))
        {
            self.epoch
        } else {
            0
        }
    }
}

lazy_static::lazy_static! {
    static ref FOCUS: watch::Sender<Focus> = watch::Sender::new(Focus::default());
}

pub(crate) fn set_focus(view_id: Uuid, session: Option<&Arc<RwLock<LoginConfigHandler>>>) {
    FOCUS.send_if_modified(|focus| {
        if let Some(session) = session {
            if focus
                .owner
                .as_ref()
                .is_some_and(|(id, owner)| *id == view_id && owner.as_ptr() == Arc::as_ptr(session))
            {
                return false;
            }
            focus.owner = Some((view_id, Arc::downgrade(session)));
        } else if focus.owner.as_ref().is_some_and(|(id, _)| *id == view_id) {
            focus.owner = None;
        } else {
            return false;
        }
        focus.epoch = focus.epoch.wrapping_add(1).max(1);
        true
    });
}

pub(crate) struct Controller {
    outbound: mpsc::Receiver<GamepadChannel>,
    feedback: mpsc::Sender<GamepadChannel>,
    session: Arc<RwLock<LoginConfigHandler>>,
    task: JoinHandle<()>,
}

impl Controller {
    pub(crate) fn start(
        session: Arc<RwLock<LoginConfigHandler>>,
        report: impl Fn(bool, String) + Send + 'static,
    ) -> Self {
        let (tx, outbound) = mpsc::channel(FRAME_QUEUE);
        let (feedback, rx) = mpsc::channel(FRAME_QUEUE);
        let worker_session = session.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = capture(worker_session, tx, rx, report).await {
                log::debug!("Gamepad capture ended: {err:#}");
            }
        });
        Self {
            outbound,
            feedback,
            session,
            task,
        }
    }

    pub(crate) async fn submit(&self, msg: GamepadChannel) -> bool {
        self.feedback.send(msg).await.is_ok()
    }

    fn is_current(&self, frame: &GamepadChannel) -> bool {
        !matches!(frame.union, Some(Union::Frame(_)))
            || FOCUS.borrow().epoch_for(&self.session) == frame.focus_epoch
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn next_outbound(controller: &mut Option<Controller>) -> Option<GamepadChannel> {
    let Some(controller) = controller else {
        return future::pending().await;
    };
    while let Some(msg) = controller.outbound.recv().await {
        if controller.is_current(&msg) {
            return Some(msg);
        }
    }
    None
}

#[derive(Default)]
struct Devices {
    pads: BTreeMap<u32, PhysicalPad>,
    epoch: u32,
    pending: VecDeque<GamepadChannel>,
}

async fn capture(
    session: Arc<RwLock<LoginConfigHandler>>,
    tx: mpsc::Sender<GamepadChannel>,
    mut feedback: mpsc::Receiver<GamepadChannel>,
    report: impl Fn(bool, String),
) -> ResultType<()> {
    let mut devices = Devices::default();
    let mut focus = FOCUS.subscribe();
    let mut scan = time::interval(HOTPLUG_INTERVAL);
    scan.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut started = false;
    loop {
        let epoch = focus.borrow_and_update().epoch_for(&session);
        if devices.epoch != epoch {
            devices.focus_changed(epoch);
            started = true;
        }
        if devices.pending.len() > MAX_DEVICES * 3 + MAX_EVENTS {
            hbb_common::bail!("Gamepad control queue is full");
        }
        tokio::select! {
            _ = focus.changed() => {}
            _ = scan.tick(), if started => devices.scan(),
            reply = feedback.recv() => {
                let Some(reply) = reply else { break };
                if !matches!(reply.union, Some(Union::Rumble(_)))
                    || FOCUS.borrow().epoch_for(&session) == reply.focus_epoch
                {
                    devices.handle_reply(reply, &report);
                }
            }
            permit = tx.reserve(), if !devices.pending.is_empty() => {
                if let Some(msg) = devices.pending.pop_front() {
                    permit?.send(msg);
                }
            }
            (index, read) = next_input(&devices.pads),
                if devices.epoch != 0 && devices.pending.is_empty() => {
                devices.handle_input(index, read);
            }
        }
    }
    Ok(())
}

impl Devices {
    fn focus_changed(&mut self, epoch: u32) {
        self.epoch = epoch;
        self.pending
            .retain(|msg| !matches!(msg.union, Some(Union::Focus(_) | Union::Frame(_))));
        self.pending
            .push_front(channel(0, epoch, Union::Focus(true)));
        let mut disconnected = Vec::new();
        for (&index, pad) in &mut self.pads {
            pad.stop_rumble();
            if epoch != 0 && pad.remote_index.is_some() {
                match pad.snapshot() {
                    Ok(frame) => self.pending.push_back(frame_message(pad.id, epoch, frame)),
                    Err(err) => disconnected.push((index, err)),
                }
            }
        }
        for (index, err) in disconnected {
            self.read_failed(index, err);
        }
    }

    fn scan(&mut self) {
        let connected = match local_devices() {
            Ok(connected) => connected,
            Err(err) => {
                hbb_common::throttled_log!(
                    ERROR_LOG_INTERVAL,
                    debug,
                    "Cannot enumerate local gamepads: {err}"
                );
                return;
            }
        };
        self.pads.retain(|index, pad| {
            if connected.get(index) == Some(&pad.sysfs) {
                return true;
            }
            self.pending
                .push_back(channel(pad.id, 0, Union::Detach(true)));
            false
        });
        for (index, sysfs) in connected {
            if self.pads.contains_key(&index) {
                continue;
            }
            match PhysicalPad::open(sysfs) {
                Ok(Some(pad)) => {
                    self.pending
                        .push_back(channel(pad.id, 0, Union::Attach(pad.desc.clone())));
                    self.pads.insert(index, pad);
                }
                Ok(None) => {}
                Err(err) => hbb_common::throttled_log!(
                    ERROR_LOG_INTERVAL,
                    debug,
                    "Cannot read local gamepad js{index}: {err:#}"
                ),
            }
        }
    }

    fn handle_reply(&mut self, mut reply: GamepadChannel, report: &impl Fn(bool, String)) {
        let Some((&index, pad)) = self
            .pads
            .iter_mut()
            .find(|(_, pad)| pad.id == reply.device_id)
        else {
            return;
        };
        match reply.union.take() {
            Some(Union::Attached(attached)) if reply.focus_epoch == 0 => match attached.union {
                Some(gamepad_attached::Union::JoystickIndex(remote))
                    if pad.remote_index.is_none() =>
                {
                    pad.remote_index = Some(remote);
                    if self.epoch != 0 {
                        match pad.snapshot() {
                            Ok(frame) => self
                                .pending
                                .push_back(frame_message(pad.id, self.epoch, frame)),
                            Err(err) => self.read_failed(index, err),
                        }
                    }
                    let translate = |key: &str| crate::lang::translate(key.to_owned());
                    let local_label = translate("Local Computer");
                    let remote_label = translate("Remote Computer");
                    let mappings: Vec<_> = self
                        .pads
                        .iter()
                        .filter_map(|(&local, pad)| {
                            Some(format!(
                                "{}\n{local_label} /dev/input/js{local} → {remote_label} /dev/input/js{}",
                                pad.desc.name, pad.remote_index?
                            ))
                        })
                        .collect();
                    if !mappings.is_empty() {
                        report(
                            false,
                            format!(
                                "{}\n\n{}",
                                translate("Gamepads mapped"),
                                mappings.join("\n\n")
                            ),
                        );
                    }
                }
                Some(gamepad_attached::Union::Failure(reason)) => {
                    pad.remote_index = None;
                    pad.stop_rumble();
                    hbb_common::throttled_log!(
                        ERROR_LOG_INTERVAL,
                        debug,
                        "Cannot map local gamepad js{index}: {reason:?}"
                    );
                    let label = if reason.enum_value()
                        == Ok(gamepad_attached::Failure::JoydevUnavailable)
                    {
                        "Remote joystick device is unavailable"
                    } else {
                        ROUTE_UNAVAILABLE
                    };
                    let label = crate::lang::translate(label.to_owned());
                    report(
                        true,
                        format!("{label}\n{}\n/dev/input/js{index}", pad.desc.name),
                    );
                }
                _ => {}
            },
            Some(Union::Rumble(effect))
                if pad.remote_index.is_some()
                    && self.epoch != 0
                    && self.epoch == reply.focus_epoch =>
            {
                if let Err(err) = pad.rumble(effect) {
                    hbb_common::throttled_log!(
                        ERROR_LOG_INTERVAL,
                        debug,
                        "Cannot play rumble on local gamepad js{index}: {err}"
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_input(&mut self, index: u32, read: io::Result<Vec<libc::input_event>>) {
        let Some(pad) = self.pads.get_mut(&index) else {
            return;
        };
        if let Err(err) = read.and_then(|events| {
            for event in events {
                if event.type_ == uapi::EV_SYN && event.code == uapi::SYN_DROPPED
                    || pad.frame.len() == MAX_EVENTS
                {
                    let snapshot = pad.snapshot()?;
                    self.pending
                        .push_back(frame_message(pad.id, self.epoch, snapshot));
                    break;
                }
                if event.type_ == uapi::EV_SYN && event.code == uapi::SYN_REPORT {
                    if !pad.frame.is_empty() {
                        let frame = mem::take(&mut pad.frame);
                        self.pending
                            .push_back(frame_message(pad.id, self.epoch, frame));
                    }
                } else if let Some(input) = pad.translate(event) {
                    pad.frame.push(input);
                }
            }
            Ok(())
        }) {
            self.read_failed(index, err);
        }
    }

    fn read_failed(&mut self, index: u32, err: io::Error) {
        if let Some(pad) = self.pads.remove(&index) {
            log::trace!("Local gamepad js{index} closed: {err}");
            self.pending
                .push_back(channel(pad.id, 0, Union::Detach(true)));
        }
    }
}

fn frame_message(id: u32, epoch: u32, events: Vec<GamepadInput>) -> GamepadChannel {
    channel(
        id,
        epoch,
        Union::Frame(GamepadFrame {
            events,
            ..Default::default()
        }),
    )
}

fn local_devices() -> io::Result<BTreeMap<u32, PathBuf>> {
    let entries = match fs::read_dir("/dev/input") {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        other => other?,
    };
    Ok(entries
        .flatten()
        .filter_map(|entry| {
            let index = joystick_index(entry.file_name().to_str()?)?;
            let sysfs = fs::canonicalize(format!("/sys/class/input/js{index}/device")).ok()?;
            if fs::read_to_string(sysfs.join("phys"))
                .is_ok_and(|phys| phys.trim_end() == VIRTUAL_PHYS)
            {
                return None;
            }
            Some((index, sysfs))
        })
        .collect())
}

async fn next_input(
    devices: &BTreeMap<u32, PhysicalPad>,
) -> (u32, io::Result<Vec<libc::input_event>>) {
    let reads: Vec<_> = devices
        .iter()
        .filter(|(_, pad)| pad.remote_index.is_some())
        .map(|(&index, pad)| {
            Box::pin(async move {
                let mut events = [uapi::event(0, 0, 0); 64];
                let read = uapi::read_events(&pad.fd, &mut events).await;
                (index, read.map(|read| events[..read].to_vec()))
            })
        })
        .collect();
    if reads.is_empty() {
        return future::pending().await;
    }
    select_all(reads).await.0
}

struct PhysicalPad {
    id: u32,
    sysfs: PathBuf,
    fd: AsyncFd<Device>,
    desc: GamepadDevice,
    remote_index: Option<u32>,
    frame: Vec<GamepadInput>,
    rumble: Option<mpsc::Sender<GamepadRumble>>,
}

impl PhysicalPad {
    fn open(sysfs: PathBuf) -> ResultType<Option<Self>> {
        let device = Device::open(event_path(&sysfs)?)?;
        if device.physical_path() == Some(VIRTUAL_PHYS) {
            return Ok(None);
        }
        let fd = device.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let desc = describe(&device, flags & libc::O_ACCMODE == libc::O_RDWR)?;
        // Turning forwarding off and on within one connection also needs fresh IDs.
        let id = LAST_DEVICE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
            .context("gamepad IDs exhausted")?
            + 1;
        Ok(Some(Self {
            id,
            sysfs,
            fd: AsyncFd::new(device)?,
            desc,
            remote_index: None,
            frame: Vec::new(),
            rumble: None,
        }))
    }

    fn snapshot(&mut self) -> io::Result<Vec<GamepadInput>> {
        // Discard evdev's backlog from while this window was unfocused.
        let mut ignored = [uapi::event(0, 0, 0); 64];
        loop {
            match uapi::read_available(self.fd.as_raw_fd(), &mut ignored) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        self.frame.clear();
        let device = self.fd.get_ref();
        let mut frame = Vec::with_capacity(self.desc.buttons.len() + self.desc.axes.len());
        if !self.desc.buttons.is_empty() {
            let pressed = device.get_key_state()?;
            frame.extend(self.desc.buttons.iter().map(|&code| {
                input(
                    Kind::Button,
                    code,
                    i32::from(pressed.contains(evdev::Key(code as u16))),
                )
            }));
        }
        if !self.desc.axes.is_empty() {
            let axes = device.get_abs_state()?;
            frame.extend(self.desc.axes.iter().map(|axis| {
                let value = axes[axis.code as usize]
                    .value
                    .clamp(axis.minimum, axis.maximum);
                input(Kind::Axis, axis.code, value)
            }));
        }
        Ok(frame)
    }

    fn translate(&self, event: libc::input_event) -> Option<GamepadInput> {
        let code = u32::from(event.code);
        match event.type_ {
            uapi::EV_KEY
                if self.desc.buttons.binary_search(&code).is_ok()
                    && (0..=1).contains(&event.value) =>
            {
                Some(input(Kind::Button, code, event.value))
            }
            uapi::EV_ABS => {
                let i = self
                    .desc
                    .axes
                    .binary_search_by_key(&code, |axis| axis.code)
                    .ok()?;
                let axis = &self.desc.axes[i];
                Some(input(
                    Kind::Axis,
                    code,
                    event.value.clamp(axis.minimum, axis.maximum),
                ))
            }
            _ => None,
        }
    }

    fn rumble(&mut self, msg: GamepadRumble) -> io::Result<()> {
        let Ok(action) = msg.action.enum_value() else {
            return Ok(());
        };
        if msg.effect_id >= self.desc.rumble_slots {
            return Ok(());
        }
        let sender = match &self.rumble {
            Some(sender) => sender,
            None if action != Action::Play => return Ok(()),
            None => {
                // Use the already-open evdev file so a reused event number cannot reroute feedback.
                let fd = unsafe { BorrowedFd::borrow_raw(self.fd.as_raw_fd()) };
                self.rumble.insert(start_rumble(fd.try_clone_to_owned()?))
            }
        };
        if let Err(err) = sender.try_send(msg) {
            self.stop_rumble();
            return Err(io::Error::other(err));
        }
        Ok(())
    }

    fn stop_rumble(&mut self) {
        self.rumble = None;
    }
}

fn start_rumble(fd: OwnedFd) -> mpsc::Sender<GamepadRumble> {
    let (tx, mut rx) = mpsc::channel(FRAME_QUEUE);
    // EVIOCSFF and EVIOCRMFF can wait for the underlying driver even on a nonblocking fd.
    tokio::task::spawn_blocking(move || {
        let mut device = RumbleDevice {
            fd,
            effects: BTreeMap::new(),
        };
        while let Some(msg) = rx.blocking_recv() {
            if rx.is_closed() {
                break;
            }
            if let Err(err) = device.apply(msg, || !rx.is_closed()) {
                hbb_common::throttled_log!(
                    ERROR_LOG_INTERVAL,
                    debug,
                    "Cannot apply local gamepad rumble: {err}"
                );
            }
        }
    });
    tx
}

struct RumbleDevice {
    fd: OwnedFd,
    effects: BTreeMap<u32, i16>,
}

impl RumbleDevice {
    fn apply(&mut self, msg: GamepadRumble, may_play: impl FnOnce() -> bool) -> io::Result<()> {
        let Ok(action) = msg.action.enum_value() else {
            return Ok(());
        };
        let fd = self.fd.as_raw_fd();
        let existing = self.effects.get(&msg.effect_id).copied();
        match (action, existing) {
            (Action::Stop, Some(id)) => play_effect(fd, id, 0),
            (Action::Erase, Some(id)) => {
                unsafe { uapi::erase_effect(fd, id as _)? };
                self.effects.remove(&msg.effect_id);
                Ok(())
            }
            (Action::Stop | Action::Erase | Action::Update, None) => Ok(()),
            (Action::Play | Action::Update, id) => {
                let repeats = i32::try_from(msg.repeats).map_err(io::Error::other)?;
                if action == Action::Play && repeats == 0 {
                    return Ok(());
                }
                let id = upload_effect(fd, id, &msg)?;
                self.effects.insert(msg.effect_id, id);
                if action == Action::Play && may_play() {
                    play_effect(fd, id, repeats)?;
                }
                Ok(())
            }
        }
    }
}

impl Drop for RumbleDevice {
    fn drop(&mut self) {
        let fd = self.fd.as_raw_fd();
        for &id in self.effects.values() {
            if let Err(err) = unsafe { uapi::erase_effect(fd, id as _) } {
                hbb_common::throttled_log!(
                    ERROR_LOG_INTERVAL,
                    debug,
                    "Cannot erase local gamepad rumble: {err}"
                );
                if let Err(err) = play_effect(fd, id, 0) {
                    log::trace!("Cannot stop local gamepad rumble: {err}");
                }
            }
        }
    }
}

fn event_path(sysfs: &Path) -> io::Result<PathBuf> {
    for entry in fs::read_dir(sysfs)? {
        let entry = entry?;
        let name = entry.file_name();
        if name
            .to_str()
            .and_then(|name| name.strip_prefix("event"))
            .is_some_and(|number| number.parse::<u32>().is_ok())
        {
            return Ok(Path::new("/dev/input").join(name));
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

fn describe(device: &Device, writable: bool) -> ResultType<GamepadDevice> {
    let id = device.input_id();
    let mut desc = GamepadDevice {
        name: device
            .name()
            .unwrap_or("Gamepad")
            .chars()
            .filter(|c| !c.is_control())
            .collect(),
        bus: u32::from(id.bus_type().0),
        vendor: u32::from(id.vendor()),
        product: u32::from(id.product()),
        version: u32::from(id.version()),
        ..Default::default()
    };
    let mut end = desc.name.len().min(libc::UINPUT_MAX_NAME_SIZE - 1);
    while !desc.name.is_char_boundary(end) {
        end -= 1;
    }
    desc.name.truncate(end);
    if desc.name.is_empty() {
        desc.name = "Gamepad".to_owned();
    }
    if let Some(keys) = device.supported_keys() {
        desc.buttons = keys
            .iter()
            .map(|key| u32::from(key.0))
            .filter(|&code| gamepad_button(code))
            .collect();
    }
    if let Some(axes) = device.supported_absolute_axes() {
        let state = device.get_abs_state()?;
        let has_right_stick = axes.contains(evdev::AbsoluteAxisType::ABS_RX)
            && axes.contains(evdev::AbsoluteAxisType::ABS_RY);
        for axis in axes.iter().filter(|axis| gamepad_axis(u32::from(axis.0))) {
            let abs = state[usize::from(axis.0)];
            let code = u32::from(axis.0);
            desc.axes.push(Axis {
                code,
                minimum: abs.minimum,
                maximum: abs.maximum,
                flat: abs.flat,
                resolution: abs.resolution,
                neutral: axis_neutral(code, abs.minimum, abs.maximum, has_right_stick),
                ..Default::default()
            });
        }
    }
    if writable {
        desc.rumble_slots = rumble_slots(device.as_raw_fd());
    }
    if !valid_device(&desc) {
        hbb_common::bail!("unsupported joystick capabilities");
    }
    Ok(desc)
}

fn rumble_slots(fd: RawFd) -> u32 {
    let mut bits = [0u8; 16];
    let rumble = usize::from(uapi::FF_RUMBLE);
    if unsafe { uapi::ff_bits(fd, &mut bits) }.is_err()
        || bits[rumble / 8] & (1 << (rumble % 8)) == 0
    {
        return 0;
    }
    let mut slots = 0;
    if unsafe { uapi::effect_count(fd, &mut slots) }.is_err() {
        return 0;
    }
    slots.clamp(0, MAX_RUMBLE_SLOTS as i32) as u32
}

fn upload_effect(fd: RawFd, existing: Option<i16>, msg: &GamepadRumble) -> io::Result<i16> {
    let mut effect: libc::ff_effect = unsafe { mem::zeroed() };
    effect.type_ = uapi::FF_RUMBLE;
    effect.id = existing.unwrap_or(-1);
    effect.replay.length = u16::try_from(msg.duration_ms).map_err(io::Error::other)?;
    effect.replay.delay = u16::try_from(msg.delay_ms).map_err(io::Error::other)?;
    let rumble = libc::ff_rumble_effect {
        strong_magnitude: u16::try_from(msg.strong).map_err(io::Error::other)?,
        weak_magnitude: u16::try_from(msg.weak).map_err(io::Error::other)?,
    };
    // The Linux UAPI union holds ff_rumble_effect when type_ is FF_RUMBLE.
    unsafe {
        effect
            .u
            .as_mut_ptr()
            .cast::<libc::ff_rumble_effect>()
            .write(rumble);
        uapi::upload_effect(fd, &mut effect)?;
    }
    if effect.id < 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(effect.id)
}

fn play_effect(fd: RawFd, id: i16, count: i32) -> io::Result<()> {
    uapi::write_events(fd, &[uapi::event(uapi::EV_FF, id as u16, count)])
}
