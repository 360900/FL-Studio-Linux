//! FL Studio settings that make the Linux prefix usable on first launch.
//!
//! FL Studio stores its devices under
//! `HKCU\\Software\\Image-Line\\FL Studio <major>\\Devices`. Writing the audio
//! device before the first launch selects WinePipe. The PipeWire
//! sequencer ports are disabled because Wine opens them as MIDI hardware and
//! they always fail. Every value is only written when it is missing, so
//! updates and repairs keep the user's own settings.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use crate::audio::AudioDriver;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{WINE_DIR_NAME, app_install_dir, wine};

pub fn configure<F>(
    prefix: &Path,
    driver: AudioDriver,
    dry_run: bool,
    log_path: Option<PathBuf>,
    on_line: F,
) -> Result<()>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(log_path, &mut sink);
    let key = fl_key(prefix);
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");
    if !dry_run && !wine.is_file() {
        return Ok(());
    }

    report.info(format!("FL Studio settings: {key}"));
    let audio = format!("{key}\\Devices\\Audio output");
    let installed = dry_run
        || crate::config::read_prefix_state(prefix, ".audio_driver").as_deref()
            == Some(driver.label());
    if let Some(device) = driver.fl_device()
        && installed
    {
        reg_add(
            prefix,
            &wine_dir,
            "Device name",
            device,
            &audio,
            dry_run,
            &mut report,
        )?;
        reg_add(
            prefix,
            &wine_dir,
            "Sample rate",
            "48000",
            &audio,
            dry_run,
            &mut report,
        )?;
        reg_add(
            prefix,
            &wine_dir,
            "Buffer length (DS)",
            "256",
            &audio,
            dry_run,
            &mut report,
        )?;
    } else if driver == AudioDriver::None {
        clear_winepipe_device(prefix, &wine_dir, &audio, dry_run, &mut report)?;
    }

    // These are PipeWire's internal sequencer clients, not MIDI hardware.
    // Leaving them enabled makes FL Studio raise "The driver was not enabled"
    // and, on some systems, a MIDI memory error.
    for name in [
        "PipeWire-System - input",
        "PipeWire-RT-Event - input",
        "Midi Through Port-0",
    ] {
        let key = format!("{key}\\Devices\\MIDI input\\{name}");
        reg_add(
            prefix,
            &wine_dir,
            "Enabled",
            "0",
            &key,
            dry_run,
            &mut report,
        )?;
        reg_add(prefix, &wine_dir, "Port", "-1", &key, dry_run, &mut report)?;
    }

    // With no hardware controller, FL's input scan still opens the failing
    // PipeWire ports. Turning input off when there is no hardware keeps the
    // first startup clean; FL's MIDI settings turn it back on.
    let hardware = hardware_midi_present();
    report.info(if hardware {
        "MIDI hardware found; FL Studio MIDI input defaults to on"
    } else {
        "no MIDI hardware found; FL Studio MIDI input defaults to off"
    });
    reg_add(
        prefix,
        &wine_dir,
        "Enable MIDI input",
        if hardware { "1" } else { "0" },
        &format!("{key}\\General"),
        dry_run,
        &mut report,
    )?;
    Ok(())
}

const VIRTUAL_MIDI: &[&str] = &[
    "System",
    "Midi Through",
    "WINE midi driver",
    "PipeWire",
    "PipeWire-System",
    "PipeWire-RT-Event",
];

fn hardware_midi_present() -> bool {
    match std::fs::read_to_string("/proc/asound/seq/clients") {
        Ok(text) => hardware_midi_in(&text),
        // Unknown: keep MIDI enabled rather than hiding a real controller.
        Err(_) => true,
    }
}

fn hardware_midi_in(text: &str) -> bool {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("Client")?;
            let (_, name) = rest.split_once(':')?;
            let name = name.trim().strip_prefix('"')?;
            let name = name.split('"').next()?;
            Some(name)
        })
        .any(|name| !VIRTUAL_MIDI.contains(&name))
}

fn fl_key(prefix: &Path) -> String {
    let year = app_install_dir(prefix)
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .and_then(|name| {
            name.split(|c: char| !c.is_ascii_digit())
                .find(|part| !part.is_empty())
                .and_then(|part| part.parse::<u32>().ok())
        });
    let major = match year {
        Some(year) if year >= 2000 => year - 2000,
        Some(year) => year,
        None => 26,
    };
    format!("HKCU\\Software\\Image-Line\\FL Studio {major}")
}

