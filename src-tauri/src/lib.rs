//! LTBR·FM Receiver — Tauri command surface. All audio work happens in the
//! [`engine`] module; commands here just poke shared control state or hand the
//! engine a play/stop intent.

pub mod dsp;
pub mod engine;
pub mod output;
pub mod spectrum;
pub mod stream;
pub mod update;
pub mod window_menu;

use engine::{Engine, PRESETS};
use tauri::{Manager, State};

#[tauri::command]
fn play(engine: State<Engine>, url: String) {
    engine.play(url);
}

#[tauri::command]
fn pause(engine: State<Engine>) {
    // A live stream cannot be resumed mid-buffer, so pause == stop the session.
    engine.stop();
}

#[tauri::command]
fn stop(engine: State<Engine>) {
    engine.stop();
}

#[tauri::command]
fn set_volume(engine: State<Engine>, level: f32) {
    engine.controls().set_volume(level);
}

#[tauri::command]
fn set_mute(engine: State<Engine>, muted: bool) {
    engine.controls().set_muted(muted);
}

#[tauri::command]
fn set_eq_band(engine: State<Engine>, index: usize, db: f32) {
    engine.controls().set_band(index, db);
}

#[tauri::command]
fn set_preamp(engine: State<Engine>, db: f32) {
    engine.controls().set_preamp(db);
}

#[tauri::command]
fn apply_preset(engine: State<Engine>, name: String) {
    if let Some((_, gains)) = PRESETS.iter().find(|(n, _)| *n == name) {
        for (i, g) in gains.iter().enumerate() {
            engine.controls().set_band(i, *g);
        }
    }
}

#[tauri::command]
fn open_download_page() {
    update::open_download_page();
}

#[tauri::command]
fn open_home_page() {
    update::open_external(update::HOME_PAGE);
}

/// Quit the whole app, not just the window — the audio engine dies with the
/// process, so no teardown is needed beyond exiting.
#[tauri::command]
fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

/// Try to open the window manager's own window menu at (x, y), webview
/// coordinates. Returns false where no WM menu is available (non-Linux, or a
/// WM without the protocol) so the frontend can fall back to its HTML menu.
#[tauri::command]
fn show_window_menu(window: tauri::WebviewWindow, x: f64, y: f64) -> bool {
    window_menu::show(window, x, y)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let engine = Engine::new(app.handle().clone());
            app.manage(engine);
            update::spawn_checker(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            play,
            pause,
            stop,
            set_volume,
            set_mute,
            set_eq_band,
            set_preamp,
            apply_preset,
            open_download_page,
            open_home_page,
            show_window_menu,
            quit,
        ])
        .run(tauri::generate_context!())
        .expect("error while running LTBR·FM Receiver");
}
