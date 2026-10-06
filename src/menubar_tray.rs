//! The macOS menu bar item: renders Poller frames with tray-icon and routes
//! clicks back to the shared menu bar state.

use std::collections::HashMap;
use std::io::Cursor;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Instant;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::autostop::AUTO_STOP_ENV;
use crate::backend::Backend;
use crate::error::Error;
use crate::menubar_proc::{lock_menubar, menubar_pid_path};
use crate::menubar_state::{
    Frame, MENUBAR_POLL_INTERVAL, MenuAction, MenuEntry, Poller, Shared, applescript_quote, dimmed_rgba,
};
use crate::settings::settings_path;

const MENUBAR_ICON: &[u8] = include_bytes!("../assets/menubar-template.png");

enum UserEvent {
    Frame(Frame),
    Menu(MenuEvent),
    Quit,
}

struct IconImage {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

impl IconImage {
    fn icon(&self, dim: bool) -> Result<Icon, Error> {
        let rgba = if dim { dimmed_rgba(&self.rgba) } else { self.rgba.clone() };
        Icon::from_rgba(rgba, self.width, self.height).map_err(|err| Error::invalid(format!("menu bar icon: {err}")))
    }
}

/// Blocks until the menu bar item quits. A locked pidfile keeps it a
/// singleton and lets the TUI's toggle find and stop it.
pub fn run(backend: Arc<dyn Backend>) -> Result<(), Error> {
    let pid_path =
        menubar_pid_path().ok_or_else(|| Error::invalid("cannot resolve a config directory for the pidfile"))?;
    let lock = lock_menubar(&pid_path)?;
    let image = decode_icon(MENUBAR_ICON)?;

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();

    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT])?;
    let signal_proxy = proxy.clone();
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = signal_proxy.send_event(UserEvent::Quit);
        }
    });
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let (kick, kicks) = mpsc::sync_channel(1);
    let env_auto_stop = std::env::var(AUTO_STOP_ENV).unwrap_or_default();
    let shared = Shared::new(backend, settings_path(), env_auto_stop, kick);
    let mut poller = Some((Poller::new(Arc::clone(&shared)), kicks));
    let mut tray: Option<TrayIcon> = None;
    let mut actions: HashMap<MenuId, MenuAction> = HashMap::new();

    event_loop.run(move |event, _, control_flow| {
        let _lock = &lock;
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                match TrayIconBuilder::new().with_tooltip("colima").build() {
                    Ok(icon) => tray = Some(icon),
                    Err(err) => {
                        eprintln!("{} menubar: {err}", crate::NAME);
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                }
                if let Some((mut poller, kicks)) = poller.take() {
                    let proxy = proxy.clone();
                    thread::spawn(move || {
                        loop {
                            if proxy.send_event(UserEvent::Frame(poller.sync(Instant::now()))).is_err() {
                                return;
                            }
                            let _ = kicks.recv_timeout(MENUBAR_POLL_INTERVAL);
                        }
                    });
                }
            }
            Event::UserEvent(UserEvent::Frame(frame)) => {
                if let Some(tray) = &tray {
                    show(tray, frame, &image, &mut actions);
                }
            }
            Event::UserEvent(UserEvent::Menu(event)) => match actions.get(event.id()) {
                Some(MenuAction::Quit) => *control_flow = ControlFlow::Exit,
                Some(MenuAction::Open) => {
                    thread::spawn(open_tui);
                }
                Some(action) => shared.run(action),
                None => {}
            },
            Event::UserEvent(UserEvent::Quit) => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    })
}

fn show(tray: &TrayIcon, frame: Frame, image: &IconImage, actions: &mut HashMap<MenuId, MenuAction>) {
    tray.set_title(Some(&frame.title));
    if let Some(dim) = frame.dim {
        match image.icon(dim) {
            Ok(icon) => {
                let _ = tray.set_icon_templated(Some(icon));
            }
            Err(err) => eprintln!("{} menubar: {err}", crate::NAME),
        }
    }
    if let Some(entries) = frame.menu {
        let menu = Menu::new();
        actions.clear();
        if let Err(err) = build(entries, &|item| menu.append(item), actions) {
            eprintln!("{} menubar: {err}", crate::NAME);
        }
        tray.set_menu(Some(Box::new(menu)));
    }
}

type Append<'a> = &'a dyn Fn(&dyn IsMenuItem) -> tray_icon::menu::Result<()>;

/// Materializes menu entries, mapping each clickable item's id to its action.
fn build(
    entries: Vec<MenuEntry>,
    append: Append,
    actions: &mut HashMap<MenuId, MenuAction>,
) -> tray_icon::menu::Result<()> {
    for entry in entries {
        match entry {
            MenuEntry::Label(text) => append(&MenuItem::new(text, false, None))?,
            MenuEntry::Item(text, action) => {
                let item = MenuItem::new(text, true, None);
                actions.insert(item.id().clone(), action);
                append(&item)?;
            }
            MenuEntry::Separator => append(&PredefinedMenuItem::separator())?,
            MenuEntry::Submenu(text, children) => {
                let submenu = Submenu::new(text, true);
                build(children, &|item| submenu.append(item), actions)?;
                append(&submenu)?;
            }
        }
    }
    Ok(())
}

fn decode_icon(png_bytes: &[u8]) -> Result<IconImage, Error> {
    let invalid = |err: &dyn std::fmt::Display| Error::invalid(format!("menu bar icon: {err}"));
    let mut reader = png::Decoder::new(Cursor::new(png_bytes)).read_info().map_err(|e| invalid(&e))?;
    let size = reader.output_buffer_size().ok_or_else(|| invalid(&"image too large"))?;
    let mut rgba = vec![0; size];
    let info = reader.next_frame(&mut rgba).map_err(|e| invalid(&e))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(invalid(&"expected 8-bit RGBA"));
    }
    rgba.truncate(info.buffer_size());
    Ok(IconImage { rgba, width: info.width, height: info.height })
}

fn open_tui() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let shell_command = format!("'{}'", executable.to_string_lossy().replace('\'', r"'\''"));
    let _ = Command::new("osascript")
        .args([
            "-e",
            "tell application \"Terminal\" to activate",
            "-e",
            &format!("tell application \"Terminal\" to do script {}", applescript_quote(&shell_command)),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes() {
        let image = decode_icon(MENUBAR_ICON).unwrap();
        assert_eq!((image.width, image.height, image.rgba.len()), (64, 64, 64 * 64 * 4));
        assert!(image.icon(true).is_ok());
    }
}
