use super::{
    channel, gamepad_axis, gamepad_button, joystick_index, uapi, valid_device, ERROR_LOG_INTERVAL,
    MAX_DEVICES, MAX_EVENTS, VIRTUAL_PHYS,
};
use base::message_proto::{
    gamepad_attached::{self, Failure},
    gamepad_channel::Union,
    gamepad_input::Kind,
    gamepad_rumble::Action,
    GamepadAttached, GamepadChannel, GamepadDevice, GamepadFrame, GamepadRumble,
};
use hbb_common::{
    anyhow::Context,
    futures::{future::select_all, SinkExt, StreamExt},
    libc, log,
    protobuf::Message as _,
    tokio::{
        self,
        io::{unix::AsyncFd, AsyncRead, AsyncWrite},
        sync::mpsc,
        task::JoinHandle,
        time::{self, Duration, Instant},
    },
    ResultType,
};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CStr, CString},
    fs, future, io, mem,
    os::fd::AsRawFd,
};

const IPC_PACKET_LIMIT: usize = 16 * 1024;
const QUEUE_CAPACITY: usize = 64;
const BACKLOG_CAPACITY: usize = 512;
const BACKEND_WAIT: Duration = Duration::from_secs(3);
const BACKEND_STOPPED: &str = "the gamepad backend stopped";
pub(crate) const IPC_POSTFIX: &str = "_uinput_gamepad";

pub(crate) struct Session {
    commands: mpsc::Sender<GamepadChannel>,
    replies: mpsc::Receiver<GamepadChannel>,
    backlog: VecDeque<(GamepadChannel, Instant)>,
    task: JoinHandle<()>,
}

impl Session {
    pub(crate) fn start() -> Self {
        let (commands, rx) = mpsc::channel(QUEUE_CAPACITY);
        let (tx, replies) = mpsc::channel(QUEUE_CAPACITY);
        let task = tokio::spawn(async move {
            let result = match uapi::open_uinput() {
                Ok(_) => serve(Link::Direct(rx, tx)).await,
                Err(open_error) => match crate::ipc::connect(1000, IPC_POSTFIX).await {
                    Ok(mut ipc) => {
                        ipc.set_max_packet_length(IPC_PACKET_LIMIT);
                        proxy(ipc, rx, tx).await
                    }
                    Err(ipc_error) => Err(open_error).context(format!(
                        "cannot connect to the gamepad service: {ipc_error}"
                    )),
                },
            };
            if let Err(err) = result {
                hbb_common::throttled_log!(
                    ERROR_LOG_INTERVAL,
                    debug,
                    "Gamepad forwarding stopped: {err:#}"
                );
            }
        });
        Self {
            commands,
            replies,
            backlog: VecDeque::new(),
            task,
        }
    }

    pub(crate) fn submit(&mut self, msg: GamepadChannel) -> ResultType<()> {
        let msg = if self.backlog.is_empty() {
            match self.commands.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(msg)) => msg,
                Err(mpsc::error::TrySendError::Closed(_)) => hbb_common::bail!(BACKEND_STOPPED),
            }
        } else {
            msg
        };
        if self.backlog.len() >= BACKLOG_CAPACITY {
            hbb_common::bail!("Gamepad command backlog is full");
        }
        self.backlog.push_back((msg, Instant::now() + BACKEND_WAIT));
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn next_event(
    session: &mut Option<Session>,
) -> ResultType<Option<GamepadChannel>> {
    let Some(session) = session else {
        return future::pending().await;
    };
    let Some((_, deadline)) = session.backlog.front() else {
        return session
            .replies
            .recv()
            .await
            .map(Some)
            .context(BACKEND_STOPPED);
    };
    // Dispatch one command at a time so permission updates can take effect while the backend is busy.
    tokio::select! {
        permit = time::timeout_at(*deadline, session.commands.reserve()) => {
            let permit = permit.context("timed out waiting for the gamepad backend")?.context(BACKEND_STOPPED)?;
            if let Some((msg, _)) = session.backlog.pop_front() {
                permit.send(msg);
            }
            Ok(None)
        }
        reply = session.replies.recv() => reply.map(Some).context(BACKEND_STOPPED),
    }
}

