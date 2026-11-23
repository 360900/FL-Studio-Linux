//! Remove the FL Studio prefix, menu entries, handlers and icon.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::desktop::{self, DESKTOP_FILE, MIME_FILE, URL_HANDLER_FILE};
use crate::journal::Report;
use crate::runner::{self, RunOptions, Stream};
use crate::{WINE_DIR_NAME, config, wine};

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
    // A symlinked prefix: delete what it points to as well as the link.
    let is_link = opts
        .prefix
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    let target = if is_link {
        let target = link_target(&opts.prefix)?;
        config::validate_prefix(&target)?;
        target
    } else {
        opts.prefix.clone()
    };
    if !opts.dry_run && target.exists() && !config::looks_like_prefix(&target) {
        bail!(
            "{} does not look like a Wine prefix; refusing to delete it",
            target.display()
        );
    }
    let prefixes = [opts.prefix.clone(), target.clone()];
    let staging = staging_path(&target)?;
    report.info(format!(
        "Uninstalling FL Studio from {}",
        opts.prefix.display()
    ));

    let paths = owned_paths(&prefixes, &mut report);
    if opts.dry_run {
        for path in &paths {
            report.info(format!("would remove {}", path.display()));
        }
        report.info("would remove FL Studio entries Wine created for this prefix");
        if target.exists() {
            report.info(format!("would run rm -rf {}", target.display()));
        } else {
            report.info(format!("{} does not exist", target.display()));
        }
        if is_link {
            report.info(format!("would remove the link {}", opts.prefix.display()));
        }
        return Ok(outcome);
    }

    stop_prefix(&opts.prefix, &mut report);

    for path in paths {
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
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
    outcome.removed_entries += remove_fl_wine_entries(&prefixes, &mut report);
    if outcome.removed_entries > 0 {
        refresh_databases(&mut report);
    }

    // An earlier uninstall that was interrupted while deleting.
    if staging.exists() {
        report.info(format!("finishing the removal of {}", staging.display()));
        remove_tree(&staging, &mut report)?;
    }
    if target.exists() {
        report.info(format!("deleting {}", target.display()));
        // Move the prefix aside first: an interrupted delete then leaves
        // only the staging folder, which the next run finishes.
        if fs::rename(&target, &staging).is_ok() {
            remove_tree(&staging, &mut report)?;
        } else {
            remove_tree(&target, &mut report)?;
        }
        outcome.prefix_removed = true;
        report.info("prefix deleted");
    } else {
        report.info("prefix not found; nothing to delete");
    }
    if is_link && fs::remove_file(&opts.prefix).is_ok() {
        report.info(format!("removed the link {}", opts.prefix.display()));
    }

    report.info("FL Studio projects and samples outside the prefix were kept");
    Ok(outcome)
}

/// Where a symlinked prefix points, also when the target no longer exists.
fn link_target(link: &Path) -> Result<PathBuf> {
    let target = fs::read_link(link).with_context(|| format!("reading {}", link.display()))?;
    let target = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or(Path::new("/")).join(target)
    };
    Ok(target.canonicalize().unwrap_or(target))
}

/// `.<name>.removing` next to the prefix.
fn staging_path(prefix: &Path) -> Result<PathBuf> {
    let name = prefix
        .file_name()
        .with_context(|| format!("{} has no folder name", prefix.display()))?;
    let mut staging = std::ffi::OsString::from(".");
    staging.push(name);
    staging.push(".removing");
    Ok(prefix.with_file_name(staging))
}

fn remove_tree(path: &Path, report: &mut Report<'_>) -> Result<()> {
    if let Err(error) = fs::remove_dir_all(path) {
        report.warn(format!(
            "initial removal failed ({error}); retrying after chmod"
        ));
        make_writable(path);
        if fs::remove_dir_all(path).is_err() {
            report.warn("retry failed; falling back to rm -rf");
            let _ = Command::new("rm")
                .args(["-rf", "--"])
                .arg(path)
                .status()
                .context("running rm -rf")?;
        }
    }
    if path.exists() {
        bail!(
            "could not delete {}; close FL Studio and any Wine processes, then retry",
            path.display()
        );
    }
    Ok(())
}

