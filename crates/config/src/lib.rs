use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot determine the current user's home directory")]
    NoHome,
    #[error("{path} must have permissions {expected:o}, found {actual:o}")]
    InsecurePermissions {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },
    #[error("private key must be located below {0}")]
    KeyOutsideDirectory(PathBuf),
    #[error("configuration I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cannot serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported config version {0}; expected version 1")]
    UnsupportedVersion(u32),
    #[error(
        "target IDs must be unique and target identity fields must be non-empty (invalid target {0:?})"
    )]
    InvalidTarget(String),
    #[error("invalid configuration setting: {0}")]
    InvalidSetting(&'static str),
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub keys: PathBuf,
    pub data: PathBuf,
    pub run: PathBuf,
    pub bin: PathBuf,
    pub socket: PathBuf,
    pub database: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self, ConfigError> {
        let root = dirs::home_dir().ok_or(ConfigError::NoHome)?.join(".aissh");
        Ok(Self::under(root))
    }

    pub fn under(root: PathBuf) -> Self {
        let data = root.join("data");
        let run = root.join("run");
        Self {
            config: root.join("config.toml"),
            keys: root.join("keys"),
            bin: root.join("bin"),
            socket: run.join("aisshd.sock"),
            database: data.join("aissh.db"),
            root,
            data,
            run,
        }
    }

    pub fn ensure(&self) -> Result<(), ConfigError> {
        for path in [&self.root, &self.keys, &self.data, &self.run, &self.bin] {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default = "default_retention")]
    pub retention_days: u32,
    #[serde(default = "default_idle")]
    pub idle_timeout_seconds: u64,
    #[serde(default = "default_connect")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_keepalive")]
    pub keepalive_seconds: u64,
    #[serde(default = "default_recording")]
    pub recording_limit_mib: u64,
    pub quit_daemon_on_app_exit: bool,
    pub launch_at_login: bool,
    #[serde(default)]
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub auth: Auth,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    Password {
        password: String,
    },
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

const fn default_retention() -> u32 {
    30
}
const fn default_idle() -> u64 {
    1800
}
const fn default_connect() -> u64 {
    15
}
const fn default_keepalive() -> u64 {
    30
}
const fn default_recording() -> u64 {
    500
}
const fn default_port() -> u16 {
    22
}

impl Config {
    pub fn default_config() -> Self {
        Self {
            version: 1,
            retention_days: default_retention(),
            idle_timeout_seconds: default_idle(),
            connect_timeout_seconds: default_connect(),
            keepalive_seconds: default_keepalive(),
            recording_limit_mib: default_recording(),
            quit_daemon_on_app_exit: false,
            launch_at_login: false,
            targets: Vec::new(),
        }
    }

    pub fn ensure_exists(paths: &Paths) -> Result<bool, ConfigError> {
        paths.ensure()?;
        if paths.config.exists() {
            return Ok(false);
        }
        Self::default_config().save_atomic(paths)?;
        Ok(true)
    }

    pub fn load(paths: &Paths) -> Result<Self, ConfigError> {
        require_mode(&paths.config, 0o600)?;
        let config: Self = toml::from_str(&fs::read_to_string(&paths.config)?)?;
        config.validate(paths)?;
        Ok(config)
    }

    pub fn validate(&self, paths: &Paths) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err(ConfigError::InvalidSetting("retention_days"));
        }
        if self.idle_timeout_seconds == 0 {
            return Err(ConfigError::InvalidSetting("idle_timeout_seconds"));
        }
        if self.connect_timeout_seconds == 0 {
            return Err(ConfigError::InvalidSetting("connect_timeout_seconds"));
        }
        if self.keepalive_seconds == 0 {
            return Err(ConfigError::InvalidSetting("keepalive_seconds"));
        }
        if self.recording_limit_mib == 0 {
            return Err(ConfigError::InvalidSetting("recording_limit_mib"));
        }
        let mut ids = HashSet::new();
        for target in &self.targets {
            if target.id.is_empty()
                || target.name.is_empty()
                || target.host.is_empty()
                || target.username.is_empty()
                || target.port == 0
                || !ids.insert(&target.id)
            {
                return Err(ConfigError::InvalidTarget(target.id.clone()));
            }
            if let Auth::PrivateKey { path, .. } = &target.auth {
                let key = if path.is_absolute() {
                    path.clone()
                } else {
                    paths.keys.join(path)
                };
                let canonical = key.canonicalize()?;
                let keys = paths.keys.canonicalize()?;
                if !canonical.starts_with(&keys) {
                    return Err(ConfigError::KeyOutsideDirectory(keys));
                }
                require_mode(&canonical, 0o600)?;
            }
        }
        Ok(())
    }

    pub fn save_atomic(&self, paths: &Paths) -> Result<(), ConfigError> {
        paths.ensure()?;
        self.validate(paths)?;
        let temp = paths.root.join("config.toml.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        fs::rename(temp, &paths.config)?;
        Ok(())
    }
}

