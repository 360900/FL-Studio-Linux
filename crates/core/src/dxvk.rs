//! DXVK installation and removal.
//!
//! DXVK is enabled by default to map Direct3D calls from FL Studio and plugins
//! to Vulkan. It can be removed with `repair --dxvk false`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::WINE_DIR_NAME;
use crate::config;
use crate::http;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::wine;

pub const DXVK_REPO: &str = "doitsujin/dxvk";
pub const DXVK_FALLBACK_VERSION: &str = "3.1.1";
const DXVK_DLLS: [&str; 5] = [
    "d3d8.dll",
    "d3d9.dll",
    "d3d10core.dll",
    "d3d11.dll",
    "dxgi.dll",
];

pub fn install<F>(
    prefix: &Path,
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
    if dry_run {
        let dlls = DXVK_DLLS.join(" ");
        report.info(format!(
            "    curl -fL https://github.com/{DXVK_REPO}/releases/download/v<latest>/dxvk-<latest>.tar.gz -o <cache>"
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

    let version = latest_version().unwrap_or_else(|| DXVK_FALLBACK_VERSION.to_string());
    let archive = http::download_cached(
        &mut report,
        &format!(
            "https://github.com/{DXVK_REPO}/releases/download/v{version}/dxvk-{version}.tar.gz"
        ),
        &format!("dxvk-{version}.tar.gz"),
        Duration::from_secs(600),
    )?;

    let cache = prefix.join(".cache");
    let root = cache.join(format!("dxvk-{version}"));
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    if !root.join("x64/d3d9.dll").is_file() {
        let mut cmd = Command::new("tar");
        cmd.arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&cache)
            .arg("--no-same-owner");
        let result = runner::run(
            cmd,
            RunOptions::default().stall(Duration::from_secs(300)),
            |stream, line| report.line(stream, line),
        )?;
        if !result.success() {
            bail!("extracting DXVK failed: {}", result.tail_text(10));
        }
    }

    wine::kill_prefix(prefix, Some(&wine_dir), &mut report);
    let system32 = prefix.join("drive_c/windows/system32");
    let syswow64 = prefix.join("drive_c/windows/syswow64");
    fs::create_dir_all(&system32)?;
    for dll in DXVK_DLLS {
        let source = root.join("x64").join(dll);
        if !source.is_file() {
            bail!("the DXVK archive is missing x64/{dll}");
        }
        fs::copy(&source, system32.join(dll))
            .with_context(|| format!("copying {}", source.display()))?;
        if syswow64.is_dir() {
            let source = [root.join("x86").join(dll), root.join("x32").join(dll)]
                .into_iter()
                .find(|path| path.is_file());
            if let Some(source) = source {
                fs::copy(&source, syswow64.join(dll))
                    .with_context(|| format!("copying {}", source.display()))?;
            }
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
    config::write_prefix_state(prefix, ".dxvk_version", &version)?;
    report.info(format!("installed DXVK {version}"));
    Ok(Some(version))
}

/// Remove the DLLs and overrides if a previous install enabled DXVK.
pub fn remove<F>(prefix: &Path, dry_run: bool, log_path: Option<PathBuf>, on_line: F) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let Some(version) = config::read_prefix_state(prefix, ".dxvk_version") else {
        return Ok(());
    };
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");
    report.info(format!("removing DXVK {version}"));
    if dry_run {
        report.info("    remove the DXVK DLLs from system32 and syswow64");
        report.info("    delete their DllOverrides entries");
        report.info(format!(
            "    WINEPREFIX={} {}/bin/wineboot -u",
            prefix.display(),
            wine_dir.display()
        ));
        return Ok(());
    }

    wine::kill_prefix(prefix, Some(&wine_dir), &mut report);
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
        run_quiet(&mut report, cmd, prefix, &wine_dir);
    }
    // Restore Wine's builtin DLL placeholders in system32 and syswow64.
    wine::run_wineboot_update(prefix, &wine_dir, &mut report);
    let _ = fs::remove_file(config::prefix_state_path(prefix, ".dxvk_version"));
    report.info("DXVK removed");
    Ok(())
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

fn latest_version() -> Option<String> {
    let url = format!("https://api.github.com/repos/{DXVK_REPO}/releases/latest");
    let text = http::fetch_text(&url).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    Some(tag.trim().trim_start_matches('v').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_does_not_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let mut lines = Vec::new();
        install(&prefix, true, None, |_, line| lines.push(line.to_string())).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("dxvk"));
        assert!(text.contains("/v d3d11 "));
        assert!(text.contains("/v dxgi "));
        assert!(!prefix.join(".cache").exists());
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

    #[test]
    fn strips_the_release_prefix() {
        assert_eq!("v3.1.1".trim_start_matches('v'), "3.1.1");
    }
}