fn stop_prefix(prefix: &Path, report: &mut Report<'_>) {
    let wine_dir = prefix.join(WINE_DIR_NAME);
    wine::kill_prefix(prefix, Some(&wine_dir), report);
    std::thread::sleep(Duration::from_millis(800));
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
    let add = if metadata.is_dir() { 0o700 } else { 0o600 };
    let mode = metadata.permissions().mode() | add;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    if metadata.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

/// Menu entries, handlers, icon and MIME type that belong to this prefix.
/// Files that launch another prefix, e.g. after an install elsewhere, stay.
fn owned_paths(prefixes: &[PathBuf], report: &mut Report<'_>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Some(data) = xdg_data_home() else {
        return paths;
    };
    let applications = data.join("applications");
    let launcher = applications.join(DESKTOP_FILE);
    let owned = !launcher.exists() || file_references(&launcher, prefixes);
    let mut candidates = vec![
        applications.join(DESKTOP_FILE),
        applications.join(URL_HANDLER_FILE),
        applications.join("wine-protocol-flstudio.desktop"),
        applications.join("wine-protocol-fl-cloud-plugins.desktop"),
    ];
    if let Some(desktop) = desktop::desktop_dir() {
        candidates.push(desktop.join("FL Studio.desktop"));
    }
    for path in candidates {
        if path.symlink_metadata().is_err() {
            continue;
        }
        if file_references(&path, prefixes) {
            paths.push(path);
        } else {
            report.info(format!(
                "keeping {}: it launches another prefix",
                path.display()
            ));
        }
    }
    if owned {
        paths.extend(
            desktop::icon_paths(&data)
                .into_iter()
                .filter(|path| path.symlink_metadata().is_ok()),
        );
        paths.push(data.join("mime/packages").join(MIME_FILE));
    }
    paths
}

/// True when a desktop file's text names one of the prefixes.
fn file_references(path: &Path, prefixes: &[PathBuf]) -> bool {
    fs::read_to_string(path).is_ok_and(|text| references(&text, prefixes))
}

/// The prefix path appears as a whole path, not as the start of a longer one.
pub(crate) fn references(text: &str, prefixes: &[PathBuf]) -> bool {
    prefixes
        .iter()
        .filter_map(|prefix| prefix.to_str())
        .any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            !prefix.is_empty()
                && text.match_indices(prefix).any(|(index, _)| {
                    text[index + prefix.len()..]
                        .chars()
                        .next()
                        .is_none_or(|next| matches!(next, '"' | '\'' | ' ' | '/' | '\n' | '\t'))
                })
        })
}

/// Remove this prefix's FL Studio entries from Wine's menu folder, leaving
/// other Image-Line applications and other prefixes alone.
fn remove_fl_wine_entries(prefixes: &[PathBuf], report: &mut Report<'_>) -> usize {
    let Some(data) = xdg_data_home() else {
        return 0;
    };
    let root = data.join("applications/wine/Programs/Image-Line");
    let removed = remove_fl_entries_in(&root, prefixes, report);
    prune_empty_dirs(&root);
    removed
}

fn remove_fl_entries_in(dir: &Path, prefixes: &[PathBuf], report: &mut Report<'_>) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += remove_fl_entries_in(&path, prefixes, report);
        } else if path.extension().is_some_and(|ext| ext == "desktop") {
            let text = fs::read_to_string(&path).unwrap_or_default();
            let lower = text.to_ascii_lowercase();
            if (path.to_string_lossy().contains("FL Studio") || lower.contains("name=fl studio"))
                && references(&text, prefixes)
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
    Some(config::data_home())
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
    }

    #[test]
    fn matches_whole_prefix_paths_only() {
        let prefixes = [PathBuf::from("/home/me/FL")];
        assert!(references(
            "Exec=env WINEPREFIX=\"/home/me/FL\" wine",
            &prefixes
        ));
        assert!(references(
            "Exec=/usr/bin/fl-studio-linux launch --prefix /home/me/FL %F",
            &prefixes
        ));
        assert!(references("Path=/home/me/FL\n", &prefixes));
        assert!(!references(
            "Exec=env WINEPREFIX=\"/home/me/FL2\" wine",
            &prefixes
        ));
        assert!(!references(
            "Exec=env WINEPREFIX=\"/home/me/.wine\" wine",
            &prefixes
        ));
    }

    #[test]
    fn deletes_the_target_of_a_symlinked_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        fs::create_dir_all(target.join("drive_c")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let opts = UninstallOptions {
            prefix: link.clone(),
            dry_run: false,
            log_path: None,
        };
        let outcome = uninstall(&opts, |_, _| {}).unwrap();
        assert!(outcome.prefix_removed);
        assert!(!target.exists());
        assert!(link.symlink_metadata().is_err());
    }

    #[test]
    fn finishes_an_interrupted_removal() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        let staging = staging_path(&prefix).unwrap();
        assert_eq!(staging, dir.path().join(".prefix.removing"));
        fs::create_dir_all(staging.join("drive_c/windows")).unwrap();
        let opts = UninstallOptions {
            prefix,
            dry_run: false,
            log_path: None,
        };
        uninstall(&opts, |_, _| {}).unwrap();
        assert!(!staging.exists());
    }

    #[test]
    fn refuses_folders_that_are_not_prefixes() {
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("Games");
        fs::create_dir_all(games.join("wine-ge-8-26")).unwrap();
        let opts = UninstallOptions {
            prefix: games.clone(),
            dry_run: false,
            log_path: None,
        };
        assert!(uninstall(&opts, |_, _| {}).is_err());
        assert!(games.join("wine-ge-8-26").is_dir());
    }

    #[test]
    fn applications_dir_stays_under_xdg_or_home() {
        let dir = applications_dir().expect("applications dir");
        assert!(dir.ends_with("applications"));
    }

    #[test]
    fn make_writable_keeps_files_non_world_writable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("readonly");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
        make_writable(&file);
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
