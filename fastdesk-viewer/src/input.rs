use fastdesk_proto::BTN_LEFT;
use fastdesk_proto::BTN_MIDDLE;
use fastdesk_proto::BTN_RIGHT;
use fastdesk_proto::ClientMsg;
use minifb::Key;
use minifb::MouseButton;
use minifb::MouseMode;
use minifb::Window;
use std::collections::HashSet;

pub struct InputTracker {
    pressed_keys: HashSet<Key>,
    last_x: u16,
    last_y: u16,
    last_buttons: u8,
}

impl Default for InputTracker {
    fn default() -> Self {
        Self {
            pressed_keys: HashSet::new(),
            last_x: 0,
            last_y: 0,
            last_buttons: 0,
        }
    }
}

impl InputTracker {
    pub fn poll(&mut self, window: &Window, width: u16, height: u16) -> Vec<ClientMsg> {
        let mut msgs = Vec::new();

        let current_keys: HashSet<Key> = window.get_keys().into_iter().collect();
        for &k in self.pressed_keys.difference(&current_keys) {
            if let Some(sc) = key_to_xt_scancode(k) {
                msgs.push(ClientMsg::KeyEvent {
                    scancode: sc,
                    make: false,
                });
            }
        }
        for &k in current_keys.difference(&self.pressed_keys) {
            if let Some(sc) = key_to_xt_scancode(k) {
                msgs.push(ClientMsg::KeyEvent {
                    scancode: sc,
                    make: true,
                });
            }
        }
        self.pressed_keys = current_keys;

        let mut button_mask = 0u8;
        if window.get_mouse_down(MouseButton::Left) {
            button_mask |= BTN_LEFT;
        }
        if window.get_mouse_down(MouseButton::Middle) {
            button_mask |= BTN_MIDDLE;
        }
        if window.get_mouse_down(MouseButton::Right) {
            button_mask |= BTN_RIGHT;
        }

        let (nx, ny) = if let Some((mx, my)) = window.get_mouse_pos(MouseMode::Clamp) {
            let max_x = width.saturating_sub(1).max(1) as f32;
            let max_y = height.saturating_sub(1).max(1) as f32;
            let nx = ((mx.clamp(0.0, max_x) / max_x) * 32767.0).round() as u16;
            let ny = ((my.clamp(0.0, max_y) / max_y) * 32767.0).round() as u16;
            (nx, ny)
        } else {
            (self.last_x, self.last_y)
        };

        let wheel_delta = window
            .get_scroll_wheel()
            .map(|(_, dy)| (dy / 12.0).round().clamp(-8.0, 8.0) as i16)
            .unwrap_or(0);

        if nx != self.last_x || ny != self.last_y || button_mask != self.last_buttons || wheel_delta != 0 {
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
