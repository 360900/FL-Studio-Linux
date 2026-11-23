//! Launch FL Studio with the prefix's own Wine and a stable environment.

use std::fs::{self, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::http;
use crate::journal::Report;
use crate::runner::Stream;
use crate::{WINE_DIR_NAME, app_exe};

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub prefix: PathBuf,
    pub args: Vec<String>,
    pub dry_run: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LaunchOutcome {
    pub target: PathBuf,
    pub args: Vec<String>,
}

pub fn launch<F>(opts: &LaunchOptions, on_line: F) -> Result<LaunchOutcome>
where
    F: FnMut(Stream, &str),
{
    let mut sink = on_line;
    let mut report = Report::new(opts.log_path.clone(), &mut sink);
    let wine_dir = opts.prefix.join(WINE_DIR_NAME);
    let wine = wine_dir.join("bin/wine");
    crate::config::validate_prefix(&opts.prefix)?;

    let Some(fl_exe) = app_exe(&opts.prefix) else {
        bail!(
            "FL Studio is not installed in {}; run the installer again",
            opts.prefix.display()
        );
    };
    if !opts.dry_run && !wine.is_file() {
        bail!(
            "Wine is not installed at {}; run the setup again",
            wine.display()
        );
    }

    let envs = launch_env(&opts.prefix, &wine_dir);
    let urls = opts.args.iter().any(|arg| is_url(arg));
    // Always run FL Studio through the prefix's own Wine. Running the PE
    // directly lets the kernel's binfmt handler pick the system Wine, which
    // loses prefix isolation and breaks WebView2's GPU compositor.
    let (program, args, display_target) = if urls {
        let mut args = vec!["start".to_string()];
        args.extend(opts.args.iter().cloned());
        (wine.clone(), args, wine)
    } else {
        let mut args = Vec::with_capacity(opts.args.len() + 1);
        args.push(fl_exe.display().to_string());
        args.extend(convert_paths(&opts.prefix, &wine_dir, &opts.args));
        (wine.clone(), args, fl_exe.clone())
    };

    let mut cmd = Command::new(&program);
    cmd.args(&args);
    if !urls && let Some(dir) = fl_exe.parent() {
        cmd.current_dir(dir);
    }
    for (key, value) in &envs {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    if opts.dry_run {
        report.info(format!(
            "would run {} {}",
            program.display(),
            args.join(" ")
        ));
        for (key, value) in &envs {
            report.info(format!("env {key}={value}"));
        }
        return Ok(LaunchOutcome {
            target: display_target,
            args,
        });
    }

    // Keep Wine's errors: they are the only clue when a plugin crashes. A
    // link handed to a running FL Studio keeps that instance's log.
    attach_log(&mut cmd, !urls);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    report.info(format!(
        "launched {} (pid {})",
        display_target.display(),
        child.id()
    ));
    // Reap the process when it exits, so a long-running GUI collects no
    // zombies. A CLI that exits first hands it to init instead.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(LaunchOutcome {
        target: display_target,
        args,
    })
}

pub fn is_url(arg: &str) -> bool {
    arg.starts_with("flstudio://") || arg.starts_with("fl-cloud-plugins://")
}

/// Existing Unix paths are handed to FL Studio as Windows paths.
fn convert_paths(prefix: &Path, wine_dir: &Path, args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| {
            if is_url(arg) || !Path::new(arg).exists() {
                return arg.clone();
            }
            windows_path(prefix, wine_dir, Path::new(arg)).unwrap_or_else(|| arg.clone())
        })
        .collect()
}

