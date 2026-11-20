use hbb_common::ResultType;
use x11rb::{
    connection::Connection,
    protocol::{
        xinput::{ConnectionExt as _, Device, DeviceType, EventMask, GrabOwner, XIEventMask},
        xproto::{Atom, ConnectionExt as _, GrabMode, GrabStatus, Window, MOTION_NOTIFY_EVENT},
        xtest::ConnectionExt as _,
        Event,
    },
    rust_connection::RustConnection,
    CURRENT_TIME, NONE,
};

const RELATIVE_MOTION: u8 = 1;

pub(super) struct LocalInput {
    x11: RustConnection,
    root: Window,
    xtest: Atom,
}

impl LocalInput {
    pub(super) fn grab(display: &str) -> ResultType<Self> {
        // Xorg releases every grab when this connection closes, including after a helper crash.
        let (x11, screen) = x11rb::connect(Some(display))?;
        let root = x11.setup().roots[screen].root;
        let version = x11.xinput_xi_query_version(2, 0)?.reply()?;
        if version.major_version < 2 {
            hbb_common::bail!("XInput 2 is required to block local input");
        }
        let xtest = x11.intern_atom(true, b"XTEST Device")?.reply()?.atom;
        if xtest == NONE {
            hbb_common::bail!("X11 does not identify the XTEST devices used for remote input");
        }
        x11.xinput_xi_select_events(
            root,
            &[EventMask {
                deviceid: Device::ALL.into(),
                mask: vec![XIEventMask::HIERARCHY],
            }],
        )?
        .check()?;
        let input = Self { x11, root, xtest };
        input.grab_devices(true)?;
        Ok(input)
    }

    pub(super) fn refresh(&self) -> ResultType<()> {
        let mut changed = false;
        while let Some(event) = self.x11.poll_for_event()? {
            changed |= matches!(event, Event::XinputHierarchy(_));
        }
        // A grab detaches a device from its master. An attached local device needs
        // grabbing even if the X server did not report a hierarchy change.
        self.grab_devices(changed)
    }

    fn grab_devices(&self, include_floating: bool) -> ResultType<()> {
        let mut grabbed_pointer = false;
        for device in self.x11.xinput_xi_query_device(Device::ALL)?.reply()?.infos {
            let local_device = matches!(
                device.type_,
                DeviceType::SLAVE_POINTER | DeviceType::SLAVE_KEYBOARD
            ) || (include_floating
                && device.type_ == DeviceType::FLOATING_SLAVE);
            if !device.enabled || !local_device {
                continue;
            }
            let id = device.deviceid;
            if self
                .x11
                .xinput_xi_list_properties(id)?
                .reply()?
                .properties
                .contains(&self.xtest)
            {
                continue;
            }
            // Async with an empty event mask discards input instead of replaying it on release.
            let grab = self
                .x11
                .xinput_xi_grab_device(
                    self.root,
                    CURRENT_TIME,
                    NONE,
                    id,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    GrabOwner::NO_OWNER,
                    &[],
                )?
                .reply()?;
            if grab.status != GrabStatus::SUCCESS {
                hbb_common::bail!(
                    "Could not block local input device {} ({id}): {:?}",
                    String::from_utf8_lossy(&device.name),
                    grab.status
                );
            }
            grabbed_pointer |= device.type_ == DeviceType::SLAVE_POINTER;
        }
        if grabbed_pointer {
            // Older libxdo uses XWarpPointer, which Xorg routes through the last
            // active device even if it was detached. Select XTEST without moving.
            self.x11
                .xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    RELATIVE_MOTION,
                    CURRENT_TIME,
                    NONE,
                    0,
                    0,
                    0,
                )?
                .check()?;
        }
        Ok(())
    }
}
