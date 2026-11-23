//! Small network helpers built on curl/wget so the core stays dependency-free.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::deps::which;
use crate::journal::Report;
use crate::runner::{self, RunOptions, RunResult};

const USER_AGENT: &str = "fl-studio-linux";
/// Every download and redirect must stay on HTTPS: the FL Studio and
/// WebView2 installers have no pinned checksum.
const CURL_DOWNLOAD: &[&str] = &[
    "--proto",
    "=https",
    "--proto-redir",
    "=https",
    "-fL",
    "--retry",
    "3",
    "--retry-delay",
    "2",
    "--connect-timeout",
    "30",
    "--progress-bar",
    "-A",
    USER_AGENT,
];

/// Large downloads kept outside the prefix, so a reinstall or a new prefix
/// does not fetch them again.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(crate::config::cache_home().join(crate::APP_NAME))
}

/// Hold a lock on `.<name>.lock` next to `path` while a file is fetched, so a
/// background prefetch, the GUI and the CLI never write the same file at
/// once. The lock is released when the returned file is dropped.
fn lock_download(path: &Path, cancel: Option<&Arc<AtomicBool>>) -> Result<File> {
    let parent = path.parent().context("download path has no folder")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = path
        .file_name()
        .context("download path has no file name")?
        .to_string_lossy();
    let lock_path = parent.join(format!(".{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    bail!("download cancelled");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            // Filesystems without locks: carry on unlocked.
            Err(TryLockError::Error(_)) => return Ok(file),
        }
    }
}

/// Download `url` into the cache as `name`. A cached copy is revalidated with
/// curl's `-z` against the server's Last-Modified, so an unchanged file costs
/// one request and a newer release is still picked up. When the server cannot
/// be reached the cached copy is used.
pub(crate) fn download_cached(
    report: &mut Report<'_>,
    url: &str,
    name: &str,
    stall: Duration,
) -> Result<PathBuf> {
    download_cached_cancellable(report, url, name, stall, None)
}

pub(crate) fn download_cached_cancellable(
    report: &mut Report<'_>,
    url: &str,
    name: &str,
    stall: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf> {
    let path = cache_dir()?.join(name);
    // A background prefetch may be fetching the same file.
    let _lock = lock_download(&path, cancel.as_ref())?;
    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        bail!("download cancelled");
    }
    if !path.is_file() {
        download_with_cancel(report, url, &path, stall, cancel)?;
        return Ok(path);
    }
    if which("curl").is_none() {
        // wget cannot revalidate into a separate file, so fetch it again
        // and keep the cached copy only when that fails.
        report.info(format!("refreshing the cached {name}"));
        if let Err(error) = download_with_cancel(report, url, &path, stall, cancel.clone()) {
            if cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                bail!("download cancelled");
            }
            report.warn(format!(
                "could not refresh {name} ({error:#}); using the cached copy"
            ));
        }
        return Ok(path);
    }

    let partial = path.with_extension("check.part");
    let _ = std::fs::remove_file(&partial);
    report.info(format!("checking {url} for a newer {name}"));
    let mut cmd = Command::new("curl");
    cmd.args(CURL_DOWNLOAD)
        .arg("-R")
        .arg("-z")
        .arg(&path)
        .arg("-o")
        .arg(&partial)
        .arg(url);
    let checked = run_with_cancel(report, cmd, stall, cancel.clone());
    match checked {
        // On 304 Not Modified curl writes no file.
        Ok(result)
            if result.success()
                && partial
                    .metadata()
                    .map(|meta| meta.len() > 0)
                    .unwrap_or(false) =>
        {
            finish(partial, &path)?;
            report.info(format!("downloaded a newer {name}"));
        }
        Ok(result) if result.success() => {
            let _ = std::fs::remove_file(&partial);
            report.info(format!("cached {name} is current"));
        }
        other => {
            let _ = std::fs::remove_file(&partial);
            if cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                bail!("download cancelled");
            }
            let reason = match other {
                Ok(result) => failure("curl", &result),
                Err(error) => format!("{error:#}"),
            };
            report.warn(format!(
                "could not check for a newer {name} ({reason}); using the cached copy"
            ));
        }
    }
    Ok(path)
}

