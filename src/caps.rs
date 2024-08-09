//! Only toggle on super+caps if no other keys were clicked between
//! pressing and releasing this hotkey

pub struct ActivateOnRelease<F> {
    f: F,
    /// Will be set to false if any other presses occur.
    should_activate: bool,
}

impl<F: FnMut()> ActivateOnRelease<F> {
    pub fn new(f: F) -> Self {
        Self {
            f,
            should_activate: false,
        }
    }

    pub fn await_release(&mut self) {
        log::debug!("starting wait for release");
        self.should_activate = true;
    }

    pub fn interrupt(&mut self) {
        if self.should_activate {
            log::debug!("interrupted on-release key activation");
        }
        self.should_activate = false;
    }

    pub fn maybe_activate(&mut self) {
        if self.should_activate {
            log::debug!("activated on release");
            (self.f)();
        }
        self.should_activate = false;
    }
}
