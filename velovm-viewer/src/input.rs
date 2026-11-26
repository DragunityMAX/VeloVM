use fastdesk_proto::BTN_LEFT;
use fastdesk_proto::BTN_MIDDLE;
use fastdesk_proto::BTN_RIGHT;
use fastdesk_proto::ClientMsg;
use minifb::InputCallback;
use minifb::Key;
use minifb::MouseButton;
use minifb::MouseMode;
use minifb::Window;
use raw_window_handle::HasDisplayHandle;
use raw_window_handle::RawDisplayHandle;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

const WAYLAND_REPEAT_DELAY: Duration = Duration::from_millis(400);
const WAYLAND_REPEAT_INTERVAL: Duration = Duration::from_millis(33);

fn is_repeatable_scancode(sc: u16) -> bool {
    !matches!(
        sc,
        0x1d | 0x2a
            | 0x36
            | 0x38
            | 0x3a
            | 0x45
            | 0x46
            | 0xe01d
            | 0xe037
            | 0xe038
            | 0xe05b
            | 0xe05c
            | XT_PAUSE_SCANCODE
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBackend {
    X11,
    Wayland,
    Windows,
    AppKit,
    Other,
}

impl ScrollBackend {
    pub fn from_window(window: &Window) -> Self {
        match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_)) => Self::X11,
            Ok(RawDisplayHandle::Wayland(_)) => Self::Wayland,
            Ok(RawDisplayHandle::Windows(_)) => Self::Windows,
            Ok(RawDisplayHandle::AppKit(_)) => Self::AppKit,
            _ => Self::Other,
        }
    }

    pub fn normalize_delta(self, raw_dy: f32) -> f32 {
        match self {
            // minifb X11 emits +/-1.0 per discrete wheel click.
            Self::X11 => raw_dy,
            // minifb Windows multiplies WM_MOUSEWHEEL (+/-120 per detent) by 0.1 -> +/-12.0.
            Self::Windows => raw_dy / 12.0,
            // minifb Wayland passes surface-space wl_pointer::Axis directly (+ is down, ~10 per detent).
            Self::Wayland => -raw_dy / 10.0,
            // minifb macOS passes NSEvent.deltaY directly.
            Self::AppKit => raw_dy,
            Self::Other => raw_dy,
        }
    }
}

pub fn displayed_image_bounds(
    win_width: usize,
    win_height: usize,
    guest_width: u16,
    guest_height: u16,
) -> Option<ImageBounds> {
    if win_width == 0 || win_height == 0 || guest_width == 0 || guest_height == 0 {
        return None;
    }
    let ww = win_width.min(i32::MAX as usize) as i32;
    let wh = win_height.min(i32::MAX as usize) as i32;
    let buffer_aspect = guest_width as f32 / guest_height as f32;
    let win_aspect = ww as f32 / wh as f32;

    if buffer_aspect > win_aspect {
        let new_height = ((ww as f32 / buffer_aspect) as i32).clamp(1, wh);
        let y_offset = (wh - new_height) / 2;
        Some(ImageBounds {
            x: 0.0,
            y: y_offset as f32,
            width: ww as f32,
            height: new_height as f32,
        })
    } else {
        let new_width = ((wh as f32 * buffer_aspect) as i32).clamp(1, ww);
        let x_offset = (ww - new_width) / 2;
        Some(ImageBounds {
            x: x_offset as f32,
            y: 0.0,
            width: new_width as f32,
            height: wh as f32,
        })
    }
}

pub fn normalize_pointer_pos(
    mx: f32,
    my: f32,
    win_width: usize,
    win_height: usize,
    guest_width: u16,
    guest_height: u16,
) -> Option<(u16, u16)> {
    let bounds = displayed_image_bounds(win_width, win_height, guest_width, guest_height)?;
    let max_x = (bounds.width - 1.0).max(0.0);
    let max_y = (bounds.height - 1.0).max(0.0);
    let rx = (mx - bounds.x).clamp(0.0, max_x);
    let ry = (my - bounds.y).clamp(0.0, max_y);
    let nx = if max_x > 0.0 {
        ((rx / max_x) * 32767.0).round() as u16
    } else {
        0
    };
    let ny = if max_y > 0.0 {
        ((ry / max_y) * 32767.0).round() as u16
    } else {
        0
    };
    Some((nx, ny))
}

const XT_PAUSE_SCANCODE: u16 = 0xe145;

