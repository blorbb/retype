use std::{collections::HashSet, process, sync::mpsc, time::Duration};

use evdev::{uinput::VirtualDevice, AttributeSet, EventSummary, EventType, KeyCode};

// mod caps;
// mod find;
// mod map;
// mod modifiers;
// mod num;
// mod send;
// mod tray;
// use crate::{modifiers::Modifier, num::IncrementalU16, tray::create_tray_item};

#[derive(Debug, Clone, Copy)]
enum KeyState {
    Pressed,
    Released,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(10));
        eprintln!("killing");
        process::exit(1);
    });

    let mut kbs: Vec<_> = evdev::enumerate()
        .map(|(_path, dev)| dev)
        .filter(|dev| {
            let Some(keys) = dev.supported_keys() else {
                return false;
            };

            [
                KeyCode::KEY_A,
                KeyCode::KEY_Z,
                KeyCode::KEY_ENTER,
                KeyCode::KEY_SPACE,
            ]
            .iter()
            .all(|key| keys.contains(*key))
        })
        .collect();

    let all_keys: AttributeSet<_> = kbs
        .iter()
        .filter_map(|kb| kb.supported_keys())
        .flatten()
        .collect();
    // mouse also supports a bunch of stuff that makes it look like a keyboard.
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
        println!("grabbing {:?}", kb.name());
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
                        eprintln!("error in {kb_name}: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    let mut state = GlobalState::new();

    while let Ok((_kb, events)) = rx.recv() {
        // println!("{kb} got event {events:?}");

        if events.iter().any(|ev| ev.event_type() == EventType::KEY) {
            for ev in events {
                match ev.destructure() {
                    EventSummary::Key(_ev, code, value) => {
                        let pressed = match value {
                            0 => KeyState::Released,
                            1 => KeyState::Pressed,
                            _ => {
                                // weird event, pass through
                                virtual_dev.emit(&[ev])?;
                                continue;
                            }
                        };

                        let mut block = false;
                        process_event(&mut state, &mut virtual_dev, code, pressed, &mut block);
                        if !block {
                            virtual_dev.emit(&[ev])?;
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

    Ok(())
}

fn process_event(
    state: &mut GlobalState,
    dev: &mut VirtualDevice,
    code: KeyCode,
    keystate: KeyState,
    // set to true to block this input
    block: &mut bool,
) {
    match keystate {
        KeyState::Pressed => state.keys_pressed.insert(code),
        KeyState::Released => state.keys_pressed.remove(&code),
    };

    eprintln!("{code:?} {keystate:?}");
}

struct GlobalState {
    keys_pressed: HashSet<KeyCode>,
}

impl GlobalState {
    pub fn new() -> Self {
        Self {
            keys_pressed: HashSet::new(),
        }
    }
}

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
