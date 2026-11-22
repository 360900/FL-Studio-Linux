//! Remove the FL Studio prefix, menu entries, handlers and icon.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::desktop::{DESKTOP_FILE, ICON_FILE, MIME_FILE, URL_HANDLER_FILE};
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{WINE_DIR_NAME, config};

#[derive(Debug, Clone)]
pub struct UninstallOptions {
    pub prefix: PathBuf,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct UninstallOutcome {
    pub removed_entries: usize,
    pub prefix_removed: bool,
}

pub fn uninstall<F>(opts: &UninstallOptions, on_line: F) -> Result<UninstallOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    // A log inside the prefix would be deleted with it, and writing the final
    // lines would recreate the directory we just removed.
    let log_path = opts
        .log_path
        .clone()
        .filter(|path| !path.starts_with(&opts.prefix));
    let mut report = Report::new(log_path, &mut sink);
    let mut outcome = UninstallOutcome::default();

    config::validate_prefix(&opts.prefix)?;
    if !opts.dry_run && opts.prefix.exists() && !config::looks_like_prefix(&opts.prefix) {
        bail!(
            "{} does not look like a Wine prefix; refusing to delete it",
            opts.prefix.display()
        );
    }
    report.info(format!(
        "Uninstalling FL Studio from {}",
        opts.prefix.display()
    ));

    if opts.dry_run {
        for path in removal_paths() {
            report.info(format!("would remove {}", path.display()));
        }
        report.info("would remove FL Studio entries created by Wine");
        if opts.prefix.exists() {
            report.info(format!("would run rm -rf {}", opts.prefix.display()));
        } else {
            report.info(format!("{} does not exist", opts.prefix.display()));
        }
        return Ok(outcome);
    }

    stop_prefix(&opts.prefix, &mut report);

    for path in removal_paths() {
        if !path.exists() {
            continue;
        }
        let result = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                outcome.removed_entries += 1;
                report.info(format!("removed {}", path.display()));
            }
            Err(error) => {
                report.warn(format!("could not remove {}: {error}", path.display()));
            }
        }
    }
    outcome.removed_entries += remove_fl_wine_entries(&mut report);
    if outcome.removed_entries > 0 {
        refresh_databases(&mut report);
    }

    if opts.prefix.exists() {
        report.info(format!("deleting {}", opts.prefix.display()));
        if let Err(error) = fs::remove_dir_all(&opts.prefix) {
            report.warn(format!(
                "initial removal failed ({error}); retrying after chmod"
            ));
            make_writable(&opts.prefix);
            if fs::remove_dir_all(&opts.prefix).is_err() {
                report.warn("retry failed; falling back to rm -rf");
                let _ = Command::new("rm")
                    .arg("-rf")
                    .arg(&opts.prefix)
                    .status()
                    .context("running rm -rf")?;
            }
        }
        if opts.prefix.exists() {
            bail!(
                "could not delete {}; close FL Studio and any Wine processes, then retry",
                opts.prefix.display()
            );
        }
        outcome.prefix_removed = true;
        report.info("prefix deleted");
    } else {
        report.info("prefix not found; nothing to delete");
    }

    let reset = config::default_prefix();
    report.info(format!("install location reset to {}", reset.display()));
    report.info("FL Studio projects and samples outside the prefix were kept");
    Ok(outcome)
}

fn stop_prefix(prefix: &Path, report: &mut Report<'_>) {
    let wine_dir = prefix.join(WINE_DIR_NAME);
    let wineserver = if wine_dir.join("bin/wineserver").exists() {
        wine_dir.join("bin/wineserver")
    } else {
        PathBuf::from("wineserver")
    };
    if wineserver == Path::new("wineserver") && crate::deps::which("wineserver").is_none() {
        return;
    }
    let mut cmd = Command::new(&wineserver);
    cmd.arg("-k");
    cmd.env("WINEPREFIX", prefix);
    match runner::run(
        cmd,
        RunOptions::default().stall(Duration::from_secs(30)).quiet(),
        |stream, line| report.line(stream, line),
    ) {
        Ok(_) => std::thread::sleep(Duration::from_millis(800)),
        Err(error) => report.warn(format!("could not stop Wine processes: {error}")),
    }
}

fn refresh_databases(report: &mut Report<'_>) {
    if let Some(dir) = applications_dir()
        && crate::deps::which("update-desktop-database").is_some()
    {
        let mut cmd = Command::new("update-desktop-database");
        cmd.arg(dir);
        let _ = runner::run(
            cmd,
            RunOptions::default().stall(Duration::from_secs(60)).quiet(),
            |stream, line| report.line(stream, line),
        );
    }
    if let Some(mime) = mime_dir()
        && crate::deps::which("update-mime-database").is_some()
    {
        let mut cmd = Command::new("update-mime-database");
        cmd.arg(mime);
        let _ = runner::run(
            cmd,
            RunOptions::default().stall(Duration::from_secs(60)).quiet(),
            |stream, line| report.line(stream, line),
        );
    }
}

fn make_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o777));
    if metadata.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

fn removal_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Some(data) = xdg_data_home() else {
        return paths;
    };
    let applications = data.join("applications");
    paths.push(applications.join(DESKTOP_FILE));
    paths.push(applications.join(URL_HANDLER_FILE));
    paths.push(applications.join("wine-protocol-flstudio.desktop"));
    paths.push(applications.join("wine-protocol-fl-cloud-plugins.desktop"));
    paths.push(data.join("icons/hicolor/scalable/apps").join(ICON_FILE));
    paths.push(data.join("mime/packages").join(MIME_FILE));
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join("Desktop/FL Studio.desktop"));
    }
    paths
}

/// Remove only FL Studio entries from Wine's menu folder, leaving other
/// Image-Line applications alone.
fn remove_fl_wine_entries(report: &mut Report<'_>) -> usize {
    let Some(data) = xdg_data_home() else {
        return 0;
    };
    let root = data.join("applications/wine/Programs/Image-Line");
    let removed = remove_fl_entries_in(&root, report);
    prune_empty_dirs(&root);
    removed
}

fn remove_fl_entries_in(dir: &Path, report: &mut Report<'_>) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += remove_fl_entries_in(&path, report);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            let text = fs::read_to_string(&path).unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if (path.to_string_lossy().contains("FL Studio") || lower.contains("name=fl studio"))
                && fs::remove_file(&path).is_ok()
            {
                removed += 1;
                report.info(format!("removed {}", path.display()));
            }
        }
    }
    removed
}

fn prune_empty_dirs(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_empty_dirs(&path);
        }
    }
    let empty = fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false);
    if empty {
        let _ = fs::remove_dir(dir);
    }
}

pub fn applications_dir() -> Option<PathBuf> {
    Some(xdg_data_home()?.join("applications"))
}

fn mime_dir() -> Option<PathBuf> {
    Some(xdg_data_home()?.join("mime"))
}

fn xdg_data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_keeps_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        fs::write(prefix.join("marker"), b"keep").unwrap();
        let opts = UninstallOptions {
            prefix: prefix.clone(),
            dry_run: true,
            log_path: None,
        };
        let mut lines = Vec::new();
        let outcome = uninstall(&opts, |_, line| lines.push(line.to_string())).unwrap();
        assert!(!outcome.prefix_removed);
        assert!(prefix.join("marker").exists());
        let text = lines.join("\n");
        assert!(text.contains("would run rm -rf"));
        assert!(text.contains(DESKTOP_FILE));
    }

    #[test]
    fn applications_dir_stays_under_xdg_or_home() {
        let dir = applications_dir().expect("applications dir");
        assert!(dir.ends_with("applications"));
    }
}
