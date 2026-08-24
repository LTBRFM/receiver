//! Self-update on startup, with a passive fallback.
//!
//! Wherever the installation can replace itself — macOS `.app`, the Windows
//! installers, a Linux AppImage — the app checks the signed update manifest at
//! every start and, when a newer version exists, downloads and installs it in
//! place and restarts. The frontend only *observes* this via `update://state`
//! events (progress overlay); it cannot start, steer, or point the updater
//! anywhere, and a payload that does not verify against the public key
//! embedded in `tauri.conf.json` aborts the install.
//!
//! Where the package manager owns the files (`.deb`/`.rpm`) the old passive
//! behavior remains: poll the release API, and on a newer version emit
//! `update_available` so the UI lights the click-to-download indicator.
//!
//! Either way, failure is invisible: offline, API down, bad payload — nothing
//! happens and the player keeps playing.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

const LATEST_URL: &str = "https://www.ltbr.fm/api/player/latest";
pub const DOWNLOAD_PAGE: &str = "https://www.ltbr.fm/player#download";
pub const HOME_PAGE: &str = "https://www.ltbr.fm";

/// Channel the frontend listens on for auto-update progress.
const STATE_EVENT: &str = "update://state";

/// How often to re-check while the app is running.
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// Small delay before the first check so startup stays snappy.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(3);

/// Progress payload for the frontend overlay.
///
/// `phase` is one of `downloading`, `installing`, `dismiss`. The overlay
/// appears on the first `downloading` event — a check that finds nothing must
/// never flash UI — and hides again on `dismiss` (a failed download).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateState {
    phase: &'static str,
    downloaded: u64,
    /// Absent when the server sends no `Content-Length`, in which case the UI
    /// shows an indeterminate bar rather than inventing a percentage.
    total: Option<u64>,
}

/// True where the installation can replace itself in place.
///
/// On Linux only an AppImage can (the `APPIMAGE` env var is set by the
/// AppImage runtime); a `.deb`/`.rpm` install is owned by the package manager.
/// macOS and Windows installs always can.
pub fn supports_in_app_install() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("APPIMAGE").is_some()
    } else {
        true
    }
}

/// Entry point called once from setup. Picks the silent auto-update flow or
/// the passive indicator flow depending on how the app is installed.
pub fn spawn_auto_update(app: AppHandle) {
    if supports_in_app_install() {
        tauri::async_runtime::spawn(async move {
            tokio_sleep(FIRST_CHECK_DELAY).await;
            loop {
                // Never returns if an update was installed (the app restarts).
                try_auto_update(&app).await;
                tokio_sleep(CHECK_INTERVAL).await;
            }
        });
    } else {
        spawn_passive_checker(app);
    }
}

/// One check-and-install attempt. Any failure emits `dismiss` (so a
/// half-drawn overlay never lingers) and returns; success never returns.
async fn try_auto_update(app: &AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(_) => return,
    };
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        // No update, or the manifest was unreachable/invalid — stay silent.
        _ => return,
    };

    let progress = std::sync::Mutex::new(0u64);
    let emit = |phase: &'static str, downloaded: u64, total: Option<u64>| {
        let _ = app.emit(
            STATE_EVENT,
            UpdateState {
                phase,
                downloaded,
                total,
            },
        );
    };

    let installed = update
        .download_and_install(
            |chunk, total| {
                let downloaded = {
                    let mut acc = progress.lock().unwrap_or_else(|e| e.into_inner());
                    *acc += chunk as u64;
                    *acc
                };
                emit("downloading", downloaded, total);
            },
            || emit("installing", 0, None),
        )
        .await;

    match installed {
        // Only reached on macOS and Linux. On Windows the plugin hands off to
        // the installer and exits the process inside the call above.
        Ok(()) => app.restart(),
        // Broken connection, signature mismatch, disk full — hide the overlay
        // and carry on with the running version; the next interval retries.
        Err(_) => emit("dismiss", 0, None),
    }
}

/// The pre-updater behavior, kept for `.deb`/`.rpm` installs: poll the
/// release API and light the indicator; the click opens the download page.
fn spawn_passive_checker(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("ltbr-update".into())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK_DELAY);
            loop {
                if let Some(latest) = fetch_latest_version() {
                    if is_newer(&latest, env!("CARGO_PKG_VERSION")) {
                        let _ = app.emit(
                            "update_available",
                            serde_json::json!({ "version": latest }),
                        );
                    }
                }
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
}

/// Async sleep on the tauri runtime without depending on tokio directly.
async fn tokio_sleep(duration: Duration) {
    let (tx, mut rx) = tauri::async_runtime::channel::<()>(1);
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        drop(tx);
    });
    let _ = rx.recv().await;
}

fn fetch_latest_version() -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("LTBR-FM-Receiver/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let resp = client.get(LATEST_URL).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().ok()?;
    let data: serde_json::Value = serde_json::from_str(&body).ok()?;
    let version = data.get("version")?.as_str()?;
    Some(version.trim().trim_start_matches('v').to_string())
}

/// Numeric semver-style comparison; unparseable segments count as 0.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    }
    let l = parts(latest);
    let c = parts(current);
    for i in 0..l.len().max(c.len()) {
        let a = l.get(i).copied().unwrap_or(0);
        let b = c.get(i).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

/// Open the download page in the system browser.
pub fn open_download_page() {
    open_external(DOWNLOAD_PAGE);
}

/// Open an arbitrary URL in the system browser (used e.g. for the wordmark
/// linking out to the station's site).
pub fn open_external(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::{is_newer, supports_in_app_install};

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.1.2", "0.1.1")); // tag-style prefix
        assert!(is_newer("0.1.1.1", "0.1.1")); // longer wins on extra segment
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.1")); // parses as 0 -> not newer
    }

    #[test]
    fn in_app_install_support() {
        // On macOS and Windows the install can always replace itself; on
        // Linux only under an AppImage runtime, which sets $APPIMAGE. The
        // test environment is never an AppImage.
        if cfg!(target_os = "linux") {
            assert!(!supports_in_app_install());
        } else {
            assert!(supports_in_app_install());
        }
    }
}
