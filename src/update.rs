//! `unharness update [VERSION] [--check] [--prerelease]`: update unharness
//! itself.
//!
//! Only a shell-installer install is updated in place: dist's installer
//! leaves a receipt in `~/.config/unharness/` (also with `install-updater =
//! false`, which only drops the separate `unharness-update` binary), and
//! axoupdater reads it, runs the new release's installer and replaces the
//! binary. A Homebrew or `cargo install` binary belongs to that tool, so the
//! command to run is named instead. It runs only when asked, as unharness
//! itself, outside any sandbox.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use axoupdater::{AxoUpdater, ReleaseSource, ReleaseSourceType, UpdateRequest, Version};
use colored::*;

const APP: &str = "unharness";
const OWNER: &str = "Liquescent-Development";
const REPO: &str = "unharness";
const INSTALLER: &str = "curl -fsSL https://github.com/Liquescent-Development/unharness/releases/latest/download/unharness-installer.sh | sh";
const CARGO_INSTALL: &str =
    "cargo install --locked --force --git https://github.com/Liquescent-Development/unharness";
const BREW_UPGRADE: &str = "brew upgrade unharness";

/// How the running binary was installed, which decides who may replace it.
#[derive(Debug, PartialEq, Eq)]
pub enum Method {
    /// The shell installer, whose receipt is for this binary.
    Installer,
    Homebrew,
    /// `cargo install` (`.crates2.json` beside the `bin` directory lists it).
    Cargo,
    /// Neither: a local build, or an install whose receipt is gone.
    Unknown,
}

impl Method {
    pub fn describe(&self) -> &'static str {
        match self {
            Method::Installer => "installed by the shell installer",
            Method::Homebrew => "installed by Homebrew",
            Method::Cargo => "installed by cargo install",
            Method::Unknown => "no install receipt or cargo install record (a local build?)",
        }
    }

    /// The command that updates an install of this kind.
    pub fn how(&self) -> String {
        match self {
            Method::Installer => "unharness update".to_string(),
            Method::Homebrew => BREW_UPGRADE.to_string(),
            Method::Cargo => CARGO_INSTALL.to_string(),
            Method::Unknown => format!(
                "rerun the installer ({INSTALLER}), or pull and rebuild if you built it yourself"
            ),
        }
    }
}

/// Decide from where the binary is and what the installers left behind.
/// `receipt` is the receipt's modification time when it is for this binary,
/// `cargo_record` that of the `cargo install` record listing it. Both exist
/// when one tool installed over the other in the same `bin`; the newer wins.
fn classify(exe: &Path, receipt: Option<SystemTime>, cargo_record: Option<SystemTime>) -> Method {
    if exe.components().any(|c| c.as_os_str() == "Cellar") {
        return Method::Homebrew;
    }
    match (receipt, cargo_record) {
        (Some(r), Some(c)) if c > r => Method::Cargo,
        (Some(_), _) => Method::Installer,
        (None, Some(_)) => Method::Cargo,
        (None, None) => Method::Unknown,
    }
}

/// What `detect` found.
pub struct Install {
    pub method: Method,
    pub exe: PathBuf,
    /// The receipt, when one exists, and whether it is for this binary.
    pub receipt: Option<(PathBuf, bool)>,
    /// Loaded from the receipt; present when `method` is `Installer`.
    updater: Option<AxoUpdater>,
}

/// Look at the running binary. No network.
pub fn detect() -> Result<Install> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("cannot locate the running unharness binary")?;

    let receipt_path = receipt_path();
    let mut updater = AxoUpdater::new_for(APP);
    let mut receipt = None;
    let mut receipt_time = None;
    if let Some(path) = receipt_path
        && updater.load_receipt().is_ok()
    {
        let ours = updater.check_receipt_is_for_this_executable()?;
        if ours {
            receipt_time = mtime(&path);
        }
        receipt = Some((path, ours));
    }
    let cargo_time = cargo_record(&exe);

    let method = classify(&exe, receipt_time, cargo_time);
    Ok(Install {
        updater: (method == Method::Installer).then_some(updater),
        method,
        exe,
        receipt,
    })
}

/// Where dist's installer writes the receipt (axoupdater's own lookup:
/// `$XDG_CONFIG_HOME/unharness` when that exists, then `~/.config/unharness`).
fn receipt_path() -> Option<PathBuf> {
    let name = format!("{APP}-receipt.json");
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(|d| PathBuf::from(d).join(APP));
    let home = dirs::home_dir().map(|h| h.join(".config").join(APP));
    [xdg.filter(|d| d.exists()), home]
        .into_iter()
        .flatten()
        .map(|d| d.join(&name))
        .find(|p| p.exists())
}

/// The modification time of the `cargo install` record that lists this
/// binary: `.crates2.json` in the root above its `bin` directory.
fn cargo_record(exe: &Path) -> Option<SystemTime> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let record = bin.parent()?.join(".crates2.json");
    let text = std::fs::read_to_string(&record).ok()?;
    lists_unharness(&text).then(|| mtime(&record)).flatten()
}

