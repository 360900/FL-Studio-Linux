use std::env;
use std::path::PathBuf;
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DepCheck {
    pub name: &'static str,
    pub command: &'static str,
    pub required: bool,
    pub found: bool,
    pub version: Option<String>,
}

struct Spec {
    name: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    required: bool,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "winetricks",
        command: "winetricks",
        args: &["--version"],
        required: true,
    },
    Spec {
        name: "7z",
        command: "7z",
        args: &["i"],
        required: true,
    },
    Spec {
        name: "cabextract",
        command: "cabextract",
        args: &["--version"],
        required: true,
    },
    Spec {
        name: "tar",
        command: "tar",
        args: &["--version"],
        required: true,
    },
    Spec {
        name: "xz",
        command: "xz",
        args: &["--version"],
        required: true,
    },
    Spec {
        name: "sha256sum",
        command: "sha256sum",
        args: &["--version"],
        required: true,
    },
    Spec {
        name: "curl",
        command: "curl",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "wget",
        command: "wget",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "unzip",
        command: "unzip",
        args: &["-v"],
        required: false,
    },
    Spec {
        name: "zstd",
        command: "zstd",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "lspci",
        command: "lspci",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "system wine",
        command: "wine",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "sudo",
        command: "sudo",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "pkexec",
        command: "pkexec",
        args: &["--version"],
        required: false,
    },
    Spec {
        name: "xdg-mime",
        command: "xdg-mime",
        args: &["--version"],
        required: false,
    },
];

pub fn check_all() -> Vec<DepCheck> {
    SPECS.iter().map(check).collect()
}

fn check(spec: &Spec) -> DepCheck {
    let result = Command::new(spec.command).args(spec.args).output();
    let (found, version) = match result {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).to_string();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&output.stderr).to_string();
            }
            let version = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| line.chars().take(100).collect::<String>());
            (true, version)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (false, None),
        Err(err) => (true, Some(format!("error: {err}"))),
    };
    DepCheck {
        name: spec.name,
        command: spec.command,
        required: spec.required,
        found,
        version,
    }
}

pub fn missing(checks: &[DepCheck]) -> Vec<&DepCheck> {
    checks.iter().filter(|c| c.required && !c.found).collect()
}

/// Both download tools absent means the installer cannot fetch Wine or
/// FL Studio, so treat that as a missing dependency too.
pub fn has_download_tool(checks: &[DepCheck]) -> bool {
    checks
        .iter()
        .any(|c| (c.command == "curl" || c.command == "wget") && c.found)
}

/// A Vulkan driver manifest and the API version it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VulkanIcd {
    pub name: String,
    pub api_version: Option<(u32, u32)>,
}

impl VulkanIcd {
    /// Mesa's lavapipe and SwiftShader render on the CPU; they do not make a
    /// GPU usable for DXVK.
    pub fn is_software(&self) -> bool {
        let name = self.name.to_ascii_lowercase();
        name.starts_with("lvp") || name.contains("swiftshader")
    }
}

/// Vulkan ICD manifest names, for display.
pub fn vulkan_icds() -> Vec<String> {
    let mut names: Vec<String> = vulkan_drivers().into_iter().map(|icd| icd.name).collect();
    names.sort();
    names.dedup();
    names
}

/// The highest Vulkan version a hardware driver declares, or `None` when no
/// hardware Vulkan driver is installed.
pub fn vulkan_api_version() -> Option<(u32, u32)> {
    vulkan_drivers()
        .iter()
        .filter(|icd| !icd.is_software())
        .filter_map(|icd| icd.api_version)
        .max()
}

/// Vulkan driver manifests, searched where the Vulkan loader looks for them.
pub fn vulkan_drivers() -> Vec<VulkanIcd> {
    let mut files = Vec::new();
    // The loader uses only these when they are set.
    let forced = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"]
        .iter()
        .find_map(|key| env::var_os(key).filter(|value| !value.is_empty()));
    if let Some(forced) = forced {
        for path in env::split_paths(&forced) {
            if path.is_dir() {
                files.extend(json_files(&path));
            } else {
                files.push(path);
            }
        }
    } else {
        for dir in icd_dirs() {
            files.extend(json_files(&dir));
        }
    }
    files
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_string();
            let api_version = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| icd_api_version(&text));
            Some(VulkanIcd { name, api_version })
        })
        .collect()
}

fn icd_dirs() -> Vec<PathBuf> {
    let absolute = |key: &str| {
        env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let list = |key: &str, default: &str| -> Vec<PathBuf> {
        let value = env::var_os(key)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.into());
        env::split_paths(&value)
            .filter(|path| path.is_absolute())
            .collect()
    };
    let home = absolute("HOME");
    let mut roots = Vec::new();
    roots.extend(
        absolute("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|home| home.join(".config"))),
    );
    roots.extend(list("XDG_CONFIG_DIRS", "/etc/xdg"));
    roots.push(PathBuf::from("/etc"));
    roots.extend(
        absolute("XDG_DATA_HOME").or_else(|| home.as_ref().map(|home| home.join(".local/share"))),
    );
    roots.extend(list("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    // NixOS exposes the system's drivers here.
    roots.push(PathBuf::from("/run/opengl-driver/share"));
    let mut dirs: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| root.join("vulkan/icd.d"))
        .collect();
    dirs.dedup();
    dirs
}

fn json_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect()
}

/// `ICD.api_version` of a manifest, e.g. `"1.4.303"`, as (major, minor).
fn icd_api_version(text: &str) -> Option<(u32, u32)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let version = value.get("ICD")?.get("api_version")?.as_str()?;
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Locate a command on PATH without executing it.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_icd_api_version() {
        let nvidia = r#"{"file_format_version":"1.0.1","ICD":{"library_path":"libGLX_nvidia.so.0","api_version":"1.4.312"}}"#;
        assert_eq!(icd_api_version(nvidia), Some((1, 4)));
        let old = r#"{"ICD":{"api_version":"1.2.175"}}"#;
        assert_eq!(icd_api_version(old), Some((1, 2)));
        assert_eq!(icd_api_version(r#"{"ICD":{}}"#), None);
        assert_eq!(icd_api_version("not json"), None);
    }

    #[test]
    fn software_drivers_do_not_count() {
        let lavapipe = VulkanIcd {
            name: "lvp_icd.x86_64.json".into(),
            api_version: Some((1, 4)),
        };
        let radv = VulkanIcd {
            name: "radeon_icd.x86_64.json".into(),
            api_version: Some((1, 4)),
        };
        assert!(lavapipe.is_software());
        assert!(!radv.is_software());
    }
}
