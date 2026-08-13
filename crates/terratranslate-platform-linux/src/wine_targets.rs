//! Discovery and attachment planning for Wine/Proton guest processes.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WineProcessArchitecture {
    X86,
    X86_64,
}

impl WineProcessArchitecture {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WineTarget {
    pub process_id: u32,
    pub executable: String,
    pub architecture: WineProcessArchitecture,
    pub prefix: PathBuf,
    pub runtime: String,
    pub runtime_command: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WineArtifacts {
    pub injector_x86: PathBuf,
    pub injector_x86_64: PathBuf,
    pub hook_x86: PathBuf,
    pub hook_x86_64: PathBuf,
}

impl WineArtifacts {
    pub fn host_defaults() -> Self {
        let lib = std::env::var_os("TERRATRANSLATE_WINE_LIB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib/terratranslate/wine"));
        let libexec = std::env::var_os("TERRATRANSLATE_WINE_LIBEXEC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/libexec/terratranslate/wine"));
        Self {
            injector_x86: libexec.join("i686/terratranslate-wine-injector.exe"),
            injector_x86_64: libexec.join("x86_64/terratranslate-wine-injector.exe"),
            hook_x86: lib.join("i686/terratranslate_wine_hook.dll"),
            hook_x86_64: lib.join("x86_64/terratranslate_wine_hook.dll"),
        }
    }

