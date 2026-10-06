//! `floki`: Everything-class file-name search window (thin pipe client).
//!
//! Layout: query line on top, virtualized results list, status line. Lives
//! in the tray; `Ctrl+Alt+Space` toggles; `Esc` hides.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod client;
mod icon_art;
mod menu;
mod meta;
mod model;
mod startup;
mod theme;
mod tray;

use eframe::egui::ViewportBuilder;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyManager,
};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("floki=info")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    // COM once on the UI thread for the Properties dialog (`ShellExecuteExW`
    // with SEE_MASK_INVOKEIDLIST).
    actions::ensure_com_initialized();

    // `--minimized` (startup launch): start hidden, live in the tray.
    let minimized = std::env::args().any(|a| a == "--minimized");
    // `--settings[=drives]` opens Settings; `--search=<text>` pre-fills the query.
    let settings = std::env::args().find_map(|a| match a.as_str() {
        "--settings" => Some(app::SettingsTab::General),
        "--settings=drives" => Some(app::SettingsTab::Drives),
        "--settings=mcp" => Some(app::SettingsTab::Mcp),
        _ => None,
    });
    let search = std::env::args().find_map(|a| a.strip_prefix("--search=").map(str::to_owned));
    // `--theme=dark|light|system` overrides the stored theme for this run.
    let theme_arg =
        std::env::args().find_map(|a| a.strip_prefix("--theme=").map(theme::ThemeMode::from_str));

    // Global hotkey must be created on the same thread as the event loop; the
    // manager stays alive on `main`'s stack for the whole `run_native` call.
    // `FLOKI_NO_HOTKEY=1` skips registration so a second dev instance can run
    // alongside a production one (the hotkey is a global singleton; the
    // `HotKey` value itself is still created so `FlokiApp` keeps an id).
    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
    let _hotkey_manager = if std::env::var_os("FLOKI_NO_HOTKEY").is_some() {
        None
    } else {
        let manager = GlobalHotKeyManager::new()
            .map_err(|e| anyhow::anyhow!("global hotkey manager: {e}"))?;
        manager
            .register(hotkey)
            .map_err(|e| anyhow::anyhow!("register Ctrl+Alt+Space: {e}"))?;
        Some(manager)
    };

    // Same for the tray icon; a failure only costs the tray, not the window.
    let tray_handles = match tray::build_tray() {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!("tray unavailable: {e:#}");
            None
        }
    };

    let native_options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Floki")
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(std::sync::Arc::new(eframe::egui::IconData {
                rgba: icon_art::render(64),
                width: 64,
                height: 64,
            }))
            .with_visible(!minimized),
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        "Floki",
        native_options,
        Box::new(move |cc| {
            // Theme: stored choice wins; first run follows the OS.
            let stored = cc
                .storage
                .and_then(|s| s.get_string("floki-theme"))
                .map(|s| theme::ThemeMode::from_str(&s));
            let initial =
                theme_arg
                    .or(stored)
                    .unwrap_or_else(|| match cc.egui_ctx.system_theme() {
                        Some(eframe::egui::Theme::Dark) => theme::ThemeMode::Dark,
                        Some(eframe::egui::Theme::Light) => theme::ThemeMode::Light,
                        None => theme::ThemeMode::System,
                    });
            theme::install_fonts(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx, initial);
            // Startup toggle: the HKCU Run key is the only source of truth
            // (a cached copy went stale whenever the key changed elsewhere).
            let run_on_startup = startup::is_enabled();
            let mut app =
                app::FlokiApp::new(hotkey, tray_handles, initial, !minimized, run_on_startup);
            if theme_arg.is_some() {
                app.keep_stored_theme();
            }
            app.set_mcp_config(floki_mcp::McpConfig::load());
            app.launch(settings, search);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}
