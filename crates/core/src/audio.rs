//! Low-latency ASIO drivers installed into the private Wine build.
//!
//! WinePipe talks to PipeWire directly. Its binaries are bundled so the
//! install does not need a compiler toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::WINE_DIR_NAME;
use crate::config;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::wine;

const WINEPIPE_SO: &[u8] = include_bytes!("../../../vendor/winepipe/winepipe.so");
const WINEPIPE_DLL64: &[u8] = include_bytes!("../../../vendor/winepipe/winepipe-x86_64.dll");
const WINEPIPE_DLL32: &[u8] = include_bytes!("../../../vendor/winepipe/winepipe-i386.dll");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioDriver {
    #[default]
    #[serde(alias = "wineasio")]
    WinePipe,
    None,
}

impl AudioDriver {
    pub fn label(&self) -> &'static str {
        match self {
            AudioDriver::WinePipe => "winepipe",
            AudioDriver::None => "none",
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            AudioDriver::WinePipe => "WinePipe",
            AudioDriver::None => "Off",
        }
    }

    /// Name FL Studio stores in `Devices\\Audio output`.
    pub fn fl_device(&self) -> Option<&'static str> {
        match self {
            AudioDriver::WinePipe => Some("1WinePipe"),
            AudioDriver::None => None,
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "winepipe" => Some(AudioDriver::WinePipe),
            // Old configs that selected WineASIO fall back to WinePipe.
            "wineasio" => Some(AudioDriver::WinePipe),
            "none" => Some(AudioDriver::None),
            _ => None,
        }
    }

    pub fn from_index(index: i32) -> Self {
        match index {
            1 => AudioDriver::None,
            _ => AudioDriver::WinePipe,
        }
    }

    pub fn index(&self) -> i32 {
        match self {
            AudioDriver::WinePipe => 0,
            AudioDriver::None => 1,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AudioReport {
    pub driver: AudioDriver,
    pub installed: bool,
    pub detail: Option<String>,
}

pub fn install<F>(
    prefix: &Path,
    driver: AudioDriver,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<AudioReport>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    if driver == AudioDriver::None {
        report.info("low-latency audio: off");
        remove_winepipe(prefix, dry_run, &mut report);
        if !dry_run {
            let _ = fs::remove_file(config::prefix_state_path(prefix, ".audio_driver"));
        }
        return Ok(AudioReport {
            driver,
            installed: false,
            detail: Some("disabled".to_string()),
        });
    }

    if !dry_run && let Err(reason) = winepipe_ready() {
        report.warn(format!(
            "{reason}; skipping WinePipe, so FL Studio keeps its default audio driver"
        ));
        return Ok(AudioReport {
            driver,
            installed: false,
            detail: Some(reason),
        });
    }

    let wine_dir = prefix.join(WINE_DIR_NAME);
    let lib = wine_dir.join("lib/wine");
    if !dry_run && !lib.join("x86_64-unix/ntdll.so").is_file() {
        bail!(
            "Wine is not installed in {}; cannot install {}",
            prefix.display(),
            driver.title()
        );
    }

    report.info(format!("low-latency audio: {}", driver.title()));
    match driver {
        AudioDriver::WinePipe => install_winepipe(prefix, &wine_dir, dry_run, &mut report)?,
        AudioDriver::None => unreachable!(),
    }
    if !dry_run {
        config::write_prefix_state(prefix, ".audio_driver", driver.label())?;
    }
    Ok(AudioReport {
        driver,
        installed: true,
        detail: None,
    })
}

/// WinePipe links against libpipewire and talks to the user's PipeWire
/// server; without either, FL Studio would get a device that never plays.
pub fn winepipe_ready() -> std::result::Result<(), String> {
    if crate::deps::library_present("libpipewire-0.3.so.0") == Some(false) {
        return Err("libpipewire is not installed".to_string());
    }
    if !pipewire_running() {
        return Err("PipeWire is not running".to_string());
    }
    Ok(())
}

/// The PipeWire server's socket exists in the user's runtime directory.
pub fn pipewire_running() -> bool {
    use std::os::unix::fs::MetadataExt;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            let uid = fs::metadata("/proc/self").ok()?.uid();
            Some(PathBuf::from(format!("/run/user/{uid}")))
        });
    let name = std::env::var("PIPEWIRE_REMOTE")
        .ok()
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .unwrap_or_else(|| "pipewire-0".to_string());
    runtime.is_some_and(|dir| dir.join(name).exists())
}

