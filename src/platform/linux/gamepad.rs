use base::message_proto::{gamepad_channel::Union, GamepadChannel, GamepadDevice, Message};
#[cfg(any(feature = "flutter", test))]
use base::message_proto::{gamepad_input::Kind, GamepadInput};

#[cfg(feature = "flutter")]
pub(crate) mod client;
pub(crate) mod host;
mod uapi;

pub(crate) const CAPABILITY: &str = "gamepad_forwarding";
pub(crate) const ERROR_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_DEVICES: usize = 1024;
const MAX_EVENTS: usize = 128;
const MAX_RUMBLE_SLOTS: u32 = 64;
const VIRTUAL_PHYS: &str = "rustdesk/gamepad";

fn channel(device_id: u32, focus_epoch: u32, union: Union) -> GamepadChannel {
    GamepadChannel {
        device_id,
        focus_epoch,
        union: Some(union),
        ..Default::default()
    }
}

pub(crate) fn message(channel: GamepadChannel) -> Message {
    let mut msg = Message::new();
    msg.set_gamepad_channel(channel);
    msg
}

fn joystick_index(name: &str) -> Option<u32> {
    name.strip_prefix("js")?.parse().ok()
}

fn gamepad_button(code: u32) -> bool {
    // BTN_0..BTN_9, joystick/gamepad, wheel gears, D-pad, grips and extra buttons.
    matches!(code, 0x100..=0x109 | 0x120..=0x13e | 0x150..=0x151 | 0x220..=0x227 | 0x2c0..=0x2e7)
}

fn gamepad_axis(code: u32) -> bool {
    // Multitouch axes and mouse/keyboard capabilities must never reach uinput.
    matches!(code, 0x00..=0x0a | 0x10..=0x1b | 0x28)
}

#[cfg(any(feature = "flutter", test))]
fn axis_neutral(code: u32, min: i32, max: i32, has_right_stick: bool) -> i32 {
    // ABS_HAT1 and ABS_HAT2 can also represent directional hats with signed ranges.
    let hat_trigger = min >= 0 && matches!(code, 0x12..=0x15);
    // ABS_Z/RZ are triggers on controllers with ABS_RX/RY for the right stick.
    if hat_trigger || matches!(code, 6 | 9 | 10) || has_right_stick && matches!(code, 2 | 5) {
        min
    } else if min < 0 && max > 0 {
        0
    } else {
        ((i64::from(min) + i64::from(max) + 1) / 2) as i32
    }
}

fn valid_device(device: &GamepadDevice) -> bool {
    if device.name.is_empty()
        || device.name.len() >= hbb_common::libc::UINPUT_MAX_NAME_SIZE
        || device.name.chars().any(char::is_control)
        || [device.bus, device.vendor, device.product, device.version]
            .iter()
            .any(|&id| id > u32::from(u16::MAX))
        || device.rumble_slots > MAX_RUMBLE_SLOTS
        || device.buttons.len() + device.axes.len() > MAX_EVENTS
        || (device.buttons.is_empty() && device.axes.is_empty())
    {
        return false;
    }

    // The sender advertises each control once in Linux code order.
    device.buttons.windows(2).all(|pair| pair[0] < pair[1])
        && device.buttons.iter().copied().all(gamepad_button)
        && device
            .axes
            .windows(2)
            .all(|pair| pair[0].code < pair[1].code)
        && device.axes.iter().all(|axis| {
            gamepad_axis(axis.code)
                && (axis.minimum..=axis.maximum).contains(&axis.neutral)
                && (0..=i64::from(i32::MAX))
                    .contains(&(i64::from(axis.maximum) - i64::from(axis.minimum)))
                && axis.flat >= 0
                && i64::from(axis.flat) <= i64::from(axis.maximum) - i64::from(axis.minimum)
                && axis.resolution >= 0
        })
}

#[cfg(any(feature = "flutter", test))]
fn input(kind: Kind, code: u32, value: i32) -> GamepadInput {
    GamepadInput {
        kind: kind.into(),
        code,
        value,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_analog_hat_triggers_release_to_minimum() {
        for code in 0x12..=0x15 {
            assert_eq!(axis_neutral(code, 0, 255, false), 0);
            assert_eq!(axis_neutral(code, -1, 1, false), 0);
        }
    }

    #[test]
    fn gamepad_standard_grip_buttons_are_forwarded() {
        assert!((0x224..=0x227).all(gamepad_button));
    }
}
