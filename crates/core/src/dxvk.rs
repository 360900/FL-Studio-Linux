//! DXVK installation and removal.
//!
//! DXVK is enabled by default to map Direct3D calls from FL Studio and plugins
//! to Vulkan. It can be removed with `repair --dxvk false`. Releases are
//! pinned with their checksums: DXVK 3 needs a Vulkan 1.4 driver, so older
//! drivers get the last 2.x release instead.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::WINE_DIR_NAME;
use crate::config;
use crate::deps;
use crate::http;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::wine;

pub const DXVK_REPO: &str = "doitsujin/dxvk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub version: &'static str,
    sha256: &'static str,
    /// Lowest Vulkan version the release runs on.
    pub min_vulkan: (u32, u32),
}

/// Newest first.
pub const RELEASES: [Release; 2] = [
    Release {
        version: "3.1.1",
        sha256: "40565b4a724aadc4433fa4e010b4b23916d9b1f1baeee64e17186db94f54e608",
        min_vulkan: (1, 4),
    },
    Release {
        version: "2.7.1",
        sha256: "d85ce7c79f57ecd765aaa1b9e7007cb875e6fde9f6d331df799bce73d513ce87",
        min_vulkan: (1, 3),
    },
];

/// The newest release the driver supports, or `None` without a usable driver.
pub fn release_for(vulkan: Option<(u32, u32)>) -> Option<Release> {
    let vulkan = vulkan?;
    RELEASES
        .into_iter()
        .find(|release| vulkan >= release.min_vulkan)
}

const DXVK_DLLS: [&str; 5] = [
    "d3d8.dll",
    "d3d9.dll",
    "d3d10core.dll",
    "d3d11.dll",
    "dxgi.dll",
];

