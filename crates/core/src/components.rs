//! Windows fonts and plugin runtimes inside the prefix.
//!
//! FLEX crashes on startup without Arial Bold and Verdana, so the Microsoft
//! core fonts are not cosmetic. The MS Gothic font renders musical symbols
//! such as sharps and flats. Visual C++, GDI+ and the shader compiler are the
//! runtimes most Windows plugins expect.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use crate::WINE_DIR_NAME;
use crate::http;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};

const MSGOTHIC_URL: &str =
    "https://raw.githubusercontent.com/FSKiller/Microsoft-Fonts/main/msgothic.ttc";
const WEBVIEW2_URL: &str = "https://go.microsoft.com/fwlink/?linkid=2124701";
const WEBVIEW2_INSTALLER: &str = "MicrosoftEdgeWebView2RuntimeInstallerX64.exe";
const MSGOTHIC_NAME: &str = "msgothic.ttc";
const PLUGIN_RUNTIMES: &[&str] = &["vcrun2022", "gdiplus", "d3dcompiler_47"];
const REQUIRED_FONTS: &[&str] = &["arialbd.ttf", "verdana.ttf"];

#[derive(Debug, Clone, Default)]
pub struct ComponentReport {
    pub fonts: bool,
    pub msgothic: bool,
    pub runtimes: bool,
    pub failures: Vec<String>,
}

pub fn install<F>(
    prefix: &Path,
    fonts: bool,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<ComponentReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let mut outcome = ComponentReport::default();

    if fonts {
        outcome.fonts = install_fonts(prefix, dry_run, &mut report)?;
        outcome.msgothic = install_msgothic(prefix, dry_run, &mut report)?;
        if !outcome.fonts {
            outcome.failures.push("Microsoft fonts".to_string());
        }
        if !outcome.msgothic {
            outcome.failures.push("MS Gothic".to_string());
        }
    }

    outcome.runtimes = install_runtimes(prefix, dry_run, &mut report)?;
    if !outcome.runtimes {
        outcome.failures.push("plugin runtimes".to_string());
    }

    Ok(outcome)
}

/// Repair path: only install the fonts when they are actually missing.
pub fn ensure_fonts<F>(
    prefix: &Path,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<bool>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let fonts = install_fonts(prefix, dry_run, &mut report)?;
    let _ = install_msgothic(prefix, dry_run, &mut report)?;
    Ok(fonts)
}

/// Repair path: reinstall the plugin runtimes in case an earlier attempt
/// failed or a plugin installer replaced them.
pub fn ensure_runtimes<F>(
    prefix: &Path,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<bool>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    install_runtimes(prefix, dry_run, &mut report)
}

fn install_fonts(prefix: &Path, dry_run: bool, report: &mut Report<'_>) -> Result<bool> {
    report.info("Microsoft core fonts");
    if fonts_present(prefix) && !dry_run {
        report.info("fonts are already installed");
        return Ok(true);
    }
    if dry_run {
        report.info(format!(
            "    WINEPREFIX={} winetricks --unattended --no-isolate --optout --force corefonts",
            prefix.display()
        ));
        report.info("    fallback: winetricks --force tahoma allfonts");
        return Ok(true);
    }
    if crate::deps::which("winetricks").is_none() {
        report.warn("skipping fonts: winetricks is missing");
        return Ok(false);
    }

    let installed = winetricks(prefix, "corefonts", report)?;
    if !installed || !fonts_present(prefix) {
        report.warn("corefonts did not provide every font; installing tahoma");
        let _ = winetricks(prefix, "tahoma", report)?;
    }
    if !fonts_present(prefix) {
        report.warn("Arial Bold or Verdana is still missing; trying allfonts");
        let _ = winetricks(prefix, "allfonts", report)?;
    }
    let ok = fonts_present(prefix);
    if ok {
        report.info("Microsoft fonts installed");
    } else {
        report.warn("Arial Bold or Verdana is still missing; FLEX may crash");
    }
    Ok(ok)
}

fn install_runtimes(prefix: &Path, dry_run: bool, report: &mut Report<'_>) -> Result<bool> {
    report.info("plugin runtimes: Visual C++, GDI+ and the shader compiler");
    let mut all_ok = true;
    for component in PLUGIN_RUNTIMES {
        if dry_run {
            report.info(format!(
                "    WINEPREFIX={} winetricks --unattended --no-isolate --optout --force {component}",
                prefix.display()
            ));
            continue;
        }
        if crate::deps::which("winetricks").is_none() {
            report.warn("skipping plugin runtimes: winetricks is missing");
            return Ok(false);
        }
        if !winetricks(prefix, component, report)? {
            all_ok = false;
            report.warn(format!("{component} failed; continuing"));
        }
    }
    Ok(all_ok)
}

