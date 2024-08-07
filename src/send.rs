use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

use std::thread;

use rdev::Key;
use std::sync::mpsc;

use crate::modifiers;
use crate::num::IncrementalU16;

pub struct Thread {
    sender: mpsc::Sender<Box<dyn FnOnce() + Send>>,
}

impl Thread {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        thread::spawn(move || {
            while let Ok(closure) = receiver.recv() {
                log::trace!("received closure to run");
                closure()
            }
        });
        Self { sender }
    }

    pub fn run<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.sender.send(Box::new(f)).expect("failed to send");
    }
}

pub static KEYS_PRESSED: LazyLock<RwLock<HashSet<Key>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

// Recursive or repeated button clicks should be run in a new thread
// Avoid creating this new thread every time a click is made, just reuse an old one.
pub(crate) static KEYPRESSES: LazyLock<Thread> = LazyLock::new(Thread::new);

/// Presses and releases the provided key.
pub fn click(key: rdev::Key) {
    press(key);
    release(key);
}

pub fn press(key: rdev::Key) {
    rdev::simulate(&rdev::EventType::KeyPress(key)).unwrap();
}

pub fn release(key: rdev::Key) {
    rdev::simulate(&rdev::EventType::KeyRelease(key)).unwrap();
}

pub fn is_pressed(key: rdev::Key) -> bool {
    KEYS_PRESSED.read().unwrap().contains(&key)
}

pub fn maybe_repeat(button: Key, number_history: &mut IncrementalU16) {
    if number_history.value() == 0 {
        log::trace!("no number history, not repeating");
        return;
    }

    if modifiers::is_modifier(button) || button == Key::CapsLock {
        log::trace!("not repeating key {button:?}");
        return;
    }

    let repeats = number_history.value();
    number_history.clear();

    log::debug!("repeating key {button:?} {repeats} times");
    KEYPRESSES.run(move || {
        // automatically clicks once, so repeat - 1
        // saturate just in case
        for _ in 0..repeats.saturating_sub(1) {
            click(button)
        }
    });
}
