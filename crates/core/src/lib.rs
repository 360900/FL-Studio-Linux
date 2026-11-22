//! Headless engine for the FL Studio Linux installer.

pub mod audio;
pub mod components;
pub mod config;
pub mod deps;
pub mod desktop;
pub mod distro;
pub mod doctor;
pub mod dxvk;
pub mod flstudio;
pub mod gpu;
pub mod http;
pub mod install;
pub mod installer;
mod journal;
pub mod launch;
pub mod manifest;
pub mod packages;
pub mod runner;
pub mod uninstall;
pub mod wine;

use std::fs;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "fl-studio-linux";
pub const PREFIX_DIR_NAME: &str = ".FLStudioLinux";
pub const WINE_DIR_NAME: &str = "wine-fl";
pub const APP_DIR_REL: &str = "drive_c/Program Files/Image-Line";
pub const LAUNCHER_NAME: &str = "fl-studio-linux";
pub const GUI_NAME: &str = "fl-studio-linux-gui";

/// The Image-Line directory inside a prefix.
pub fn app_dir(prefix: &Path) -> PathBuf {
    prefix.join(APP_DIR_REL)
}

/// Every installed FL Studio directory, oldest name first.
pub fn app_dirs(prefix: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(app_dir(prefix)) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .map(|name| name.to_string_lossy().starts_with("FL Studio"))
                    .unwrap_or(false)
        })
        .filter(|path| exe_in(path).is_some())
        .collect();
    dirs.sort_by_key(|a| version_key(a));
    dirs
}

/// The executable to start, preferring the 64-bit build.
pub fn app_exe(prefix: &Path) -> Option<PathBuf> {
    let dirs = app_dirs(prefix);
    dirs.last().and_then(|dir| exe_in(dir))
}

/// The newest FL Studio installation directory.
pub fn app_install_dir(prefix: &Path) -> Option<PathBuf> {
    app_dirs(prefix).into_iter().last()
}

/// The version folder's year, used only to pick the newest install.
fn version_key(path: &Path) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            name.split(|c: char| !c.is_ascii_digit())
                .find(|part| !part.is_empty())
        })
        .and_then(|year| year.parse().ok())
        .unwrap_or(0)
}

fn exe_in(dir: &Path) -> Option<PathBuf> {
    for name in ["FL64.exe", "FL.exe"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Convert a Unix path to the `Z:` drive form Wine exposes.
pub fn to_windows_path(path: &Path) -> String {
    format!("Z:{}", path.to_string_lossy().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(prefix: &Path, folder: &str, name: &str) -> PathBuf {
        let dir = app_dir(prefix).join(folder);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(name);
        fs::write(&exe, b"MZ").unwrap();
        exe
    }

    #[test]
    fn finds_the_newest_installation() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), "FL Studio 21", "FL64.exe");
        let newest = install(dir.path(), "FL Studio 2026", "FL64.exe");
        assert_eq!(app_exe(dir.path()).unwrap(), newest);
        assert_eq!(app_dirs(dir.path()).len(), 2);
    }

    #[test]
    fn falls_back_to_the_32_bit_binary() {
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), "FL Studio 2026", "FL.exe");
        assert_eq!(app_exe(dir.path()).unwrap(), exe);
    }

    #[test]
    fn ignores_folders_without_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(app_dir(dir.path()).join("FL Studio 2026")).unwrap();
        assert!(app_exe(dir.path()).is_none());
    }

    #[test]
    fn windows_paths_use_z_drive() {
        assert_eq!(
            to_windows_path(Path::new("/home/me/My Track.flp")),
            "Z:\\home\\me\\My Track.flp"
        );
    }
}
