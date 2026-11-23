use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio::AudioDriver;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub prefix: PathBuf,
    /// Install DXVK into the prefix. On by default: FL Studio and its plugins
    /// render through Direct3D, and DXVK maps it to Vulkan.
    pub dxvk: bool,
    /// Install Microsoft fonts and the musical-symbol font. FLEX crashes
    /// without the core fonts.
    pub fonts: bool,
    /// Low-latency ASIO driver installed into the private Wine build.
    pub audio: AudioDriver,
    /// Override distro detection, mainly for dry-run testing.
    pub distro_override: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: default_prefix(),
            dxvk: true,
            fonts: true,
            audio: AudioDriver::WinePipe,
            distro_override: None,
        }
    }
}

impl Config {
    /// Load the app config, falling back to defaults when the file is absent.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.prefix = expand_home(&cfg.prefix);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        write_atomic(path, text.as_bytes())
            .with_context(|| format!("writing config {}", path.display()))
    }
}

/// Write through a temporary file and a rename, so an interrupted write
/// never leaves a truncated file behind.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let result = fs::write(&temp, bytes)
        .with_context(|| format!("writing {}", temp.display()))
        .and_then(|()| {
            fs::rename(&temp, path)
                .with_context(|| format!("moving {} to {}", temp.display(), path.display()))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Reject locations that are clearly not a private Wine prefix. The
/// uninstaller deletes this directory recursively, so a typo must not erase
/// the home directory, one of its parents or a system tree.
pub fn validate_prefix(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("the prefix path is empty");
    }
    if path.to_str().is_none() {
        anyhow::bail!("the prefix path must be valid UTF-8: {}", path.display());
    }
    if !path.is_absolute() {
        anyhow::bail!("the prefix must be an absolute path: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("the prefix must not contain '..': {}", path.display());
    }
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let home = home_dir();
    let home_resolved = home.canonicalize().unwrap_or_else(|_| home.clone());
    let forbidden: Vec<PathBuf> = [
        "/",
        "/home",
        "/var/home",
        "/root",
        "/usr",
        "/usr/local",
        "/etc",
        "/var",
        "/opt",
        "/boot",
        "/srv",
        "/tmp",
        "/mnt",
        "/media",
        "/run",
        "/run/media",
    ]
    .iter()
    .flat_map(|dir| {
        let dir = PathBuf::from(dir);
        let resolved = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        [dir, resolved]
    })
    .collect();
    let dangerous = |candidate: &Path| {
        forbidden.iter().any(|dir| candidate == dir)
            // A user's removable-media folder, e.g. /run/media/<user>.
            || candidate.parent().is_some_and(|parent| {
                parent == Path::new("/media") || parent == Path::new("/run/media")
            })
            || home.starts_with(candidate)
            || home_resolved.starts_with(candidate)
    };
    if dangerous(path) || dangerous(&resolved) || is_mount_point(&resolved) {
        anyhow::bail!("refusing to use {} as the FL Studio prefix", path.display());
    }
    Ok(())
}

/// A whole mounted filesystem, such as a data drive, is never a prefix.
fn is_mount_point(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .any(|mount| Path::new(&unescape_mount(mount)) == path)
}

/// mountinfo writes space, tab, newline and backslash as octal escapes.
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && let Some(digits) = bytes.get(index + 1..index + 4)
            && let Ok(digits) = std::str::from_utf8(digits)
            && let Ok(value) = u8::from_str_radix(digits, 8)
        {
            out.push(value);
            index += 4;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A directory this tool (or Wine) created. The uninstaller uses this to
/// refuse deleting a folder it does not recognise.
pub fn looks_like_prefix(path: &Path) -> bool {
    path.join("system.reg").is_file()
        || path.join("drive_c").is_dir()
        || path.join(crate::manifest::MANIFEST_FILE).is_file()
        || path
            .join(crate::WINE_DIR_NAME)
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
}

pub fn config_dir() -> PathBuf {
    config_home().join(crate::APP_NAME)
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn default_prefix() -> PathBuf {
    home_dir().join(crate::PREFIX_DIR_NAME)
}

/// An XDG base directory variable; the spec says relative values are ignored.
fn xdg_var(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> PathBuf {
    xdg_var("XDG_CONFIG_HOME").unwrap_or_else(|| home_dir().join(".config"))
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_home() -> PathBuf {
    xdg_var("XDG_DATA_HOME").unwrap_or_else(|| home_dir().join(".local/share"))
}

/// `$XDG_CACHE_HOME`, else `~/.cache`.
pub fn cache_home() -> PathBuf {
    xdg_var("XDG_CACHE_HOME").unwrap_or_else(|| home_dir().join(".cache"))
}

/// `$HOME`, else the home directory from the user database. Falls back to
/// `/`, where writes fail loudly, instead of a path relative to the current
/// directory.
pub fn home_dir() -> PathBuf {
    xdg_var("HOME")
        .or_else(passwd_home)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let uid = fs::metadata("/proc/self").ok()?.uid().to_string();
    let entry = std::process::Command::new("getent")
        .args(["passwd", &uid])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .or_else(|| {
            fs::read_to_string("/etc/passwd").ok().and_then(|text| {
                text.lines()
                    .find(|line| line.split(':').nth(2) == Some(uid.as_str()))
                    .map(str::to_string)
            })
        })?;
    entry
        .trim()
        .split(':')
        .nth(5)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Expand a leading `~` or `~/...` to the user home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

// Per-prefix state files written by the installer.
pub fn prefix_state_path(prefix: &Path, name: &str) -> PathBuf {
    prefix.join(name)
}

pub fn write_prefix_state(prefix: &Path, name: &str, value: &str) -> Result<()> {
    write_atomic(&prefix_state_path(prefix, name), value.as_bytes())
}

pub fn read_prefix_state(prefix: &Path, name: &str) -> Option<String> {
    fs::read_to_string(prefix_state_path(prefix, name))
        .ok()
        .map(|v| v.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config {
            prefix: PathBuf::from("/tmp/prefix"),
            dxvk: true,
            fonts: false,
            audio: AudioDriver::None,
            distro_override: Some("ubuntu".into()),
        };
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.prefix, PathBuf::from("/tmp/prefix"));
        assert!(loaded.dxvk);
        assert!(!loaded.fonts);
        assert_eq!(loaded.audio, AudioDriver::None);
        assert_eq!(loaded.distro_override.as_deref(), Some("ubuntu"));
    }

    #[test]
    fn old_wineasio_config_falls_back_to_winepipe() {
        let cfg: Config = toml::from_str("audio = \"wineasio\"").unwrap();
        assert_eq!(cfg.audio, AudioDriver::WinePipe);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let cfg = Config::load_from(Path::new("/nonexistent/fl/config.toml")).unwrap();
        assert!(cfg.prefix.ends_with(crate::PREFIX_DIR_NAME));
        assert!(cfg.fonts);
        assert!(cfg.dxvk);
        assert_eq!(cfg.audio, AudioDriver::WinePipe);
    }

    #[test]
    fn expands_tilde() {
        let home = home_dir();
        assert_eq!(expand_home(Path::new("~")), home);
        assert_eq!(expand_home(Path::new("~/x")), home.join("x"));
        assert_eq!(expand_home(Path::new("/abs")), PathBuf::from("/abs"));
    }

    #[test]
    fn refuses_dangerous_prefixes() {
        for path in [
            "/",
            "/home",
            "/var/home",
            "/usr",
            "/usr/local",
            "/tmp",
            "/etc",
            "/mnt",
            "/media",
            "/media/someone",
            "/run/media/someone",
        ] {
            assert!(validate_prefix(Path::new(path)).is_err(), "{path}");
        }
        assert!(validate_prefix(Path::new("relative")).is_err());
        assert!(validate_prefix(Path::new("/home/../home")).is_err());
        let home = home_dir();
        assert!(validate_prefix(&home).is_err());
        if let Some(parent) = home.parent() {
            assert!(validate_prefix(parent).is_err());
        }
        assert!(validate_prefix(&home.join(".FLStudioLinux")).is_ok());
        assert!(validate_prefix(Path::new("/mnt/data/FL")).is_ok());
    }

    #[test]
    fn rejects_non_utf8_prefixes() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/opt/fl\xff"));
        assert!(validate_prefix(path).is_err());
    }

    #[test]
    fn unescapes_mountinfo_paths() {
        assert_eq!(unescape_mount("/mnt/My\\040Drive"), "/mnt/My Drive");
        assert_eq!(unescape_mount("/plain"), "/plain");
        assert_eq!(unescape_mount("/trail\\04"), "/trail\\04");
    }

    #[test]
    fn recognises_a_prefix() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_prefix(dir.path()));
        fs::create_dir_all(dir.path().join("drive_c")).unwrap();
        assert!(looks_like_prefix(dir.path()));
    }

    #[test]
    fn unrelated_wine_builds_do_not_make_a_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wine-ge-8-26")).unwrap();
        fs::create_dir_all(dir.path().join(crate::WINE_DIR_NAME)).unwrap();
        assert!(!looks_like_prefix(dir.path()));

        let ours = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            "wine-11.22-amd64-wow64",
            ours.path().join(crate::WINE_DIR_NAME),
        )
        .unwrap();
        assert!(looks_like_prefix(ours.path()));
    }

    #[test]
    fn atomic_writes_replace_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn prefix_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        write_prefix_state(dir.path(), ".dxvk_version", "3.1.1").unwrap();
        assert_eq!(
            read_prefix_state(dir.path(), ".dxvk_version").as_deref(),
            Some("3.1.1")
        );
        assert_eq!(read_prefix_state(dir.path(), ".missing"), None);
    }
}
