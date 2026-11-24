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

struct KeyEventCollector(Arc<Mutex<VecDeque<(Key, bool)>>>);

impl InputCallback for KeyEventCollector {
    fn add_char(&mut self, _uni_char: u32) {}

    fn set_key_state(&mut self, key: Key, state: bool) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((key, state));
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
    key_events: Arc<Mutex<VecDeque<(Key, bool)>>>,
    held_keys: Vec<Key>,
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
            held_keys: Vec::new(),
            was_active: false,
            unfocused: false,
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
        self.unfocused = false;
        self.suppressed_buttons = 0;
        window.set_input_callback(Box::new(KeyEventCollector(Arc::clone(&self.key_events))));
    }

    #[cfg(test)]
    pub fn push_key_event(&self, key: Key, is_down: bool) {
        self.key_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((key, is_down));
    }

    pub fn release_all(&mut self) -> Vec<ClientMsg> {
        self.key_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let mut msgs = Vec::new();
        for key in self.held_keys.drain(..).rev() {
            if let Some(sc) = key_to_xt_scancode(key) {
                msgs.push(ClientMsg::KeyEvent {
                    scancode: sc,
                    make: false,
                });
            }
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
        let mut msgs = Vec::new();
        let events: Vec<(Key, bool)> = {
            let mut q = self.key_events.lock().unwrap_or_else(|e| e.into_inner());
            q.drain(..).collect()
        };

        for (key, is_down) in events {
            if is_down {
                if (!self.unfocused || snap.is_active) && !self.held_keys.contains(&key) {
                    if let Some(sc) = key_to_xt_scancode(key) {
                        self.held_keys.push(key);
                        msgs.push(ClientMsg::KeyEvent {
                            scancode: sc,
                            make: true,
                        });
                    }
                }
            } else if let Some(pos) = self.held_keys.iter().position(|&k| k == key) {
                self.held_keys.swap_remove(pos);
                if let Some(sc) = key_to_xt_scancode(key) {
                    msgs.push(ClientMsg::KeyEvent {
                        scancode: sc,
                        make: false,
                    });
                }
            }
        }

        if snap.is_active {
            self.was_active = true;
            self.unfocused = false;
        } else if self.was_active {
            self.was_active = false;
            self.unfocused = true;
            self.suppressed_buttons = snap.raw_buttons;
            self.scroll_accum = 0.0;
            for key in self.held_keys.drain(..).rev() {
                if let Some(sc) = key_to_xt_scancode(key) {
                    msgs.push(ClientMsg::KeyEvent {
                        scancode: sc,
                        make: false,
                    });
                }
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
        }

        if self.unfocused {
            self.suppressed_buttons &= snap.raw_buttons;
            return msgs;
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

        // Host auto-repeat (repeated down for already-held key) does not duplicate make.
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
        assert_eq!(keys, vec![(0x30, true)]);
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
}