fn winetricks(prefix: &Path, component: &str, report: &mut Report<'_>) -> Result<bool> {
    report.info(format!("winetricks {component}"));
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let mut cmd = Command::new("winetricks");
    cmd.args([
        "--unattended",
        "--no-isolate",
        "--optout",
        "--force",
        component,
    ]);
    kill_prefix(prefix, &wine_dir, report)?;
    let mut options = wine_env(prefix, &wine_dir);
    options.stall_timeout = Some(Duration::from_secs(1800));
    let result = runner::run(cmd, options, |stream, line| report.line(stream, line))?;
    let _ = kill_prefix(prefix, &wine_dir, report);
    Ok(result.success())
}

fn install_msgothic(prefix: &Path, dry_run: bool, report: &mut Report<'_>) -> Result<bool> {
    report.info("MS Gothic musical symbols");
    let destination = fonts_dir(prefix).join(MSGOTHIC_NAME);
    if destination.is_file() && !dry_run {
        report.info("MS Gothic is already installed");
        return Ok(true);
    }
    if dry_run {
        let cached = http::cache_dir()?.join(MSGOTHIC_NAME);
        report.info(format!(
            "    curl -fLR -z {0} -o {0} {MSGOTHIC_URL}",
            cached.display()
        ));
        report.info(format!(
            "    cp {} {}",
            cached.display(),
            destination.display()
        ));
        report.info(format!(
            "    WINEPREFIX={} {}/bin/wineboot -u",
            prefix.display(),
            prefix.join(WINE_DIR_NAME).display()
        ));
        return Ok(true);
    }

    let cached = match http::download_cached(
        report,
        MSGOTHIC_URL,
        MSGOTHIC_NAME,
        Duration::from_secs(300),
    ) {
        Ok(cached) => cached,
        Err(error) => {
            report.warn(format!("could not download MS Gothic: {error:#}"));
            return Ok(false);
        }
    };
    let fonts = fonts_dir(prefix);
    if let Err(error) = fs::create_dir_all(&fonts) {
        report.warn(format!("could not create {}: {error}", fonts.display()));
        return Ok(false);
    }
    if let Err(error) = fs::copy(&cached, &destination) {
        report.warn(format!(
            "could not copy MS Gothic to {}: {error}",
            destination.display()
        ));
        return Ok(false);
    }

    // Wine builds its font registry during wineboot.
    let wineboot = prefix.join(WINE_DIR_NAME).join("bin/wineboot");
    if wineboot.is_file() {
        let mut cmd = Command::new(&wineboot);
        cmd.arg("-u");
        let mut options = wine_env(prefix, &prefix.join(WINE_DIR_NAME));
        options.stall_timeout = Some(Duration::from_secs(300));
        options.envs.push((
            "WINEDLLOVERRIDES".into(),
            "mscoree=;mshtml=;winemenubuilder.exe=d;winegstreamer=".into(),
        ));
        let _ = runner::run(cmd, options, |stream, line| report.line(stream, line));
    }
    report.info(format!("MS Gothic installed at {}", destination.display()));
    Ok(destination.is_file())
}

fn fonts_dir(prefix: &Path) -> PathBuf {
    prefix.join("drive_c/windows/Fonts")
}

/// True when the fonts FLEX needs are present.
pub fn fonts_present(prefix: &Path) -> bool {
    let fonts = fonts_dir(prefix);
    REQUIRED_FONTS.iter().all(|name| fonts.join(name).is_file())
}

