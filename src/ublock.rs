//! Bundled ad blocker.
//!
//! Ships uBOLite rather than full uBlock Origin: uBO is still Manifest V2, and
//! WebView2 publishes no manifest-version policy (its extension docs 404), so
//! MV2 loading is unverifiable. uBOLite is MV3 and certain to load.
//!
//! WebView2 wants an *unpacked* folder, and wry hands it every immediate
//! subdirectory of the extensions path, so each extension needs its own named
//! folder with `manifest.json` at the top.

use std::io::Read;
use std::path::PathBuf;

use crate::Ev;
use tao::event_loop::EventLoopProxy;

const REPO: &str = "uBlockOrigin/uBOL-home";
const NAME: &str = "uBOLite";
const UA: &str = "prism-browser";

/// Parent directory handed to wry; every subfolder is loaded as an extension.
pub fn root() -> PathBuf {
    let d = crate::state_dir().join("extensions");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn dir() -> PathBuf {
    root().join(NAME)
}

fn stamp() -> PathBuf {
    root().join(".installed-tag")
}

pub fn installed_tag() -> Option<String> {
    std::fs::read_to_string(stamp())
        .ok()
        .map(|s| s.trim().to_owned())
}

pub fn is_installed() -> bool {
    dir().join("manifest.json").exists()
}

/// Latest upstream release: (tag, zip download URL).
fn latest() -> Option<(String, String)> {
    let body = ureq::get(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))
    .header("User-Agent", UA)
    .header("Accept", "application/vnd.github+json")
    .call()
    .ok()?
    .body_mut()
    .read_to_string()
    .ok()?;

    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = v["tag_name"].as_str()?.to_owned();
    let url = v["assets"].as_array()?.iter().find(|a| {
        a["name"]
            .as_str()
            .is_some_and(|n| n.ends_with(".chromium.zip"))
    })?["browser_download_url"]
        .as_str()?
        .to_owned();
    Some((tag, url))
}

/// Downloads and unpacks into a fresh folder, then swaps it in. Extraction is
/// staged so a failed download can never leave a half-written extension that
/// WebView2 would then refuse to load.
fn install(tag: &str, url: &str) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    ureq::get(url)
        .header("User-Agent", UA)
        .call()
        .map_err(std::io::Error::other)?
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)?;

    let staging = root().join(".staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(std::io::Error::other)?
        .extract(&staging)
        .map_err(std::io::Error::other)?;

    // uBOLite's zip puts manifest.json at the root; uBO's nests it one level.
    // Normalise so the installed folder always has the manifest on top.
    let src = if staging.join("manifest.json").exists() {
        staging.clone()
    } else {
        std::fs::read_dir(&staging)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.join("manifest.json").exists())
            .ok_or_else(|| std::io::Error::other("no manifest.json in archive"))?
    };

    let target = dir();
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&src, &target)?;
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::write(stamp(), tag)?;
    Ok(())
}

/// Installs if missing, or upgrades if upstream moved. Returns a status line.
pub fn sync() -> String {
    let have = installed_tag();
    match latest() {
        Some((tag, url)) => {
            if have.as_deref() == Some(tag.as_str()) && is_installed() {
                format!("uBlock Origin Lite {tag}")
            } else {
                match install(&tag, &url) {
                    Ok(()) => format!("uBlock Origin Lite {tag} (updated)"),
                    Err(e) => match have {
                        Some(t) => format!("uBlock Origin Lite {t} (update failed: {e})"),
                        None => format!("ad blocker unavailable: {e}"),
                    },
                }
            }
        }
        // Offline is fine as long as something is already unpacked.
        None => match have {
            Some(t) => format!("uBlock Origin Lite {t} (offline)"),
            None => "ad blocker unavailable (offline)".to_owned(),
        },
    }
}

/// First run has nothing unpacked, and the extension can only be registered
/// when the webview is built -- so that one case is worth a short wait rather
/// than silently shipping an unprotected first session.
pub fn ensure_first_run() {
    if !is_installed() {
        let _ = sync();
    }
}

/// Update check for every later launch; never blocks the window appearing.
pub fn spawn_sync(proxy: EventLoopProxy<Ev>) {
    std::thread::spawn(move || {
        let label = sync();
        let _ = proxy.send_event(Ev::Shield(is_installed(), label));
    });
}