/// Install the DXVK release the host's Vulkan driver supports. Fails, after
/// removing an earlier DXVK, when no driver is recent enough.
pub fn install<F>(
    prefix: &Path,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<Option<String>>
where
    F: FnMut(Stream, &str),
{
    install_for(
        prefix,
        deps::vulkan_api_version(),
        dry_run,
        log_path,
        on_line,
    )
}

fn install_for<F>(
    prefix: &Path,
    vulkan: Option<(u32, u32)>,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<Option<String>>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");

    report.info("DXVK for Direct3D plugins");
    let release = release_for(vulkan);
    if dry_run {
        let dlls = DXVK_DLLS.join(" ");
        let Some(release) = release else {
            report.info(format!("    skip DXVK: {}", unsupported_reason(vulkan)));
            return Ok(None);
        };
        let version = release.version;
        report.info(format!("    {}", release_url(version)));
        report.info(format!(
            "    echo '{}  dxvk-{version}.tar.gz' | sha256sum -c -",
            release.sha256
        ));
        report.info(format!(
            "    cp <dxvk>/x64/{dlls} {}",
            prefix.join("drive_c/windows/system32").display()
        ));
        report.info(format!(
            "    cp <dxvk>/x86/{dlls} {}",
            prefix.join("drive_c/windows/syswow64").display()
        ));
        for dll in DXVK_DLLS {
            report.info(format!(
                "    {} reg add HKCU\\Software\\Wine\\DllOverrides /v {} /d native,builtin /f",
                wine.display(),
                dll.trim_end_matches(".dll")
            ));
        }
        return Ok(None);
    }

    if !wine.is_file() {
        bail!(
            "Wine is not installed at {}; run wine-setup first",
            wine.display()
        );
    }

    let Some(release) = release else {
        // A DXVK left from an earlier install would break Direct3D entirely.
        remove_installed(prefix, &wine_dir, &mut report);
        bail!(
            "{}; plugins use WineD3D instead",
            unsupported_reason(vulkan)
        );
    };
    let version = release.version;
    report.info(format!(
        "Vulkan {}.{} driver found; using DXVK {version}",
        vulkan.map_or(0, |v| v.0),
        vulkan.map_or(0, |v| v.1)
    ));
    let archive = http::download_verified(
        &mut report,
        &release_url(version),
        &format!("dxvk-{version}.tar.gz"),
        release.sha256,
        Duration::from_secs(600),
    )?;

    let cache = prefix.join(".cache");
    let root = cache.join(format!("dxvk-{version}"));
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    if !extracted(&root) {
        extract(&archive, &cache, &root, &mut report)?;
    }

    wine::kill_prefix(prefix, Some(&wine_dir), &mut report);
    let system32 = prefix.join("drive_c/windows/system32");
    let syswow64 = prefix.join("drive_c/windows/syswow64");
    fs::create_dir_all(&system32)?;
    for dll in DXVK_DLLS {
        let source = root.join("x64").join(dll);
        fs::copy(&source, system32.join(dll))
            .with_context(|| format!("copying {}", source.display()))?;
        if syswow64.is_dir()
            && let Some(source) = dll_32(&root, dll)
        {
            fs::copy(&source, syswow64.join(dll))
                .with_context(|| format!("copying {}", source.display()))?;
        }
    }

    for dll in DXVK_DLLS {
        set_override(
            prefix,
            &wine,
            &wine_dir,
            dll.trim_end_matches(".dll"),
            &mut report,
        );
    }
    config::write_prefix_state(prefix, ".dxvk_version", version)?;
    report.info(format!("installed DXVK {version}"));
    Ok(Some(version.to_string()))
}

fn release_url(version: &str) -> String {
    format!("https://github.com/{DXVK_REPO}/releases/download/v{version}/dxvk-{version}.tar.gz")
}

fn unsupported_reason(vulkan: Option<(u32, u32)>) -> String {
    let (major, minor) = RELEASES[RELEASES.len() - 1].min_vulkan;
    match vulkan {
        Some((found_major, found_minor)) => format!(
            "the Vulkan driver supports {found_major}.{found_minor}, but DXVK needs {major}.{minor} or newer"
        ),
        None => format!("no Vulkan driver found; DXVK needs Vulkan {major}.{minor} or newer"),
    }
}

fn dll_32(root: &Path, dll: &str) -> Option<PathBuf> {
    [root.join("x32").join(dll), root.join("x86").join(dll)]
        .into_iter()
        .find(|path| path.is_file())
}

/// Every DLL is present, so an interrupted extraction is not mistaken for a
/// finished one.
fn extracted(root: &Path) -> bool {
    DXVK_DLLS
        .iter()
        .all(|dll| root.join("x64").join(dll).is_file() && dll_32(root, dll).is_some())
}

/// Extract into a staging folder and move it into place only when complete.
fn extract(archive: &Path, cache: &Path, root: &Path, report: &mut Report<'_>) -> Result<()> {
    let staging = cache.join(".dxvk-extract");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let mut cmd = Command::new("tar");
    cmd.arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&staging)
        .arg("--no-same-owner");
    let result = runner::run(
        cmd,
        RunOptions::default().stall(Duration::from_secs(300)),
        |stream, line| report.line(stream, line),
    )?;
    if !result.success() {
        let _ = fs::remove_dir_all(&staging);
        bail!("extracting DXVK failed: {}", result.tail_text(10));
    }
    let name = root.file_name().context("DXVK folder name")?;
    let staged = staging.join(name);
    if !extracted(&staged) {
        let _ = fs::remove_dir_all(&staging);
        bail!(
            "the DXVK archive is missing some of {}",
            DXVK_DLLS.join(", ")
        );
    }
    let _ = fs::remove_dir_all(root);
    let moved = fs::rename(&staged, root)
        .with_context(|| format!("moving {} to {}", staged.display(), root.display()));
    let _ = fs::remove_dir_all(&staging);
    moved
}

/// Remove the DLLs and overrides if a previous install enabled DXVK.
pub fn remove<F>(prefix: &Path, dry_run: bool, log_path: Option<PathBuf>, on_line: F) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let wine_dir = prefix.join(WINE_DIR_NAME);
    if dry_run {
        let Some(version) = config::read_prefix_state(prefix, ".dxvk_version") else {
            return Ok(());
        };
        report.info(format!("removing DXVK {version}"));
        report.info("    remove the DXVK DLLs from system32 and syswow64");
        report.info("    delete their DllOverrides entries");
        report.info(format!(
            "    WINEPREFIX={} {}/bin/wineboot -u",
            prefix.display(),
            wine_dir.display()
        ));
        return Ok(());
    }
    remove_installed(prefix, &wine_dir, &mut report);
    Ok(())
}

