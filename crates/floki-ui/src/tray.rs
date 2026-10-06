//! Tray icon: 32x32 render of `icon_art`, Show/Settings/Quit menu, event pump.
//!
//! The `TrayIcon` + menu items must stay alive for the whole process (they are
//! kept in [`TrayHandles`], held by `main` across `run_native`). Events are
//! drained each egui frame via the crates' `receiver()` channels.

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Actions the egui loop should take after draining tray/menu queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Settings,
    Quit,
}

pub struct TrayHandles {
    _tray: TrayIcon,
    _menu: Menu,
    show_id: tray_icon::menu::MenuId,
    settings_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

/// The 32x32 tray icon (same artwork as the window and the exe).
#[must_use]
pub fn make_icon_rgba() -> (Vec<u8>, u32, u32) {
    (crate::icon_art::render(32), 32, 32)
}

pub fn build_tray() -> anyhow::Result<TrayHandles> {
    let (rgba, w, h) = make_icon_rgba();
    let icon = Icon::from_rgba(rgba, w, h).map_err(|e| anyhow::anyhow!("bad tray icon: {e:?}"))?;
    let menu = Menu::new();
    let show = MenuItem::new("Show Floki", true, None);
    let settings = MenuItem::new("Settings…", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&show)
        .map_err(|e| anyhow::anyhow!("tray menu append: {e}"))?;
    menu.append(&settings)
        .map_err(|e| anyhow::anyhow!("tray menu append: {e}"))?;
    menu.append(&PredefinedMenuItem::separator())
        .map_err(|e| anyhow::anyhow!("tray menu append: {e}"))?;
    menu.append(&quit)
        .map_err(|e| anyhow::anyhow!("tray menu append: {e}"))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_menu_on_left_click(false)
        .with_icon(icon)
        .with_tooltip("Floki — file search")
        .build()
        .map_err(|e| anyhow::anyhow!("tray build: {e}"))?;
    Ok(TrayHandles {
        _tray: tray,
        _menu: menu,
        show_id: show.id().clone(),
        settings_id: settings.id().clone(),
        quit_id: quit.id().clone(),
    })
}

/// Drain pending tray + menu events, mapping them to [`TrayAction`].
/// Left-click (or double-click) shows the window.
pub fn pump_tray(handles: Option<&TrayHandles>) -> Vec<TrayAction> {
    let mut out = Vec::new();
    while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
        match ev {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => out.push(TrayAction::Show),
            _ => {}
        }
    }
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        if let Some(h) = handles {
            if ev.id() == &h.show_id {
                out.push(TrayAction::Show);
            } else if ev.id() == &h.settings_id {
                out.push(TrayAction::Settings);
            } else if ev.id() == &h.quit_id {
                out.push(TrayAction::Quit);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_32x32_opaque_rgba() {
        let (rgba, w, h) = make_icon_rgba();
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        // A pixel inside the tile is fully opaque.
        let i = (28 * 32 + 4) * 4;
        assert_eq!(rgba[i + 3], 255);
        // Corners are transparent (rounded square).
        assert_eq!(rgba[3], 0);
    }

    #[test]
    fn pump_with_no_events_is_empty() {
        assert!(pump_tray(None).is_empty());
    }
}
