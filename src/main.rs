use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{LazyLock, mpsc},
};

use evdev::{AttributeSet, EventSummary, EventType, KeyCode, KeyEvent, uinput::VirtualDevice};
use tracing::{debug, error, info, level_filters::LevelFilter, trace};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

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
    //     std::thread::sleep(std::time::Duration::from_secs(10));
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
    let mut virtual_dev = VirtualDevice::builder()?
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

    let mut state = GlobalState::new();

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
                                virtual_dev.emit(&[ev])?;
                                continue;
                            }
                        };

                        let mut block = false;
                        process_event(&mut state, &mut virtual_dev, code, pressed, &mut block)?;
                        if !block {
                            virtual_dev.emit(&[ev])?;
                        } else {
                            debug!("blocked");
                        }
                    }
                    // ignore others
                    _ => virtual_dev.emit(&[ev])?,
                }
            }
        } else {
            // batch emit all events
            virtual_dev.emit(&events)?;
        }
    }

    // everything is ungrabbed when retype exits
    Ok(())
}

struct GlobalState {
    keys_pressed: HashSet<KeyCode>,
    immediately_after_meta_caps: bool,
    /// Number of times to repeat the next pressed character.
    ///
    /// 0 means no numbers have been pressed yet.
    repeat: u16,
}

impl GlobalState {
    fn new() -> Self {
        Self {
            keys_pressed: HashSet::new(),
            immediately_after_meta_caps: false,
            repeat: 0,
        }
    }

    fn caps_pressed(&self) -> bool {
        self.keys_pressed.contains(&KeyCode::KEY_CAPSLOCK)
    }
}

fn process_event(
    s: &mut GlobalState,
    dev: &mut VirtualDevice,
    key: KeyCode,
    state: KeyState,
    // set to true to block this input
    block: &mut bool,
) -> io::Result<()> {
    set_pressed(&mut s.keys_pressed, key, state);

    debug!("real {key:?} {state:?}");

    if key == KeyCode::KEY_F1 && state == KeyState::Pressed && s.caps_pressed() {
        return Err(io::Error::other("user pressed caps+f1 to kill retype"));
    }

    // caps lock handling
    if key == KeyCode::KEY_CAPSLOCK {
        *block = true;

        // if press meta -> caps -> release caps, then actually toggle capslock
        if s.keys_pressed.contains(&KeyCode::KEY_LEFTMETA) {
            match state {
                KeyState::Pressed | KeyState::Repeat => {
                    s.immediately_after_meta_caps = true;
                }
                KeyState::Released => {
                    if s.immediately_after_meta_caps {
                        info!("toggling capslock");
                        click(dev, KeyCode::KEY_CAPSLOCK)?;
                    }
                }
            }
        }

        match state {
            KeyState::Pressed => {
                // Press J -> caps, release J and start mapping to left
                for k in CAPS_REMAP.keys() {
                    release(dev, *k)?;
                }
            }
            KeyState::Repeat => {}
            KeyState::Released => {
                // press caps -> J -> release caps, then release what J was mapped to
                for k in CAPS_REMAP.values() {
                    release(dev, *k)?;
                }
            }
        }

        return Ok(());
    }

    s.immediately_after_meta_caps = false;
    if key == KeyCode::KEY_ESC && state == KeyState::Pressed {
        s.repeat = 0;
        info!("reset repeat to 0 due to esc");
    }

    // mappings while caps lock is pressed
    if s.caps_pressed() {
        if let Some(mapped) = CAPS_REMAP.get(&key) {
            *block = true;
            if s.repeat != 0 && state == KeyState::Pressed {
                info!("repeating {mapped:?} {} times", s.repeat);
                click_repeat(dev, *mapped, s.repeat)?;
                s.repeat = 0;
            } else {
                emit(dev, *mapped, state)?;
            }
        } else if let Some(digit) = DIGITS.get(&key)
            && state == KeyState::Pressed
        {
            *block = true;
            s.repeat = s.repeat.saturating_mul(10).saturating_add(*digit);
            info!("repeat set to {}", s.repeat);
        }
    }
    // maybe repeat
    else if s.repeat != 0
        && state == KeyState::Pressed
        && !MODIFIERS.contains(&key)
        && key != KeyCode::KEY_CAPSLOCK
    {
        *block = true;
        info!("repeating {key:?} {} times", s.repeat);
        click_repeat(dev, key, s.repeat)?;
        s.repeat = 0;
    }

    Ok(())
}

fn emit(dev: &mut VirtualDevice, key: KeyCode, state: KeyState) -> io::Result<()> {
    trace!("virtual {key:?} {state:?}");
    dev.emit(&[*KeyEvent::new_now(key, state.into())])
}

#[expect(dead_code)]
fn press(dev: &mut VirtualDevice, key: KeyCode) -> io::Result<()> {
    emit(dev, key, KeyState::Pressed)
}

fn release(dev: &mut VirtualDevice, key: KeyCode) -> io::Result<()> {
    emit(dev, key, KeyState::Released)
}

fn click(dev: &mut VirtualDevice, key: KeyCode) -> io::Result<()> {
    trace!("virtual {key:?} clicked");
    dev.emit(&[
        *KeyEvent::new(key, KeyState::Pressed.into()),
        *KeyEvent::new(key, KeyState::Released.into()),
    ])
}

fn click_repeat(dev: &mut VirtualDevice, key: KeyCode, repeat: u16) -> io::Result<()> {
    trace!("virtual {key:?} clicked {repeat} times");
    for _ in 0..repeat {
        click(dev, key)?;
        // repeating clicks too quickly makes them fail sometimes.
        // a small delay works to make it fully consistent.
        // blocking the thread is also what we want,
        // e.g. if i type 100 down then X, I want the X to only
        // appear after I finish the 100 down.
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    Ok(())
}

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
