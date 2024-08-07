use std::{sync::mpsc, thread, time::Duration};

use rdev::Key;

use crate::{
    modifiers::Modifier,
    send::{self, click, press, release},
};

#[derive(Debug)]
pub struct Finder {
    pub direction: Direction,
    pub selection: Selection,
}

impl Finder {
    pub fn new(direction: Direction) -> Self {
        let selection = get_text(direction);
        Self {
            direction,
            selection,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Direction {
    Forward,
    Backward,
}

#[derive(Debug)]
pub struct Selection(mpsc::Receiver<String>);

impl Selection {
    pub fn recv(&self) -> String {
        self.0.recv().unwrap()
    }
}

pub fn get_text(direction: Direction) -> Selection {
    // only one string will be sent
    let (tx, rx) = mpsc::sync_channel::<String>(1);

    // make sure ctrl is not held so cursor doesn't jump all the way to end of page
    send::KEYPRESSES.run(move || {
        Modifier::Ctrl.release();

        // shift-end
        press(Key::ShiftLeft);
        {
            let button = match direction {
                Direction::Forward => Key::End,
                Direction::Backward => Key::Home,
            };
            click(button);
            click(button);
        }
        release(Key::ShiftLeft);
        log::trace!("selected");

        thread::sleep(Duration::from_millis(100));

        let text = selection::get_text();
        log::info!("got selected string {text:?}");

        // move cursor back
        // in case user presses shift before this runs
        Modifier::Shift.release();
        click(match direction {
            Direction::Forward => Key::LeftArrow,
            Direction::Backward => Key::RightArrow,
        });
        log::trace!("moved cursor to original position");
        // send AFTER cursor has moved back so that calculations are
        // done from the correct starting point
        tx.send(text).unwrap();
    });

    return Selection(rx);
}

pub fn move_to_char(text: &str, char_to_find: char, direction: Direction, to_select: bool) {
    log::debug!("moving to char {char_to_find} in direction {direction:?}, select = {to_select}");
    Modifier::Shift.release();

    let move_amount = match direction {
        Direction::Forward => text
            .chars()
            .enumerate()
            .position(|(i, char)| char == char_to_find && i != 0),
        Direction::Backward => {
            // find last occurrence of this char
            let mut last_occurrence = None;
            let num_chars = text.chars().count();
            text.chars().enumerate().for_each(|(i, char)| {
                if char == char_to_find && i != num_chars - 1 {
                    last_occurrence = Some(num_chars - 1 - i)
                }
            });
            last_occurrence
        }
    };

    for _ in 0..move_amount.map_or(0, |amount| amount + 1) {
        if to_select {
            press(Key::ShiftLeft);
        }
        click(match direction {
            Direction::Forward => Key::RightArrow,
            Direction::Backward => Key::LeftArrow,
        });
        if to_select {
            release(Key::ShiftLeft);
        }
    }

    log::info!("moving {move_amount:?} characters");
}