/// Install Microsoft's Evergreen WebView2 runtime when FL Studio's own
/// installer skipped it. It is required for the Sounds tab, Help and FL Cloud.
pub fn install_webview2<F>(
    prefix: &Path,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<bool>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    report.info("Microsoft Edge WebView2 runtime");
    if webview2_installed(prefix) && !dry_run {
        report.info("WebView2 is already installed");
        return Ok(true);
    }
    if dry_run {
        let cached = http::cache_dir()?.join(WEBVIEW2_INSTALLER);
        report.info(format!(
            "    curl -fLR -z {0} -o {0} {WEBVIEW2_URL}",
            cached.display()
        ));
        report.info(format!(
            "    WINEPREFIX={} {}/bin/wine {} /silent /install",
            prefix.display(),
            prefix.join(WINE_DIR_NAME).display(),
            cached.display()
        ));
        return Ok(true);
    }
    if !prefix.join(WINE_DIR_NAME).join("bin/wine").is_file() {
        report.warn("skipping WebView2: Wine is missing");
        return Ok(false);
    }
    let installer = match http::download_cached(
        &mut report,
        WEBVIEW2_URL,
        WEBVIEW2_INSTALLER,
        Duration::from_secs(900),
    ) {
        Ok(installer) => installer,
        Err(error) => {
            report.warn(format!("could not download WebView2: {error:#}"));
            return Ok(false);
        }
    };
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");
    kill_prefix(prefix, &wine_dir, &mut report)?;
    let mut cmd = Command::new(&wine);
    cmd.arg(&installer).args(["/silent", "/install"]);
    let mut options = wine_env(prefix, &wine_dir);
    options.stall_timeout = Some(Duration::from_secs(1800));
    let _ = runner::run(cmd, options, |stream, line| report.line(stream, line));
    // The bootstrapper can return before the runtime appears.
    for _ in 0..150 {
        if webview2_installed(prefix) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let _ = kill_prefix(prefix, &wine_dir, &mut report);
    if webview2_installed(prefix) {
        report.info("WebView2 installed");
        Ok(true)
    } else {
        report.warn("WebView2 was not found after the installer finished");
        Ok(false)
    }
}

/// The installed WebView2 runtime, if FL Studio's installer managed to add it.
pub fn webview2_installed(prefix: &Path) -> bool {
    ["Program Files (x86)", "Program Files"]
        .iter()
        .map(|programs| {
            prefix
                .join("drive_c")
                .join(programs)
                .join("Microsoft/EdgeWebView/Application")
        })
        .filter_map(|application| fs::read_dir(application).ok())
        .flatten()
        .flatten()
        .any(|entry| entry.path().join("msedgewebview2.exe").is_file())
}

fn wine_env(prefix: &Path, wine_dir: &Path) -> RunOptions {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut options = RunOptions::default();
    options = options.env("WINEPREFIX", prefix.display().to_string());
    options = options.env("WINEDEBUG", "-all,fixme-all");
    options = options.env("WINESERVER_TIMEOUT", "60");
    options = options.env("WINETRICKS_GUI", "none");
    options = options.env("PATH", format!("{}:{path}", wine_dir.join("bin").display()));
    options = options.env("WINE", wine_dir.join("bin/wine").display().to_string());
    options = options.env(
        "WINESERVER",
        wine_dir.join("bin/wineserver").display().to_string(),
    );
    options
}

fn kill_prefix(prefix: &Path, wine_dir: &Path, report: &mut Report<'_>) -> Result<()> {
    let wineserver = wine_dir.join("bin/wineserver");
    let mut cmd = if wineserver.is_file() {
        Command::new(wineserver)
    } else if let Some(path) = crate::deps::which("wineserver") {
        Command::new(path)
    } else {
        return Ok(());
    };
    cmd.arg("-k");
    let mut options = RunOptions::default().stall(Duration::from_secs(30)).quiet();
    options = options.env("WINEPREFIX", prefix.display().to_string());
    let _ = runner::run(cmd, options, |stream, line| report.line(stream, line));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_core_fonts() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!fonts_present(dir.path()));
        let fonts = fonts_dir(dir.path());
        fs::create_dir_all(&fonts).unwrap();
        for name in REQUIRED_FONTS {
            fs::write(fonts.join(name), b"font").unwrap();
        }
        assert!(fonts_present(dir.path()));
    }

    #[test]
    fn webview2_detection_looks_both_program_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!webview2_installed(dir.path()));
        let application = dir
            .path()
            .join("drive_c/Program Files (x86)/Microsoft/EdgeWebView/Application");
        fs::create_dir_all(application.join("SetupMetrics")).unwrap();
        assert!(!webview2_installed(dir.path()));
        let exe = application.join("142.0.3595.94/msedgewebview2.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(exe, b"exe").unwrap();
        assert!(webview2_installed(dir.path()));
    }

    #[test]
    fn dry_run_has_no_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let report = install(dir.path(), true, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(report.fonts);
        assert!(report.runtimes);
        assert!(text.contains("corefonts"));
        assert!(text.contains("tahoma"));
        assert!(text.contains("msgothic.ttc"));
        assert!(text.contains("vcrun2022"));
        assert!(text.contains("d3dcompiler_47"));
        assert!(!fonts_dir(dir.path()).exists());
    }

    #[test]
    fn fonts_can_be_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        install(dir.path(), false, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(!text.contains("corefonts"));
        assert!(text.contains("vcrun2022"));
    }
}