fn install_winepipe(
    prefix: &Path,
    wine_dir: &Path,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let lib = wine_dir.join("lib/wine");
    let system32 = prefix.join("drive_c/windows/system32");
    let syswow64 = prefix.join("drive_c/windows/syswow64");
    let files = [
        (lib.join("x86_64-unix/winepipe.so"), WINEPIPE_SO),
        (lib.join("x86_64-windows/winepipe.dll"), WINEPIPE_DLL64),
        (lib.join("i386-windows/winepipe.dll"), WINEPIPE_DLL32),
        (system32.join("winepipe.dll"), WINEPIPE_DLL64),
    ];
    if dry_run {
        for (path, _) in &files {
            report.info(format!("    install -Dm644 {}", path.display()));
        }
        if syswow64.is_dir() {
            report.info(format!(
                "    install -Dm644 {}",
                syswow64.join("winepipe.dll").display()
            ));
        }
        report.info(format!(
            "    WINEPREFIX={} {} regsvr32 /s winepipe.dll",
            prefix.display(),
            wine_dir.join("bin/wine").display()
        ));
        report.info(format!(
            "    WINEPREFIX={} {} 'C:\\\\windows\\\\syswow64\\\\regsvr32.exe' /s winepipe.dll",
            prefix.display(),
            wine_dir.join("bin/wine").display()
        ));
        return Ok(());
    }

    wine::kill_prefix(prefix, Some(wine_dir), report);
    for (path, bytes) in &files {
        write_file(path, bytes)?;
    }
    if syswow64.is_dir() {
        write_file(&syswow64.join("winepipe.dll"), WINEPIPE_DLL32)?;
    }

    regsvr(prefix, wine_dir, "winepipe.dll", false, false, report)?;
    if syswow64.is_dir()
        && let Err(error) = regsvr(prefix, wine_dir, "winepipe.dll", true, false, report)
    {
        report.warn(format!("32-bit WinePipe registration failed: {error}"));
    }
    report.info("WinePipe registered");
    Ok(())
}

fn remove_winepipe(prefix: &Path, dry_run: bool, report: &mut Report<'_>) {
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let lib = wine_dir.join("lib/wine");
    let system32 = prefix.join("drive_c/windows/system32");
    let syswow64 = prefix.join("drive_c/windows/syswow64");
    let files = [
        lib.join("x86_64-unix/winepipe.so"),
        lib.join("x86_64-windows/winepipe.dll"),
        lib.join("i386-windows/winepipe.dll"),
        system32.join("winepipe.dll"),
        syswow64.join("winepipe.dll"),
    ];
    let has_files = files.iter().any(|path| path.is_file());
    let has_marker = config::read_prefix_state(prefix, ".audio_driver").is_some();
    if !has_files && !has_marker {
        return;
    }

    if dry_run {
        report.info("    unregister and remove WinePipe from the Wine prefix");
        return;
    }

    if wine_dir.join("bin/wine").is_file() {
        wine::kill_prefix(prefix, Some(&wine_dir), report);
        let _ = regsvr(prefix, &wine_dir, "winepipe.dll", false, true, report);
        if syswow64.is_dir() {
            let _ = regsvr(prefix, &wine_dir, "winepipe.dll", true, true, report);
        }
    }
    for path in &files {
        let _ = fs::remove_file(path);
    }
    report.info("WinePipe removed");
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn regsvr(
    prefix: &Path,
    wine_dir: &Path,
    dll: &str,
    wow64: bool,
    unregister: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let mut cmd = Command::new(wine_dir.join("bin/wine"));
    if wow64 {
        cmd.arg("C:\\windows\\syswow64\\regsvr32.exe");
    } else {
        cmd.arg("regsvr32");
    }
    if unregister {
        cmd.arg("/u");
    }
    cmd.arg("/s").arg(dll);
    run_regsvr(prefix, wine_dir, cmd, report)
}

fn run_regsvr(prefix: &Path, wine_dir: &Path, cmd: Command, report: &mut Report<'_>) -> Result<()> {
    let mut options = RunOptions::default().stall(Duration::from_secs(120));
    options.envs = wine::wine_env(prefix, wine_dir, "-all");
    let result = runner::run(cmd, options, |stream, line| report.line(stream, line))?;
    if !result.success() {
        bail!("regsvr32 failed: {}", result.tail_text(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_names_match_fl_studio() {
        assert_eq!(AudioDriver::WinePipe.fl_device(), Some("1WinePipe"));
        assert_eq!(AudioDriver::None.fl_device(), None);
        assert_eq!(AudioDriver::from_index(1), AudioDriver::None);
        assert_eq!(AudioDriver::WinePipe.index(), 0);
    }

    #[test]
    fn none_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let report = install(dir.path(), AudioDriver::None, false, None, |_, _| {}).unwrap();
        assert!(!report.installed);
        assert!(!dir.path().join(".audio_driver").exists());
    }

    #[test]
    fn dry_run_does_not_touch_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let mut lines = Vec::new();
        install(prefix, AudioDriver::WinePipe, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("winepipe.so"));
        assert!(text.contains("regsvr32"));
        assert!(!prefix.join(".audio_driver").exists());
    }

    #[test]
    fn none_removes_existing_winepipe_files() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let dll = prefix.join("drive_c/windows/system32/winepipe.dll");
        fs::create_dir_all(dll.parent().unwrap()).unwrap();
        fs::write(&dll, b"dll").unwrap();
        config::write_prefix_state(prefix, ".audio_driver", "winepipe").unwrap();

        let report = install(prefix, AudioDriver::None, false, None, |_, _| {}).unwrap();
        assert!(!report.installed);
        assert!(!dll.exists());
        assert!(!prefix.join(".audio_driver").exists());
    }
}