fn windows_path(prefix: &Path, wine_dir: &Path, path: &Path) -> Option<String> {
    let search_path = std::env::var("PATH").unwrap_or_default();
    let output = Command::new(wine_dir.join("bin/winepath"))
        .args(["-w"])
        .arg(path)
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", "-all")
        // Keep winepath on the prefix's Wine so it starts the private
        // wineserver instead of the system one.
        .env("PATH", prepend_path(&wine_dir.join("bin"), &search_path))
        .env("WINE", wine_dir.join("bin/wine"))
        .env("WINESERVER", wine_dir.join("bin/wineserver"))
        .env("WINEDLLPATH", wine_dir.join("lib/wine"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn launch_env(prefix: &Path, wine_dir: &Path) -> Vec<(String, String)> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut envs = vec![
        ("WINEPREFIX".into(), prefix.display().to_string()),
        // Errors only: keep crash lines without drowning the log in fixmes.
        ("WINEDEBUG".into(), "-all,err+all".into()),
        ("PATH".into(), prepend_path(&wine_dir.join("bin"), &path)),
        (
            "WINE".into(),
            wine_dir.join("bin/wine").display().to_string(),
        ),
        (
            "WINESERVER".into(),
            wine_dir.join("bin/wineserver").display().to_string(),
        ),
        (
            "WINEDLLPATH".into(),
            wine_dir.join("lib/wine").display().to_string(),
        ),
        ("WINEDLLOVERRIDES".into(), "winemenubuilder.exe=d".into()),
        // NVIDIA blocks each OpenGL plugin's buffer swap on vblank otherwise.
        ("__GL_SYNC_TO_VBLANK".into(), "0".into()),
        ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP".into(), "0".into()),
        ("__GL_SHADER_DISK_CACHE".into(), "1".into()),
    ];
    // FL Studio and its plugins are X11 clients; Wine uses XWayland on
    // Wayland. Without a DISPLAY, Wine picks its own driver.
    if let Ok(display) = std::env::var("DISPLAY")
        && !display.is_empty()
    {
        envs.push(("DISPLAY".into(), display));
    }
    if let Ok(xauthority) = std::env::var("XAUTHORITY") {
        envs.push(("XAUTHORITY".into(), xauthority));
    }
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        // FL Studio ASIO goes through PipeWire's PulseAudio layer.
        envs.push(("PULSE_LATENCY_MSEC".into(), "20".into()));
    }
    envs
}

/// `dir` in front of a `PATH` value, without an empty entry (which would
/// mean the current directory) when `path` is empty.
pub(crate) fn prepend_path(dir: &Path, path: &str) -> String {
    if path.is_empty() {
        dir.display().to_string()
    } else {
        format!("{}:{path}", dir.display())
    }
}

/// Wine writes plugin crash reports to stderr. A menu launch has no terminal,
/// so keep the last run in the cache instead.
fn attach_log(cmd: &mut Command, truncate: bool) {
    let Ok(dir) = http::cache_dir() else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let log = dir.join("launch.log");
    match OpenOptions::new().create(true).append(true).open(&log) {
        Ok(file) => {
            if truncate {
                let _ = file.set_len(0);
            }
            match file.try_clone() {
                Ok(errors) => {
                    cmd.stdout(Stdio::from(file)).stderr(Stdio::from(errors));
                }
                Err(_) => {
                    cmd.stdout(Stdio::null()).stderr(Stdio::null());
                }
            }
        }
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
}

/// Refuse to touch a prefix while FL Studio runs from it: every setup step
/// stops the prefix's wineserver, which would close FL Studio and lose
/// unsaved work.
pub fn ensure_not_running(prefix: &Path) -> Result<()> {
    if let Some(pid) = running_instance(prefix) {
        bail!(
            "FL Studio is running from {} (pid {pid}). Save your work and close it, then try again.",
            prefix.display()
        );
    }
    Ok(())
}

/// The pid of an FL Studio process using `prefix`, found through `/proc`.
pub fn running_instance(prefix: &Path) -> Option<u32> {
    let wanted = prefix
        .canonicalize()
        .unwrap_or_else(|_| prefix.to_path_buf());
    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
        let dir = entry.path();
        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
        if !is_fl_process(comm.trim(), &cmdline) {
            return None;
        }
        let environ = fs::read(dir.join("environ")).ok()?;
        let value = environ
            .split(|byte| *byte == 0)
            .find_map(|var| var.strip_prefix(b"WINEPREFIX="))?;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(value));
        let path = path.canonicalize().unwrap_or(path);
        (path == wanted).then_some(pid)
    })
}

fn is_fl_process(comm: &str, cmdline: &[u8]) -> bool {
    let is_fl = |name: &str| {
        let name = name.to_ascii_lowercase();
        let file = name.rsplit(['\\', '/']).next().unwrap_or(&name);
        file == "fl64.exe" || file == "fl.exe"
    };
    is_fl(comm)
        || cmdline
            .split(|byte| *byte == 0)
            .any(|arg| is_fl(&String::from_utf8_lossy(arg)))
}

