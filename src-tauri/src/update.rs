//! Update check against the LTBR·FM release API. Entirely passive: on
//! failure (offline, API down, bad payload) nothing happens and the player
//! keeps playing; on success with a newer version an `update_available`
//! event lights the indicator in the UI.

use std::time::Duration;

use tauri::{AppHandle, Emitter};

const LATEST_URL: &str = "https://www.ltbr.fm/api/player/latest";
pub const DOWNLOAD_PAGE: &str = "https://www.ltbr.fm/player#download";
pub const HOME_PAGE: &str = "https://www.ltbr.fm";

/// How often to re-check while the app is running.
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// Small delay before the first check so startup stays snappy.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(3);

pub fn spawn_checker(app: AppHandle) {
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
    use super::is_newer;

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
}
