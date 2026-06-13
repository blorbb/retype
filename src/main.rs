use std::{
    collections::HashSet, convert::Infallible, ffi::OsStr, io, option::Option, path::Path,
    process::ExitCode, sync::mpsc, thread, time::Duration,
};

use anyhow::{Context, bail};
use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind};
use evdev::{EventSummary, EventType, KeyCode, KeyEvent, uinput::VirtualDevice};
use tracing::{debug, error, info, level_filters::LevelFilter, trace, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
use unicode_segmentation::UnicodeSegmentation;

mod keys;

const RETYPE_KEYBOARD: &str = "retype-keyboard";
const DEVICES_DIR: &str = "/dev/input";

fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    // uncomment if doing something dangerous
    // std::thread::spawn(|| {
    //     std::thread::sleep(std::time::Duration::from_secs(20));
    //     error!("killing");
    //     std::process::exit(1);
    // });

    let kbs: Vec<_> = evdev::enumerate()
        .map(|(_path, dev)| dev)
        .filter(probably_keyboard_filter)
        .collect();

    let virtual_dev = VirtualDevice::builder()?
        .name(RETYPE_KEYBOARD)
        .with_keys(keys::all_keys())?
        .build()?;

    let (grabber, events) = KbGrabber::new();
    listen_for_kb_conns(grabber.clone());
    kbs.into_iter().for_each(|kb| grabber.add_keyboard(kb));

    let mut handler = KbEventHandler::new(virtual_dev);

    while let Some((kb, events)) = events.recv() {
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

    // everything is ungrabbed when retype exits,
    // even if the devices aren't dropped (which calls ungrab)
    Ok(())
}

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

#[derive(Clone)]
struct KbGrabber {
    tx: mpsc::Sender<(&'static str, Vec<evdev::InputEvent>)>,
}

struct KbEventListener {
    rx: mpsc::Receiver<(&'static str, Vec<evdev::InputEvent>)>,
}

impl KbGrabber {
    fn new() -> (Self, KbEventListener) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, KbEventListener { rx })
    }

    /// Grabs and listens to events from the provided device.
    fn add_keyboard(&self, mut kb: evdev::Device) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match kb.grab() {
                Ok(()) => info!("grabbed {:?}", kb.name()),
                Err(e) => error!("failed to grab {:?}: {e:#}", kb.name()),
            };

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
}

impl KbEventListener {
    fn recv(&self) -> Option<(&str, Vec<evdev::InputEvent>)> {
        self.rx.recv().ok()
    }
}

fn listen_for_kb_conns(grabber: KbGrabber) {
    let filter_event = move |ev: inotify::Event<&OsStr>| {
        let name = ev
            .name
            .context("missing name")?
            .to_str()
            .context("non-utf8 device name")?;
        if !name.starts_with("event") {
            bail!("device {name} does not start with 'event'");
        }

        let dev = evdev::Device::open(Path::new(DEVICES_DIR).join(name))
            .context(format!("failed to open device {DEVICES_DIR}/{name}"))?;

        if dev.name() == Some(RETYPE_KEYBOARD) {
            bail!("keyboard is retype's keyboard");
        }
        info!("found keyboard {:?}", dev.name());
        if !probably_keyboard_filter(&dev) {
            bail!("device {:?} is not a keyboard", dev.name());
        }

        Ok(dev)
    };

    std::thread::spawn(move || {
        let span = tracing::info_span!("inotify");
        let _enter = span.enter();

        let Err(e): anyhow::Result<Infallible> = (|| {
            let mut inotify = inotify::Inotify::init().context("failed to init")?;
            // listening to CREATE doesn't work because the permissions aren't set correctly on creation.
            // Need to watch for attribute changes immediately after the device is created.
            // The device should fail to be grabbed in cases other than the keyboard being created.
            inotify
                .watches()
                .add(DEVICES_DIR, inotify::WatchMask::ATTRIB)
                .context("failed to listen to /dev/input")?;

            let mut buf = [0; 1024];
            loop {
                let events = inotify
                    .read_events_blocking(&mut buf)
                    .context("failed to read events")?;
                for ev in events {
                    info!("received event: {ev:?}");

                    match filter_event(ev) {
                        Ok(kb) => grabber.add_keyboard(kb),
                        // errors are normal
                        Err(e) => warn!("did not grab device: {e:#}"),
                    }
                }
            }
        })();

        error!("{e:#}");
    });
}

fn probably_keyboard_filter(dev: &evdev::Device) -> bool {
    let Some(keys) = dev.supported_keys() else {
        return false;
    };

    keys.contains(KeyCode::KEY_A)
        && keys.contains(KeyCode::KEY_Z)
        && keys.contains(KeyCode::KEY_ENTER)
        && keys.contains(KeyCode::KEY_SPACE)
        && dev
            .supported_relative_axes()
            .is_none_or(|axes| axes.iter().len() == 0)
        && keys.iter().all(|k| keys::all_keys().contains(k))
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

struct KbEventHandler {
    /// Avoid using this directly, prefer one of the methods on [`Handler`] instead.
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

impl KbEventHandler {
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
                    for k in keys::caps_remap().keys() {
                        if self.virtual_keys_pressed.contains(k) {
                            self.release(*k)?;
                        }
                    }
                }
                KeyState::Repeat => {}
                KeyState::Released => {
                    // press caps -> J -> release caps, then release what J was mapped to
                    for k in keys::caps_remap().values() {
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
                    // set to none now to avoid logs about cancelling find
                    // due to pressing right/left.
                    self.find = None;

                    info!("moving {move_amount} characters");

                    let pressed_modifiers = self.release_all_virtual()?;
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

                    // repress any modifiers that were held down before
                    pressed_modifiers
                        .into_iter()
                        .try_for_each(|k| self.press(k))?;
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
            if let Some(mapped) = keys::caps_remap().get(&key) {
                if self.repeat != 0 && state == KeyState::Pressed {
                    self.click_repeat(*mapped, self.repeat)?;
                    self.repeat = 0;
                } else {
                    self.emit(*mapped, state)?;
                }
                return Ok(());
            } else if let Some(digit) = keys::digits().get(&key)
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

                let pressed_modifiers = self.release_all_virtual()?;
                self.long_wait();
                self.press(KeyCode::KEY_LEFTSHIFT)?;
                self.long_wait();
                self.click(direction_key)?;
                self.long_wait();
                self.click(direction_key)?;
                self.long_wait();
                self.release(KeyCode::KEY_LEFTSHIFT)?;
                self.long_wait();
                self.click(return_to_normal_key)?;
                self.long_wait();

                pressed_modifiers
                    .into_iter()
                    .try_for_each(|k| self.press(k))?;

                match self.get_selection() {
                    Some(selection) => {
                        info!("found selection: {selection}");
                        self.find = Some(Find {
                            select: !keys::shifts().is_disjoint(&self.real_keys_pressed),
                            direction: if is_forwards {
                                Direction::Forwards
                            } else {
                                Direction::Backwards
                            },
                            text: selection,
                        })
                    }
                    None => error!("failed to get selection"),
                };

                return Ok(());
            }
        }
        // maybe repeat
        else if self.repeat != 0
            && state == KeyState::Pressed
            && !keys::modifiers().contains(&key)
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

    fn emit(&mut self, key: KeyCode, state: KeyState) -> io::Result<()> {
        debug!("virtual {key:?} {state:?}");
        self.vdev.emit(&[*KeyEvent::new_now(key, state.into())])?;
        set_pressed(&mut self.virtual_keys_pressed, key, state);
        if state == KeyState::Pressed && !keys::modifiers().contains(&key) {
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
        if !keys::modifiers().contains(&key) {
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

    /// A longer wait that is more consistently enough for apps to register.
    fn long_wait(&self) {
        thread::sleep(Duration::from_micros(1000));
    }

    /// Returns all modifiers that are currently pressed. These modifiers
    /// should probably be repressed after any virtual actions.
    fn release_all_virtual(&mut self) -> io::Result<Vec<KeyCode>> {
        debug!(
            "releasing all virtual keys: {:?}",
            self.virtual_keys_pressed
        );

        let mut modifiers = vec![];
        for k in self.virtual_keys_pressed.drain() {
            self.vdev
                .emit(&[*KeyEvent::new_now(k, KeyState::Released.into())])?;

            if keys::modifiers().contains(&k) {
                modifiers.push(k);
            }
        }

        Ok(modifiers)
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
        let pressing_other_modifier = self
            .real_keys_pressed
            .intersection(keys::modifiers())
            .find(|k| !keys::shifts().contains(*k))
            .is_some();
        if pressing_other_modifier {
            return None;
        }

        let shift = self
            .real_keys_pressed
            .intersection(keys::shifts())
            .next()
            .is_some();
        keys::to_char(key, shift)
    }
}

fn set_pressed(set: &mut HashSet<KeyCode>, key: KeyCode, state: KeyState) {
    match state {
        KeyState::Released => set.remove(&key),
        KeyState::Pressed | KeyState::Repeat => set.insert(key),
    };
}
