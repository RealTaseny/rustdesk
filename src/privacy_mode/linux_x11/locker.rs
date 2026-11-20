use dbus::{
    blocking::{stdintf::org_freedesktop_dbus::Properties, Connection as Bus},
    Message, MessageType, Path,
};
use gtk::gio::{self, prelude::SettingsExt};
use hbb_common::ResultType;
use std::time::Duration;
use x11rb::{
    protocol::{
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xproto::{ConnectionExt as _, EventMask, GrabMode, GrabStatus},
    },
    rust_connection::RustConnection,
    CURRENT_TIME, NONE,
};

const BUS_TIMEOUT: Duration = Duration::from_secs(1);
const GNOME: &str = "org.gnome.Shell.ScreenShield";
const BUS: &str = "org.freedesktop.DBus";

pub(super) struct Locker {
    bus: Bus,
    owner: String,
    path: &'static str,
    interface: &'static str,
    gnome_pid: Option<u32>,
}

pub(super) struct LockRequest {
    serial: Option<u32>,
    replied: bool,
}

impl Locker {
    pub(super) fn discover(system_bus: &Bus, uid: u32, x11: &RustConnection) -> ResultType<Self> {
        let bus = Bus::new_session()?;
        let daemon = bus.with_proxy(BUS, "/org/freedesktop/DBus", BUS_TIMEOUT);
        for (name, path, interface) in [
            (GNOME, "/org/gnome/ScreenSaver", "org.gnome.ScreenSaver"),
            (
                "org.kde.screensaver",
                "/org/freedesktop/ScreenSaver",
                "org.freedesktop.ScreenSaver",
            ),
        ] {
            let (has_owner,): (bool,) = daemon.method_call(BUS, "NameHasOwner", (name,))?;
            if !has_owner {
                continue;
            }
            let (owner,): (String,) = daemon.method_call(BUS, "GetNameOwner", (name,))?;
            let (owner_uid,): (u32,) =
                daemon.method_call(BUS, "GetConnectionUnixUser", (&owner,))?;
            if owner_uid != uid {
                hbb_common::bail!("The screen locker is owned by a different desktop user");
            }
            let (pid,): (u32,) =
                daemon.method_call(BUS, "GetConnectionUnixProcessID", (&owner,))?;
            if !has_x11_client(x11, NONE, pid)? {
                hbb_common::bail!("The screen locker does not belong to this X11 server");
            }
            let proxy = bus.with_proxy(&owner, path, BUS_TIMEOUT);
            let (xml,): (String,) =
                proxy.method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())?;
            if !xml.contains("<method name=\"Lock\"") && !xml.contains("<method name='Lock'") {
                hbb_common::bail!("The desktop screen locker does not expose Lock");
            }
            proxy.method_call::<(bool,), _, _, _>(interface, "GetActive", ())?;
            let gnome_pid = if name == GNOME {
                check_gnome_lock_policy(system_bus, uid)?;
                Some(pid)
            } else {
                None
            };
            return Ok(Self {
                bus,
                owner,
                path,
                interface,
                gnome_pid,
            });
        }
        hbb_common::bail!("No supported screen locker is running on the desktop session bus");
    }

    pub(super) fn request(
        &self,
        x11: &RustConnection,
        locked_hint: Option<bool>,
        lock_via_logind: impl FnOnce() -> ResultType<()>,
    ) -> ResultType<LockRequest> {
        if let (Some(pid), Some(hint)) = (self.gnome_pid, locked_hint) {
            let active = self.active()?;
            // Only use the non-animated logind request from a corroborated state.
            // During unlock GNOME can report active until its modal is already gone.
            if (!hint && !active) || (hint && active && gnome_has_lock_grabs(x11, pid)?) {
                lock_via_logind()?;
                return Ok(LockRequest {
                    serial: None,
                    replied: false,
                });
            }
        }
        let message = Message::new_method_call(&self.owner, self.path, self.interface, "Lock")
            .map_err(hbb_common::anyhow::Error::msg)?;
        let serial =
            self.bus.channel().send(message).map_err(|_| {
                hbb_common::anyhow::anyhow!("Could not send the desktop Lock request")
            })?;
        Ok(LockRequest {
            serial: Some(serial),
            replied: false,
        })
    }

    pub(super) fn confirmed(
        &self,
        request: &mut LockRequest,
        x11: &RustConnection,
        locked_hint: bool,
    ) -> ResultType<bool> {
        let channel = self.bus.channel();
        channel
            .read_write(Some(Duration::ZERO))
            .map_err(|_| hbb_common::anyhow::anyhow!("The desktop session bus disconnected"))?;
        while let Some(mut message) = channel.pop_message() {
            if request.serial.is_some() && message.get_reply_serial() == request.serial {
                message.as_result()?;
                request.replied = message.msg_type() == MessageType::MethodReturn
                    && message.sender().as_deref() == Some(self.owner.as_str());
            }
        }
        let active = self.active()?;
        if !locked_hint || !active {
            return Ok(false);
        }
        if request.replied {
            return Ok(true);
        }
        if let (None, Some(pid)) = (request.serial, self.gnome_pid) {
            return gnome_has_lock_grabs(x11, pid);
        }
        Ok(false)
    }

    fn active(&self) -> ResultType<bool> {
        let (active,): (bool,) = self
            .bus
            .with_proxy(&self.owner, self.path, BUS_TIMEOUT)
            .method_call(self.interface, "GetActive", ())?;
        Ok(active)
    }
}

