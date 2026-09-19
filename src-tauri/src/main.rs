// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // On Wayland the compositor ignores keep-above (there is no protocol for
    // it) and rounded, transparent corners depend on the compositor too. Run
    // through XWayland instead, where both just work — unless the user has
    // chosen a backend themselves.
    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    ltbrfm_player_lib::run()
}
