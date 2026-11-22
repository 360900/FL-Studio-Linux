//! FL Studio settings that make the Linux prefix usable on first launch.
//!
//! FL Studio stores its devices under
//! `HKCU\\Software\\Image-Line\\FL Studio <major>\\Devices`. Writing the audio
//! device before the first launch selects WinePipe. The PipeWire
//! sequencer ports are disabled because Wine opens them as MIDI hardware and
//! they always fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use crate::audio::AudioDriver;
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{WINE_DIR_NAME, app_install_dir};

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
    let installed = dry_run
        || crate::config::read_prefix_state(prefix, ".audio_driver").as_deref()
            == Some(driver.label());
    if let Some(device) = driver.fl_device()
        && installed
    {
        let audio = format!("{key}\\Devices\\Audio output");
        reg_add(
            prefix,
            &wine,
            "Device name",
            device,
            &audio,
            dry_run,
            &mut report,
        )?;
        reg_add(
            prefix,
            &wine,
            "Sample rate",
            "48000",
            &audio,
            dry_run,
            &mut report,
        )?;
        reg_add(
            prefix,
            &wine,
            "Buffer length (DS)",
            "256",
            &audio,
            dry_run,
            &mut report,
        )?;
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
        reg_add(prefix, &wine, "Enabled", "0", &key, dry_run, &mut report)?;
        reg_add(prefix, &wine, "Port", "-1", &key, dry_run, &mut report)?;
    }

    // With no hardware controller, FL's input scan still opens the failing
    // PipeWire ports. Turning input off when there is no hardware keeps the
    // startup clean; a later repair re-enables it once a controller appears.
    let hardware = hardware_midi_present();
    report.info(if hardware {
        "MIDI hardware found; leaving FL Studio MIDI input enabled"
    } else {
        "no MIDI hardware found; disabling FL Studio MIDI input"
    });
    reg_add(
        prefix,
        &wine,
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

#[allow(clippy::too_many_arguments)]
fn reg_add(
    prefix: &Path,
    wine: &Path,
    name: &str,
    value: &str,
    key: &str,
    dry_run: bool,
    report: &mut Report<'_>,
) -> Result<()> {
    let mut cmd = Command::new(wine);
    cmd.args([
        "reg", "add", key, "/v", name, "/t", "REG_SZ", "/d", value, "/f",
    ]);
    if dry_run {
        report.info(format!(
            "    WINEPREFIX={} {} reg add {} /v '{}' /d '{}' /f",
            prefix.display(),
            wine.display(),
            key,
            name,
            value
        ));
        return Ok(());
    }
    let mut options = RunOptions::default().stall(Duration::from_secs(60));
    options = options.env("WINEPREFIX", prefix.display().to_string());
    options = options.env("WINEDEBUG", "-all");
    options = options.env("WINESERVER_TIMEOUT", "60");
    options = options.env(
        "WINEDLLPATH",
        prefix
            .join(WINE_DIR_NAME)
            .join("lib/wine")
            .display()
            .to_string(),
    );
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
}
