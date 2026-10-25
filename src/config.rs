use serde::{Deserialize, Serialize};
use std::{env, fs, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    pub remote: String,
    pub folder: String,
    pub mountpoint: PathBuf,
    pub cache_gib: u32,
    pub mount_at_start: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub profiles: Vec<Profile>,
}

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub autostart: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self, String> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        let base = |key: &str, fallback: &str| {
            env::var_os(key)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        Ok(Self {
            config: base("XDG_CONFIG_HOME", ".config").join("rclone-tray/profiles.toml"),
            cache: base("XDG_CACHE_HOME", ".cache").join("rclone-tray"),
            logs: base("XDG_STATE_HOME", ".local/state").join("rclone-tray"),
            autostart: base("XDG_CONFIG_HOME", ".config").join("autostart/rclone-tray.desktop"),
        })
    }

    pub fn load(&self) -> Result<Config, String> {
        match fs::read_to_string(&self.config) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("Cannot read profiles: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("Cannot read {}: {e}", self.config.display())),
        }
    }

    pub fn save(&self, config: &Config) -> Result<(), String> {
        fs::create_dir_all(self.config.parent().unwrap()).map_err(|e| e.to_string())?;
        let text = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
        let temporary = self.config.with_extension("toml.tmp");
        fs::write(&temporary, text).map_err(|e| e.to_string())?;
        fs::rename(temporary, &self.config).map_err(|e| e.to_string())
    }

    pub fn set_autostart(&self, enabled: bool) -> Result<(), String> {
        if enabled {
            let executable = env::current_exe()
                .map_err(|e| e.to_string())?
                .with_file_name("rclone-tray");
            let escaped = executable
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('`', "\\`")
                .replace('$', "\\$")
                .replace('%', "%%");
            fs::create_dir_all(self.autostart.parent().unwrap()).map_err(|e| e.to_string())?;
            fs::write(&self.autostart, format!(
                "[Desktop Entry]\nType=Application\nName=Rclone Tray\nExec=\"{escaped}\" --background\nIcon=drive-harddisk\nTerminal=false\n"
            )).map_err(|e| e.to_string())
        } else {
            match fs::remove_file(&self.autostart) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_round_trip_and_corrupt_config_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("profiles.toml"),
            cache: dir.path().join("cache"),
            logs: dir.path().join("logs"),
            autostart: dir.path().join("autostart.desktop"),
        };
        let profile = Profile {
            id: 7,
            name: "Work files".into(),
            remote: "work".into(),
            folder: "Reports/2026".into(),
            mountpoint: dir.path().join("mount with spaces"),
            cache_gib: 12,
            mount_at_start: true,
        };
        paths
            .save(&Config {
                profiles: vec![profile],
            })
            .unwrap();
        let loaded = paths.load().unwrap();
        assert_eq!(loaded.profiles[0].folder, "Reports/2026");
        assert_eq!(loaded.profiles[0].cache_gib, 12);
        fs::write(&paths.config, "[broken").unwrap();
        assert!(paths.load().is_err());
        assert_eq!(fs::read_to_string(paths.config).unwrap(), "[broken");
    }
}
