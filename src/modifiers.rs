use rdev::Key;

use crate::send::{release, KEYS_PRESSED};

pub fn is_modifier(button: Key) -> bool {
    use Key::{Alt, AltGr, ControlLeft, ControlRight, MetaLeft, MetaRight, ShiftLeft, ShiftRight};
    matches!(
        button,
        Alt | AltGr | ShiftLeft | ShiftRight | ControlLeft | ControlRight | MetaLeft | MetaRight
    )
}

/// Any modifier, without caring about whether it is the left or right button.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[allow(dead_code)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    /// Whether any of LSelf, RSelf or Self is pressed.
    pub fn is_pressed(&self) -> bool {
        let keys_pressed = KEYS_PRESSED.read().unwrap();
        match self {
            Modifier::Ctrl => {
                keys_pressed.contains(&Key::ControlLeft)
                    || keys_pressed.contains(&Key::ControlRight)
            }
            Modifier::Shift => {
                keys_pressed.contains(&Key::ShiftLeft) || keys_pressed.contains(&Key::ShiftRight)
            }
            Modifier::Alt => keys_pressed.contains(&Key::Alt) || keys_pressed.contains(&Key::AltGr),
            Modifier::Super => {
                keys_pressed.contains(&Key::MetaLeft) || keys_pressed.contains(&Key::MetaRight)
            }
        }
    }

    /// Releases all ways of pressing this modifier.
    pub fn release(&self) {
        match self {
            Modifier::Ctrl => {
                release(Key::ControlLeft);
                release(Key::ControlRight);
            }
            Modifier::Shift => {
                release(Key::ShiftLeft);
                release(Key::ShiftRight);
            }
            Modifier::Alt => {
                release(Key::Alt);
                release(Key::AltGr);
            }
            Modifier::Super => {
                release(Key::MetaLeft);
                release(Key::MetaRight);
            }
        }
    }
}
