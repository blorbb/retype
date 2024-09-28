use std::sync::mpsc;

use tray_item::{IconSource, TrayItem};

pub enum Message {
    Quit,
    ToggleEnabled,
}

const ENABLED_ICON: &str = "gtk-connect";
const DISABLED_ICON: &str = "gtk-disconnect";

pub fn create_tray_item() -> (TrayItem, mpsc::Receiver<Message>) {
    let mut tray =
        TrayItem::new("retype - navigation", IconSource::Resource(ENABLED_ICON)).unwrap();

    let (tray_tx, tray_rx) = mpsc::sync_channel(1);

    tray.add_menu_item("Toggle Hotkeys", {
        let tx = tray_tx.clone();
        move || tx.send(Message::ToggleEnabled).unwrap()
    })
    .unwrap();

    tray.inner_mut().add_separator().unwrap();

    tray.add_menu_item("Quit", {
        let tx = tray_tx.clone();
        move || tx.send(Message::Quit).unwrap()
    })
    .unwrap();

    log::info!("tray item created");

    (tray, tray_rx)
}

pub fn set_icon(tray: &mut TrayItem, enabled: bool) {
    let icon = if enabled { ENABLED_ICON } else { DISABLED_ICON };
    match tray.set_icon(IconSource::Resource(icon)) {
        Ok(_) => {}
        Err(e) => log::error!("error setting tray icon: {e:?}"),
    };
}
