use rdev::Key;

use crate::modifiers::Modifier;

pub fn caps_remap(button: Key) -> Option<Key> {
    Some(match button {
        Key::KeyI => Key::UpArrow,
        Key::KeyJ => Key::LeftArrow,
        Key::KeyL => Key::RightArrow,
        Key::KeyK => Key::DownArrow,
        Key::KeyH => Key::Home,
        Key::SemiColon => Key::End,
        _ => return None,
    })
}

pub fn char_clicked(ev: &rdev::Event) -> Option<char> {
    // need .to_ascii_lowercase() because sometimes name is in uppercase
    // for some reason. sometimes it also becomes \r.
    ev.name
        .as_ref()
        .and_then(|s| s.chars().next())
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .map(|c| {
            if Modifier::Shift.is_pressed() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
}

pub fn number_key_to_digit(button: Key) -> Option<u8> {
    use Key::{Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9};

    Some(match button {
        Num0 => 0,
        Num1 => 1,
        Num2 => 2,
        Num3 => 3,
        Num4 => 4,
        Num5 => 5,
        Num6 => 6,
        Num7 => 7,
        Num8 => 8,
        Num9 => 9,
        _ => return None,
    })
}
