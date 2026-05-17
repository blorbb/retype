use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    ffi::OsStr,
    io,
    option::Option,
    path::Path,
    process::ExitCode,
    sync::{LazyLock, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context, bail};
use arboard::{Clipboard, GetExtLinux, LinuxClipboardKind};
use evdev::{AttributeSet, EventSummary, EventType, KeyCode, KeyEvent, uinput::VirtualDevice};
use tracing::{debug, error, info, level_filters::LevelFilter, trace, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};
use unicode_segmentation::UnicodeSegmentation;

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
        .with_keys(&ALL_KEYS)?
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
        && keys.iter().all(|k| ALL_KEYS.contains(k))
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

    fn emit(&mut self, key: KeyCode, state: KeyState) -> io::Result<()> {
        debug!("virtual {key:?} {state:?}");
        self.vdev.emit(&[*KeyEvent::new_now(key, state.into())])?;
        set_pressed(&mut self.virtual_keys_pressed, key, state);
        if state == KeyState::Pressed && !MODIFIERS.contains(&key) {
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
        if !MODIFIERS.contains(&key) {
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
        let pressing_other_modifier = self
            .real_keys_pressed
            .intersection(&MODIFIERS)
            .find(|k| !SHIFT.contains(*k))
            .is_some();
        if pressing_other_modifier {
            return None;
        }

        let shift = self.real_keys_pressed.intersection(&SHIFT).next().is_some();
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

static SHIFT: LazyLock<HashSet<KeyCode>> =
    LazyLock::new(|| HashSet::from_iter([KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT]));

// Copied from the private `KeyCode::NAME_MAP` const.
const NAME_MAP: &[(&str, KeyCode)] = &[
    ("KEY_RESERVED", KeyCode(0)),
    ("KEY_ESC", KeyCode(1)),
    ("KEY_1", KeyCode(2)),
    ("KEY_2", KeyCode(3)),
    ("KEY_3", KeyCode(4)),
    ("KEY_4", KeyCode(5)),
    ("KEY_5", KeyCode(6)),
    ("KEY_6", KeyCode(7)),
    ("KEY_7", KeyCode(8)),
    ("KEY_8", KeyCode(9)),
    ("KEY_9", KeyCode(10)),
    ("KEY_0", KeyCode(11)),
    ("KEY_MINUS", KeyCode(12)),
    ("KEY_EQUAL", KeyCode(13)),
    ("KEY_BACKSPACE", KeyCode(14)),
    ("KEY_TAB", KeyCode(15)),
    ("KEY_Q", KeyCode(16)),
    ("KEY_W", KeyCode(17)),
    ("KEY_E", KeyCode(18)),
    ("KEY_R", KeyCode(19)),
    ("KEY_T", KeyCode(20)),
    ("KEY_Y", KeyCode(21)),
    ("KEY_U", KeyCode(22)),
    ("KEY_I", KeyCode(23)),
    ("KEY_O", KeyCode(24)),
    ("KEY_P", KeyCode(25)),
    ("KEY_LEFTBRACE", KeyCode(26)),
    ("KEY_RIGHTBRACE", KeyCode(27)),
    ("KEY_ENTER", KeyCode(28)),
    ("KEY_LEFTCTRL", KeyCode(29)),
    ("KEY_A", KeyCode(30)),
    ("KEY_S", KeyCode(31)),
    ("KEY_D", KeyCode(32)),
    ("KEY_F", KeyCode(33)),
    ("KEY_G", KeyCode(34)),
    ("KEY_H", KeyCode(35)),
    ("KEY_J", KeyCode(36)),
    ("KEY_K", KeyCode(37)),
    ("KEY_L", KeyCode(38)),
    ("KEY_SEMICOLON", KeyCode(39)),
    ("KEY_APOSTROPHE", KeyCode(40)),
    ("KEY_GRAVE", KeyCode(41)),
    ("KEY_LEFTSHIFT", KeyCode(42)),
    ("KEY_BACKSLASH", KeyCode(43)),
    ("KEY_Z", KeyCode(44)),
    ("KEY_X", KeyCode(45)),
    ("KEY_C", KeyCode(46)),
    ("KEY_V", KeyCode(47)),
    ("KEY_B", KeyCode(48)),
    ("KEY_N", KeyCode(49)),
    ("KEY_M", KeyCode(50)),
    ("KEY_COMMA", KeyCode(51)),
    ("KEY_DOT", KeyCode(52)),
    ("KEY_SLASH", KeyCode(53)),
    ("KEY_RIGHTSHIFT", KeyCode(54)),
    ("KEY_KPASTERISK", KeyCode(55)),
    ("KEY_LEFTALT", KeyCode(56)),
    ("KEY_SPACE", KeyCode(57)),
    ("KEY_CAPSLOCK", KeyCode(58)),
    ("KEY_F1", KeyCode(59)),
    ("KEY_F2", KeyCode(60)),
    ("KEY_F3", KeyCode(61)),
    ("KEY_F4", KeyCode(62)),
    ("KEY_F5", KeyCode(63)),
    ("KEY_F6", KeyCode(64)),
    ("KEY_F7", KeyCode(65)),
    ("KEY_F8", KeyCode(66)),
    ("KEY_F9", KeyCode(67)),
    ("KEY_F10", KeyCode(68)),
    ("KEY_NUMLOCK", KeyCode(69)),
    ("KEY_SCROLLLOCK", KeyCode(70)),
    ("KEY_KP7", KeyCode(71)),
    ("KEY_KP8", KeyCode(72)),
    ("KEY_KP9", KeyCode(73)),
    ("KEY_KPMINUS", KeyCode(74)),
    ("KEY_KP4", KeyCode(75)),
    ("KEY_KP5", KeyCode(76)),
    ("KEY_KP6", KeyCode(77)),
    ("KEY_KPPLUS", KeyCode(78)),
    ("KEY_KP1", KeyCode(79)),
    ("KEY_KP2", KeyCode(80)),
    ("KEY_KP3", KeyCode(81)),
    ("KEY_KP0", KeyCode(82)),
    ("KEY_KPDOT", KeyCode(83)),
    ("KEY_ZENKAKUHANKAKU", KeyCode(85)),
    ("KEY_102ND", KeyCode(86)),
    ("KEY_F11", KeyCode(87)),
    ("KEY_F12", KeyCode(88)),
    ("KEY_RO", KeyCode(89)),
    ("KEY_KATAKANA", KeyCode(90)),
    ("KEY_HIRAGANA", KeyCode(91)),
    ("KEY_HENKAN", KeyCode(92)),
    ("KEY_KATAKANAHIRAGANA", KeyCode(93)),
    ("KEY_MUHENKAN", KeyCode(94)),
    ("KEY_KPJPCOMMA", KeyCode(95)),
    ("KEY_KPENTER", KeyCode(96)),
    ("KEY_RIGHTCTRL", KeyCode(97)),
    ("KEY_KPSLASH", KeyCode(98)),
    ("KEY_SYSRQ", KeyCode(99)),
    ("KEY_RIGHTALT", KeyCode(100)),
    ("KEY_LINEFEED", KeyCode(101)),
    ("KEY_HOME", KeyCode(102)),
    ("KEY_UP", KeyCode(103)),
    ("KEY_PAGEUP", KeyCode(104)),
    ("KEY_LEFT", KeyCode(105)),
    ("KEY_RIGHT", KeyCode(106)),
    ("KEY_END", KeyCode(107)),
    ("KEY_DOWN", KeyCode(108)),
    ("KEY_PAGEDOWN", KeyCode(109)),
    ("KEY_INSERT", KeyCode(110)),
    ("KEY_DELETE", KeyCode(111)),
    ("KEY_MACRO", KeyCode(112)),
    ("KEY_MUTE", KeyCode(113)),
    ("KEY_VOLUMEDOWN", KeyCode(114)),
    ("KEY_VOLUMEUP", KeyCode(115)),
    ("KEY_POWER", KeyCode(116)),
    ("KEY_KPEQUAL", KeyCode(117)),
    ("KEY_KPPLUSMINUS", KeyCode(118)),
    ("KEY_PAUSE", KeyCode(119)),
    ("KEY_SCALE", KeyCode(120)),
    ("KEY_KPCOMMA", KeyCode(121)),
    ("KEY_HANGEUL", KeyCode(122)),
    ("KEY_HANJA", KeyCode(123)),
    ("KEY_YEN", KeyCode(124)),
    ("KEY_LEFTMETA", KeyCode(125)),
    ("KEY_RIGHTMETA", KeyCode(126)),
    ("KEY_COMPOSE", KeyCode(127)),
    ("KEY_STOP", KeyCode(128)),
    ("KEY_AGAIN", KeyCode(129)),
    ("KEY_PROPS", KeyCode(130)),
    ("KEY_UNDO", KeyCode(131)),
    ("KEY_FRONT", KeyCode(132)),
    ("KEY_COPY", KeyCode(133)),
    ("KEY_OPEN", KeyCode(134)),
    ("KEY_PASTE", KeyCode(135)),
    ("KEY_FIND", KeyCode(136)),
    ("KEY_CUT", KeyCode(137)),
    ("KEY_HELP", KeyCode(138)),
    ("KEY_MENU", KeyCode(139)),
    ("KEY_CALC", KeyCode(140)),
    ("KEY_SETUP", KeyCode(141)),
    ("KEY_SLEEP", KeyCode(142)),
    ("KEY_WAKEUP", KeyCode(143)),
    ("KEY_FILE", KeyCode(144)),
    ("KEY_SENDFILE", KeyCode(145)),
    ("KEY_DELETEFILE", KeyCode(146)),
    ("KEY_XFER", KeyCode(147)),
    ("KEY_PROG1", KeyCode(148)),
    ("KEY_PROG2", KeyCode(149)),
    ("KEY_WWW", KeyCode(150)),
    ("KEY_MSDOS", KeyCode(151)),
    ("KEY_COFFEE", KeyCode(152)),
    ("KEY_DIRECTION", KeyCode(153)),
    ("KEY_ROTATE_DISPLAY", KeyCode(153)),
    ("KEY_CYCLEWINDOWS", KeyCode(154)),
    ("KEY_MAIL", KeyCode(155)),
    ("KEY_BOOKMARKS", KeyCode(156)),
    ("KEY_COMPUTER", KeyCode(157)),
    ("KEY_BACK", KeyCode(158)),
    ("KEY_FORWARD", KeyCode(159)),
    ("KEY_CLOSECD", KeyCode(160)),
    ("KEY_EJECTCD", KeyCode(161)),
    ("KEY_EJECTCLOSECD", KeyCode(162)),
    ("KEY_NEXTSONG", KeyCode(163)),
    ("KEY_PLAYPAUSE", KeyCode(164)),
    ("KEY_PREVIOUSSONG", KeyCode(165)),
    ("KEY_STOPCD", KeyCode(166)),
    ("KEY_RECORD", KeyCode(167)),
    ("KEY_REWIND", KeyCode(168)),
    ("KEY_PHONE", KeyCode(169)),
    ("KEY_ISO", KeyCode(170)),
    ("KEY_CONFIG", KeyCode(171)),
    ("KEY_HOMEPAGE", KeyCode(172)),
    ("KEY_REFRESH", KeyCode(173)),
    ("KEY_EXIT", KeyCode(174)),
    ("KEY_MOVE", KeyCode(175)),
    ("KEY_EDIT", KeyCode(176)),
    ("KEY_SCROLLUP", KeyCode(177)),
    ("KEY_SCROLLDOWN", KeyCode(178)),
    ("KEY_KPLEFTPAREN", KeyCode(179)),
    ("KEY_KPRIGHTPAREN", KeyCode(180)),
    ("KEY_NEW", KeyCode(181)),
    ("KEY_REDO", KeyCode(182)),
    ("KEY_F13", KeyCode(183)),
    ("KEY_F14", KeyCode(184)),
    ("KEY_F15", KeyCode(185)),
    ("KEY_F16", KeyCode(186)),
    ("KEY_F17", KeyCode(187)),
    ("KEY_F18", KeyCode(188)),
    ("KEY_F19", KeyCode(189)),
    ("KEY_F20", KeyCode(190)),
    ("KEY_F21", KeyCode(191)),
    ("KEY_F22", KeyCode(192)),
    ("KEY_F23", KeyCode(193)),
    ("KEY_F24", KeyCode(194)),
    ("KEY_PLAYCD", KeyCode(200)),
    ("KEY_PAUSECD", KeyCode(201)),
    ("KEY_PROG3", KeyCode(202)),
    ("KEY_PROG4", KeyCode(203)),
    ("KEY_DASHBOARD", KeyCode(204)),
    ("KEY_SUSPEND", KeyCode(205)),
    ("KEY_CLOSE", KeyCode(206)),
    ("KEY_PLAY", KeyCode(207)),
    ("KEY_FASTFORWARD", KeyCode(208)),
    ("KEY_BASSBOOST", KeyCode(209)),
    ("KEY_PRINT", KeyCode(210)),
    ("KEY_HP", KeyCode(211)),
    ("KEY_CAMERA", KeyCode(212)),
    ("KEY_SOUND", KeyCode(213)),
    ("KEY_QUESTION", KeyCode(214)),
    ("KEY_EMAIL", KeyCode(215)),
    ("KEY_CHAT", KeyCode(216)),
    ("KEY_SEARCH", KeyCode(217)),
    ("KEY_CONNECT", KeyCode(218)),
    ("KEY_FINANCE", KeyCode(219)),
    ("KEY_SPORT", KeyCode(220)),
    ("KEY_SHOP", KeyCode(221)),
    ("KEY_ALTERASE", KeyCode(222)),
    ("KEY_CANCEL", KeyCode(223)),
    ("KEY_BRIGHTNESSDOWN", KeyCode(224)),
    ("KEY_BRIGHTNESSUP", KeyCode(225)),
    ("KEY_MEDIA", KeyCode(226)),
    ("KEY_SWITCHVIDEOMODE", KeyCode(227)),
    ("KEY_KBDILLUMTOGGLE", KeyCode(228)),
    ("KEY_KBDILLUMDOWN", KeyCode(229)),
    ("KEY_KBDILLUMUP", KeyCode(230)),
    ("KEY_SEND", KeyCode(231)),
    ("KEY_REPLY", KeyCode(232)),
    ("KEY_FORWARDMAIL", KeyCode(233)),
    ("KEY_SAVE", KeyCode(234)),
    ("KEY_DOCUMENTS", KeyCode(235)),
    ("KEY_BATTERY", KeyCode(236)),
    ("KEY_BLUETOOTH", KeyCode(237)),
    ("KEY_WLAN", KeyCode(238)),
    ("KEY_UWB", KeyCode(239)),
    ("KEY_UNKNOWN", KeyCode(240)),
    ("KEY_VIDEO_NEXT", KeyCode(241)),
    ("KEY_VIDEO_PREV", KeyCode(242)),
    ("KEY_BRIGHTNESS_CYCLE", KeyCode(243)),
    ("KEY_BRIGHTNESS_AUTO", KeyCode(244)),
    ("KEY_DISPLAY_OFF", KeyCode(245)),
    ("KEY_WWAN", KeyCode(246)),
    ("KEY_RFKILL", KeyCode(247)),
    ("KEY_MICMUTE", KeyCode(248)),
    ("BTN_0", KeyCode(256)),
    ("BTN_1", KeyCode(257)),
    ("BTN_2", KeyCode(258)),
    ("BTN_3", KeyCode(259)),
    ("BTN_4", KeyCode(260)),
    ("BTN_5", KeyCode(261)),
    ("BTN_6", KeyCode(262)),
    ("BTN_7", KeyCode(263)),
    ("BTN_8", KeyCode(264)),
    ("BTN_9", KeyCode(265)),
    ("BTN_LEFT", KeyCode(272)),
    ("BTN_RIGHT", KeyCode(273)),
    ("BTN_MIDDLE", KeyCode(274)),
    ("BTN_SIDE", KeyCode(275)),
    ("BTN_EXTRA", KeyCode(276)),
    ("BTN_FORWARD", KeyCode(277)),
    ("BTN_BACK", KeyCode(278)),
    ("BTN_TASK", KeyCode(279)),
    ("BTN_TRIGGER", KeyCode(288)),
    ("BTN_THUMB", KeyCode(289)),
    ("BTN_THUMB2", KeyCode(290)),
    ("BTN_TOP", KeyCode(291)),
    ("BTN_TOP2", KeyCode(292)),
    ("BTN_PINKIE", KeyCode(293)),
    ("BTN_BASE", KeyCode(294)),
    ("BTN_BASE2", KeyCode(295)),
    ("BTN_BASE3", KeyCode(296)),
    ("BTN_BASE4", KeyCode(297)),
    ("BTN_BASE5", KeyCode(298)),
    ("BTN_BASE6", KeyCode(299)),
    ("BTN_DEAD", KeyCode(303)),
    ("BTN_SOUTH", KeyCode(304)),
    ("BTN_EAST", KeyCode(305)),
    ("BTN_C", KeyCode(306)),
    ("BTN_NORTH", KeyCode(307)),
    ("BTN_WEST", KeyCode(308)),
    ("BTN_Z", KeyCode(309)),
    ("BTN_TL", KeyCode(310)),
    ("BTN_TR", KeyCode(311)),
    ("BTN_TL2", KeyCode(312)),
    ("BTN_TR2", KeyCode(313)),
    ("BTN_SELECT", KeyCode(314)),
    ("BTN_START", KeyCode(315)),
    ("BTN_MODE", KeyCode(316)),
    ("BTN_THUMBL", KeyCode(317)),
    ("BTN_THUMBR", KeyCode(318)),
    ("BTN_TOOL_PEN", KeyCode(320)),
    ("BTN_TOOL_RUBBER", KeyCode(321)),
    ("BTN_TOOL_BRUSH", KeyCode(322)),
    ("BTN_TOOL_PENCIL", KeyCode(323)),
    ("BTN_TOOL_AIRBRUSH", KeyCode(324)),
    ("BTN_TOOL_FINGER", KeyCode(325)),
    ("BTN_TOOL_MOUSE", KeyCode(326)),
    ("BTN_TOOL_LENS", KeyCode(327)),
    ("BTN_TOOL_QUINTTAP", KeyCode(328)),
    ("BTN_TOUCH", KeyCode(330)),
    ("BTN_STYLUS", KeyCode(331)),
    ("BTN_STYLUS2", KeyCode(332)),
    ("BTN_TOOL_DOUBLETAP", KeyCode(333)),
    ("BTN_TOOL_TRIPLETAP", KeyCode(334)),
    ("BTN_TOOL_QUADTAP", KeyCode(335)),
    ("BTN_GEAR_DOWN", KeyCode(336)),
    ("BTN_GEAR_UP", KeyCode(337)),
    ("KEY_OK", KeyCode(352)),
    ("KEY_SELECT", KeyCode(353)),
    ("KEY_GOTO", KeyCode(354)),
    ("KEY_CLEAR", KeyCode(355)),
    ("KEY_POWER2", KeyCode(356)),
    ("KEY_OPTION", KeyCode(357)),
    ("KEY_INFO", KeyCode(358)),
    ("KEY_TIME", KeyCode(359)),
    ("KEY_VENDOR", KeyCode(360)),
    ("KEY_ARCHIVE", KeyCode(361)),
    ("KEY_PROGRAM", KeyCode(362)),
    ("KEY_CHANNEL", KeyCode(363)),
    ("KEY_FAVORITES", KeyCode(364)),
    ("KEY_EPG", KeyCode(365)),
    ("KEY_PVR", KeyCode(366)),
    ("KEY_MHP", KeyCode(367)),
    ("KEY_LANGUAGE", KeyCode(368)),
    ("KEY_TITLE", KeyCode(369)),
    ("KEY_SUBTITLE", KeyCode(370)),
    ("KEY_ANGLE", KeyCode(371)),
    ("KEY_ZOOM", KeyCode(372)),
    ("KEY_FULL_SCREEN", KeyCode(372)),
    ("KEY_MODE", KeyCode(373)),
    ("KEY_KEYBOARD", KeyCode(374)),
    ("KEY_SCREEN", KeyCode(375)),
    ("KEY_PC", KeyCode(376)),
    ("KEY_TV", KeyCode(377)),
    ("KEY_TV2", KeyCode(378)),
    ("KEY_VCR", KeyCode(379)),
    ("KEY_VCR2", KeyCode(380)),
    ("KEY_SAT", KeyCode(381)),
    ("KEY_SAT2", KeyCode(382)),
    ("KEY_CD", KeyCode(383)),
    ("KEY_TAPE", KeyCode(384)),
    ("KEY_RADIO", KeyCode(385)),
    ("KEY_TUNER", KeyCode(386)),
    ("KEY_PLAYER", KeyCode(387)),
    ("KEY_TEXT", KeyCode(388)),
    ("KEY_DVD", KeyCode(389)),
    ("KEY_AUX", KeyCode(390)),
    ("KEY_MP3", KeyCode(391)),
    ("KEY_AUDIO", KeyCode(392)),
    ("KEY_VIDEO", KeyCode(393)),
    ("KEY_DIRECTORY", KeyCode(394)),
    ("KEY_LIST", KeyCode(395)),
    ("KEY_MEMO", KeyCode(396)),
    ("KEY_CALENDAR", KeyCode(397)),
    ("KEY_RED", KeyCode(398)),
    ("KEY_GREEN", KeyCode(399)),
    ("KEY_YELLOW", KeyCode(400)),
    ("KEY_BLUE", KeyCode(401)),
    ("KEY_CHANNELUP", KeyCode(402)),
    ("KEY_CHANNELDOWN", KeyCode(403)),
    ("KEY_FIRST", KeyCode(404)),
    ("KEY_LAST", KeyCode(405)),
    ("KEY_AB", KeyCode(406)),
    ("KEY_NEXT", KeyCode(407)),
    ("KEY_RESTART", KeyCode(408)),
    ("KEY_SLOW", KeyCode(409)),
    ("KEY_SHUFFLE", KeyCode(410)),
    ("KEY_BREAK", KeyCode(411)),
    ("KEY_PREVIOUS", KeyCode(412)),
    ("KEY_DIGITS", KeyCode(413)),
    ("KEY_TEEN", KeyCode(414)),
    ("KEY_TWEN", KeyCode(415)),
    ("KEY_VIDEOPHONE", KeyCode(416)),
    ("KEY_GAMES", KeyCode(417)),
    ("KEY_ZOOMIN", KeyCode(418)),
    ("KEY_ZOOMOUT", KeyCode(419)),
    ("KEY_ZOOMRESET", KeyCode(420)),
    ("KEY_WORDPROCESSOR", KeyCode(421)),
    ("KEY_EDITOR", KeyCode(422)),
    ("KEY_SPREADSHEET", KeyCode(423)),
    ("KEY_GRAPHICSEDITOR", KeyCode(424)),
    ("KEY_PRESENTATION", KeyCode(425)),
    ("KEY_DATABASE", KeyCode(426)),
    ("KEY_NEWS", KeyCode(427)),
    ("KEY_VOICEMAIL", KeyCode(428)),
    ("KEY_ADDRESSBOOK", KeyCode(429)),
    ("KEY_MESSENGER", KeyCode(430)),
    ("KEY_DISPLAYTOGGLE", KeyCode(431)),
    ("KEY_SPELLCHECK", KeyCode(432)),
    ("KEY_LOGOFF", KeyCode(433)),
    ("KEY_DOLLAR", KeyCode(434)),
    ("KEY_EURO", KeyCode(435)),
    ("KEY_FRAMEBACK", KeyCode(436)),
    ("KEY_FRAMEFORWARD", KeyCode(437)),
    ("KEY_CONTEXT_MENU", KeyCode(438)),
    ("KEY_MEDIA_REPEAT", KeyCode(439)),
    ("KEY_10CHANNELSUP", KeyCode(440)),
    ("KEY_10CHANNELSDOWN", KeyCode(441)),
    ("KEY_IMAGES", KeyCode(442)),
    ("KEY_PICKUP_PHONE", KeyCode(445)),
    ("KEY_HANGUP_PHONE", KeyCode(446)),
    ("KEY_DEL_EOL", KeyCode(448)),
    ("KEY_DEL_EOS", KeyCode(449)),
    ("KEY_INS_LINE", KeyCode(450)),
    ("KEY_DEL_LINE", KeyCode(451)),
    ("KEY_FN", KeyCode(464)),
    ("KEY_FN_ESC", KeyCode(465)),
    ("KEY_FN_F1", KeyCode(466)),
    ("KEY_FN_F2", KeyCode(467)),
    ("KEY_FN_F3", KeyCode(468)),
    ("KEY_FN_F4", KeyCode(469)),
    ("KEY_FN_F5", KeyCode(470)),
    ("KEY_FN_F6", KeyCode(471)),
    ("KEY_FN_F7", KeyCode(472)),
    ("KEY_FN_F8", KeyCode(473)),
    ("KEY_FN_F9", KeyCode(474)),
    ("KEY_FN_F10", KeyCode(475)),
    ("KEY_FN_F11", KeyCode(476)),
    ("KEY_FN_F12", KeyCode(477)),
    ("KEY_FN_1", KeyCode(478)),
    ("KEY_FN_2", KeyCode(479)),
    ("KEY_FN_D", KeyCode(480)),
    ("KEY_FN_E", KeyCode(481)),
    ("KEY_FN_F", KeyCode(482)),
    ("KEY_FN_S", KeyCode(483)),
    ("KEY_FN_B", KeyCode(484)),
    ("KEY_BRL_DOT1", KeyCode(497)),
    ("KEY_BRL_DOT2", KeyCode(498)),
    ("KEY_BRL_DOT3", KeyCode(499)),
    ("KEY_BRL_DOT4", KeyCode(500)),
    ("KEY_BRL_DOT5", KeyCode(501)),
    ("KEY_BRL_DOT6", KeyCode(502)),
    ("KEY_BRL_DOT7", KeyCode(503)),
    ("KEY_BRL_DOT8", KeyCode(504)),
    ("KEY_BRL_DOT9", KeyCode(505)),
    ("KEY_BRL_DOT10", KeyCode(506)),
    ("KEY_NUMERIC_0", KeyCode(512)),
    ("KEY_NUMERIC_1", KeyCode(513)),
    ("KEY_NUMERIC_2", KeyCode(514)),
    ("KEY_NUMERIC_3", KeyCode(515)),
    ("KEY_NUMERIC_4", KeyCode(516)),
    ("KEY_NUMERIC_5", KeyCode(517)),
    ("KEY_NUMERIC_6", KeyCode(518)),
    ("KEY_NUMERIC_7", KeyCode(519)),
    ("KEY_NUMERIC_8", KeyCode(520)),
    ("KEY_NUMERIC_9", KeyCode(521)),
    ("KEY_NUMERIC_STAR", KeyCode(522)),
    ("KEY_NUMERIC_POUND", KeyCode(523)),
    ("KEY_NUMERIC_A", KeyCode(524)),
    ("KEY_NUMERIC_B", KeyCode(525)),
    ("KEY_NUMERIC_C", KeyCode(526)),
    ("KEY_NUMERIC_D", KeyCode(527)),
    ("KEY_CAMERA_FOCUS", KeyCode(528)),
    ("KEY_WPS_BUTTON", KeyCode(529)),
    ("KEY_TOUCHPAD_TOGGLE", KeyCode(530)),
    ("KEY_TOUCHPAD_ON", KeyCode(531)),
    ("KEY_TOUCHPAD_OFF", KeyCode(532)),
    ("KEY_CAMERA_ZOOMIN", KeyCode(533)),
    ("KEY_CAMERA_ZOOMOUT", KeyCode(534)),
    ("KEY_CAMERA_UP", KeyCode(535)),
    ("KEY_CAMERA_DOWN", KeyCode(536)),
    ("KEY_CAMERA_LEFT", KeyCode(537)),
    ("KEY_CAMERA_RIGHT", KeyCode(538)),
    ("KEY_ATTENDANT_ON", KeyCode(539)),
    ("KEY_ATTENDANT_OFF", KeyCode(540)),
    ("KEY_ATTENDANT_TOGGLE", KeyCode(541)),
    ("KEY_LIGHTS_TOGGLE", KeyCode(542)),
    ("BTN_DPAD_UP", KeyCode(544)),
    ("BTN_DPAD_DOWN", KeyCode(545)),
    ("BTN_DPAD_LEFT", KeyCode(546)),
    ("BTN_DPAD_RIGHT", KeyCode(547)),
    ("KEY_ALS_TOGGLE", KeyCode(560)),
    ("KEY_BUTTONCONFIG", KeyCode(576)),
    ("KEY_TASKMANAGER", KeyCode(577)),
    ("KEY_JOURNAL", KeyCode(578)),
    ("KEY_CONTROLPANEL", KeyCode(579)),
    ("KEY_APPSELECT", KeyCode(580)),
    ("KEY_SCREENSAVER", KeyCode(581)),
    ("KEY_VOICECOMMAND", KeyCode(582)),
    ("KEY_ASSISTANT", KeyCode(583)),
    ("KEY_KBD_LAYOUT_NEXT", KeyCode(584)),
    ("KEY_BRIGHTNESS_MIN", KeyCode(592)),
    ("KEY_BRIGHTNESS_MAX", KeyCode(593)),
    ("KEY_KBDINPUTASSIST_PREV", KeyCode(608)),
    ("KEY_KBDINPUTASSIST_NEXT", KeyCode(609)),
    ("KEY_KBDINPUTASSIST_PREVGROUP", KeyCode(610)),
    ("KEY_KBDINPUTASSIST_NEXTGROUP", KeyCode(611)),
    ("KEY_KBDINPUTASSIST_ACCEPT", KeyCode(612)),
    ("KEY_KBDINPUTASSIST_CANCEL", KeyCode(613)),
    ("KEY_RIGHT_UP", KeyCode(614)),
    ("KEY_RIGHT_DOWN", KeyCode(615)),
    ("KEY_LEFT_UP", KeyCode(616)),
    ("KEY_LEFT_DOWN", KeyCode(617)),
    ("KEY_ROOT_MENU", KeyCode(618)),
    ("KEY_MEDIA_TOP_MENU", KeyCode(619)),
    ("KEY_NUMERIC_11", KeyCode(620)),
    ("KEY_NUMERIC_12", KeyCode(621)),
    ("KEY_AUDIO_DESC", KeyCode(622)),
    ("KEY_3D_MODE", KeyCode(623)),
    ("KEY_NEXT_FAVORITE", KeyCode(624)),
    ("KEY_STOP_RECORD", KeyCode(625)),
    ("KEY_PAUSE_RECORD", KeyCode(626)),
    ("KEY_VOD", KeyCode(627)),
    ("KEY_UNMUTE", KeyCode(628)),
    ("KEY_FASTREVERSE", KeyCode(629)),
    ("KEY_SLOWREVERSE", KeyCode(630)),
    ("KEY_DATA", KeyCode(631)),
    ("KEY_ONSCREEN_KEYBOARD", KeyCode(632)),
    ("KEY_PRIVACY_SCREEN_TOGGLE", KeyCode(633)),
    ("KEY_SELECTIVE_SCREENSHOT", KeyCode(634)),
    ("BTN_TRIGGER_HAPPY1", KeyCode(704)),
    ("BTN_TRIGGER_HAPPY2", KeyCode(705)),
    ("BTN_TRIGGER_HAPPY3", KeyCode(706)),
    ("BTN_TRIGGER_HAPPY4", KeyCode(707)),
    ("BTN_TRIGGER_HAPPY5", KeyCode(708)),
    ("BTN_TRIGGER_HAPPY6", KeyCode(709)),
    ("BTN_TRIGGER_HAPPY7", KeyCode(710)),
    ("BTN_TRIGGER_HAPPY8", KeyCode(711)),
    ("BTN_TRIGGER_HAPPY9", KeyCode(712)),
    ("BTN_TRIGGER_HAPPY10", KeyCode(713)),
    ("BTN_TRIGGER_HAPPY11", KeyCode(714)),
    ("BTN_TRIGGER_HAPPY12", KeyCode(715)),
    ("BTN_TRIGGER_HAPPY13", KeyCode(716)),
    ("BTN_TRIGGER_HAPPY14", KeyCode(717)),
    ("BTN_TRIGGER_HAPPY15", KeyCode(718)),
    ("BTN_TRIGGER_HAPPY16", KeyCode(719)),
    ("BTN_TRIGGER_HAPPY17", KeyCode(720)),
    ("BTN_TRIGGER_HAPPY18", KeyCode(721)),
    ("BTN_TRIGGER_HAPPY19", KeyCode(722)),
    ("BTN_TRIGGER_HAPPY20", KeyCode(723)),
    ("BTN_TRIGGER_HAPPY21", KeyCode(724)),
    ("BTN_TRIGGER_HAPPY22", KeyCode(725)),
    ("BTN_TRIGGER_HAPPY23", KeyCode(726)),
    ("BTN_TRIGGER_HAPPY24", KeyCode(727)),
    ("BTN_TRIGGER_HAPPY25", KeyCode(728)),
    ("BTN_TRIGGER_HAPPY26", KeyCode(729)),
    ("BTN_TRIGGER_HAPPY27", KeyCode(730)),
    ("BTN_TRIGGER_HAPPY28", KeyCode(731)),
    ("BTN_TRIGGER_HAPPY29", KeyCode(732)),
    ("BTN_TRIGGER_HAPPY30", KeyCode(733)),
    ("BTN_TRIGGER_HAPPY31", KeyCode(734)),
    ("BTN_TRIGGER_HAPPY32", KeyCode(735)),
    ("BTN_TRIGGER_HAPPY33", KeyCode(736)),
    ("BTN_TRIGGER_HAPPY34", KeyCode(737)),
    ("BTN_TRIGGER_HAPPY35", KeyCode(738)),
    ("BTN_TRIGGER_HAPPY36", KeyCode(739)),
    ("BTN_TRIGGER_HAPPY37", KeyCode(740)),
    ("BTN_TRIGGER_HAPPY38", KeyCode(741)),
    ("BTN_TRIGGER_HAPPY39", KeyCode(742)),
    ("BTN_TRIGGER_HAPPY40", KeyCode(743)),
];

static ALL_KEYS: LazyLock<AttributeSet<KeyCode>> = LazyLock::new(|| {
    NAME_MAP
        .iter()
        .filter(|(name, _)| name.starts_with("KEY_"))
        .map(|(_, k)| *k)
        .collect()
});
