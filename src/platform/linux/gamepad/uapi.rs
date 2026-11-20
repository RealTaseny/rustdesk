use hbb_common::{libc, tokio::io::unix::AsyncFd};
use std::{
    fs::{File, OpenOptions},
    io, mem,
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::OpenOptionsExt,
    },
    slice,
};

pub(super) const EV_SYN: u16 = 0;
pub(super) const EV_KEY: u16 = 1;
pub(super) const EV_ABS: u16 = 3;
pub(super) const EV_FF: u16 = 0x15;
pub(super) const EV_UINPUT: u16 = 0x101;
pub(super) const SYN_REPORT: u16 = 0;
pub(super) const FF_RUMBLE: u16 = 0x50;
pub(super) const UI_FF_UPLOAD: u16 = 1;
pub(super) const UI_FF_ERASE: u16 = 2;

nix::ioctl_none!(create, b'U', 1);
nix::ioctl_write_ptr!(setup, b'U', 3, libc::uinput_setup);
nix::ioctl_write_ptr!(setup_abs, b'U', 4, libc::uinput_abs_setup);
nix::ioctl_read_buf!(sysname, b'U', 44, u8);
nix::ioctl_write_int!(set_evbit, b'U', 100);
nix::ioctl_write_int!(set_keybit, b'U', 101);
nix::ioctl_write_int!(set_ffbit, b'U', 107);
// UI_SET_PHYS encodes sizeof(char *) but takes the character buffer itself.
nix::ioctl_write_ptr_bad!(
    set_phys,
    nix::request_code_write!(b'U', 108, mem::size_of::<*const libc::c_char>()),
    libc::c_char
);
nix::ioctl_readwrite!(begin_ff_upload, b'U', 200, libc::uinput_ff_upload);
nix::ioctl_write_ptr!(end_ff_upload, b'U', 201, libc::uinput_ff_upload);
nix::ioctl_readwrite!(begin_ff_erase, b'U', 202, libc::uinput_ff_erase);
nix::ioctl_write_ptr!(end_ff_erase, b'U', 203, libc::uinput_ff_erase);

pub(super) fn open_uinput() -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK);
    match options.open("/dev/uinput") {
        Err(err) if err.kind() == io::ErrorKind::NotFound => options.open("/dev/input/uinput"),
        result => result,
    }
}

pub(super) fn event(type_: u16, code: u16, value: i32) -> libc::input_event {
    // These input timestamps are filled in by the kernel.
    let mut event: libc::input_event = unsafe { mem::zeroed() };
    event.type_ = type_;
    event.code = code;
    event.value = value;
    event
}

pub(super) async fn read_events(
    device: &AsyncFd<impl AsRawFd>,
    events: &mut [libc::input_event],
) -> io::Result<usize> {
    device
        .async_io(hbb_common::tokio::io::Interest::READABLE, |fd| {
            read_available(fd.as_raw_fd(), events)
        })
        .await
}

pub(super) fn read_available(fd: RawFd, events: &mut [libc::input_event]) -> io::Result<usize> {
    let bytes = unsafe {
        slice::from_raw_parts_mut(events.as_mut_ptr().cast::<u8>(), mem::size_of_val(events))
    };
    let read = nix::unistd::read(fd, bytes).map_err(io::Error::from)?;
    if read == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if read % mem::size_of::<libc::input_event>() != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(read / mem::size_of::<libc::input_event>())
}

pub(super) fn write_events(fd: RawFd, events: &[libc::input_event]) -> io::Result<()> {
    let mut bytes =
        unsafe { slice::from_raw_parts(events.as_ptr().cast::<u8>(), mem::size_of_val(events)) };
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if written == 0 || written as usize % mem::size_of::<libc::input_event>() != 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}