fn require_mode(path: &Path, expected: u32) -> Result<(), ConfigError> {
    let actual = fs::metadata(path)?.permissions().mode() & 0o777;
    if actual != expected {
        return Err(ConfigError::InsecurePermissions {
            path: path.to_path_buf(),
            expected,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn refuses_world_readable_config() {
        let root = std::env::temp_dir().join(format!(
            "aissh-config-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::under(root);
        paths.ensure().unwrap();
        fs::write(&paths.config, "version = 1\n").unwrap();
        fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Config::load(&paths),
            Err(ConfigError::InsecurePermissions { .. })
        ));
        fs::remove_dir_all(&paths.root).unwrap();
    }

    #[test]
    fn credentials_do_not_appear_in_target_summary_shape() {
        let target = Target {
            id: "prod".into(),
            name: "Production".into(),
            host: "host".into(),
            port: 22,
            username: "root".into(),
            auth: Auth::Password {
                password: "secret".into(),
            },
        };
        let public = aissh_target_summary(&target);
        assert!(!serde_json::to_string(&public).unwrap().contains("secret"));
    }

    #[test]
    fn creates_secure_empty_default_config() {
        let root = std::env::temp_dir().join(format!(
            "aissh-default-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::under(root);
        assert!(Config::ensure_exists(&paths).unwrap());
        assert!(!Config::ensure_exists(&paths).unwrap());
        let config = Config::load(&paths).unwrap();
        assert!(config.targets.is_empty());
        assert_eq!(
            fs::metadata(&paths.config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(&paths.root).unwrap();
    }

    #[test]
    fn rejects_config_without_required_app_settings() {
        let root = std::env::temp_dir().join(format!(
            "aissh-required-settings-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::under(root);
        paths.ensure().unwrap();
        fs::write(
            &paths.config,
            concat!(
                "version = 1\n",
                "retention_days = 30\n",
                "idle_timeout_seconds = 1800\n",
                "connect_timeout_seconds = 15\n",
                "keepalive_seconds = 30\n",
                "recording_limit_mib = 500\n",
            ),
        )
        .unwrap();
        fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600)).unwrap();

        let error = Config::load(&paths).unwrap_err().to_string();
        assert!(error.contains("missing field `quit_daemon_on_app_exit`"));
        fs::remove_dir_all(&paths.root).unwrap();
    }

    #[test]
    fn saves_and_loads_every_editable_setting() {
        let root = std::env::temp_dir().join(format!(
            "aissh-roundtrip-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::under(root);
        paths.ensure().unwrap();
        let key = paths.keys.join("test-key");
        fs::write(&key, "test private key").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();

        let config = Config {
            version: 1,
            retention_days: 90,
            idle_timeout_seconds: 900,
            connect_timeout_seconds: 25,
            keepalive_seconds: 45,
            recording_limit_mib: 750,
            quit_daemon_on_app_exit: true,
            launch_at_login: true,
            targets: vec![
                Target {
                    id: "password-host".into(),
                    name: "Password Host".into(),
                    host: "password.example.test".into(),
                    port: 2222,
                    username: "alice".into(),
                    auth: Auth::Password {
                        password: "login-secret".into(),
                    },
                },
                Target {
                    id: "key-host".into(),
                    name: "Key Host".into(),
                    host: "key.example.test".into(),
                    port: 22,
                    username: "bob".into(),
                    auth: Auth::PrivateKey {
                        path: PathBuf::from("test-key"),
                        passphrase: Some("key-secret".into()),
                    },
                },
            ],
        };

        config.save_atomic(&paths).unwrap();
        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.retention_days, 90);
        assert_eq!(loaded.idle_timeout_seconds, 900);
        assert_eq!(loaded.connect_timeout_seconds, 25);
        assert_eq!(loaded.keepalive_seconds, 45);
        assert_eq!(loaded.recording_limit_mib, 750);
        assert!(loaded.quit_daemon_on_app_exit);
        assert!(loaded.launch_at_login);
        assert_eq!(loaded.targets.len(), 2);
        assert_eq!(loaded.targets[0].port, 2222);
        assert!(matches!(
            &loaded.targets[0].auth,
            Auth::Password { password } if password == "login-secret"
        ));
        assert!(matches!(
            &loaded.targets[1].auth,
            Auth::PrivateKey { path, passphrase }
                if path == &PathBuf::from("test-key")
                    && passphrase.as_deref() == Some("key-secret")
        ));
        assert_eq!(
            fs::metadata(&paths.config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(&paths.root).unwrap();
    }

    fn aissh_target_summary(target: &Target) -> serde_json::Value {
        serde_json::json!({"id": target.id, "name": target.name, "host": target.host, "port": target.port, "username": target.username})
    }
}