fn check_gnome_lock_policy(system_bus: &Bus, uid: u32) -> ResultType<()> {
    let schema = gio::SettingsSchemaSource::default()
        .and_then(|source| source.lookup("org.gnome.desktop.lockdown", true));
    if let Some(schema) = schema {
        let settings = gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None);
        if settings.boolean("disable-lock-screen") {
            hbb_common::bail!("GNOME screen locking is disabled by policy");
        }
    }
    let accounts_service = "org.freedesktop.Accounts";
    let (user,): (Path<'static>,) = system_bus
        .with_proxy(accounts_service, "/org/freedesktop/Accounts", BUS_TIMEOUT)
        .method_call(accounts_service, "FindUserById", (i64::from(uid),))?;
    let password_mode: i32 = system_bus
        .with_proxy(accounts_service, user, BUS_TIMEOUT)
        .get("org.freedesktop.Accounts.User", "PasswordMode")?;
    // libaccountsservice uses 2 for accounts without a password prompt.
    if password_mode == 2 {
        hbb_common::bail!("The GNOME session has no password-protected lock screen");
    }
    Ok(())
}

fn has_x11_client(x11: &RustConnection, client: u32, pid: u32) -> ResultType<bool> {
    let ids = x11
        .res_query_client_ids(&[ClientIdSpec {
            client,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        }])?
        .reply()?;
    Ok(ids.ids.iter().any(|id| id.value.as_slice() == [pid]))
}

fn gnome_has_lock_grabs(x11: &RustConnection, pid: u32) -> ResultType<bool> {
    let focus = x11.get_input_focus()?.reply()?.focus;
    if focus <= 1 || !has_x11_client(x11, focus, pid)? {
        return Ok(false);
    }
    // GNOME releases the master grabs before animating the screen away while the
    // D-Bus active state and logind hint can both still describe the previous lock.
    let keyboard = x11
        .grab_keyboard(false, focus, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
        .reply()?;
    if keyboard.status == GrabStatus::SUCCESS {
        x11.ungrab_keyboard(CURRENT_TIME)?.check()?;
    }
    if keyboard.status != GrabStatus::ALREADY_GRABBED {
        return Ok(false);
    }
    let pointer = x11
        .grab_pointer(
            false,
            focus,
            EventMask::NO_EVENT,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            NONE,
            NONE,
            CURRENT_TIME,
        )?
        .reply()?;
    if pointer.status == GrabStatus::SUCCESS {
        x11.ungrab_pointer(CURRENT_TIME)?.check()?;
    }
    Ok(pointer.status == GrabStatus::ALREADY_GRABBED)
}