/// Remove DXVK when the prefix records an installed version.
fn remove_installed(prefix: &Path, wine_dir: &Path, report: &mut Report<'_>) {
    let Some(version) = config::read_prefix_state(prefix, ".dxvk_version") else {
        return;
    };
    let wine = wine_dir.join("bin/wine");
    report.info(format!("removing DXVK {version}"));
    wine::kill_prefix(prefix, Some(wine_dir), report);
    for dll in DXVK_DLLS {
        let _ = fs::remove_file(prefix.join("drive_c/windows/system32").join(dll));
        let _ = fs::remove_file(prefix.join("drive_c/windows/syswow64").join(dll));
        let mut cmd = Command::new(&wine);
        cmd.args([
            "reg",
            "delete",
            "HKCU\\Software\\Wine\\DllOverrides",
            "/v",
            dll.trim_end_matches(".dll"),
            "/f",
        ]);
        run_quiet(report, cmd, prefix, wine_dir);
    }
    // Restore Wine's builtin DLL placeholders in system32 and syswow64.
    wine::run_wineboot_update(prefix, wine_dir, report);
    let _ = fs::remove_file(config::prefix_state_path(prefix, ".dxvk_version"));
    report.info("DXVK removed");
}

fn set_override(prefix: &Path, wine: &Path, wine_dir: &Path, name: &str, report: &mut Report<'_>) {
    let mut cmd = Command::new(wine);
    cmd.args([
        "reg",
        "add",
        "HKCU\\Software\\Wine\\DllOverrides",
        "/v",
        name,
        "/t",
        "REG_SZ",
        "/d",
        "native,builtin",
        "/f",
    ]);
    let mut options = RunOptions::default().stall(Duration::from_secs(60));
    options.envs = wine::wine_env(prefix, wine_dir, "-all,fixme-all");
    match runner::run(cmd, options, |stream, line| report.line(stream, line)) {
        Ok(result) if result.success() => {}
        Ok(result) => report.warn(format!(
            "could not set DLL override {name}: {}",
            result.status
        )),
        Err(error) => report.warn(format!("could not set DLL override {name}: {error}")),
    }
}

fn run_quiet(report: &mut Report<'_>, cmd: Command, prefix: &Path, wine_dir: &Path) {
    let mut options = RunOptions::default().stall(Duration::from_secs(60)).quiet();
    options.envs = wine::wine_env(prefix, wine_dir, "-all,fixme-all");
    let _ = runner::run(cmd, options, |stream, line| report.line(stream, line));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_release_the_driver_supports() {
        assert_eq!(release_for(Some((1, 4))).unwrap().version, "3.1.1");
        assert_eq!(release_for(Some((2, 0))).unwrap().version, "3.1.1");
        assert_eq!(release_for(Some((1, 3))).unwrap().version, "2.7.1");
        assert!(release_for(Some((1, 2))).is_none());
        assert!(release_for(None).is_none());
        assert!(RELEASES.iter().all(|release| release.sha256.len() == 64));
    }

    #[test]
    fn dry_run_does_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let mut lines = Vec::new();
        install_for(&prefix, Some((1, 3)), true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("dxvk-2.7.1.tar.gz"));
        assert!(text.contains("sha256sum"));
        assert!(text.contains("/v d3d11 "));
        assert!(text.contains("/v dxgi "));
        assert!(!prefix.join(".cache").exists());
    }

    #[test]
    fn dry_run_without_vulkan_skips_dxvk() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let version = install_for(dir.path(), None, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        assert!(version.is_none());
        assert!(lines.join("\n").contains("skip DXVK"));
    }

    #[test]
    fn partial_extraction_is_not_complete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dxvk-3.1.1");
        fs::create_dir_all(root.join("x64")).unwrap();
        fs::create_dir_all(root.join("x32")).unwrap();
        fs::write(root.join("x64/d3d9.dll"), b"MZ").unwrap();
        assert!(!extracted(&root));
        for dll in DXVK_DLLS {
            fs::write(root.join("x64").join(dll), b"MZ").unwrap();
            fs::write(root.join("x32").join(dll), b"MZ").unwrap();
        }
        assert!(extracted(&root));
    }

    #[test]
    fn remove_is_a_no_op_without_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        remove(dir.path(), false, None, |_, _| {}).unwrap();
        assert!(!dir.path().join(".cache").exists());
    }

    #[test]
    fn remove_dry_run_restores_builtins() {
        let dir = tempfile::tempdir().unwrap();
        config::write_prefix_state(dir.path(), ".dxvk_version", "3.1.1").unwrap();
        let mut lines = Vec::new();
        remove(dir.path(), true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        assert!(lines.join("\n").contains("wineboot -u"));
    }
}
