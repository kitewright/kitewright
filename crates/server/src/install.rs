//! `kite install`: download a Chrome-for-Testing `chrome-headless-shell` build
//! into the shared install cache so the engine can find it automatically (no
//! more "bring your own Chromium"). The cache path is shared with the engine
//! via [`kitewright_engine::install_cache_dir`], and the engine's browser-path
//! resolution falls back to whatever this command installs.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use kitewright_engine::{headless_shell_binary_name, install_cache_dir};

/// Chrome-for-Testing "known good versions with downloads" manifest.
const CFT_MANIFEST_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json";

/// Map a Rust `(OS, ARCH)` pair (`std::env::consts::{OS, ARCH}`) to a
/// Chrome-for-Testing platform id. Returns `None` for platforms CfT does not
/// publish a `chrome-headless-shell` build for (e.g. linux-arm64).
pub fn map_platform(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("mac-arm64"),
        ("macos", "x86_64") => Some("mac-x64"),
        ("linux", "x86_64") => Some("linux64"),
        ("windows", "x86_64") => Some("win64"),
        _ => None,
    }
}

/// The current platform's CfT id, or a helpful error.
fn current_platform() -> Result<&'static str> {
    map_platform(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "no Chrome-for-Testing chrome-headless-shell build for {}/{} — \
             set BROWSER_EXECUTABLE to an existing Chrome/Chromium instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })
}

/// Pick the latest version in the CfT manifest that ships a
/// `chrome-headless-shell` download for `platform`, returning `(version, url)`.
/// The manifest's `versions` array is chronological, so we scan from the end.
fn pick_download_url(manifest: &serde_json::Value, platform: &str) -> Option<(String, String)> {
    let versions = manifest.get("versions")?.as_array()?;
    for entry in versions.iter().rev() {
        let version = entry.get("version").and_then(|v| v.as_str())?;
        let downloads = entry
            .get("downloads")
            .and_then(|d| d.get("chrome-headless-shell"))
            .and_then(|c| c.as_array());
        let Some(downloads) = downloads else { continue };
        for dl in downloads {
            if dl.get("platform").and_then(|p| p.as_str()) == Some(platform) {
                if let Some(url) = dl.get("url").and_then(|u| u.as_str()) {
                    return Some((version.to_string(), url.to_string()));
                }
            }
        }
    }
    None
}

/// Directory a given version is extracted into.
fn version_dir(version: &str) -> PathBuf {
    install_cache_dir().join(version)
}

/// Where the binary lands after extracting the CfT zip:
/// `<version>/chrome-headless-shell-<platform>/chrome-headless-shell[.exe]`.
fn binary_path(version: &str, platform: &str) -> PathBuf {
    version_dir(version)
        .join(format!("chrome-headless-shell-{platform}"))
        .join(headless_shell_binary_name())
}

const HELP: &str = "\
kite install — download a headless Chromium for kitewright

USAGE:
    kite install [--help]

Downloads the latest stable Chrome-for-Testing `chrome-headless-shell` build
for this platform into the kitewright install cache and makes the engine use
it automatically (no BROWSER_EXECUTABLE needed). Re-running is a no-op once a
build is present.

The cache lives under $KITE_CACHE_DIR (or the OS cache dir) in
`kitewright/chrome-headless-shell/<version>/`.
";

/// Entry point for `kite install`. `args` are the arguments AFTER the
/// `install` subcommand (e.g. `--help`).
pub async fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(());
    }

    let platform = current_platform()?;

    // Fast path: already installed.
    if let Some(existing) = kitewright_engine::find_installed_browser() {
        println!(
            "chrome-headless-shell already installed at:\n  {}",
            existing.display()
        );
        println!("(delete the cache dir to force a re-download)");
        return Ok(());
    }

    println!("Resolving latest Chrome-for-Testing build for {platform}...");
    let client = reqwest::Client::builder()
        .user_agent(concat!("kitewright/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")?;

    let manifest: serde_json::Value = client
        .get(CFT_MANIFEST_URL)
        .send()
        .await
        .context("failed to fetch Chrome-for-Testing manifest")?
        .error_for_status()
        .context("Chrome-for-Testing manifest request failed")?
        .json()
        .await
        .context("failed to parse Chrome-for-Testing manifest JSON")?;

    let (version, url) = pick_download_url(&manifest, platform).with_context(|| {
        format!("no chrome-headless-shell download found for platform {platform}")
    })?;

    let target = binary_path(&version, platform);
    if target.is_file() {
        println!(
            "chrome-headless-shell {version} already present:\n  {}",
            target.display()
        );
        return Ok(());
    }

    println!("Downloading chrome-headless-shell {version}...\n  {url}");
    let bytes = client
        .get(&url)
        .send()
        .await
        .context("failed to download chrome-headless-shell zip")?
        .error_for_status()
        .context("chrome-headless-shell download request failed")?
        .bytes()
        .await
        .context("failed to read download body")?;

    let dest = version_dir(&version);
    let _ = tokio::fs::remove_dir_all(&dest).await;
    tokio::fs::create_dir_all(&dest)
        .await
        .with_context(|| format!("failed to create {}", dest.display()))?;

    println!("Extracting {} bytes to {}...", bytes.len(), dest.display());
    // Zip extraction is synchronous CPU work; keep it off the async worker.
    let dest_for_task = dest.clone();
    let data = bytes.to_vec();
    tokio::task::spawn_blocking(move || extract_zip(&data, &dest_for_task))
        .await
        .context("extraction task panicked")??;

    if !target.is_file() {
        bail!(
            "extraction completed but the expected binary was not found at {}",
            target.display()
        );
    }
    make_executable(&target)?;

    println!("\nInstalled chrome-headless-shell {version}");
    println!("Binary: {}", target.display());
    println!(
        "The engine will use it automatically when no BROWSER_EXECUTABLE or system Chrome is set."
    );
    Ok(())
}

