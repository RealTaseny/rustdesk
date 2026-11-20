# Changelog

## 1.5.3 — 2026-11-20

This release is for Linux x86-64.

- Added an opt-in privacy screen for incoming X11 desktop sessions. Enable it under **Settings → Security** to hide the local display and block local keyboard and mouse input during a session. A supported X11 screen locker is required. ([#1](https://github.com/RealTaseny/rustdesk/pull/1))
- Added gamepad forwarding for Linux-to-Linux desktop sessions. Enable **Forward gamepads** in the session's keyboard menu to send input from the selected foreground tab. Compatible controllers support rumble, and the client shows the remote `/dev/input/jsN` mapping. ([#2](https://github.com/RealTaseny/rustdesk/pull/2))