    fn for_architecture(&self, architecture: WineProcessArchitecture) -> (&Path, &Path) {
        match architecture {
            WineProcessArchitecture::X86 => (&self.injector_x86, &self.hook_x86),
            WineProcessArchitecture::X86_64 => (&self.injector_x86_64, &self.hook_x86_64),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WineTargetError {
    #[error("Wine process discovery is unavailable in Flatpak; install and run the host build")]
    Flatpak,
    #[error("read process information: {0}")]
    Io(#[from] std::io::Error),
    #[error("{kind} artifact is missing: {path}")]
    MissingArtifact { kind: &'static str, path: PathBuf },
    #[error("Wine injector failed with {status}")]
    Injector { status: ExitStatus },
}

/// Finds Wine/Proton processes visible through `/proc`. Each result has a
/// guest PE image found in the process mappings, avoiding PID and host-loader
/// guesses for x86-versus-x86-64 selection.
pub fn discover_wine_targets() -> Result<Vec<WineTarget>, WineTargetError> {
    if Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some() {
        return Err(WineTargetError::Flatpak);
    }
    discover_wine_targets_in(Path::new("/proc"))
}

fn discover_wine_targets_in(proc_root: &Path) -> Result<Vec<WineTarget>, WineTargetError> {
    let mut targets = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(process_id) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let directory = entry.path();
        let environment = match fs::read(directory.join("environ")) {
            Ok(bytes) => parse_nul_pairs(&bytes),
            Err(_) => continue,
        };
        let prefix = environment
            .iter()
            .find(|(key, _)| key == "WINEPREFIX")
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                environment
                    .iter()
                    .find(|(key, _)| key == "STEAM_COMPAT_DATA_PATH")
                    .map(|(_, value)| PathBuf::from(value).join("pfx"))
            });
        let Some(prefix) = prefix else {
            continue;
        };
        let command_line = fs::read(directory.join("cmdline")).unwrap_or_default();
        let arguments = parse_nul_values(&command_line);
        let runtime_command = environment
            .iter()
            .find(|(key, _)| key == "WINELOADER")
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                arguments.first().and_then(|argument| {
                    argument
                        .to_ascii_lowercase()
                        .contains("wine")
                        .then(|| PathBuf::from(argument))
                })
            })
            .unwrap_or_else(|| PathBuf::from("wine"));
        let mapped_images = fs::read_to_string(directory.join("maps"))
            .ok()
            .into_iter()
            .flat_map(|maps| mapped_executables(&maps).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let Some((executable, architecture)) = mapped_images
            .iter()
            .find_map(|path| pe_architecture(path).map(|architecture| (path.clone(), architecture)))
            .or_else(|| {
                arguments.iter().find_map(|argument| {
                    let path = Path::new(argument);
                    pe_architecture(path).map(|architecture| (path.to_path_buf(), architecture))
                })
            })
        else {
            continue;
        };
        targets.push(WineTarget {
            process_id,
            executable: executable.to_string_lossy().into_owned(),
            architecture,
            prefix,
            runtime: if environment
                .iter()
                .any(|(key, _)| key == "STEAM_COMPAT_DATA_PATH")
            {
                "proton".into()
            } else {
                "wine".into()
            },
            runtime_command,
        });
    }
    targets.sort_by_key(|target| target.process_id);
    Ok(targets)
}

/// Runs the architecture-matched injector inside the selected prefix. Paths
/// are passed as separate arguments and Wine converts host absolute paths
/// through its `Z:` mapping; no shell is involved.
pub fn attach_wine_target(
    target: &WineTarget,
    artifacts: &WineArtifacts,
    config: &Path,
) -> Result<(), WineTargetError> {
    let (injector, hook) = artifacts.for_architecture(target.architecture);
    for (kind, path) in [
        ("injector", injector),
        ("hook DLL", hook),
        ("config", config),
    ] {
        if !path.is_file() {
            return Err(WineTargetError::MissingArtifact {
                kind,
                path: path.to_owned(),
            });
        }
    }
    let wine = std::env::var_os("TERRATRANSLATE_WINE_COMMAND")
        .unwrap_or_else(|| OsString::from(&target.runtime_command));
    let status = Command::new(wine)
        .env("WINEPREFIX", &target.prefix)
        .arg(injector)
        .arg("--process-id")
        .arg(target.process_id.to_string())
        .arg("--dll")
        .arg(hook)
        .arg("--config")
        .arg(config)
        .status()?;
    if !status.success() {
        return Err(WineTargetError::Injector { status });
    }
    Ok(())
}

fn parse_nul_pairs(bytes: &[u8]) -> Vec<(String, String)> {
    parse_nul_values(bytes)
        .into_iter()
        .filter_map(|value| {
            let (key, value) = value.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn parse_nul_values(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect()
}

fn mapped_executables(maps: &str) -> impl Iterator<Item = PathBuf> + '_ {
    maps.lines().filter_map(|line| {
        let path = line.split_whitespace().nth(5)?;
        path.to_ascii_lowercase()
            .ends_with(".exe")
            .then(|| PathBuf::from(path))
    })
}

fn pe_architecture(path: &Path) -> Option<WineProcessArchitecture> {
    let bytes = fs::read(path).ok()?;
    if bytes.get(..2)? != b"MZ" {
        return None;
    }
    let offset = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if bytes.get(offset..offset + 4)? != b"PE\0\0" {
        return None;
    }
    match u16::from_le_bytes(bytes.get(offset + 4..offset + 6)?.try_into().ok()?) {
        0x014c => Some(WineProcessArchitecture::X86),
        0x8664 => Some(WineProcessArchitecture::X86_64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discovers_prefix_executable_and_architecture() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tt-wine-target-{nonce}"));
        let process = root.join("42");
        fs::create_dir_all(&process).unwrap();
        let image = root.join("game.exe");
        let mut pe = vec![0; 0x86];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
        fs::write(&image, pe).unwrap();
        fs::write(process.join("environ"), b"WINEPREFIX=/games/prefix\0").unwrap();
        fs::write(process.join("cmdline"), b"wine64\0game.exe\0").unwrap();
        fs::write(
            process.join("maps"),
            format!("00400000-00500000 r-xp 0 00:00 0 {}\n", image.display()),
        )
        .unwrap();
        // A non-numeric entry must be ignored.
        symlink(&process, root.join("self")).unwrap();

        let targets = discover_wine_targets_in(&root).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].process_id, 42);
        assert_eq!(targets[0].architecture, WineProcessArchitecture::X86_64);
        assert_eq!(targets[0].prefix, Path::new("/games/prefix"));
        assert_eq!(targets[0].runtime_command, Path::new("wine64"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requires_a_real_pe_image() {
        assert_eq!(pe_architecture(Path::new("/does/not/exist")), None);
    }
}