pub(crate) fn from_controller(msg: &GamepadChannel) -> bool {
    let valid = match msg.union.as_ref() {
        Some(Union::Focus(true)) => msg.device_id == 0,
        Some(Union::Close(true)) => msg.device_id == 0 && msg.focus_epoch == 0,
        Some(Union::Attach(desc)) => {
            msg.device_id != 0 && msg.focus_epoch == 0 && valid_device(desc)
        }
        Some(Union::Detach(true)) => msg.device_id != 0 && msg.focus_epoch == 0,
        Some(Union::Frame(frame)) => {
            msg.device_id != 0
                && msg.focus_epoch != 0
                && (1..=MAX_EVENTS).contains(&frame.events.len())
                && frame
                    .events
                    .iter()
                    .all(|event| match event.kind.enum_value() {
                        Ok(Kind::Button) => {
                            (0..=1).contains(&event.value) && gamepad_button(event.code)
                        }
                        Ok(Kind::Axis) => gamepad_axis(event.code),
                        Err(_) => false,
                    })
        }
        _ => false,
    };
    valid && msg.compute_size() <= IPC_PACKET_LIMIT as u64
}

pub(crate) fn spawn_service(mut conn: crate::ipc::Connection) {
    conn.set_max_packet_length(IPC_PACKET_LIMIT);
    tokio::spawn(async move {
        if let Err(err) = serve(Link::Service(conn)).await {
            log::trace!("Gamepad IPC ended: {err:#}");
        }
    });
}

enum Link {
    Direct(mpsc::Receiver<GamepadChannel>, mpsc::Sender<GamepadChannel>),
    Service(crate::ipc::Connection),
}

impl Link {
    async fn recv(&mut self) -> ResultType<Option<GamepadChannel>> {
        match self {
            Self::Direct(rx, _) => Ok(rx.recv().await),
            Self::Service(conn) => Ok(Some(read_ipc(conn).await?)),
        }
    }

    async fn send(&mut self, msg: GamepadChannel) -> ResultType<()> {
        match self {
            Self::Direct(_, tx) => Ok(tx.send(msg).await?),
            Self::Service(conn) => write_ipc(conn, &msg).await,
        }
    }
}

async fn read_ipc<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut crate::ipc::ConnectionTmpl<T>,
) -> ResultType<GamepadChannel> {
    let bytes = conn.next_raw().await?;
    Ok(GamepadChannel::parse_from_bytes(&bytes)?)
}

async fn write_ipc<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut crate::ipc::ConnectionTmpl<T>,
    msg: &GamepadChannel,
) -> ResultType<()> {
    conn.send_raw(msg.write_to_bytes()?.into()).await
}

async fn proxy<T: AsyncRead + AsyncWrite + Unpin>(
    conn: crate::ipc::ConnectionTmpl<T>,
    mut commands: mpsc::Receiver<GamepadChannel>,
    replies: mpsc::Sender<GamepadChannel>,
) -> ResultType<()> {
    let (mut write, mut read) = conn.into_framed().split();
    let send_commands = async {
        while let Some(msg) = commands.recv().await {
            write.send(msg.write_to_bytes()?.into()).await?;
        }
        Ok(())
    };
    let receive_replies = async {
        while let Some(bytes) = read.next().await {
            replies
                .send(GamepadChannel::parse_from_bytes(&bytes?)?)
                .await?;
        }
        hbb_common::bail!("the gamepad service closed its IPC connection");
    };
    tokio::select! {
        result = send_commands => result,
        result = receive_replies => result,
    }
}