/// Extract every entry of an in-memory zip into `dest`, preserving unix file
/// permissions recorded in the archive.
fn extract_zip(data: &[u8], dest: &Path) -> Result<()> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(data)).context("failed to open downloaded zip")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("failed to read zip entry")?;
        let Some(rel) = entry.enclosed_name() else {
            // Skip entries with unsafe (path-traversal) names.
            continue;
        };
        let out_path = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .with_context(|| format!("failed to create {}", out_path.display()))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .with_context(|| format!("failed to create {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// Ensure the browser binary is executable (chmod +x) on unix. No-op elsewhere.
fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("failed to stat {}", path.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("failed to chmod +x {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_mapping_covers_supported_targets() {
        assert_eq!(map_platform("macos", "aarch64"), Some("mac-arm64"));
        assert_eq!(map_platform("macos", "x86_64"), Some("mac-x64"));
        assert_eq!(map_platform("linux", "x86_64"), Some("linux64"));
        assert_eq!(map_platform("windows", "x86_64"), Some("win64"));
        // Unsupported: CfT publishes no chrome-headless-shell for these.
        assert_eq!(map_platform("linux", "aarch64"), None);
        assert_eq!(map_platform("freebsd", "x86_64"), None);
    }

    #[test]
    fn binary_path_layout_matches_cft_zip() {
        let p = binary_path("120.0.6099.109", "mac-arm64");
        assert!(p.ends_with(format!(
            "120.0.6099.109/chrome-headless-shell-mac-arm64/{}",
            headless_shell_binary_name()
        )));
    }

    #[test]
    fn pick_download_url_selects_latest_matching_platform() {
        let manifest = serde_json::json!({
            "versions": [
                {
                    // Older; has the download.
                    "version": "113.0.5672.0",
                    "downloads": {
                        "chrome-headless-shell": [
                            { "platform": "linux64", "url": "https://ex/113/linux64.zip" },
                            { "platform": "mac-arm64", "url": "https://ex/113/mac-arm64.zip" }
                        ]
                    }
                },
                {
                    // Newest, but no chrome-headless-shell for our platform.
                    "version": "999.0.0.0",
                    "downloads": {
                        "chrome-headless-shell": [
                            { "platform": "win64", "url": "https://ex/999/win64.zip" }
                        ]
                    }
                }
            ]
        });
        // Latest with a linux64 build is 113.
        assert_eq!(
            pick_download_url(&manifest, "linux64"),
            Some((
                "113.0.5672.0".to_string(),
                "https://ex/113/linux64.zip".to_string()
            ))
        );
        // win64 only exists in 999.
        assert_eq!(
            pick_download_url(&manifest, "win64"),
            Some((
                "999.0.0.0".to_string(),
                "https://ex/999/win64.zip".to_string()
            ))
        );
        // No build for this platform anywhere.
        assert_eq!(pick_download_url(&manifest, "mac-x64"), None);
    }

    #[test]
    fn pick_download_url_skips_versions_without_headless_shell() {
        // Early CfT versions have no chrome-headless-shell key at all.
        let manifest = serde_json::json!({
            "versions": [
                { "version": "100.0.0.0", "downloads": { "chrome": [] } },
                {
                    "version": "120.0.0.0",
                    "downloads": {
                        "chrome-headless-shell": [
                            { "platform": "linux64", "url": "https://ex/120/linux64.zip" }
                        ]
                    }
                }
            ]
        });
        assert_eq!(
            pick_download_url(&manifest, "linux64"),
            Some((
                "120.0.0.0".to_string(),
                "https://ex/120/linux64.zip".to_string()
            ))
        );
    }
}
