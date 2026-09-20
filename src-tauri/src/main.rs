mod agent;
mod config;
mod db;
mod llm;
mod logging;
mod models;
mod rhythm;
mod screenpipe;

use anyhow::Result;
use chrono::{Local, Utc};
use models::{BootstrapData, ChatMessage, DiaryEntry};
use reqwest::Client;
use std::{
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
};
use tauri::{Manager, State};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri_plugin_positioner::{Position, WindowExt};

#[cfg(target_os = "macos")]
use tauri_nspanel::{tauri_panel, ManagerExt as PanelManagerExt, WebviewWindowExt};

#[cfg(target_os = "macos")]
tauri_panel! {
    panel!(CompanionPanel {
        config: {
            can_become_key_window: true,
            can_become_main_window: false,
            is_floating_panel: true
        }
    })
}

pub struct AppState {
    data_dir: PathBuf,
    db_path: PathBuf,
    http: Client,
    // Prevent the scheduled observer and the "observe now" button from
    // hitting screenpipe at the same time. screenpipe intentionally keeps a
    // small search-concurrency budget.
    observation_lock: tokio::sync::Mutex<()>,
    shutting_down: AtomicBool,
}

#[tauri::command]
async fn bootstrap(state: State<'_, Arc<AppState>>) -> Result<BootstrapData, String> {
    let cfg = config::load(&state.data_dir).map_err(err)?;
    let rhythm = rhythm::status(&state.data_dir).map_err(err)?;
    let messages = db::recent_messages(&state.db_path, 50).map_err(err)?;
    let last_observation = db::recent_observations(&state.db_path, 1).map_err(err)?.pop();
    let date = Local::now().format("%Y-%m-%d").to_string();
    let today_diary = db::get_diary(&state.db_path, &date).map_err(err)?;
    // Keep UI refreshes cheap: a bootstrap happens after observations and chat
    // events, so probing /search here would create unnecessary search pressure.
    let screenpipe_ok = if rhythm.sleeping { false } else { screenpipe::health(&state.http, &cfg.screenpipe_url).await };
    let log_path = logging::path().unwrap_or_else(|| state.data_dir.join("companion.log"));
    let screenpipe_log_path = state.data_dir.join("screenpipe.log");
    Ok(BootstrapData {
        config: cfg,
        messages,
        last_observation,
        today_diary,
        screenpipe_ok,
        rhythm,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        log_path: log_path.to_string_lossy().into_owned(),
        screenpipe_log_path: screenpipe_log_path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
async fn send_message(text: String, state: State<'_, Arc<AppState>>) -> Result<ChatMessage, String> {
    let text = text.trim().to_string();
    if text.is_empty() { return Err("empty message".into()); }
    let cfg = config::load(&state.data_dir).map_err(err)?;
    let rhythm_status = rhythm::status(&state.data_dir).map_err(err)?;
    if rhythm_status.sleeping {
        return Err(format!("{}は寝ています。", cfg.companion_name));
    }
    let now = Utc::now().to_rfc3339();
    db::insert_message(&state.db_path, &now, "user", &text, false).map_err(err)?;
    let history = db::recent_messages(&state.db_path, 24).map_err(err)?;
    let observations = db::recent_observations(&state.db_path, 10).map_err(err)?;
    let reply = llm::chat(&state.http, &cfg, &text, &history, &observations).await.map_err(err)?;
    let created_at = Utc::now().to_rfc3339();
    let id = db::insert_message(&state.db_path, &created_at, "assistant", &reply, false).map_err(err)?;
    Ok(ChatMessage { id, created_at, role:"assistant".into(), text:reply, proactive:false })
}

#[tauri::command]
async fn save_config(new_config: config::AppConfig, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    config::save(&state.data_dir, &new_config).map_err(err)
}

#[tauri::command]
async fn generate_diary(state: State<'_, Arc<AppState>>) -> Result<DiaryEntry, String> {
    let cfg = config::load(&state.data_dir).map_err(err)?;
    let rhythm_status = rhythm::status(&state.data_dir).map_err(err)?;
    if rhythm_status.sleeping {
        return Err(format!("{}は寝ています。", cfg.companion_name));
    }
    agent::generate_diary_for_today(state.inner(), &cfg).await.map_err(err)
}

#[tauri::command]
async fn observe_now(app: tauri::AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let cfg = config::load(&state.data_dir).map_err(err)?;
    let rhythm_status = rhythm::status(&state.data_dir).map_err(err)?;
    if rhythm_status.sleeping {
        return Err(format!("{}は寝ています。", cfg.companion_name));
    }
    logging::info("manual observation requested");
    agent::observe_once(&app, state.inner(), &cfg).await.map_err(err)
}

#[tauri::command]
async fn quit_all(app: tauri::AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.shutting_down.store(true, Ordering::SeqCst);
    logging::info("Quit requested from settings; stopping screenpipe and companion");
    let cfg = config::load(&state.data_dir).map_err(err)?;
    if let Err(e) = screenpipe::stop_daemon(&state.http, &cfg.screenpipe_url, &state.data_dir).await {
        // Quitting the companion should not be blocked by a recorder shutdown
        // failure, but keep the reason in the persistent log.
        logging::error(&format!("screenpipe shutdown: {e:#}"));
    }
    logging::info("Yandere Companion exiting");
    app.exit(0);

    // `AppHandle::exit` is the normal path. Keep a short fail-safe for the
    // custom macOS panel/runtime so an unexpected event-loop quirk cannot
    // leave a zombie tray app after the user explicitly chose Quit.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(750));
        std::process::exit(0);
    });
    Ok(())
}

fn err<E: std::fmt::Display>(e: E) -> String {
    let message = e.to_string();
    logging::error(&message);
    message
}

#[cfg(target_os = "macos")]
fn configure_macos_companion_panel(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    use objc2_app_kit::{NSScreenSaverWindowLevel, NSWindowCollectionBehavior, NSWindowStyleMask};

    // A regular NSWindow is not sufficient for overlays above another app's
    // full-screen Space. Convert the Tauri webview window to a true NSPanel.
    // This is the same AppKit window class used by floating palettes/overlays.
    let panel = window.to_panel::<CompanionPanel>()?;
    panel.set_collection_behavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::CanJoinAllApplications
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.set_level(NSScreenSaverWindowLevel as i64);
    panel.set_floating_panel(true);
    panel.set_hides_on_deactivate(false);
    panel.set_becomes_key_only_if_needed(false);
    panel.set_works_when_modal(true);
    panel.set_style_mask(
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::NonactivatingPanel,
    );
    logging::info("macOS companion window converted to NSPanel for fullscreen Spaces");
    Ok(())
}

#[cfg(target_os = "macos")]
fn running_from_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

fn main() {
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init());

    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }

    builder
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let _ = logging::init(&data_dir)?;

            #[cfg(target_os = "macos")]
            {
                let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
                let _ = app.handle().set_dock_visibility(false);

                use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
                app.handle().plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))?;
                // Avoid registering target/debug/yandere-companion during cargo run.
                // Launch the packaged .app once and it will register itself for login.
                if running_from_app_bundle() {
                    let manager = app.autolaunch();
                    if !manager.is_enabled().unwrap_or(false) {
                        if let Err(e) = manager.enable() {
                            logging::error(&format!("autostart: {e}"));
                        }
                    }
                }
            }

            app.handle().plugin(tauri_plugin_positioner::init())?;

            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main") {
                configure_macos_companion_panel(&window)?;
            }

            let db_path = data_dir.join("companion.sqlite3");
            db::init(&db_path)?;
            let _ = config::load(&data_dir)?;

            let state = Arc::new(AppState {
                data_dir,
                db_path,
                http: Client::builder().timeout(std::time::Duration::from_secs(90)).build()?,
                observation_lock: tokio::sync::Mutex::new(()),
                shutting_down: AtomicBool::new(false),
            });
            app.manage(state.clone());

            let tray = TrayIconBuilder::new()
                .tooltip("Yandere Companion")
                .title("♡")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        #[cfg(target_os = "macos")]
                        {
                            if let Ok(panel) = tray.app_handle().get_webview_panel("main") {
                                // `is_visible` may be true while the old window is only visible
                                // in another Space. Only hide when this panel is actually focused;
                                // otherwise re-show it in the current Space.
                                let focused = panel
                                    .to_window()
                                    .and_then(|w| w.is_focused().ok())
                                    .unwrap_or(false);
                                if panel.is_visible() && focused {
                                    panel.hide();
                                } else {
                                    if let Some(window) = panel.to_window() {
                                        let _ = window.move_window(Position::TrayCenter);
                                    }
                                    panel.show_and_make_key();
                                    panel.order_front_regardless();
                                }
                                return;
                            }
                        }

                        // Non-macOS fallback.
                        if let Some(window) = tray.app_handle().get_webview_window("main") {
                            if window.is_focused().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.move_window(Position::TrayCenter);
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;
            let _tray = tray;

            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(agent::run_loop(app_handle, state));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![bootstrap, send_message, save_config, generate_diary, observe_now, quit_all])
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Yandere Companion");
}