fn lists_unharness(crates2: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(crates2) else {
        return false;
    };
    let Some(installs) = v.get("installs").and_then(|i| i.as_object()) else {
        return false;
    };
    installs.iter().any(|(key, entry)| {
        key.split(' ').next() == Some(APP)
            && entry
                .get("bins")
                .and_then(|b| b.as_array())
                .is_some_and(|b| b.iter().any(|n| n.as_str() == Some(APP)))
    })
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn running_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

/// `0.3.0` and `v0.3.0` both name the release tagged `v0.3.0`.
fn request(version: Option<&str>, prerelease: bool) -> UpdateRequest {
    match version {
        Some(v) => UpdateRequest::SpecificVersion(v.trim_start_matches('v').to_string()),
        None if prerelease => UpdateRequest::LatestMaybePrerelease,
        None => UpdateRequest::Latest,
    }
}

/// Point an updater at the GitHub releases; the token is the one the
/// installer itself reads, for rate limits.
fn configure(updater: &mut AxoUpdater, req: UpdateRequest) -> Result<()> {
    updater.set_release_source(ReleaseSource {
        release_type: ReleaseSourceType::GitHub,
        owner: OWNER.to_string(),
        name: REPO.to_string(),
        app_name: APP.to_string(),
    });
    // Decide by the binary that is running, not by the version the receipt
    // was written for.
    updater.set_current_version(running_version())?;
    updater.configure_version_specifier(req);
    if let Ok(token) = std::env::var("UNHARNESS_GITHUB_TOKEN") {
        updater.set_github_token(&token);
    }
    Ok(())
}

pub async fn run(version: Option<String>, check: bool, prerelease: bool) -> Result<()> {
    let install = detect()?;
    let current = running_version();
    let req = request(version.as_deref(), prerelease);

    if check {
        let mut updater = AxoUpdater::new_for(APP);
        configure(&mut updater, req)?;
        let latest = updater
            .query_new_version()
            .await
            .context("cannot reach the GitHub releases")?
            .cloned();
        match latest {
            Some(v) if v > current => {
                println!(
                    "unharness {} is available (this is {current}).",
                    v.to_string().bold()
                );
                println!("This binary: {}", install.method.describe());
                println!("Update with: {}", install.method.how());
            }
            Some(v) => println!("unharness {current} is up to date (latest release: {v})."),
            None => bail!("no release found"),
        }
        return Ok(());
    }

    let Some(mut updater) = install.updater else {
        explain(&install);
        bail!("unharness was not updated");
    };
    configure(&mut updater, req)?;
    updater.enable_installer_output();
    match updater.run().await.context("the update failed")? {
        Some(done) => {
            println!();
            let verb = if done.new_version > current {
                "Updated"
            } else {
                "Installed"
            };
            println!(
                "{} unharness {current} → {} in {}",
                verb.green().bold(),
                done.new_version.to_string().bold(),
                done.install_prefix.join("bin").as_str()
            );
            println!("Running unharness sessions keep the old version until they are restarted.");
        }
        None => println!("unharness {current} is up to date."),
    }
    Ok(())
}

/// Why this binary is not updated in place, and what does update it.
fn explain(install: &Install) {
    println!(
        "{} {}: {}, so unharness does not replace it.",
        "[-]".dimmed(),
        install.exe.display(),
        install.method.describe()
    );
    if let Some((path, false)) = &install.receipt {
        println!(
            "    The install receipt at {} is for another copy of unharness.",
            path.display()
        );
    }
    println!("Update with: {}", install.method.how());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn classify_by_place_and_records() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let t1 = t0 + Duration::from_secs(1);
        let cargo = Path::new("/home/u/.cargo/bin/unharness");
        let brew = Path::new("/opt/homebrew/Cellar/unharness/0.3.0/bin/unharness");

        assert_eq!(classify(brew, None, None), Method::Homebrew);
        // A receipt cannot make a brew binary ours to replace.
        assert_eq!(classify(brew, Some(t0), None), Method::Homebrew);
        assert_eq!(classify(cargo, Some(t0), None), Method::Installer);
        assert_eq!(classify(cargo, None, Some(t0)), Method::Cargo);
        // Installed over each other: the later one placed the binary.
        assert_eq!(classify(cargo, Some(t0), Some(t1)), Method::Cargo);
        assert_eq!(classify(cargo, Some(t1), Some(t0)), Method::Installer);
        assert_eq!(
            classify(
                Path::new("/src/unharness/target/release/unharness"),
                None,
                None
            ),
            Method::Unknown
        );
    }

    #[test]
    fn cargo_record_lists_the_binary() {
        let listed = r#"{"installs":{"unharness 0.2.0 (path+file:///src/unharness)":{"bins":["unharness"]}}}"#;
        assert!(lists_unharness(listed));
        let other = r#"{"installs":{"unharness-extra 1.0.0 (registry+x)":{"bins":["unharness"]},"ripgrep 14.0.0 (registry+x)":{"bins":["rg"]}}}"#;
        assert!(!lists_unharness(other));
        assert!(!lists_unharness("not json"));
    }

    #[test]
    fn cargo_record_is_beside_bin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("unharness");
        assert!(cargo_record(&exe).is_none());
        std::fs::write(
            dir.path().join(".crates2.json"),
            r#"{"installs":{"unharness 0.3.0 (git+https://x)":{"bins":["unharness"]}}}"#,
        )
        .unwrap();
        assert!(cargo_record(&exe).is_some());
        // Not in a `bin` directory: no install root to look in.
        assert!(cargo_record(&dir.path().join("unharness")).is_none());
    }

    #[test]
    fn version_requests() {
        assert!(matches!(request(None, false), UpdateRequest::Latest));
        assert!(matches!(
            request(None, true),
            UpdateRequest::LatestMaybePrerelease
        ));
        assert!(matches!(
            request(Some("v0.3.0"), false),
            UpdateRequest::SpecificVersion(v) if v == "0.3.0"
        ));
    }
}