/// Download a pinned asset into the cache and verify its SHA-256. A cached
/// copy whose digest already matches is reused without hitting the network.
pub(crate) fn download_verified(
    report: &mut Report<'_>,
    url: &str,
    name: &str,
    expected_sha256: &str,
    stall: Duration,
) -> Result<PathBuf> {
    let path = cache_dir()?.join(name);
    let _lock = lock_download(&path, None)?;
    if path.is_file() {
        match sha256_file(&path) {
            Ok(hash) if hash == expected_sha256 => {
                report.info(format!("using cached {name}"));
                return Ok(path);
            }
            Ok(_) => {
                report.warn(format!(
                    "{name} failed its SHA-256 check; downloading it again"
                ));
                let _ = std::fs::remove_file(&path);
            }
            Err(error) => return Err(error),
        }
    }

    download(report, url, &path, stall)?;
    if sha256_file(&path)? == expected_sha256 {
        return Ok(path);
    }

    report.warn(format!(
        "{name} failed its SHA-256 check; downloading it again"
    ));
    let _ = std::fs::remove_file(&path);
    download(report, url, &path, stall)?;
    if sha256_file(&path)? != expected_sha256 {
        let _ = std::fs::remove_file(&path);
        bail!("{name} failed its SHA-256 check twice");
    }
    Ok(path)
}

/// Download `url` into `destination`, removing a partial file on failure.
pub(crate) fn download(
    report: &mut Report<'_>,
    url: &str,
    destination: &Path,
    stall: Duration,
) -> Result<()> {
    download_with_cancel(report, url, destination, stall, None)
}

fn download_with_cancel(
    report: &mut Report<'_>,
    url: &str,
    destination: &Path,
    stall: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let partial = destination.with_extension(format!(
        "{}.part",
        destination
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("download")
    ));
    report.info(format!("downloading {url}"));

    let has_curl = which("curl").is_some();
    let has_wget = which("wget").is_some();
    if !has_curl && !has_wget {
        bail!("curl or wget is required to download {url}");
    }

    let mut last_error = String::new();
    if has_curl {
        let mut cmd = Command::new("curl");
        cmd.args(CURL_DOWNLOAD)
            // Keep the server's timestamp so `download_cached` can revalidate.
            .arg("-R")
            .arg("-o")
            .arg(&partial)
            .arg(url);
        match run_with_cancel(report, cmd, stall, cancel.clone()) {
            Ok(result) if result.success() => return finish(partial, destination),
            Ok(result) => last_error = failure("curl", &result),
            Err(error) => last_error = format!("{error:#}"),
        }
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            let _ = std::fs::remove_file(&partial);
            bail!("download cancelled");
        }
    }

    if has_wget {
        let mut cmd = Command::new("wget");
        cmd.args([
            "--https-only",
            "--tries=3",
            "--timeout=30",
            "-U",
            USER_AGENT,
            "-O",
        ])
        .arg(&partial)
        .arg(url);
        let error = match run_with_cancel(report, cmd, stall, cancel.clone()) {
            Ok(result) if result.success() => return finish(partial, destination),
            Ok(result) => failure("wget", &result),
            Err(error) => format!("{error:#}"),
        };
        last_error = if last_error.is_empty() {
            error
        } else {
            format!("{last_error}; {error}")
        };
    }

    let _ = std::fs::remove_file(&partial);
    bail!("could not download {url}: {last_error}");
}

fn run_with_cancel(
    report: &mut Report<'_>,
    cmd: Command,
    stall: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<RunResult> {
    let mut options = RunOptions::default().stall(stall);
    if let Some(flag) = cancel {
        options = options.cancel(flag);
    }
    runner::run(cmd, options, |stream, line| report.line(stream, line))
}

/// Why a download tool failed: its exit status or stall, and its last lines.
fn failure(tool: &str, result: &RunResult) -> String {
    let reason = if result.stalled {
        "stalled".to_string()
    } else {
        format!("exited with {}", result.status)
    };
    let tail = result.tail_text(3);
    if tail.is_empty() {
        format!("{tool} {reason}")
    } else {
        format!("{tool} {reason}: {}", tail.replace('\n', " | "))
    }
}

fn finish(partial: std::path::PathBuf, destination: &Path) -> Result<()> {
    std::fs::rename(&partial, destination)
        .with_context(|| format!("moving {} to {}", partial.display(), destination.display()))
}

/// Hex SHA-256 of a file, using the system `sha256sum`.
pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    if which("sha256sum").is_none() {
        bail!("sha256sum is required to verify downloads");
    }
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .with_context(|| format!("hashing {}", path.display()))?;
    if !output.status.success() {
        bail!("sha256sum failed for {}", path.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let hash = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if hash.len() != 64 {
        bail!("unexpected sha256sum output for {}", path.display());
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_lowercase_hex() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data");
        std::fs::write(&file, b"fl studio").unwrap();
        let hash = sha256_file(&file).unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_held_download_lock_waits_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("setup.exe");
        let held = lock_download(&path, None).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(lock_download(&path, Some(&cancel)).is_err());
        drop(held);
        // A process another test spawns at this moment briefly shares the
        // descriptor, so the lock can outlive `drop`; wait like real callers.
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(5));
                cancel.store(true, Ordering::Relaxed);
            });
        }
        assert!(lock_download(&path, Some(&cancel)).is_ok());
    }
}