async fn serve(mut link: Link) -> ResultType<()> {
    let mut devices: BTreeMap<u32, VirtualPad> = BTreeMap::new();
    let mut focus_epoch = 0;
    loop {
        tokio::select! {
            command = link.recv() => {
                let Some(mut command) = command? else { break };
                if !from_controller(&command) {
                    continue;
                }
                let id = command.device_id;
                match command.union.take() {
                    Some(Union::Focus(_)) if focus_epoch != command.focus_epoch => {
                        if focus_epoch != 0 {
                            // Closing a device also releases its state if the neutral frame fails.
                            devices.retain(|id, pad| match pad.neutralize() {
                                Ok(()) => true,
                                Err(err) => {
                                    hbb_common::throttled_log!(ERROR_LOG_INTERVAL, debug, "Cannot release virtual gamepad {id}: {err}");
                                    false
                                }
                            });
                        }
                        focus_epoch = command.focus_epoch;
                    }
                    Some(Union::Attach(desc)) if !devices.contains_key(&id) => {
                        let status = if devices.len() >= MAX_DEVICES {
                            gamepad_attached::Union::Failure(Failure::InvalidDevice.into())
                        } else {
                            match VirtualPad::open(desc).await {
                                Ok((pad, index)) => {
                                    devices.insert(id, pad);
                                    gamepad_attached::Union::JoystickIndex(index)
                                }
                                Err(failure) => gamepad_attached::Union::Failure(failure.into()),
                            }
                        };
                        link.send(channel(id, 0, Union::Attached(GamepadAttached {
                            union: Some(status),
                            ..Default::default()
                        }))).await?;
                    }
                    Some(Union::Detach(_)) => {
                        devices.remove(&id);
                    }
                    Some(Union::Close(_)) => break,
                    Some(Union::Frame(frame)) if focus_epoch == command.focus_epoch => {
                        if let Some(pad) = devices.get(&id) {
                            if let Some(events) = input_events(&pad.desc, frame) {
                                if let Err(err) = uapi::write_events(pad.fd.as_raw_fd(), &events) {
                                    hbb_common::throttled_log!(ERROR_LOG_INTERVAL, debug, "Virtual gamepad {id} closed: {err}");
                                    devices.remove(&id);
                                    link.send(device_failed(id)).await?;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            (id, event) = next_feedback(&devices) => {
                let Some(pad) = devices.get_mut(&id) else { continue };
                let event = match event {
                    Ok(event) => event,
                    Err(err) => {
                        hbb_common::throttled_log!(ERROR_LOG_INTERVAL, debug, "Virtual gamepad {id} feedback closed: {err}");
                        devices.remove(&id);
                        link.send(device_failed(id)).await?;
                        continue;
                    }
                };
                match pad.feedback(event) {
                    Ok(Some(effect)) if focus_epoch != 0 => {
                        link.send(channel(id, focus_epoch, Union::Rumble(effect))).await?;
                    }
                    Ok(_) => {}
                    Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {
                        log::trace!("Ignoring expired or invalid rumble request for virtual gamepad {id}");
                    }
                    Err(err) => {
                        hbb_common::throttled_log!(ERROR_LOG_INTERVAL, debug, "Cannot respond to virtual gamepad {id} rumble request: {err}");
                    }
                }
            }
        }
    }
    Ok(())
}

fn device_failed(id: u32) -> GamepadChannel {
    channel(
        id,
        0,
        Union::Attached(GamepadAttached {
            union: Some(gamepad_attached::Union::Failure(
                Failure::BackendUnavailable.into(),
            )),
            ..Default::default()
        }),
    )
}

fn input_events(desc: &GamepadDevice, frame: GamepadFrame) -> Option<Vec<libc::input_event>> {
    let mut events = Vec::with_capacity(frame.events.len() + 1);
    for ev in frame.events {
        let type_ = match ev.kind.enum_value().ok()? {
            Kind::Button
                if (0..=1).contains(&ev.value) && desc.buttons.binary_search(&ev.code).is_ok() =>
            {
                uapi::EV_KEY
            }
            Kind::Axis => {
                let axis = &desc.axes[desc.axes.binary_search_by_key(&ev.code, |a| a.code).ok()?];
                if !(axis.minimum..=axis.maximum).contains(&ev.value) {
                    return None;
                }
                uapi::EV_ABS
            }
            _ => return None,
        };
        events.push(uapi::event(type_, ev.code as u16, ev.value));
    }
    events.push(uapi::event(uapi::EV_SYN, uapi::SYN_REPORT, 0));
    Some(events)
}

async fn next_feedback(
    devices: &BTreeMap<u32, VirtualPad>,
) -> (u32, io::Result<libc::input_event>) {
    let reads: Vec<_> = devices
        .iter()
        .filter(|(_, pad)| !pad.effects.is_empty())
        .map(|(&id, pad)| Box::pin(async move { (id, pad.read_feedback().await) }))
        .collect();
    if reads.is_empty() {
        return future::pending().await;
    }
    select_all(reads).await.0
}

struct VirtualPad {
    fd: AsyncFd<fs::File>,
    desc: GamepadDevice,
    effects: Vec<Option<GamepadRumble>>,
}

impl VirtualPad {
    async fn open(desc: GamepadDevice) -> Result<(Self, u32), Failure> {
        let (fd, directory) = Self::register(&desc).map_err(|err| {
            hbb_common::throttled_log!(
                ERROR_LOG_INTERVAL,
                debug,
                "Cannot create a virtual gamepad: {err:#}"
            );
            Failure::BackendUnavailable
        })?;
        let deadline = Instant::now() + Duration::from_secs(1);
        let index = loop {
            if let Ok(entries) = fs::read_dir(&directory) {
                if let Some(index) = entries.flatten().find_map(|entry| {
                    let index = joystick_index(entry.file_name().to_str()?)?;
                    fs::metadata(format!("/dev/input/js{index}")).ok()?;
                    Some(index)
                }) {
                    break index;
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::JoydevUnavailable);
            }
            time::sleep(Duration::from_millis(20)).await;
        };
        Ok((
            Self {
                effects: vec![None; desc.rumble_slots as usize],
                fd,
                desc,
            },
            index,
        ))
    }

    fn register(desc: &GamepadDevice) -> ResultType<(AsyncFd<fs::File>, String)> {
        let file = uapi::open_uinput()?;
        let fd = file.as_raw_fd();
        let mut setup: libc::uinput_setup = unsafe { mem::zeroed() };
        setup.id = libc::input_id {
            bustype: desc.bus as u16,
            vendor: desc.vendor as u16,
            product: desc.product as u16,
            version: desc.version as u16,
        };
        setup.ff_effects_max = desc.rumble_slots;
        for (to, from) in setup.name.iter_mut().zip(desc.name.bytes()) {
            *to = from as _;
        }
        let phys = CString::new(VIRTUAL_PHYS)?;
        // Only capabilities checked by valid_device and fixed kernel ABI structs reach ioctl.
        unsafe {
            uapi::set_phys(fd, phys.as_ptr())?;
            if !desc.buttons.is_empty() {
                uapi::set_evbit(fd, uapi::EV_KEY as _)?;
                for &button in &desc.buttons {
                    uapi::set_keybit(fd, button as _)?;
                }
            }
            if !desc.axes.is_empty() {
                uapi::set_evbit(fd, uapi::EV_ABS as _)?;
                for axis in &desc.axes {
                    let mut abs: libc::uinput_abs_setup = mem::zeroed();
                    abs.code = axis.code as u16;
                    abs.absinfo.value = axis.neutral;
                    abs.absinfo.minimum = axis.minimum;
                    abs.absinfo.maximum = axis.maximum;
                    // evdev has already applied the source device's fuzz filter.
                    abs.absinfo.flat = axis.flat;
                    abs.absinfo.resolution = axis.resolution;
                    uapi::setup_abs(fd, &abs)?;
                }
            }
            if desc.rumble_slots > 0 {
                uapi::set_evbit(fd, uapi::EV_FF as _)?;
                uapi::set_ffbit(fd, uapi::FF_RUMBLE as _)?;
            }
            uapi::setup(fd, &setup)?;
            uapi::create(fd)?;
        }
        let mut name = [0; 32];
        unsafe { uapi::sysname(fd, &mut name)? };
        let name = CStr::from_bytes_until_nul(&name)?.to_str()?;
        Ok((AsyncFd::new(file)?, format!("/sys/class/input/{name}")))
    }

    fn neutralize(&self) -> io::Result<()> {
        let mut events: Vec<_> = self
            .desc
            .buttons
            .iter()
            .map(|&code| uapi::event(uapi::EV_KEY, code as u16, 0))
            .chain(
                self.desc
                    .axes
                    .iter()
                    .map(|axis| uapi::event(uapi::EV_ABS, axis.code as u16, axis.neutral)),
            )
            .collect();
        events.push(uapi::event(uapi::EV_SYN, uapi::SYN_REPORT, 0));
        uapi::write_events(self.fd.as_raw_fd(), &events)
    }

    async fn read_feedback(&self) -> io::Result<libc::input_event> {
        let mut events = [uapi::event(0, 0, 0)];
        uapi::read_events(&self.fd, &mut events).await?;
        Ok(events[0])
    }

    fn feedback(&mut self, event: libc::input_event) -> io::Result<Option<GamepadRumble>> {
        let fd = self.fd.as_raw_fd();
        match (event.type_, event.code) {
            (uapi::EV_UINPUT, uapi::UI_FF_UPLOAD) => {
                let mut upload: libc::uinput_ff_upload = unsafe { mem::zeroed() };
                upload.request_id = event.value as u32;
                unsafe { uapi::begin_ff_upload(fd, &mut upload)? };
                let slot = self
                    .effects
                    .get_mut(upload.effect.id as usize)
                    .filter(|_| upload.effect.type_ == uapi::FF_RUMBLE);
                upload.retval = if slot.is_some() { 0 } else { -libc::EINVAL };
                unsafe { uapi::end_ff_upload(fd, &upload)? };
                Ok(slot.and_then(|slot| {
                    let effect = uploaded_rumble(&upload.effect);
                    slot.replace(effect.clone()).map(|_| effect)
                }))
            }
            (uapi::EV_UINPUT, uapi::UI_FF_ERASE) => {
                let mut erase: libc::uinput_ff_erase = unsafe { mem::zeroed() };
                erase.request_id = event.value as u32;
                unsafe { uapi::begin_ff_erase(fd, &mut erase)? };
                unsafe { uapi::end_ff_erase(fd, &erase)? };
                Ok(self
                    .effects
                    .get_mut(erase.effect_id as usize)
                    .and_then(Option::take)
                    .map(|mut effect| {
                        effect.action = Action::Erase.into();
                        effect
                    }))
            }
            (uapi::EV_FF, code) => Ok(rumble_playback(&self.effects, code, event.value)),
            _ => Ok(None),
        }
    }
}

fn uploaded_rumble(effect: &libc::ff_effect) -> GamepadRumble {
    // FF_RUMBLE selects the ff_rumble_effect member of the Linux UAPI union.
    let rumble = unsafe { effect.u.as_ptr().cast::<libc::ff_rumble_effect>().read() };
    GamepadRumble {
        action: Action::Update.into(),
        effect_id: effect.id as u32,
        strong: u32::from(rumble.strong_magnitude),
        weak: u32::from(rumble.weak_magnitude),
        duration_ms: u32::from(effect.replay.length),
        delay_ms: u32::from(effect.replay.delay),
        ..Default::default()
    }
}

fn rumble_playback(
    effects: &[Option<GamepadRumble>],
    slot: u16,
    count: i32,
) -> Option<GamepadRumble> {
    let repeats = u32::try_from(count).ok()?;
    let mut effect = effects.get(usize::from(slot))?.as_ref()?.clone();
    effect.action = if repeats == 0 {
        Action::Stop
    } else {
        Action::Play
    }
    .into();
    effect.repeats = repeats;
    Some(effect)
}

#[cfg(test)]
mod tests {
    use super::{super::input, *};
    use base::message_proto::gamepad_device::Axis;

    #[tokio::test]
    async fn gamepad_queue_pressure_preserves_button_release_and_backend_reply() {
        let frame = |value| {
            channel(
                1,
                1,
                Union::Frame(GamepadFrame {
                    events: vec![input(Kind::Button, 0x130, value)],
                    ..Default::default()
                }),
            )
        };
        let pressed = frame(1);
        let released = frame(0);
        let attached = channel(
            1,
            0,
            Union::Attached(GamepadAttached {
                union: Some(gamepad_attached::Union::JoystickIndex(0)),
                ..Default::default()
            }),
        );
        let (commands, mut backend) = mpsc::channel(1);
        let (reply, replies) = mpsc::channel(1);
        commands.send(pressed.clone()).await.unwrap();
        reply.send(attached.clone()).await.unwrap();
        let mut session = Some(Session {
            commands,
            replies,
            backlog: VecDeque::new(),
            task: tokio::spawn(future::pending()),
        });
        session.as_mut().unwrap().submit(released.clone()).unwrap();
        assert_eq!(next_event(&mut session).await.unwrap(), Some(attached));
        assert_eq!(backend.recv().await, Some(pressed.clone()));
        // New commands must queue behind the deferred release even with room in the backend.
        session.as_mut().unwrap().submit(pressed.clone()).unwrap();
        assert_eq!(next_event(&mut session).await.unwrap(), None);
        assert_eq!(backend.recv().await, Some(released));
        assert_eq!(next_event(&mut session).await.unwrap(), None);
        assert_eq!(backend.recv().await, Some(pressed));
    }

    #[tokio::test]
    async fn gamepad_permission_revocation_discards_the_backlog() {
        let (commands, mut backend) = mpsc::channel(1);
        let (_reply_holder, replies) = mpsc::channel(1);
        let focus = channel(0, 1, Union::Focus(true));
        commands.send(focus.clone()).await.unwrap();
        let mut session = Some(Session {
            commands,
            replies,
            backlog: VecDeque::new(),
            task: tokio::spawn(future::pending()),
        });
        session
            .as_mut()
            .unwrap()
            .submit(channel(1, 0, Union::Detach(true)))
            .unwrap();

        let (revoke, mut revoked) = mpsc::channel(1);
        revoke.send(()).await.unwrap();
        tokio::select! {
            biased;
            event = next_event(&mut session) => panic!("unexpected gamepad event: {event:?}"),
            Some(()) = revoked.recv() => { session = None; }
        }
        assert!(session.is_none());
        assert_eq!(backend.recv().await, Some(focus));
        assert_eq!(backend.recv().await, None);
    }

    #[test]
    fn gamepad_frames_preserve_analog_values_and_only_address_declared_controls() {
        let pad = GamepadDevice {
            buttons: vec![0x130],
            axes: vec![Axis {
                code: 0x09,
                minimum: 0,
                maximum: 65535,
                ..Default::default()
            }],
            ..Default::default()
        };
        let frame = GamepadFrame {
            events: vec![
                input(Kind::Button, 0x130, 1),
                input(Kind::Axis, 0x09, 40961),
            ],
            ..Default::default()
        };
        let events = input_events(&pad, frame.clone()).unwrap();
        assert_eq!(
            (events[0].type_, events[0].code, events[0].value),
            (uapi::EV_KEY, 0x130, 1)
        );
        assert_eq!(
            (events[1].type_, events[1].code, events[1].value),
            (uapi::EV_ABS, 0x09, 40961)
        );
        assert_eq!(
            (events[2].type_, events[2].code),
            (uapi::EV_SYN, uapi::SYN_REPORT)
        );
        let other_pad = GamepadDevice {
            buttons: vec![0x131],
            ..pad
        };
        assert!(input_events(&other_pad, frame).is_none());
    }

    #[test]
    fn gamepad_rumble_slots_keep_each_pads_motor_values_and_stop_independently() {
        let mut upload: libc::ff_effect = unsafe { mem::zeroed() };
        upload.type_ = uapi::FF_RUMBLE;
        upload.replay.length = 300;
        upload.replay.delay = 40;
        let mut effect = libc::ff_rumble_effect {
            strong_magnitude: 100,
            weak_magnitude: 50000,
        };
        unsafe {
            upload
                .u
                .as_mut_ptr()
                .cast::<libc::ff_rumble_effect>()
                .write(effect)
        };
        let first = [Some(uploaded_rumble(&upload))];
        effect.strong_magnitude = 42000;
        effect.weak_magnitude = 200;
        unsafe {
            upload
                .u
                .as_mut_ptr()
                .cast::<libc::ff_rumble_effect>()
                .write(effect)
        };
        let second = [Some(uploaded_rumble(&upload))];

        let play = rumble_playback(&first, 0, 2).unwrap();
        assert_eq!(play.action.enum_value(), Ok(Action::Play));
        assert_eq!((play.strong, play.weak), (100, 50000));
        assert_eq!(
            (play.duration_ms, play.delay_ms, play.repeats),
            (300, 40, 2)
        );
        let stop = rumble_playback(&second, 0, 0).unwrap();
        assert_eq!(stop.action.enum_value(), Ok(Action::Stop));
        assert_eq!((stop.strong, stop.weak), (42000, 200));
        assert!(rumble_playback(&second, 1, 1).is_none());
    }
}
