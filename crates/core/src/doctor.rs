use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::components;
use crate::config::{Config, config_path};
use crate::deps::{self, DepCheck};
use crate::distro::{Family, OsRelease};
use crate::gpu::{self, Gpu};
use crate::manifest::Manifest;
use crate::packages;
use crate::{WINE_DIR_NAME, app_exe, wine};

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub os: OsRelease,
    pub family: Family,
    pub package_manager: Option<String>,
    pub gpus: Vec<Gpu>,
    pub vulkan_icds: Vec<String>,
    pub deps: Vec<DepCheck>,
    /// Host libraries the private Wine build needs but the linker cannot find.
    pub missing_libraries: Vec<String>,
    pub pipewire: bool,
    pub package_notes: Vec<String>,
    pub config_path: PathBuf,
    pub prefix: PathBuf,
    pub prefix_exists: bool,
    pub wine_build: Option<String>,
    pub audio_driver: Option<String>,
    pub installed: bool,
    pub fl_exe: Option<PathBuf>,
    pub webview2: bool,
    pub manifest: Option<Manifest>,
    pub errors: Vec<String>,
}

pub fn run(cfg: &Config) -> DoctorReport {
    let mut errors = Vec::new();

    let os = match cfg.distro_override.as_deref() {
        Some(over) if !over.trim().is_empty() => OsRelease::from_override(over),
        _ => OsRelease::detect().unwrap_or_else(|err| {
            errors.push(format!("os-release: {err:#}"));
            OsRelease::default()
        }),
    };

    let family = os.family();

    let package_manager = {
        let pm = family.package_manager();
        (!pm.is_empty() && deps::which(pm).is_some()).then(|| pm.to_string())
    };

    let prefix = cfg.prefix.clone();
    let prefix_exists = prefix.is_dir();

    let manifest = match Manifest::load(&prefix) {
        Ok(manifest) => manifest,
        Err(err) => {
            errors.push(format!("manifest: {err:#}"));
            None
        }
    };

    let wine_build = wine::detect_installed_build(&prefix).or_else(|| {
        fs::read_link(prefix.join(WINE_DIR_NAME))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .or_else(|| {
                prefix
                    .join(WINE_DIR_NAME)
                    .join("bin/wine")
                    .is_file()
                    .then(|| WINE_DIR_NAME.to_string())
            })
    });

    let audio_driver = crate::config::read_prefix_state(&prefix, ".audio_driver")
        .and_then(|label| crate::audio::AudioDriver::from_label(&label))
        .map(|driver| driver.title().to_string())
        .or_else(|| {
            (cfg.audio != crate::audio::AudioDriver::None && prefix_exists)
                .then(|| format!("{} (not installed)", cfg.audio.title()))
        });
    let package_notes = packages::plan(&os).notes;
    let fl_exe = app_exe(&prefix);
    let installed = fl_exe.as_ref().is_some_and(|path| path.is_file())
        || manifest
            .as_ref()
            .and_then(|m| m.fl_exe.as_deref())
            .is_some_and(|path| PathBuf::from(path).is_file());

    DoctorReport {
        os,
        family,
        package_manager,
        gpus: gpu::detect(),
        vulkan_icds: deps::vulkan_icds(),
        deps: deps::check_all(),
        missing_libraries: deps::missing_libraries()
            .into_iter()
            .map(str::to_string)
            .collect(),
        pipewire: crate::audio::pipewire_running(),
        package_notes,
        config_path: config_path(),
        prefix,
        prefix_exists,
        wine_build,
        audio_driver,
        installed,
        fl_exe,
        webview2: components::webview2_installed(&cfg.prefix),
        manifest,
        errors,
    }
}