fn clear_winepipe_device(
    prefix: &Path,
    wine_dir: &Path,
    audio_key: &str,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let wine_bin = wine_dir.join("bin/wine");
    let Some(winepipe_device) = AudioDriver::WinePipe.fl_device() else {
        return Ok(());
    };
    if dry_run {
        report.info(format!(
            "    WINEPREFIX={} {} reg delete {} /v 'Device name' /f (if set to {winepipe_device})",
            prefix.display(),
            wine_bin.display(),
            audio_key
        ));
        return Ok(());
    }
    let mut query = Command::new(&wine_bin);
    query.args(["reg", "query", audio_key, "/v", "Device name"]);
    let mut options = RunOptions::default().stall(Duration::from_secs(60)).quiet();
    options.envs = wine::wine_env(prefix, wine_dir, "-all");
    let Ok(result) = runner::run(query, options, |stream, line| report.line(stream, line)) else {
        return Ok(());
    };
    if !result.success()
        || !result
            .stdout_tail
            .iter()
            .any(|line| line.contains(winepipe_device))
    {
        return Ok(());
    }
    let mut del = Command::new(&wine_bin);
    del.args(["reg", "delete", audio_key, "/v", "Device name", "/f"]);
    let mut options = RunOptions::default().stall(Duration::from_secs(60));
    options.envs = wine::wine_env(prefix, wine_dir, "-all");
    let result = runner::run(del, options, |stream, line| report.line(stream, line))?;
    if !result.success() {
        report.warn(format!(
            "could not clear {audio_key}\\Device name; continuing"
        ));
    }
    Ok(())
}

/// Write a default only when the value does not exist yet.
fn reg_add(
    prefix: &Path,
    wine_dir: &Path,
    name: &str,
    value: &str,
    key: &str,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let wine_bin = wine_dir.join("bin/wine");
    if dry_run {
        report.info(format!(
            "    WINEPREFIX={} {} reg add {} /v '{}' /d '{}' /f (if missing)",
            prefix.display(),
            wine_bin.display(),
            key,
            name,
            value
        ));
        return Ok(());
    }
    let mut query = Command::new(&wine_bin);
    query.args(["reg", "query", key, "/v", name]);
    let mut options = RunOptions::default().stall(Duration::from_secs(60)).quiet();
    options.envs = wine::wine_env(prefix, wine_dir, "-all");
    if runner::run(query, options, |stream, line| report.line(stream, line))?.success() {
        return Ok(());
    }

    let mut cmd = Command::new(&wine_bin);
    cmd.args([
        "reg", "add", key, "/v", name, "/t", "REG_SZ", "/d", value, "/f",
    ]);
    let mut options = RunOptions::default().stall(Duration::from_secs(60));
    options.envs = wine::wine_env(prefix, wine_dir, "-all");
    let result = runner::run(cmd, options, |stream, line| report.line(stream, line))?;
    if !result.success() {
        report.warn(format!("could not set {key}\\{name}; continuing"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_major_version_from_the_install_folder() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app_dir(dir.path()).join("FL Studio 2026");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("FL64.exe"), b"MZ").unwrap();
        assert_eq!(
            fl_key(dir.path()),
            "HKCU\\Software\\Image-Line\\FL Studio 26"
        );
    }

    #[test]
    fn defaults_to_26_without_an_install() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            fl_key(dir.path()),
            "HKCU\\Software\\Image-Line\\FL Studio 26"
        );
    }

    #[test]
    fn dry_run_prints_audio_and_midi() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app_dir(dir.path()).join("FL Studio 2026");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("FL64.exe"), b"MZ").unwrap();
        std::fs::write(
            crate::config::prefix_state_path(dir.path(), ".audio_driver"),
            "winepipe",
        )
        .unwrap();
        let mut lines = Vec::new();
        configure(dir.path(), AudioDriver::WinePipe, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("1WinePipe"));
        assert!(text.contains("PipeWire-System - input"));
        assert!(text.contains("/d '0'"));
        assert!(text.contains("(if missing)"));
    }

    #[test]
    fn detects_only_real_midi_clients() {
        let virtual_only = "Client   0 : \"System\" [Kernel Legacy]\n\
            Client  14 : \"Midi Through\" [Kernel Legacy]\n\
            Client 128 : \"WINE midi driver\" [User Legacy]\n\
            Client 144 : \"PipeWire-System\" [User Legacy]\n\
            Client 145 : \"PipeWire-RT-Event\" [User Legacy]\n";
        assert!(!hardware_midi_in(virtual_only));

        let keyboard = format!("{virtual_only}Client 148 : \"Arturia MiniLab 3\" [User Legacy]\n");
        assert!(hardware_midi_in(&keyboard));
    }

    #[test]
    fn dry_run_none_clears_winepipe_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        configure(dir.path(), AudioDriver::None, true, None, |_, line| {
            lines.push(line.to_string())
        })
        .unwrap();
        let text = lines.join("\n");
        assert!(text.contains("reg delete"));
        assert!(text.contains("Device name"));
    }
}