fn push_scancode_event(msgs: &mut Vec<ClientMsg>, sc: u16, make: bool) {
    if sc == XT_PAUSE_SCANCODE {
        msgs.push(ClientMsg::KeyEvent {
            scancode: 0xe11d,
            make,
        });
        msgs.push(ClientMsg::KeyEvent {
            scancode: 0x0045,
            make,
        });
    } else {
        msgs.push(ClientMsg::KeyEvent { scancode: sc, make });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyCollectorBackend {
    MinifbKey,
    Evdev,
    Macos,
}

struct KeyEventCollector {
    queue: Arc<Mutex<VecDeque<(u16, bool)>>>,
    backend: KeyCollectorBackend,
}

impl InputCallback for KeyEventCollector {
    fn add_char(&mut self, _uni_char: u32) {}

    fn set_key_state(&mut self, key: Key, state: bool) {
        if self.backend != KeyCollectorBackend::MinifbKey {
            return;
        }
        if let Some(sc) = key_to_xt_scancode(key) {
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((sc, state));
        }
    }

    fn set_evdev_key_state(&mut self, evdev_code: u32, state: bool) {
        if self.backend != KeyCollectorBackend::Evdev {
            return;
        }
        if let Some(sc) = evdev_to_xt_scancode(evdev_code) {
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((sc, state));
        }
    }

    fn set_macos_key_state(&mut self, macos_keycode: u32, state: bool) {
        if self.backend != KeyCollectorBackend::Macos {
            return;
        }
        if let Some(sc) = macos_to_xt_scancode(macos_keycode) {
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((sc, state));
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PollSnapshot {
    pub is_active: bool,
    pub win_size: (usize, usize),
    pub mouse_pos: Option<(f32, f32)>,
    pub raw_buttons: u8,
    pub raw_scroll_dy: Option<f32>,
    pub scroll_backend: ScrollBackend,
}

#[derive(Debug)]
pub struct InputTracker {
    key_events: Arc<Mutex<VecDeque<(u16, bool)>>>,
    held_scancodes: Vec<u16>,
    wayland_repeat: Option<(u16, Instant)>,
    was_active: bool,
    unfocused: bool,
    suppressed_buttons: u8,
    last_x: u16,
    last_y: u16,
    last_buttons: u8,
    scroll_accum: f32,
}

impl Default for InputTracker {
    fn default() -> Self {
        Self {
            key_events: Arc::new(Mutex::new(VecDeque::new())),
            held_scancodes: Vec::new(),
            wayland_repeat: None,
            was_active: false,
            unfocused: true,
            suppressed_buttons: 0,
            last_x: 0,
            last_y: 0,
            last_buttons: 0,
            scroll_accum: 0.0,
        }
    }
}

impl InputTracker {
    pub fn attach_window(&mut self, window: &mut Window) {
        self.key_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.was_active = window.is_active();
        self.unfocused = !self.was_active;
        self.suppressed_buttons = 0;
        let backend = match ScrollBackend::from_window(window) {
            ScrollBackend::X11 | ScrollBackend::Wayland => KeyCollectorBackend::Evdev,
            ScrollBackend::AppKit => KeyCollectorBackend::Macos,
            ScrollBackend::Windows | ScrollBackend::Other => KeyCollectorBackend::MinifbKey,
        };
        window.set_input_callback(Box::new(KeyEventCollector {
            queue: Arc::clone(&self.key_events),
            backend,
        }));
    }

    #[cfg(windows)]
    pub(crate) fn key_events_queue(&self) -> Arc<Mutex<VecDeque<(u16, bool)>>> {
        Arc::clone(&self.key_events)
    }

    #[cfg(test)]
    pub fn push_key_event(&self, key: Key, is_down: bool) {
        if let Some(sc) = key_to_xt_scancode(key) {
            self.key_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((sc, is_down));
        }
    }

    pub fn release_all(&mut self) -> Vec<ClientMsg> {
        self.key_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.wayland_repeat = None;
        let mut msgs = Vec::new();
        for sc in self.held_scancodes.drain(..).rev() {
            push_scancode_event(&mut msgs, sc, false);
        }
        if self.last_buttons != 0 {
            self.last_buttons = 0;
            msgs.push(ClientMsg::PointerEvent {
                button_mask: 0,
                x: self.last_x,
                y: self.last_y,
                wheel_delta: 0,
            });
        }
        self.scroll_accum = 0.0;
        msgs
    }

    pub fn accumulate_scroll(&mut self, raw_dy: f32, backend: ScrollBackend) -> i16 {
        let notches = backend.normalize_delta(raw_dy);
        if !notches.is_finite() || notches == 0.0 {
            return 0;
        }
        if self.scroll_accum != 0.0 && self.scroll_accum.signum() != notches.signum() {
            self.scroll_accum = 0.0;
        }
        self.scroll_accum = (self.scroll_accum + notches).clamp(-8.0, 8.0);
        const EPS: f32 = 1e-4;
        if self.scroll_accum.abs() < 1.0 - EPS {
            return 0;
        }
        let steps_f = (self.scroll_accum + self.scroll_accum.signum() * EPS).trunc();
        self.scroll_accum -= steps_f;
        if self.scroll_accum.abs() < EPS {
            self.scroll_accum = 0.0;
        }
        (steps_f as i32).clamp(-8, 8) as i16
    }

    pub fn poll_snapshot(&mut self, snap: PollSnapshot, width: u16, height: u16) -> Vec<ClientMsg> {
        self.poll_snapshot_at(snap, width, height, Instant::now())
    }

    pub fn poll_snapshot_at(
        &mut self,
        snap: PollSnapshot,
        width: u16,
        height: u16,
        now: Instant,
    ) -> Vec<ClientMsg> {
        let mut msgs = Vec::new();
        let events: Vec<(u16, bool)> = {
            let mut q = self.key_events.lock().unwrap_or_else(|e| e.into_inner());
            q.drain(..).collect()
        };

        for (sc, is_down) in events {
            if is_down {
                if !self.unfocused || snap.is_active {
                    if !self.held_scancodes.contains(&sc) {
                        self.held_scancodes.push(sc);
                    }
                    push_scancode_event(&mut msgs, sc, true);
                    if snap.scroll_backend == ScrollBackend::Wayland && is_repeatable_scancode(sc) {
                        self.wayland_repeat = Some((sc, now + WAYLAND_REPEAT_DELAY));
                    }
                }
            } else {
                if self.wayland_repeat.is_some_and(|(r_sc, _)| r_sc == sc) {
                    self.wayland_repeat = None;
                }
                if let Some(pos) = self.held_scancodes.iter().position(|&k| k == sc) {
                    self.held_scancodes.swap_remove(pos);
                    push_scancode_event(&mut msgs, sc, false);
                }
            }
        }

        if snap.is_active {
            self.was_active = true;
            self.unfocused = false;
        } else if self.was_active {
            self.was_active = false;
            self.unfocused = true;
            self.wayland_repeat = None;
            self.suppressed_buttons = snap.raw_buttons;
            self.scroll_accum = 0.0;
            for sc in self.held_scancodes.drain(..).rev() {
                push_scancode_event(&mut msgs, sc, false);
            }
            if self.last_buttons != 0 {
                self.last_buttons = 0;
                msgs.push(ClientMsg::PointerEvent {
                    button_mask: 0,
                    x: self.last_x,
                    y: self.last_y,
                    wheel_delta: 0,
                });
            }
            return msgs;
        } else {
            self.unfocused = true;
            self.wayland_repeat = None;
            self.suppressed_buttons = snap.raw_buttons;
            return msgs;
        }

        if snap.scroll_backend == ScrollBackend::Wayland {
            if let Some((sc, next_at)) = self.wayland_repeat {
                if self.held_scancodes.contains(&sc) {
                    if now >= next_at {
                        push_scancode_event(&mut msgs, sc, true);
                        let next = if now.saturating_duration_since(next_at) >= WAYLAND_REPEAT_INTERVAL
                        {
                            now + WAYLAND_REPEAT_INTERVAL
                        } else {
                            next_at + WAYLAND_REPEAT_INTERVAL
                        };
                        self.wayland_repeat = Some((sc, next));
                    }
                } else {
                    self.wayland_repeat = None;
                }
            }
        } else {
            self.wayland_repeat = None;
        }

        self.suppressed_buttons &= snap.raw_buttons;
        let button_mask = snap.raw_buttons & !self.suppressed_buttons;

        let (win_w, win_h) = snap.win_size;
        let (nx, ny) = if let Some((mx, my)) = snap.mouse_pos {
            normalize_pointer_pos(mx, my, win_w, win_h, width, height)
                .unwrap_or((self.last_x, self.last_y))
        } else {
            (self.last_x, self.last_y)
        };

        let wheel_delta = snap
            .raw_scroll_dy
            .map(|dy| self.accumulate_scroll(dy, snap.scroll_backend))
            .unwrap_or(0);

        if nx != self.last_x
            || ny != self.last_y
            || button_mask != self.last_buttons
            || wheel_delta != 0
        {
            self.last_x = nx;
            self.last_y = ny;
            self.last_buttons = button_mask;
            msgs.push(ClientMsg::PointerEvent {
                button_mask,
                x: nx,
                y: ny,
                wheel_delta,
            });
        }

        msgs
    }

    pub fn poll(&mut self, window: &Window, width: u16, height: u16) -> Vec<ClientMsg> {
        let mut raw_buttons = 0u8;
        if window.get_mouse_down(MouseButton::Left) {
            raw_buttons |= BTN_LEFT;
        }
        if window.get_mouse_down(MouseButton::Middle) {
            raw_buttons |= BTN_MIDDLE;
        }
        if window.get_mouse_down(MouseButton::Right) {
            raw_buttons |= BTN_RIGHT;
        }
        let snap = PollSnapshot {
            is_active: window.is_active(),
            win_size: window.get_size(),
            mouse_pos: window.get_unscaled_mouse_pos(MouseMode::Pass),
            raw_buttons,
            raw_scroll_dy: window.get_scroll_wheel().map(|(_, dy)| dy),
            scroll_backend: ScrollBackend::from_window(window),
        };
        self.poll_snapshot(snap, width, height)
    }
}

pub fn key_to_xt_scancode(key: Key) -> Option<u16> {
    Some(match key {
        Key::Escape => 0x01,
        Key::Key1 => 0x02,
        Key::Key2 => 0x03,
        Key::Key3 => 0x04,
        Key::Key4 => 0x05,
        Key::Key5 => 0x06,
        Key::Key6 => 0x07,
        Key::Key7 => 0x08,
        Key::Key8 => 0x09,
        Key::Key9 => 0x0a,
        Key::Key0 => 0x0b,
        Key::Minus => 0x0c,
        Key::Equal => 0x0d,
        Key::Backspace => 0x0e,
        Key::Tab => 0x0f,
        Key::Q => 0x10,
        Key::W => 0x11,
        Key::E => 0x12,
        Key::R => 0x13,
        Key::T => 0x14,
        Key::Y => 0x15,
        Key::U => 0x16,
        Key::I => 0x17,
        Key::O => 0x18,
        Key::P => 0x19,
        Key::LeftBracket => 0x1a,
        Key::RightBracket => 0x1b,
        Key::Enter => 0x1c,
        Key::LeftCtrl => 0x1d,
        Key::A => 0x1e,
        Key::S => 0x1f,
        Key::D => 0x20,
        Key::F => 0x21,
        Key::G => 0x22,
        Key::H => 0x23,
        Key::J => 0x24,
        Key::K => 0x25,
        Key::L => 0x26,
        Key::Semicolon => 0x27,
        Key::Apostrophe => 0x28,
        Key::Backquote => 0x29,
        Key::LeftShift => 0x2a,
        Key::Backslash => 0x2b,
        Key::Z => 0x2c,
        Key::X => 0x2d,
        Key::C => 0x2e,
        Key::V => 0x2f,
        Key::B => 0x30,
        Key::N => 0x31,
        Key::M => 0x32,
        Key::Comma => 0x33,
        Key::Period => 0x34,
        Key::Slash => 0x35,
        Key::RightShift => 0x36,
        Key::NumPadAsterisk => 0x37,
        Key::LeftAlt => 0x38,
        Key::Space => 0x39,
        Key::CapsLock => 0x3a,
        Key::F1 => 0x3b,
        Key::F2 => 0x3c,
        Key::F3 => 0x3d,
        Key::F4 => 0x3e,
        Key::F5 => 0x3f,
        Key::F6 => 0x40,
        Key::F7 => 0x41,
        Key::F8 => 0x42,
        Key::F9 => 0x43,
        Key::F10 => 0x44,
        Key::NumLock => 0x45,
        Key::ScrollLock => 0x46,
        Key::NumPad7 => 0x47,
        Key::NumPad8 => 0x48,
        Key::NumPad9 => 0x49,
        Key::NumPadMinus => 0x4a,
        Key::NumPad4 => 0x4b,
        Key::NumPad5 => 0x4c,
        Key::NumPad6 => 0x4d,
        Key::NumPadPlus => 0x4e,
        Key::NumPad1 => 0x4f,
        Key::NumPad2 => 0x50,
        Key::NumPad3 => 0x51,
        Key::NumPad0 => 0x52,
        Key::NumPadDot => 0x53,
        Key::F11 => 0x57,
        Key::F12 => 0x58,
        Key::NumPadEnter => 0xe01c,
        Key::RightCtrl => 0xe01d,
        Key::NumPadSlash => 0xe035,
        Key::RightAlt => 0xe038,
        Key::Home => 0xe047,
        Key::Up => 0xe048,
        Key::PageUp => 0xe049,
        Key::Left => 0xe04b,
        Key::Right => 0xe04d,
        Key::End => 0xe04f,
        Key::Down => 0xe050,
        Key::PageDown => 0xe051,
        Key::Insert => 0xe052,
        Key::Delete => 0xe053,
        Key::LeftSuper => 0xe05b,
        Key::RightSuper => 0xe05c,
        Key::Menu => 0xe05d,
        Key::Pause => XT_PAUSE_SCANCODE,
        _ => return None,
    })
}

/// Maps a physical Linux evdev keycode (`<linux/input-event-codes.h>`) to an
/// XT Set-1 scancode, independent of the host's XKB/X11 keyboard layout.
pub fn evdev_to_xt_scancode(evdev_code: u32) -> Option<u16> {
    Some(match evdev_code {
        // KEY_ESC (1) ..= KEY_KPDOT (83) match XT Set-1 scancodes 0x01..=0x53 1:1.
        1..=83 => evdev_code as u16,
        86 => 0x56, // KEY_102ND (ISO <> key)
        87 => 0x57, // KEY_F11
        88 => 0x58, // KEY_F12
        89 => 0x73, // KEY_RO (JIS / ABNT2)
        92 => 0x79, // KEY_HENKAN (JIS)
        93 => 0x70, // KEY_KATAKANAHIRAGANA (JIS)
        94 => 0x7b, // KEY_MUHENKAN (JIS)
        96 => 0xe01c, // KEY_KPENTER
        97 => 0xe01d, // KEY_RIGHTCTRL
        98 => 0xe035, // KEY_KPSLASH
        99 => 0xe037, // KEY_SYSRQ (PrintScreen)
        100 => 0xe038, // KEY_RIGHTALT (AltGr)
        102 => 0xe047, // KEY_HOME
        103 => 0xe048, // KEY_UP
        104 => 0xe049, // KEY_PAGEUP
        105 => 0xe04b, // KEY_LEFT
        106 => 0xe04d, // KEY_RIGHT
        107 => 0xe04f, // KEY_END
        108 => 0xe050, // KEY_DOWN
        109 => 0xe051, // KEY_PAGEDOWN
        110 => 0xe052, // KEY_INSERT
        111 => 0xe053, // KEY_DELETE
        119 => XT_PAUSE_SCANCODE, // KEY_PAUSE (E1 1D 45)
        121 => 0x7e, // KEY_KPCOMMA (JIS / ABNT2)
        124 => 0x7d, // KEY_YEN (JIS)
        125 => 0xe05b, // KEY_LEFTMETA
        126 => 0xe05c, // KEY_RIGHTMETA
        127 | 139 => 0xe05d, // KEY_COMPOSE / KEY_MENU
        _ => return None,
    })
}

/// Maps a physical macOS Carbon/HIToolbox virtual keycode (`NSEvent.keyCode`)
/// to an XT Set-1 scancode, including ISO 102nd (`0x0a`), ANSI Grave (`0x32`),
/// JIS keys, PrintScreen (`kVK_F13`, `0x69`), and Pause (`kVK_F15`, `0x71`).
pub fn macos_to_xt_scancode(macos_keycode: u32) -> Option<u16> {
    Some(match macos_keycode {
        0x00 => 0x1e, // kVK_ANSI_A
        0x01 => 0x1f, // kVK_ANSI_S
        0x02 => 0x20, // kVK_ANSI_D
        0x03 => 0x21, // kVK_ANSI_F
        0x04 => 0x23, // kVK_ANSI_H
        0x05 => 0x22, // kVK_ANSI_G
        0x06 => 0x2c, // kVK_ANSI_Z
        0x07 => 0x2d, // kVK_ANSI_X
        0x08 => 0x2e, // kVK_ANSI_C
        0x09 => 0x2f, // kVK_ANSI_V
        0x0a => 0x56, // kVK_ISO_Section (ISO 102nd key)
        0x0b => 0x30, // kVK_ANSI_B
        0x0c => 0x10, // kVK_ANSI_Q
        0x0d => 0x11, // kVK_ANSI_W
        0x0e => 0x12, // kVK_ANSI_E
        0x0f => 0x13, // kVK_ANSI_R
        0x10 => 0x15, // kVK_ANSI_Y
        0x11 => 0x14, // kVK_ANSI_T
        0x12 => 0x02, // kVK_ANSI_1
        0x13 => 0x03, // kVK_ANSI_2
        0x14 => 0x04, // kVK_ANSI_3
        0x15 => 0x05, // kVK_ANSI_4
        0x16 => 0x07, // kVK_ANSI_6
        0x17 => 0x06, // kVK_ANSI_5
        0x18 => 0x0d, // kVK_ANSI_Equal
        0x19 => 0x0a, // kVK_ANSI_9
        0x1a => 0x08, // kVK_ANSI_7
        0x1b => 0x0c, // kVK_ANSI_Minus
        0x1c => 0x09, // kVK_ANSI_8
        0x1d => 0x0b, // kVK_ANSI_0
        0x1e => 0x1b, // kVK_ANSI_RightBracket
        0x1f => 0x18, // kVK_ANSI_O
        0x20 => 0x16, // kVK_ANSI_U
        0x21 => 0x1a, // kVK_ANSI_LeftBracket
        0x22 => 0x17, // kVK_ANSI_I
        0x23 => 0x19, // kVK_ANSI_P
        0x24 => 0x1c, // kVK_Return
        0x25 => 0x26, // kVK_ANSI_L
        0x26 => 0x24, // kVK_ANSI_J
        0x27 => 0x28, // kVK_ANSI_Quote
        0x28 => 0x25, // kVK_ANSI_K
        0x29 => 0x27, // kVK_ANSI_Semicolon
        0x2a => 0x2b, // kVK_ANSI_Backslash
        0x2b => 0x33, // kVK_ANSI_Comma
        0x2c => 0x35, // kVK_ANSI_Slash
        0x2d => 0x31, // kVK_ANSI_N
        0x2e => 0x32, // kVK_ANSI_M
        0x2f => 0x34, // kVK_ANSI_Period
        0x30 => 0x0f, // kVK_Tab
        0x31 => 0x39, // kVK_Space
        0x32 => 0x29, // kVK_ANSI_Grave
        0x33 => 0x0e, // kVK_Delete (Backspace)
        0x35 => 0x01, // kVK_Escape
        0x36 => 0xe05c, // Right Command (RightSuper)
        0x37 => 0xe05b, // kVK_Command (LeftSuper)
        0x38 => 0x2a, // kVK_Shift (LeftShift)
        0x39 => 0x3a, // kVK_CapsLock
        0x3a => 0x38, // kVK_Option (LeftAlt)
        0x3b => 0x1d, // kVK_Control (LeftCtrl)
        0x3c => 0x36, // kVK_RightShift
        0x3d => 0xe038, // kVK_RightOption (RightAlt)
        0x3e => 0xe01d, // kVK_RightControl
        0x41 => 0x53, // kVK_ANSI_KeypadDecimal
        0x43 => 0x37, // kVK_ANSI_KeypadMultiply
        0x45 => 0x4e, // kVK_ANSI_KeypadPlus
        0x47 => 0x45, // kVK_ANSI_KeypadClear (NumLock)
        0x4b => 0xe035, // kVK_ANSI_KeypadDivide
        0x4c => 0xe01c, // kVK_ANSI_KeypadEnter
        0x4e => 0x4a, // kVK_ANSI_KeypadMinus
        0x51 => 0x0d, // kVK_ANSI_KeypadEquals
        0x52 => 0x52, // kVK_ANSI_Keypad0
        0x53 => 0x4f, // kVK_ANSI_Keypad1
        0x54 => 0x50, // kVK_ANSI_Keypad2
        0x55 => 0x51, // kVK_ANSI_Keypad3
        0x56 => 0x4b, // kVK_ANSI_Keypad4
        0x57 => 0x4c, // kVK_ANSI_Keypad5
        0x58 => 0x4d, // kVK_ANSI_Keypad6
        0x59 => 0x47, // kVK_ANSI_Keypad7
        0x5b => 0x48, // kVK_ANSI_Keypad8
        0x5c => 0x49, // kVK_ANSI_Keypad9
        0x5d => 0x7d, // kVK_JIS_Yen
        0x5e => 0x73, // kVK_JIS_Underscore (Ro)
        0x5f => 0x7e, // kVK_JIS_KeypadComma
        0x60 => 0x3f, // kVK_F5
        0x61 => 0x40, // kVK_F6
        0x62 => 0x41, // kVK_F7
        0x63 => 0x3d, // kVK_F3
        0x64 => 0x42, // kVK_F8
        0x65 => 0x43, // kVK_F9
        0x66 => 0x7b, // kVK_JIS_Eisu (Muhenkan)
        0x67 => 0x57, // kVK_F11
        0x68 => 0x70, // kVK_JIS_Kana (Katakana/Hiragana)
        0x69 => 0xe037, // kVK_F13 (PrintScreen)
        0x6b => 0x46, // kVK_F14 (ScrollLock)
        0x6d => 0x44, // kVK_F10
        0x6f => 0x58, // kVK_F12
        0x71 => XT_PAUSE_SCANCODE, // kVK_F15 (Pause)
        0x72 => 0xe052, // kVK_Help (Insert)
        0x73 => 0xe047, // kVK_Home
        0x74 => 0xe049, // kVK_PageUp
        0x75 => 0xe053, // kVK_ForwardDelete
        0x76 => 0x3e, // kVK_F4
        0x77 => 0xe04f, // kVK_End
        0x78 => 0x3c, // kVK_F2
        0x79 => 0xe051, // kVK_PageDown
        0x7a => 0x3b, // kVK_F1
        0x7b => 0xe04b, // kVK_LeftArrow
        0x7c => 0xe04d, // kVK_RightArrow
        0x7d => 0xe050, // kVK_DownArrow
        0x7e => 0xe048, // kVK_UpArrow
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_pos_handles_resized_halved_letterboxed_and_pillarboxed_windows() {
        // Halved window (960x540 for 1920x1080 guest) reaches full 0..=32767 range.
        assert_eq!(
            displayed_image_bounds(960, 540, 1920, 1080),
            Some(ImageBounds {
                x: 0.0,
                y: 0.0,
                width: 960.0,
                height: 540.0,
            })
        );
        assert_eq!(
            normalize_pointer_pos(0.0, 0.0, 960, 540, 1920, 1080),
            Some((0, 0))
        );
        assert_eq!(
            normalize_pointer_pos(959.0, 539.0, 960, 540, 1920, 1080),
            Some((32767, 32767))
        );

        // Letterboxed window (960x800 for 1920x1080 guest): image occupies y=130..670.
        assert_eq!(
            displayed_image_bounds(960, 800, 1920, 1080),
            Some(ImageBounds {
                x: 0.0,
                y: 130.0,
                width: 960.0,
                height: 540.0,
            })
        );
        assert_eq!(
            normalize_pointer_pos(0.0, 130.0, 960, 800, 1920, 1080),
            Some((0, 0))
        );
        assert_eq!(
            normalize_pointer_pos(959.0, 669.0, 960, 800, 1920, 1080),
            Some((32767, 32767))
        );
        // Top and bottom letterbox bars clamp cleanly to top/bottom guest edges.
        assert_eq!(
            normalize_pointer_pos(480.0, 50.0, 960, 800, 1920, 1080).unwrap().1,
            0
        );
        assert_eq!(
            normalize_pointer_pos(480.0, 750.0, 960, 800, 1920, 1080).unwrap().1,
            32767
        );

        // Pillarboxed window (1200x540 for 1920x1080 guest): image occupies x=120..1080.
        assert_eq!(
            displayed_image_bounds(1200, 540, 1920, 1080),
            Some(ImageBounds {
                x: 120.0,
                y: 0.0,
                width: 960.0,
                height: 540.0,
            })
        );
        assert_eq!(
            normalize_pointer_pos(120.0, 0.0, 1200, 540, 1920, 1080),
            Some((0, 0))
        );
        assert_eq!(
            normalize_pointer_pos(1079.0, 539.0, 1200, 540, 1920, 1080),
            Some((32767, 32767))
        );
        // Left and right pillarbox bars clamp cleanly to left/right guest edges.
        assert_eq!(
            normalize_pointer_pos(10.0, 270.0, 1200, 540, 1920, 1080).unwrap().0,
            0
        );
        assert_eq!(
            normalize_pointer_pos(1190.0, 270.0, 1200, 540, 1920, 1080).unwrap().0,
            32767
        );

        // Zero-sized window yields None.
        assert_eq!(displayed_image_bounds(0, 0, 1920, 1080), None);
        assert_eq!(normalize_pointer_pos(0.0, 0.0, 0, 0, 1920, 1080), None);
    }

    #[test]
    fn scroll_wheel_normalizes_per_backend_and_accumulates_fractions() {
        let mut x11 = InputTracker::default();
        assert_eq!(x11.accumulate_scroll(1.0, ScrollBackend::X11), 1);
        assert_eq!(x11.accumulate_scroll(-1.0, ScrollBackend::X11), -1);

        let mut appkit = InputTracker::default();
        assert_eq!(appkit.accumulate_scroll(1.0, ScrollBackend::AppKit), 1);
        assert_eq!(appkit.accumulate_scroll(-1.0, ScrollBackend::AppKit), -1);
        assert_eq!(appkit.accumulate_scroll(0.5, ScrollBackend::AppKit), 0);
        assert_eq!(appkit.accumulate_scroll(0.5, ScrollBackend::AppKit), 1);

        let mut win = InputTracker::default();
        assert_eq!(win.accumulate_scroll(12.0, ScrollBackend::Windows), 1);
        let win_quarter: Vec<i16> = (0..4)
            .map(|_| win.accumulate_scroll(3.0, ScrollBackend::Windows))
            .collect();
        assert_eq!(win_quarter, vec![0, 0, 0, 1]);
        let win_twelfth: Vec<i16> = (0..12)
            .map(|_| win.accumulate_scroll(1.0, ScrollBackend::Windows))
            .collect();
        assert_eq!(win_twelfth, vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        let mut wayland = InputTracker::default();
        assert_eq!(wayland.accumulate_scroll(10.0, ScrollBackend::Wayland), -1);
        assert_eq!(wayland.accumulate_scroll(-10.0, ScrollBackend::Wayland), 1);
        let wayland_quarter: Vec<i16> = (0..4)
            .map(|_| wayland.accumulate_scroll(2.5, ScrollBackend::Wayland))
            .collect();
        assert_eq!(wayland_quarter, vec![0, 0, 0, -1]);

        // Direction reversal clears opposite-sign fractional remainder immediately.
        assert_eq!(wayland.accumulate_scroll(-7.5, ScrollBackend::Wayland), 0); // +0.75
        assert_eq!(wayland.accumulate_scroll(2.5, ScrollBackend::Wayland), 0); // clears +0.75 -> -0.25
        let after_reverse: Vec<i16> = (0..3)
            .map(|_| wayland.accumulate_scroll(2.5, ScrollBackend::Wayland))
            .collect();
        assert_eq!(after_reverse, vec![0, 0, -1]);
    }

    fn snap(is_active: bool, raw_buttons: u8) -> PollSnapshot {
        PollSnapshot {
            is_active,
            win_size: (800, 600),
            mouse_pos: Some((100.0, 100.0)),
            raw_buttons,
            raw_scroll_dy: None,
            scroll_backend: ScrollBackend::X11,
        }
    }

    #[test]
    fn keyboard_events_preserve_arrival_order_and_intra_poll_taps() {
        let mut tracker = InputTracker::default();
        tracker.push_key_event(Key::LeftShift, true);
        tracker.push_key_event(Key::A, true);
        tracker.push_key_event(Key::A, false);
        tracker.push_key_event(Key::LeftShift, false);

        let msgs = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let keys: Vec<(u16, bool)> = msgs
            .into_iter()
            .filter_map(|m| match m {
                ClientMsg::KeyEvent { scancode, make } => Some((scancode, make)),
                _ => None,
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                (0x2a, true),  // LeftShift make
                (0x1e, true),  // A make
                (0x1e, false), // A break
                (0x2a, false), // LeftShift break
            ]
        );

        // Host auto-repeat (repeated down for already-held key) forwards each make without
        // duplicating held_scancodes entries.
        tracker.push_key_event(Key::B, true);
        tracker.push_key_event(Key::B, true);
        tracker.push_key_event(Key::B, true);
        let msgs = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let keys: Vec<(u16, bool)> = msgs
            .into_iter()
            .filter_map(|m| match m {
                ClientMsg::KeyEvent { scancode, make } => Some((scancode, make)),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![(0x30, true), (0x30, true), (0x30, true)]);

        // Releasing once clears the held scancode cleanly.
        tracker.push_key_event(Key::B, false);
        let msgs = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let keys: Vec<(u16, bool)> = msgs
            .into_iter()
            .filter_map(|m| match m {
                ClientMsg::KeyEvent { scancode, make } => Some((scancode, make)),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![(0x30, false)]);

        // Repeated downs followed by focus loss emit only a single break.
        tracker.push_key_event(Key::Backspace, true);
        tracker.push_key_event(Key::Backspace, true);
        let _ = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let lost = tracker.poll_snapshot(snap(false, 0), 800, 600);
        assert_eq!(
            lost,
            vec![ClientMsg::KeyEvent {
                scancode: 0x0e,
                make: false,
            }]
        );
    }

    #[test]
    fn wayland_held_key_repeats_on_timer_and_stops_on_release_or_focus_loss() {
        let wayland_snap = |is_active: bool| PollSnapshot {
            is_active,
            win_size: (800, 600),
            mouse_pos: Some((100.0, 100.0)),
            raw_buttons: 0,
            raw_scroll_dy: None,
            scroll_backend: ScrollBackend::Wayland,
        };
        let key_events = |msgs: Vec<ClientMsg>| -> Vec<(u16, bool)> {
            msgs.into_iter()
                .filter_map(|m| match m {
                    ClientMsg::KeyEvent { scancode, make } => Some((scancode, make)),
                    _ => None,
                })
                .collect()
        };

        let mut tracker = InputTracker::default();
        let t0 = Instant::now();

        // Initial press emits one make, nothing before delay expires.
        tracker.push_key_event(Key::Backspace, true);
        assert_eq!(
            key_events(tracker.poll_snapshot_at(wayland_snap(true), 800, 600, t0)),
            vec![(0x0e, true)]
        );
        assert!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY - Duration::from_millis(1),
            ))
            .is_empty()
        );

        // At delay, emits repeat make; then emits again at interval.
        assert_eq!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY,
            )),
            vec![(0x0e, true)]
        );
        assert!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY + WAYLAND_REPEAT_INTERVAL - Duration::from_millis(1),
            ))
            .is_empty()
        );
        assert_eq!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY + WAYLAND_REPEAT_INTERVAL,
            )),
            vec![(0x0e, true)]
        );

        // Release stops repeats immediately.
        tracker.push_key_event(Key::Backspace, false);
        assert_eq!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY + WAYLAND_REPEAT_INTERVAL * 2,
            )),
            vec![(0x0e, false)]
        );
        assert!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t0 + WAYLAND_REPEAT_DELAY + WAYLAND_REPEAT_INTERVAL * 4,
            ))
            .is_empty()
        );

        // Modifiers do not repeat on the Wayland timer; focus loss clears active repeat.
        let t1 = t0 + Duration::from_secs(2);
        tracker.push_key_event(Key::LeftShift, true);
        assert_eq!(
            key_events(tracker.poll_snapshot_at(wayland_snap(true), 800, 600, t1)),
            vec![(0x2a, true)]
        );
        assert!(
            key_events(tracker.poll_snapshot_at(
                wayland_snap(true),
                800,
                600,
                t1 + Duration::from_secs(1),
            ))
            .is_empty()
        );

        // Pressing a printable key starts repeating it; focus loss releases both and stops repeat.
        let t2 = t1 + Duration::from_secs(2);
        tracker.push_key_event(Key::A, true);
        assert_eq!(
            key_events(tracker.poll_snapshot_at(wayland_snap(true), 800, 600, t2)),
            vec![(0x1e, true)]
        );
        let lost = key_events(tracker.poll_snapshot_at(
            wayland_snap(false),
            800,
            600,
            t2 + WAYLAND_REPEAT_DELAY,
        ));
        assert_eq!(lost, vec![(0x1e, false), (0x2a, false)]);
    }

    #[test]
    fn focus_loss_releases_held_keys_and_buttons_cleanly() {
        let mut tracker = InputTracker::default();

        // While active, press LeftCtrl + A and left mouse button.
        tracker.push_key_event(Key::LeftCtrl, true);
        tracker.push_key_event(Key::A, true);
        let msgs = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(msgs.contains(&ClientMsg::KeyEvent {
            scancode: 0x1d,
            make: true
        }));
        assert!(msgs.contains(&ClientMsg::KeyEvent {
            scancode: 0x1e,
            make: true
        }));
        assert!(msgs.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent {
                button_mask: BTN_LEFT,
                ..
            }
        )));

        // Focus lost: emits reverse-order key breaks and button_mask=0 release.
        let lost = tracker.poll_snapshot(snap(false, BTN_LEFT), 800, 600);
        assert_eq!(
            lost,
            vec![
                ClientMsg::KeyEvent {
                    scancode: 0x1e,
                    make: false,
                },
                ClientMsg::KeyEvent {
                    scancode: 0x1d,
                    make: false,
                },
                ClientMsg::PointerEvent {
                    button_mask: 0,
                    x: 4101,
                    y: 5470,
                    wheel_delta: 0,
                },
            ]
        );

        // Even if OS reports stuck button or stray key-down while unfocused, nothing is emitted.
        tracker.push_key_event(Key::C, true);
        let while_unfocused = tracker.poll_snapshot(snap(false, BTN_LEFT), 800, 600);
        assert!(while_unfocused.is_empty());

        // Refocusing while stuck BTN_LEFT hasn't been released suppresses BTN_LEFT.
        let refocused_stuck = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(!refocused_stuck.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent { button_mask, .. } if *button_mask != 0
        )));

        // Once button is released (raw_buttons=0), a subsequent click works normally.
        let _ = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let clicked = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(clicked.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent {
                button_mask: BTN_LEFT,
                ..
            }
        )));
    }

    #[test]
    fn unfocused_startup_drops_keyboard_and_pointer_events_and_suppresses_held_buttons() {
        let mut tracker = InputTracker::default();
        tracker.push_key_event(Key::A, true);

        // Window starts unfocused with BTN_LEFT already held: emit nothing.
        let initial_unfocused = tracker.poll_snapshot(snap(false, BTN_LEFT), 800, 600);
        assert!(initial_unfocused.is_empty());

        // Second unfocused poll with another key-down also emits nothing.
        tracker.push_key_event(Key::B, true);
        let still_unfocused = tracker.poll_snapshot(snap(false, BTN_LEFT), 800, 600);
        assert!(still_unfocused.is_empty());

        // Focusing while BTN_LEFT is still held must suppress BTN_LEFT until released.
        let focused_held = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(!focused_held.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent { button_mask, .. } if *button_mask != 0
        )));

        // Releasing BTN_LEFT and clicking again sends BTN_LEFT normally.
        let _ = tracker.poll_snapshot(snap(true, 0), 800, 600);
        let clicked = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(clicked.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent {
                button_mask: BTN_LEFT,
                ..
            }
        )));
    }

    #[test]
    fn button_released_while_unfocused_does_not_suppress_click_on_focus() {
        let mut tracker = InputTracker::default();

        // Button pressed while unfocused, then released while still unfocused.
        assert!(tracker.poll_snapshot(snap(false, BTN_LEFT), 800, 600).is_empty());
        assert!(tracker.poll_snapshot(snap(false, 0), 800, 600).is_empty());

        // Clicking to focus with BTN_LEFT must not be swallowed by stale suppression.
        let clicked_on_focus = tracker.poll_snapshot(snap(true, BTN_LEFT), 800, 600);
        assert!(clicked_on_focus.iter().any(|m| matches!(
            m,
            ClientMsg::PointerEvent {
                button_mask: BTN_LEFT,
                ..
            }
        )));
    }

    #[test]
    fn alt_altgr_and_super_keys_emit_xt_scancodes() {
        let mut tracker = InputTracker::default();
        let active_snap = snap(true, 0);
        // Focus the tracker first.
        let _ = tracker.poll_snapshot(active_snap, 1280, 720);

        for (key, expected_sc) in [
            (Key::LeftAlt, 0x38u16),
            (Key::RightAlt, 0xe038u16),
            (Key::LeftSuper, 0xe05bu16),
            (Key::RightSuper, 0xe05cu16),
        ] {
            tracker.push_key_event(key, true);
            let down = tracker.poll_snapshot(active_snap, 1280, 720);
            assert_eq!(
                down,
                vec![ClientMsg::KeyEvent {
                    scancode: expected_sc,
                    make: true
                }]
            );

            tracker.push_key_event(key, false);
            let up = tracker.poll_snapshot(active_snap, 1280, 720);
            assert_eq!(
                up,
                vec![ClientMsg::KeyEvent {
                    scancode: expected_sc,
                    make: false
                }]
            );
        }
    }

    #[test]
    fn linux_evdev_collector_uses_physical_keycodes_and_ignores_layout_keysyms() {
        let mut tracker = InputTracker::default();
        let mut collector = KeyEventCollector {
            queue: Arc::clone(&tracker.key_events),
            backend: KeyCollectorBackend::Evdev,
        };
        let active_snap = PollSnapshot {
            is_active: true,
            win_size: (1280, 720),
            mouse_pos: None,
            raw_buttons: 0,
            raw_scroll_dy: None,
            scroll_backend: ScrollBackend::X11,
        };
        let _ = tracker.poll_snapshot(active_snap, 1280, 720);

        // German QWERTZ: physical Y position (evdev 21) produces keysym 'z' (Key::Z).
        // With use_evdev=true, set_evdev_key_state(21) emits 0x15 and set_key_state(Key::Z) is ignored.
        collector.set_evdev_key_state(21, true);
        collector.set_key_state(Key::Z, true);
        // French AZERTY: physical '2' key (evdev 3, 'é') has no minifb US Key, and AltGr (evdev 100)
        // produces ISO_Level3_Shift which minifb drops; physical evdev codes still reach the guest.
        collector.set_evdev_key_state(3, true);
        collector.set_evdev_key_state(100, true);
        // ISO 102nd key (evdev 86) -> 0x56.
        collector.set_evdev_key_state(86, true);

        let msgs = tracker.poll_snapshot(active_snap, 1280, 720);
        assert_eq!(
            msgs,
            vec![
                ClientMsg::KeyEvent {
                    scancode: 0x15,
                    make: true
                },
                ClientMsg::KeyEvent {
                    scancode: 0x03,
                    make: true
                },
                ClientMsg::KeyEvent {
                    scancode: 0xe038,
                    make: true
                },
                ClientMsg::KeyEvent {
                    scancode: 0x56,
                    make: true
                },
            ]
        );

        // Focus loss releases held evdev scancodes cleanly.
        let unfocused_snap = PollSnapshot {
            is_active: false,
            ..active_snap
        };
        let release_msgs = tracker.poll_snapshot(unfocused_snap, 1280, 720);
        assert_eq!(
            release_msgs,
            vec![
                ClientMsg::KeyEvent {
                    scancode: 0x56,
                    make: false
                },
                ClientMsg::KeyEvent {
                    scancode: 0xe038,
                    make: false
                },
                ClientMsg::KeyEvent {
                    scancode: 0x03,
                    make: false
                },
                ClientMsg::KeyEvent {
                    scancode: 0x15,
                    make: false
                },
            ]
        );
    }

    #[test]
    fn evdev_table_covers_every_key_in_key_to_xt_scancode() {
        let evdev_codes: Vec<u32> = (1..=83)
            .chain([
                86, 87, 88, 89, 92, 93, 94, 96, 97, 98, 99, 100, 102, 103, 104, 105, 106, 107,
                108, 109, 110, 111, 119, 121, 124, 125, 126, 127,
            ])
            .collect();
        let xt_from_evdev: std::collections::HashSet<u16> = evdev_codes
            .into_iter()
            .filter_map(evdev_to_xt_scancode)
            .collect();

        for (key, expected_sc) in [
            (Key::Escape, 0x01),
            (Key::Q, 0x10),
            (Key::A, 0x1e),
            (Key::Z, 0x2c),
            (Key::Y, 0x15),
            (Key::LeftAlt, 0x38),
            (Key::RightAlt, 0xe038),
            (Key::LeftSuper, 0xe05b),
            (Key::RightSuper, 0xe05c),
            (Key::Menu, 0xe05d),
            (Key::F11, 0x57),
            (Key::F12, 0x58),
            (Key::Delete, 0xe053),
            (Key::Pause, XT_PAUSE_SCANCODE),
        ] {
            let sc = key_to_xt_scancode(key).unwrap();
            assert_eq!(sc, expected_sc);
            assert!(
                xt_from_evdev.contains(&sc),
                "missing evdev mapping for {key:?} (0x{sc:x})"
            );
        }

        // Verify ISO 102nd, JIS/ABNT2, PrintScreen, and Pause evdev mappings.
        for (evdev_code, expected_xt) in [
            (86u32, 0x56u16),          // KEY_102ND
            (89, 0x73),                // KEY_RO
            (92, 0x79),                // KEY_HENKAN
            (93, 0x70),                // KEY_KATAKANAHIRAGANA
            (94, 0x7b),                // KEY_MUHENKAN
            (99, 0xe037),              // KEY_SYSRQ (PrintScreen)
            (119, XT_PAUSE_SCANCODE),  // KEY_PAUSE
            (121, 0x7e),               // KEY_KPCOMMA
            (124, 0x7d),               // KEY_YEN
        ] {
            assert_eq!(
                evdev_to_xt_scancode(evdev_code),
                Some(expected_xt),
                "unexpected XT mapping for evdev {evdev_code}"
            );
        }
    }

    #[test]
    fn macos_collector_maps_iso_102nd_jis_printscreen_and_pause() {
        let mut tracker = InputTracker::default();
        let mut collector = KeyEventCollector {
            queue: Arc::clone(&tracker.key_events),
            backend: KeyCollectorBackend::Macos,
        };
        let active_snap = PollSnapshot {
            is_active: true,
            win_size: (1280, 720),
            mouse_pos: None,
            raw_buttons: 0,
            raw_scroll_dy: None,
            scroll_backend: ScrollBackend::AppKit,
        };
        let _ = tracker.poll_snapshot(active_snap, 1280, 720);

        // macOS sends Key::Unknown via set_key_state for 0x0a/0x32/JIS/PrintScreen;
        // KeyCollectorBackend::Macos ignores set_key_state and translates raw keycodes.
        collector.set_macos_key_state(0x0a, true); // kVK_ISO_Section -> 0x56
        collector.set_key_state(Key::Unknown, true);
        collector.set_macos_key_state(0x32, true); // kVK_ANSI_Grave -> 0x29
        collector.set_macos_key_state(0x5d, true); // kVK_JIS_Yen -> 0x7d
        collector.set_macos_key_state(0x5e, true); // kVK_JIS_Underscore -> 0x73
        collector.set_macos_key_state(0x5f, true); // kVK_JIS_KeypadComma -> 0x7e
        collector.set_macos_key_state(0x66, true); // kVK_JIS_Eisu -> 0x7b
        collector.set_macos_key_state(0x68, true); // kVK_JIS_Kana -> 0x70
        collector.set_macos_key_state(0x69, true); // PrintScreen (F13) -> 0xe037

        let msgs = tracker.poll_snapshot(active_snap, 1280, 720);
        assert_eq!(
            msgs,
            vec![
                ClientMsg::KeyEvent { scancode: 0x56, make: true },
                ClientMsg::KeyEvent { scancode: 0x29, make: true },
                ClientMsg::KeyEvent { scancode: 0x7d, make: true },
                ClientMsg::KeyEvent { scancode: 0x73, make: true },
                ClientMsg::KeyEvent { scancode: 0x7e, make: true },
                ClientMsg::KeyEvent { scancode: 0x7b, make: true },
                ClientMsg::KeyEvent { scancode: 0x70, make: true },
                ClientMsg::KeyEvent { scancode: 0xe037, make: true },
            ]
        );
    }

    #[test]
    fn pause_key_emits_e1_1d_45_sequence_without_colliding_with_numlock() {
        let mut tracker = InputTracker::default();
        let active_snap = snap(true, 0);
        let _ = tracker.poll_snapshot(active_snap, 800, 600);

        // Hold NumLock (0x45) and Pause (E1 1D 45) simultaneously.
        tracker.push_key_event(Key::NumLock, true);
        tracker.push_key_event(Key::Pause, true);
        let down = tracker.poll_snapshot(active_snap, 800, 600);
        assert_eq!(
            down,
            vec![
                ClientMsg::KeyEvent { scancode: 0x45, make: true },
                ClientMsg::KeyEvent { scancode: 0xe11d, make: true },
                ClientMsg::KeyEvent { scancode: 0x0045, make: true },
            ]
        );

        // Releasing Pause emits E1 9D C5 in order and leaves NumLock held.
        tracker.push_key_event(Key::Pause, false);
        let up = tracker.poll_snapshot(active_snap, 800, 600);
        assert_eq!(
            up,
            vec![
                ClientMsg::KeyEvent { scancode: 0xe11d, make: false },
                ClientMsg::KeyEvent { scancode: 0x0045, make: false },
            ]
        );

        // Focus loss still releases NumLock cleanly, and if Pause is held during focus loss
        // it emits E1 9D C5 in order as well.
        tracker.push_key_event(Key::Pause, true);
        let _ = tracker.poll_snapshot(active_snap, 800, 600);
        let lost = tracker.poll_snapshot(snap(false, 0), 800, 600);
        assert_eq!(
            lost,
            vec![
                ClientMsg::KeyEvent { scancode: 0xe11d, make: false },
                ClientMsg::KeyEvent { scancode: 0x0045, make: false },
                ClientMsg::KeyEvent { scancode: 0x45, make: false },
            ]
        );

        assert!(!is_repeatable_scancode(XT_PAUSE_SCANCODE));
    }
}
