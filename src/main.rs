use std::{
    collections::{HashMap, HashSet},
    io,
    option::Option,
    sync::{LazyLock, mpsc},
    thread,
    time::Duration,
};

use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind};
use evdev::{AttributeSet, EventSummary, EventType, KeyCode, KeyEvent, uinput::VirtualDevice};
use tracing::{debug, error, info, level_filters::LevelFilter, trace};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
use unicode_segmentation::UnicodeSegmentation;

// mod caps;
// mod find;
// mod map;
// mod modifiers;
// mod num;
// mod send;
// mod tray;
// use crate::{modifiers::Modifier, num::IncrementalU16, tray::create_tray_item};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum KeyState {
    Released = 0,
    Pressed = 1,
    Repeat = 2,
}

impl From<KeyState> for i32 {
    fn from(value: KeyState) -> Self {
        value as i32
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // uncomment if doing something dangerous
    // std::thread::spawn(|| {
    //     std::thread::sleep(std::time::Duration::from_secs(20));
    //     error!("killing");
    //     std::process::exit(1);
    // });

    let mut kbs: Vec<_> = evdev::enumerate()
        .map(|(_path, dev)| dev)
        .filter(|dev| {
            let Some(keys) = dev.supported_keys() else {
                return false;
            };

            keys.contains(KeyCode::KEY_A)
                && keys.contains(KeyCode::KEY_Z)
                && keys.contains(KeyCode::KEY_ENTER)
                && keys.contains(KeyCode::KEY_SPACE)
                && !keys.contains(KeyCode::BTN_LEFT)
                && !keys.contains(KeyCode::BTN_RIGHT)
        })
        .collect();

    let all_keys: AttributeSet<_> = kbs
        .iter()
        .filter_map(|kb| kb.supported_keys())
        .flatten()
        .collect();
    // a mouse might accidentally be grabbed too, make sure to forward its movement.
    let all_axes: AttributeSet<_> = kbs
        .iter()
        .filter_map(|kb| kb.supported_relative_axes())
        .flatten()
        .collect();
    let virtual_dev = VirtualDevice::builder()?
        .name("retype-keyboard")
        .with_keys(&all_keys)?
        .with_relative_axes(&all_axes)?
        .build()?;

    for kb in &mut kbs {
        info!("grabbing {:?}", kb.name());
        kb.grab()?;
    }

    let (tx, rx) = mpsc::channel();
    for mut kb in kbs {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let kb_name: &'static str = kb.name().unwrap_or("none").to_owned().leak();
            loop {
                match kb.fetch_events() {
                    Ok(events) => _ = tx.send((kb_name, events.collect::<Vec<_>>())),
                    Err(e) => {
                        error!("error in {kb_name}: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    let mut handler = Handler::new(virtual_dev);

    while let Ok((kb, events)) = rx.recv() {
        trace!("{kb} got events {events:?}");

        if events.iter().any(|ev| ev.event_type() == EventType::KEY) {
            for ev in events {
                match ev.destructure() {
                    EventSummary::Key(_ev, code, value) => {
                        let pressed = match value {
                            0 => KeyState::Released,
                            1 => KeyState::Pressed,
                            2 => KeyState::Repeat,
                            _ => {
                                // weird event, pass through
                                handler.vdev.emit(&[ev])?;
                                continue;
                            }
                        };

                        handler.handle_event(code, pressed)?;
                    }
                    // ignore others
                    _ => handler.vdev.emit(&[ev])?,
                }
            }
        } else {
            // batch emit all events
            handler.vdev.emit(&events)?;
        }
    }

    // everything is ungrabbed when retype exits
    Ok(())
}

enum Direction {
    Forwards,
    Backwards,
}

/// Where to go after `[shift]+caps+f/d`
struct Find {
    select: bool,
    direction: Direction,
    text: String,
}

struct Handler {
    vdev: VirtualDevice,
    /// All real keys that are currently pressed, including blocked ones.
    real_keys_pressed: HashSet<KeyCode>,
    virtual_keys_pressed: HashSet<KeyCode>,
    immediately_after_meta_caps: bool,
    /// Number of times to repeat the next pressed character.
    ///
    /// 0 means no numbers have been pressed yet.
    repeat: u16,
    clipboard: Clipboard,
    find: Option<Find>,
}

impl Handler {
    fn new(vdev: VirtualDevice) -> Self {
        Self {
            vdev,
            real_keys_pressed: HashSet::new(),
            virtual_keys_pressed: HashSet::new(),
            immediately_after_meta_caps: false,
            repeat: 0,
            clipboard: Clipboard::new().unwrap(),
            find: None,
        }
    }

    fn emit(&mut self, key: KeyCode, state: KeyState) -> io::Result<()> {
        debug!("virtual {key:?} {state:?}");
        self.vdev.emit(&[*KeyEvent::new_now(key, state.into())])?;
        set_pressed(&mut self.virtual_keys_pressed, key, state);
        if state == KeyState::Pressed && FIND_CANCELLERS.contains(&key) {
            if self.find.is_some() {
                info!("cancelled find due to pressing {key:?}");
            }
            self.find = None;
        }
        Ok(())
    }

    fn press(&mut self, key: KeyCode) -> io::Result<()> {
        self.emit(key, KeyState::Pressed)?;
        Ok(())
    }

    fn release(&mut self, key: KeyCode) -> io::Result<()> {
        self.emit(key, KeyState::Released)
    }

    fn click(&mut self, key: KeyCode) -> io::Result<()> {
        debug!("virtual {key:?} clicked");
        self.vdev.emit(&[
            *KeyEvent::new_now(key, KeyState::Pressed.into()),
            *KeyEvent::new_now(key, KeyState::Released.into()),
        ])?;
        if FIND_CANCELLERS.contains(&key) {
            if self.find.is_some() {
                info!("cancelled find due to pressing {key:?}");
            }

            self.find = None;
        }
        Ok(())
    }

    fn click_repeat(&mut self, key: KeyCode, repeat: u16) -> io::Result<()> {
        debug!("virtual {key:?} clicked {repeat} times");
        for _ in 0..repeat {
            self.click(key)?;
            // repeating clicks too quickly makes them fail sometimes.
            // a small delay works to make it fully consistent.
            // blocking the thread is also what we want,
            // e.g. if i type 100 down then X, I want the X to only
            // appear after I finish the 100 down.
            self.tiny_wait();
        }
        Ok(())
    }

    /// Multiple events too quickly sometimes fails.
    ///
    /// Enough of a sleep to make the clicks consistent.
    fn tiny_wait(&self) {
        thread::sleep(Duration::from_micros(100));
    }

    fn release_all_virtual(&mut self) -> io::Result<()> {
        debug!(
            "releasing all virtual keys: {:?}",
            self.virtual_keys_pressed
        );
        for k in self.virtual_keys_pressed.drain() {
            self.vdev
                .emit(&[*KeyEvent::new_now(k, KeyState::Released.into())])?;
        }
        Ok(())
    }

    fn caps_pressed(&self) -> bool {
        self.real_keys_pressed.contains(&KeyCode::KEY_CAPSLOCK)
    }

    fn get_selection(&mut self) -> Option<String> {
        self.clipboard
            .get()
            .clipboard(LinuxClipboardKind::Primary)
            .text()
            .ok()
    }

    /// Maps the pressed key to a character, taking currently held modifiers into account.
    fn pressed_to_char(&self, key: KeyCode) -> Option<char> {
        use KeyCode as K;
        let shift = self.real_keys_pressed.contains(&K::KEY_LEFTSHIFT)
            || self.real_keys_pressed.contains(&K::KEY_RIGHTSHIFT);
        Some(match (key, shift) {
            (K::KEY_GRAVE, false) => '`',
            (K::KEY_GRAVE, true) => '~',
            (K::KEY_1, false) => '1',
            (K::KEY_1, true) => '!',
            (K::KEY_2, false) => '2',
            (K::KEY_2, true) => '@',
            (K::KEY_3, false) => '3',
            (K::KEY_3, true) => '#',
            (K::KEY_4, false) => '4',
            (K::KEY_4, true) => '$',
            (K::KEY_5, false) => '5',
            (K::KEY_5, true) => '%',
            (K::KEY_6, false) => '6',
            (K::KEY_6, true) => '^',
            (K::KEY_7, false) => '7',
            (K::KEY_7, true) => '&',
            (K::KEY_8, false) => '8',
            (K::KEY_8, true) => '*',
            (K::KEY_9, false) => '9',
            (K::KEY_9, true) => '(',
            (K::KEY_0, false) => '0',
            (K::KEY_0, true) => ')',
            (K::KEY_MINUS, false) => '-',
            (K::KEY_MINUS, true) => '_',
            (K::KEY_EQUAL, false) => '=',
            (K::KEY_EQUAL, true) => '+',
            (K::KEY_Q, false) => 'q',
            (K::KEY_Q, true) => 'Q',
            (K::KEY_W, false) => 'w',
            (K::KEY_W, true) => 'W',
            (K::KEY_E, false) => 'e',
            (K::KEY_E, true) => 'E',
            (K::KEY_R, false) => 'r',
            (K::KEY_R, true) => 'R',
            (K::KEY_T, false) => 't',
            (K::KEY_T, true) => 'T',
            (K::KEY_Y, false) => 'y',
            (K::KEY_Y, true) => 'Y',
            (K::KEY_U, false) => 'u',
            (K::KEY_U, true) => 'U',
            (K::KEY_I, false) => 'i',
            (K::KEY_I, true) => 'I',
            (K::KEY_O, false) => 'o',
            (K::KEY_O, true) => 'O',
            (K::KEY_P, false) => 'p',
            (K::KEY_P, true) => 'P',
            (K::KEY_LEFTBRACE, false) => '[',
            (K::KEY_LEFTBRACE, true) => '{',
            (K::KEY_RIGHTBRACE, false) => ']',
            (K::KEY_RIGHTBRACE, true) => '}',
            (K::KEY_BACKSLASH, false) => '\\',
            (K::KEY_BACKSLASH, true) => '|',
            (K::KEY_A, false) => 'a',
            (K::KEY_A, true) => 'A',
            (K::KEY_S, false) => 's',
            (K::KEY_S, true) => 'S',
            (K::KEY_D, false) => 'd',
            (K::KEY_D, true) => 'D',
            (K::KEY_F, false) => 'f',
            (K::KEY_F, true) => 'F',
            (K::KEY_G, false) => 'g',
            (K::KEY_G, true) => 'G',
            (K::KEY_H, false) => 'h',
            (K::KEY_H, true) => 'H',
            (K::KEY_J, false) => 'j',
            (K::KEY_J, true) => 'J',
            (K::KEY_K, false) => 'k',
            (K::KEY_K, true) => 'K',
            (K::KEY_L, false) => 'l',
            (K::KEY_L, true) => 'L',
            (K::KEY_SEMICOLON, false) => ';',
            (K::KEY_SEMICOLON, true) => ':',
            (K::KEY_APOSTROPHE, false) => '\'',
            (K::KEY_APOSTROPHE, true) => '"',
            (K::KEY_Z, false) => 'z',
            (K::KEY_Z, true) => 'Z',
            (K::KEY_X, false) => 'x',
            (K::KEY_X, true) => 'X',
            (K::KEY_C, false) => 'c',
            (K::KEY_C, true) => 'C',
            (K::KEY_V, false) => 'v',
            (K::KEY_V, true) => 'V',
            (K::KEY_B, false) => 'b',
            (K::KEY_B, true) => 'B',
            (K::KEY_N, false) => 'n',
            (K::KEY_N, true) => 'N',
            (K::KEY_M, false) => 'm',
            (K::KEY_M, true) => 'M',
            (K::KEY_COMMA, false) => ',',
            (K::KEY_COMMA, true) => '<',
            (K::KEY_DOT, false) => '.',
            (K::KEY_DOT, true) => '>',
            (K::KEY_SLASH, false) => '/',
            (K::KEY_SLASH, true) => '?',
            (K::KEY_SPACE, false) => ' ',
            (K::KEY_SPACE, true) => ' ',
            _ => return None,
        })
    }

    fn handle_event(&mut self, key: KeyCode, state: KeyState) -> anyhow::Result<()> {
        set_pressed(&mut self.real_keys_pressed, key, state);

        match state {
            KeyState::Pressed | KeyState::Released => {
                debug!("real {key:?} {state:?}");
            }
            KeyState::Repeat => {
                trace!("real {key:?} {state:?}");
            }
        }

        if key == KeyCode::KEY_F1 && state == KeyState::Pressed && self.caps_pressed() {
            anyhow::bail!("user pressed caps+f1 to kill retype");
        }

        // caps lock handling
        if key == KeyCode::KEY_CAPSLOCK {
            // if press meta -> caps -> release caps, then actually toggle capslock
            if self.real_keys_pressed.contains(&KeyCode::KEY_LEFTMETA) {
                match state {
                    KeyState::Pressed | KeyState::Repeat => {
                        self.immediately_after_meta_caps = true;
                    }
                    KeyState::Released => {
                        if self.immediately_after_meta_caps {
                            info!("toggling capslock");
                            self.click(KeyCode::KEY_CAPSLOCK)?;
                        }
                    }
                }
            }

            match state {
                KeyState::Pressed => {
                    // Press J -> caps, release J and start mapping to left
                    for k in CAPS_REMAP.keys() {
                        if self.virtual_keys_pressed.contains(k) {
                            self.release(*k)?;
                        }
                    }
                }
                KeyState::Repeat => {}
                KeyState::Released => {
                    // press caps -> J -> release caps, then release what J was mapped to
                    for k in CAPS_REMAP.values() {
                        if self.virtual_keys_pressed.contains(k) {
                            self.release(*k)?;
                        }
                    }
                }
            }

            return Ok(());
        }

        self.immediately_after_meta_caps = false;
        if key == KeyCode::KEY_ESC && state == KeyState::Pressed {
            self.repeat = 0;
            self.find = None;
            info!("reset state due to esc");
        }

        // Move to a character after caps+f/d was pressed.
        if state == KeyState::Pressed
            && let Some(char) = self.pressed_to_char(key)
            && let Some(Find {
                select,
                direction,
                text,
            }) = &self.find
        {
            info!("finding {char:?}");
            let select = *select;

            if text.contains(char) {
                let graphemes: Vec<_> = text.graphemes(true).collect();
                let move_amount = match *direction {
                    // If cursor is right next to the character, look for the next one.
                    Direction::Forwards => graphemes
                        .iter()
                        .skip(1)
                        .position(|g| g.contains(char))
                        // 1 to move AFTER the char, 1 to compensate for the skip
                        .map(|n| n + 2),
                    Direction::Backwards => graphemes
                        .iter()
                        .rev()
                        .skip(1)
                        .position(|g| g.contains(char))
                        .map(|n| n + 2),
                };

                let move_key = match *direction {
                    Direction::Forwards => KeyCode::KEY_RIGHT,
                    Direction::Backwards => KeyCode::KEY_LEFT,
                };

                if let Some(move_amount) = move_amount {
                    info!("moving {move_amount} characters");
                    self.release_all_virtual()?;
                    if select {
                        self.tiny_wait();
                        self.press(KeyCode::KEY_LEFTSHIFT)?;
                    }
                    self.tiny_wait();
                    self.click_repeat(move_key, move_amount.try_into().unwrap_or(u16::MAX))?;
                    if select {
                        self.tiny_wait();
                        self.release(KeyCode::KEY_LEFTSHIFT)?;
                    }
                } else {
                    info!("{char:?} not found in {text:?}")
                }
            } else {
                info!("{char:?} not found in {text:?}")
            }

            self.find = None;
            return Ok(());
        }

        // mappings while caps lock is pressed
        if self.caps_pressed() {
            if let Some(mapped) = CAPS_REMAP.get(&key) {
                if self.repeat != 0 && state == KeyState::Pressed {
                    self.click_repeat(*mapped, self.repeat)?;
                    self.repeat = 0;
                } else {
                    self.emit(*mapped, state)?;
                }
                return Ok(());
            } else if let Some(digit) = DIGITS.get(&key)
                && state == KeyState::Pressed
            {
                self.repeat = self.repeat.saturating_mul(10).saturating_add(*digit);
                info!("repeat set to {}", self.repeat);
                return Ok(());
            } else if (key == KeyCode::KEY_F || key == KeyCode::KEY_D) && state == KeyState::Pressed
            {
                let is_forwards = key == KeyCode::KEY_F;
                let direction_key = if is_forwards {
                    KeyCode::KEY_END
                } else {
                    KeyCode::KEY_HOME
                };
                let return_to_normal_key = if is_forwards {
                    KeyCode::KEY_LEFT
                } else {
                    KeyCode::KEY_RIGHT
                };

                self.release_all_virtual()?;
                self.tiny_wait();
                self.press(KeyCode::KEY_LEFTSHIFT)?;
                self.tiny_wait();
                self.click(direction_key)?;
                self.tiny_wait();
                self.click(direction_key)?;
                self.tiny_wait();
                self.release(KeyCode::KEY_LEFTSHIFT)?;
                self.tiny_wait();
                self.click(return_to_normal_key)?;

                // This is enough to for the primary clipboard selection to update.
                self.tiny_wait();
                match self.get_selection() {
                    Some(selection) => {
                        info!("found selection: {selection}");
                        self.find = Some(Find {
                            select: self.real_keys_pressed.contains(&KeyCode::KEY_LEFTSHIFT)
                                || self.real_keys_pressed.contains(&KeyCode::KEY_RIGHTSHIFT),
                            direction: if is_forwards {
                                Direction::Forwards
                            } else {
                                Direction::Backwards
                            },
                            text: selection,
                        })
                    }
                    None => error!("failed to get selection"),
                }

                return Ok(());
            }
        }
        // maybe repeat
        else if self.repeat != 0
            && state == KeyState::Pressed
            && !MODIFIERS.contains(&key)
            && key != KeyCode::KEY_CAPSLOCK
        {
            self.click_repeat(key, self.repeat)?;
            self.repeat = 0;
            return Ok(());
        }

        // pass through the event as usual
        self.emit(key, state)?;
        Ok(())
    }
}

// TODO: include a VIRTUALLY PRESSED map in the global state (excludes anything that was blocked).
// implement releasing everything and pressing everything.

fn set_pressed(set: &mut HashSet<KeyCode>, key: KeyCode, state: KeyState) {
    match state {
        KeyState::Released => set.remove(&key),
        KeyState::Pressed | KeyState::Repeat => set.insert(key),
    };
}

static CAPS_REMAP: LazyLock<HashMap<KeyCode, KeyCode>> = LazyLock::new(|| {
    HashMap::from_iter([
        (KeyCode::KEY_I, KeyCode::KEY_UP),
        (KeyCode::KEY_J, KeyCode::KEY_LEFT),
        (KeyCode::KEY_L, KeyCode::KEY_RIGHT),
        (KeyCode::KEY_K, KeyCode::KEY_DOWN),
        (KeyCode::KEY_H, KeyCode::KEY_HOME),
        (KeyCode::KEY_SEMICOLON, KeyCode::KEY_END),
    ])
});

static DIGITS: LazyLock<HashMap<KeyCode, u16>> = LazyLock::new(|| {
    HashMap::from_iter([
        (KeyCode::KEY_0, 0),
        (KeyCode::KEY_1, 1),
        (KeyCode::KEY_2, 2),
        (KeyCode::KEY_3, 3),
        (KeyCode::KEY_4, 4),
        (KeyCode::KEY_5, 5),
        (KeyCode::KEY_6, 6),
        (KeyCode::KEY_7, 7),
        (KeyCode::KEY_8, 8),
        (KeyCode::KEY_9, 9),
    ])
});

static MODIFIERS: LazyLock<HashSet<KeyCode>> = LazyLock::new(|| {
    HashSet::from_iter([
        KeyCode::KEY_LEFTMETA,
        KeyCode::KEY_RIGHTMETA,
        KeyCode::KEY_LEFTCTRL,
        KeyCode::KEY_RIGHTCTRL,
        KeyCode::KEY_LEFTSHIFT,
        KeyCode::KEY_RIGHTSHIFT,
        KeyCode::KEY_LEFTALT,
        KeyCode::KEY_RIGHTALT,
    ])
});

/// Pressing one of these keys will cancel the find.
static FIND_CANCELLERS: LazyLock<HashSet<KeyCode>> = LazyLock::new(|| {
    HashSet::from_iter([
        KeyCode::KEY_UP,
        KeyCode::KEY_LEFT,
        KeyCode::KEY_RIGHT,
        KeyCode::KEY_DOWN,
        KeyCode::KEY_HOME,
        KeyCode::KEY_END,
        KeyCode::KEY_BACKSPACE,
        KeyCode::KEY_DELETE,
        KeyCode::KEY_ENTER,
        KeyCode::KEY_TAB,
    ])
});

// fn old_main() {
//     if let Some(data_dir) = dirs::data_dir() {
//         _ = simplelog::WriteLogger::init(
//             log::LevelFilter::Debug,
//             Config::default(),
//             File::create(data_dir.join("retype.log")).unwrap(),
//         );
//         log_panics::init();
//     } else {
//         panic!("unable to locate data directory");
//     };
//     gtk::init().unwrap();

//     log::info!("hook installed");

//     // digits of numbers pressed by CapsLock + number
//     // occasionally cleared.
//     let mut number_history = IncrementalU16::new();
//     let mut find: Option<Finder> = None;
//     let mut to_select = false;
//     let mut enabled = true;
//     // see `caps` module docs
//     let mut caps_toggle = ActivateOnRelease::new(|| send::click(Key::CapsLock));

//     let (mut tray, tray_rx) = create_tray_item();

//     let mut handler = |ev: rdev::Event| -> Option<Event> {
//         let passthrough = Some(ev.clone());

//         if !matches!(
//             ev.event_type,
//             rdev::EventType::KeyPress(..) | rdev::EventType::KeyRelease(..)
//         ) {
//             return passthrough;
//         }

//         match tray_rx.try_recv() {
//             Ok(tray::Message::Quit) => {
//                 log::info!("quitting application");
//                 process::exit(0);
//             }
//             Ok(tray::Message::ToggleEnabled) => {
//                 enabled = !enabled;
//                 tray::set_icon(&mut tray, enabled);
//                 log::info!("set enabled to {} via tray menu", enabled);
//             }
//             Err(_) => {}
//         }

//         // initial state management
//         let key = match ev.event_type {
//             rdev::EventType::KeyPress(key) => {
//                 KEYS_PRESSED.write().unwrap().insert(key);
//                 key
//             }
//             rdev::EventType::KeyRelease(key) => {
//                 KEYS_PRESSED.write().unwrap().remove(&key);

//                 if key == Key::CapsLock {
//                     caps_toggle.maybe_activate();
//                 }

//                 return passthrough;
//             }
//             _ => return passthrough,
//         };
//         log::trace!("button event received: {ev:#?}");

//         // toggle enable/disable
//         if key == Key::KeyK && Modifier::Ctrl.is_pressed() && Modifier::Super.is_pressed() {
//             enabled = !enabled;
//             log::info!("set enabled to {} via hotkey", enabled);
//             tray::set_icon(&mut tray, enabled);
//         }

//         // hotkeys after this only active if enabled //
//         if !enabled {
//             log::trace!("hotkeys not enabled, ignoring");
//             number_history.clear();
//             find = None;
//             return passthrough;
//         }

//         caps_toggle.interrupt();

//         // clear number history if any of these are pressed
//         // maybe more in the future?
//         if matches!(key, Key::Escape) {
//             log::debug!("clearing history");
//             number_history.clear();
//             find = None;
//         }

//         // put this before the later remaps so that accidentally holding caps
//         // when finding the next character won't remap to the next character
//         if let Some(finder) = &find {
//             log::trace!("key {:?} pressed after find", key);
//             if let Some(char_to_find) = map::char_clicked(&ev) {
//                 log::info!("mapped keypress to character {char_to_find:?}");
//                 let selection = finder.selection.recv();
//                 log::info!("received selection '{selection}'");
//                 let direction = finder.direction;
//                 send::KEYPRESSES.run(move || {
//                     find::move_to_char(&selection, char_to_find, direction, to_select)
//                 });
//                 find = None;

//                 return None;
//             }
//         };

//         // simple remaps
//         if is_pressed(Key::CapsLock) {
//             log::trace!("caps is pressed");
//             if let Some(btn) = map::caps_remap(key) {
//                 log::info!("remapped {:?} to {:?}", key, btn);
//                 send::maybe_repeat(btn, &mut number_history);
//                 send::KEYPRESSES.run(move || {
//                     // one more click as repeat expects passthrough
//                     click(btn);
//                 });
//                 return None;
//             }
//             if let Some(digit) = map::number_key_to_digit(key) {
//                 log::info!("adding digit {digit} to repetitions");
//                 number_history.push_digit(digit);
//                 return None;
//             }
//             if key == Key::KeyF {
//                 find = Some(Finder::new(Direction::Forward));
//                 to_select = Modifier::Shift.is_pressed();
//                 log::info!("finding forward on next char, with selection = {to_select}");
//                 return None;
//             }
//             if key == Key::KeyD {
//                 find = Some(Finder::new(Direction::Backward));
//                 to_select = Modifier::Shift.is_pressed();
//                 log::info!("finding backward on next char, with selection = {to_select}");
//                 return None;
//             }
//         }

//         // disable capslock button, only toggle if Super is also pressed
//         if key == Key::CapsLock {
//             if Modifier::Super.is_pressed() {
//                 caps_toggle.await_release();
//             }
//             return None;
//         }

//         // repeat button multiple times if number_history has anything
//         send::maybe_repeat(key, &mut number_history);
//         passthrough
//     };

//     while let Err(e) = rdev::grab(&mut handler) {
//         log::error!("error grabbing inputs: {e:?}");
//         log::info!("restarting grab handler");
//     }

//     log::info!("application stopped");
// }
