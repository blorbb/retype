use std::fs::File;

use caps::ActivateOnRelease;
use find::{Direction, Finder};
use rdev::{Event, Key};
use send::{click, is_pressed, KEYS_PRESSED};
use simplelog::Config;

mod caps;
mod find;
mod map;
mod modifiers;
mod num;
mod send;
mod tray;
use crate::{modifiers::Modifier, num::IncrementalU16, tray::create_tray_item};

fn main() {
    if let Some(data_dir) = dirs::data_dir() {
        _ = simplelog::WriteLogger::init(
            log::LevelFilter::Debug,
            Config::default(),
            File::create(data_dir.join("retype.log")).unwrap(),
        );
        log_panics::init();
    } else {
        panic!("unable to locate data directory");
    };
    gtk::init().unwrap();

    log::info!("hook installed");

    // digits of numbers pressed by CapsLock + number
    // occasionally cleared.
    let mut number_history = IncrementalU16::new();
    let mut find: Option<Finder> = None;
    let mut to_select = false;
    let mut enabled = true;
    // see `caps` module docs
    let mut caps_toggle = ActivateOnRelease::new(|| send::click(Key::CapsLock));

    let (mut tray, tray_rx) = create_tray_item();

    let mut handler = |ev: rdev::Event| -> Option<Event> {
        let passthrough = Some(ev.clone());

        if !matches!(
            ev.event_type,
            rdev::EventType::KeyPress(..) | rdev::EventType::KeyRelease(..)
        ) {
            return passthrough;
        }

        match tray_rx.try_recv() {
            Ok(tray::Message::Quit) => {
                log::info!("quitting application");
                panic!("shutting down");
            }
            Ok(tray::Message::ToggleEnabled) => {
                enabled = !enabled;
                tray::set_icon(&mut tray, enabled);
                log::info!("set enabled to {} via tray menu", enabled);
            }
            Err(_) => {}
        }

        // initial state management
        let key = match ev.event_type {
            rdev::EventType::KeyPress(key) => {
                KEYS_PRESSED.write().unwrap().insert(key);
                key
            }
            rdev::EventType::KeyRelease(key) => {
                KEYS_PRESSED.write().unwrap().remove(&key);

                if key == Key::CapsLock {
                    caps_toggle.maybe_activate();
                }

                return passthrough;
            }
            _ => return passthrough,
        };
        log::trace!("button event received: {ev:#?}");

        // toggle enable/disable
        if key == Key::KeyK && Modifier::Ctrl.is_pressed() && Modifier::Super.is_pressed() {
            enabled = !enabled;
            log::info!("set enabled to {} via hotkey", enabled);
            tray::set_icon(&mut tray, enabled);
        }

        // hotkeys after this only active if enabled //
        if !enabled {
            log::trace!("hotkeys not enabled, ignoring");
            number_history.clear();
            find = None;
            return passthrough;
        }

        caps_toggle.interrupt();

        // clear number history if any of these are pressed
        // maybe more in the future?
        if matches!(key, Key::Escape) {
            log::debug!("clearing history");
            number_history.clear();
            find = None;
        }

        // put this before the later remaps so that accidentally holding caps
        // when finding the next character won't remap to the next character
        if let Some(finder) = &find {
            log::trace!("key {:?} pressed after find", key);
            if let Some(char_to_find) = map::char_clicked(&ev) {
                log::info!("mapped keypress to character {char_to_find:?}");
                let selection = finder.selection.recv();
                log::info!("received selection '{selection}'");
                let direction = finder.direction;
                send::KEYPRESSES.run(move || {
                    find::move_to_char(&selection, char_to_find, direction, to_select)
                });
                find = None;

                return None;
            }
        };

        // simple remaps
        if is_pressed(Key::CapsLock) {
            log::trace!("caps is pressed");
            if let Some(btn) = map::caps_remap(key) {
                log::info!("remapped {:?} to {:?}", key, btn);
                send::maybe_repeat(btn, &mut number_history);
                send::KEYPRESSES.run(move || {
                    // one more click as repeat expects passthrough
                    click(btn);
                });
                return None;
            }
            if let Some(digit) = map::number_key_to_digit(key) {
                log::info!("adding digit {digit} to repetitions");
                number_history.push_digit(digit);
                return None;
            }
            if key == Key::KeyF {
                find = Some(Finder::new(Direction::Forward));
                to_select = Modifier::Shift.is_pressed();
                log::info!("finding forward on next char, with selection = {to_select}");
                return None;
            }
            if key == Key::KeyD {
                find = Some(Finder::new(Direction::Backward));
                to_select = Modifier::Shift.is_pressed();
                log::info!("finding backward on next char, with selection = {to_select}");
                return None;
            }
        }

        // disable capslock button, only toggle if Super is also pressed
        if key == Key::CapsLock {
            if Modifier::Super.is_pressed() {
                caps_toggle.await_release();
            }
            return None;
        }

        // repeat button multiple times if number_history has anything
        send::maybe_repeat(key, &mut number_history);
        passthrough
    };

    while let Err(e) = rdev::grab(&mut handler) {
        log::error!("error grabbing inputs: {e:?}");
        log::info!("restarting grab handler");
    }

    log::info!("application stopped");
}