pub fn wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE")
        .map(|value| value.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_fl_urls() {
        assert!(is_url("flstudio://auth?token=x"));
        assert!(is_url("fl-cloud-plugins://login"));
        assert!(!is_url("/home/me/track.flp"));
        assert!(!is_url("https://image-line.com"));
    }

    #[test]
    fn dry_run_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
        fs::create_dir_all(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"wine").unwrap();
        let app = crate::app_dir(prefix).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();

        let opts = LaunchOptions {
            prefix: prefix.to_path_buf(),
            args: vec![],
            dry_run: true,
            log_path: None,
        };
        let mut lines = Vec::new();
        let outcome = launch(&opts, |_, line| lines.push(line.to_string())).unwrap();
        assert!(outcome.target.ends_with("FL64.exe"));
        assert!(outcome.args[0].ends_with("FL64.exe"));
        let text = lines.join("\n");
        assert!(text.contains(&format!("would run {}", wine.display())));
        assert!(text.contains("env WINEPREFIX="));
        assert!(text.contains("__GL_SYNC_TO_VBLANK=0"));
        assert!(text.contains("PULSE_LATENCY_MSEC=20"));
    }

    #[test]
    fn winepath_stays_on_the_prefix_wine() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("prefix");
        fs::create_dir_all(&prefix).unwrap();
        let wine_dir = dir.path().join(WINE_DIR_NAME);
        fs::create_dir_all(wine_dir.join("bin")).unwrap();
        let script = wine_dir.join("bin/winepath");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$PATH\" \"$WINESERVER\" \"$WINEPREFIX\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = windows_path(&prefix, &wine_dir, Path::new("/tmp/track.flp")).unwrap();
        assert!(output.starts_with(&format!("{}/bin:", wine_dir.display())));
        assert!(output.contains(&wine_dir.join("bin/wineserver").display().to_string()));
        assert!(output.contains(&prefix.display().to_string()));
    }

    #[test]
    fn url_arguments_use_wine_start() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let wine = prefix.join(WINE_DIR_NAME).join("bin/wine");
        fs::create_dir_all(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"wine").unwrap();
        let app = crate::app_dir(prefix).join("FL Studio 2026");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("FL64.exe"), b"exe").unwrap();

        let opts = LaunchOptions {
            prefix: prefix.to_path_buf(),
            args: vec!["flstudio://auth?code=1".into()],
            dry_run: true,
            log_path: None,
        };
        let outcome = launch(&opts, |_, _| {}).unwrap();
        assert_eq!(outcome.target, wine);
        assert_eq!(outcome.args[0], "start");
        assert_eq!(outcome.args[1], "flstudio://auth?code=1");
    }

    #[test]
    fn path_has_no_empty_entry() {
        assert_eq!(prepend_path(Path::new("/w/bin"), ""), "/w/bin");
        assert_eq!(
            prepend_path(Path::new("/w/bin"), "/usr/bin"),
            "/w/bin:/usr/bin"
        );
    }

    #[test]
    fn recognizes_fl_studio_processes() {
        let cmdline = b"C:\\Program Files\\Image-Line\\FL Studio 2026\\FL64.exe\0";
        assert!(is_fl_process("wine64-preload", cmdline));
        assert!(is_fl_process("FL64.exe", b""));
        assert!(is_fl_process(
            "x",
            b"/wine/bin/wine\0C:\\FL Studio\\FL.exe\0"
        ));
        assert!(!is_fl_process("wineserver", b"/wine/bin/wineserver\0"));
        assert!(!is_fl_process("x", b"C:\\FL Studio\\FLEngine.exe\0"));
    }

    #[test]
    fn nothing_runs_from_a_fresh_prefix() {
        let dir = tempfile::tempdir().unwrap();
        assert!(running_instance(dir.path()).is_none());
        assert!(ensure_not_running(dir.path()).is_ok());
    }

    #[test]
    fn launch_env_keeps_errors_and_vsync_off() {
        let envs = launch_env(Path::new("/p"), Path::new("/w"));
        let has = |key: &str, value: &str| {
            envs.iter()
                .any(|(env_key, env_value)| env_key == key && env_value == value)
        };
        assert!(has("WINEDEBUG", "-all,err+all"));
        assert!(!envs.iter().any(|(key, _)| key == "WINESERVER_TIMEOUT"));
        assert!(has("__GL_SYNC_TO_VBLANK", "0"));
        assert!(has("WINEDLLOVERRIDES", "winemenubuilder.exe=d"));
    }
}
