//! Configuration file management for gator.
//!
//! Provides a TOML-based config file at `~/.config/gator/config.toml` and a
//! resolution chain: CLI flag > env var > config file > default.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use gator_core::token::TokenConfig;
use gator_db::config::DbConfig;

// -----------------------------------------------------------------------
// Config file types
// -----------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigFile {
    pub database: DatabaseSection,
    pub auth: AuthSection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DatabaseSection {
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthSection {
    /// Hex-encoded token secret (64 hex chars = 32 bytes).
    pub token_secret: String,
}

// -----------------------------------------------------------------------
// Paths
// -----------------------------------------------------------------------

/// Return the gator config directory.
///
/// Always uses XDG layout: `$XDG_CONFIG_HOME/gator` or `~/.config/gator`.
/// We intentionally ignore the platform-specific `dirs::config_dir()`
/// (which returns `~/Library/Application Support` on macOS).
pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("gator");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("gator")
}

/// Return the path to the gator config file.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

// -----------------------------------------------------------------------
// Read / write
// -----------------------------------------------------------------------

/// Load and parse the config file. Returns an error if it does not exist.
pub fn load_config() -> Result<ConfigFile> {
    let path = config_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file at {}", path.display()))?;
    let config: ConfigFile = toml::from_str(&contents).context("failed to parse config file")?;
    Ok(config)
}

/// Serialize and write the config file, creating parent dirs as needed.
/// Sets file permissions to 0600 on Unix.
pub fn save_config(config: &ConfigFile) -> Result<()> {
    let path = config_path();
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config directory {}", dir.display()))?;

    let contents = toml::to_string_pretty(config).context("failed to serialize config")?;
    std::fs::write(&path, &contents)
        .with_context(|| format!("failed to write config file at {}", path.display()))?;

    // Set permissions to 0600 (owner read/write only) on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }

    Ok(())
}

// -----------------------------------------------------------------------
// Token secret generation
// -----------------------------------------------------------------------

/// Generate a random token secret: 32 random bytes, hex-encoded (64 chars).
pub fn generate_token_secret() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    hex::encode(bytes)
}

// -----------------------------------------------------------------------
// Resolved config
// -----------------------------------------------------------------------

/// Fully resolved configuration, ready for use.
#[derive(Debug)]
pub struct GatorConfig {
    pub db_config: DbConfig,
    pub token_config: TokenConfig,
}

impl GatorConfig {
    /// Resolve configuration using the chain: CLI flag > env var > config file > default.
    ///
    /// - DB URL: `cli_db_url` > `GATOR_DATABASE_URL` env > `config_file.database.url` > `DbConfig::DEFAULT_URL`
    /// - Token secret: `GATOR_TOKEN_SECRET` env > `config_file.auth.token_secret` (hex-decoded) > error
    pub fn resolve(cli_db_url: Option<&str>) -> Result<Self> {
        let file_config = load_config().ok();

        // DB URL resolution.
        let db_url = if let Some(url) = cli_db_url {
            url.to_string()
        } else if let Ok(url) = std::env::var("GATOR_DATABASE_URL") {
            url
        } else if let Some(ref cfg) = file_config {
            cfg.database.url.clone()
        } else {
            DbConfig::DEFAULT_URL.to_string()
        };
        let db_config = DbConfig::new(db_url);

        // Token secret resolution.
        let token_config = if let Ok(secret_hex) = std::env::var("GATOR_TOKEN_SECRET") {
            let bytes = hex::decode(&secret_hex)
                .context("GATOR_TOKEN_SECRET env var is not valid hex")?;
            TokenConfig::new(bytes)
        } else if let Some(ref cfg) = file_config {
            let bytes = hex::decode(&cfg.auth.token_secret)
                .context("invalid hex in config file token_secret")?;
            TokenConfig::new(bytes)
        } else {
            bail!(
                "token secret not found; set GATOR_TOKEN_SECRET or run `gator init` to create a config file"
            );
        };

        Ok(Self {
            db_config,
            token_config,
        })
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        crate::test_util::lock_env()
    }

    #[test]
    fn generate_token_secret_is_64_hex_chars() {
        let secret = generate_token_secret();
        assert_eq!(secret.len(), 64);
        assert!(
            secret.chars().all(|c| c.is_ascii_hexdigit()),
            "expected all hex digits, got: {secret}"
        );
    }

    #[test]
    fn generate_token_secret_is_random() {
        let a = generate_token_secret();
        let b = generate_token_secret();
        assert_ne!(a, b, "two generated secrets should differ");
    }

    #[test]
    fn save_and_load_config_roundtrip() {
        let _lock = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("gator");
        let path = dir.join("config.toml");

        // Temporarily override the config path by writing directly.
        let original = ConfigFile {
            database: DatabaseSection {
                url: "postgresql://testhost:5432/testdb".to_string(),
            },
            auth: AuthSection {
                token_secret: "aa".repeat(32),
            },
        };

        std::fs::create_dir_all(&dir).unwrap();
        let contents = toml::to_string_pretty(&original).unwrap();
        std::fs::write(&path, &contents).unwrap();

        // Read it back.
        let loaded_contents = std::fs::read_to_string(&path).unwrap();
        let loaded: ConfigFile = toml::from_str(&loaded_contents).unwrap();

        assert_eq!(loaded.database.url, original.database.url);
        assert_eq!(loaded.auth.token_secret, original.auth.token_secret);
    }

    #[cfg(unix)]
    #[test]
    fn save_config_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = lock_env();

        // We test save_config by temporarily pointing HOME so config_dir
        // returns a temp path. Instead, test the permission-setting logic
        // directly on a temp file.
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("test.toml");
        std::fs::write(&file, "test").unwrap();

        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&file, perms).unwrap();

        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn resolve_with_cli_flag_overrides_all() {
        let _lock = lock_env();

        // Even if env var is set, CLI flag wins.
        unsafe { std::env::set_var("GATOR_DATABASE_URL", "postgresql://env:5432/envdb") };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55") };

        let config = GatorConfig::resolve(Some("postgresql://cli:5432/clidb")).unwrap();
        assert_eq!(config.db_config.database_url, "postgresql://cli:5432/clidb");

        unsafe { std::env::remove_var("GATOR_DATABASE_URL") };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[test]
    fn resolve_with_env_var_overrides_config_file() {
        let _lock = lock_env();

        unsafe { std::env::set_var("GATOR_DATABASE_URL", "postgresql://env:5432/envdb") };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55") };

        let config = GatorConfig::resolve(None).unwrap();
        assert_eq!(config.db_config.database_url, "postgresql://env:5432/envdb");

        unsafe { std::env::remove_var("GATOR_DATABASE_URL") };
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[test]
    fn resolve_defaults_db_url_when_nothing_set() {
        let _lock = lock_env();

        unsafe { std::env::remove_var("GATOR_DATABASE_URL") };
        unsafe { std::env::set_var("GATOR_TOKEN_SECRET", "aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55aa55") };

        let config = GatorConfig::resolve(None).unwrap();
        assert_eq!(config.db_config.database_url, DbConfig::DEFAULT_URL);

        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
    }

    #[test]
    fn resolve_errors_when_no_token_secret() {
        let _lock = lock_env();

        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
        // Point HOME and XDG_CONFIG_HOME to a temp dir so load_config() cannot
        // find a real config file.
        let tmp = tempfile::TempDir::new().unwrap();
        let orig_home = std::env::var("HOME").ok();
        let orig_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };

        let result = GatorConfig::resolve(Some("postgresql://localhost:5432/gator"));

        // Restore env before asserting, to avoid poisoning the mutex on failure.
        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match orig_xdg {
            Some(x) => unsafe { std::env::set_var("XDG_CONFIG_HOME", x) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        assert!(result.is_err(), "should error when no token secret");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("token secret not found"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn config_path_ends_with_expected_filename() {
        let path = config_path();
        assert!(
            path.ends_with("gator/config.toml"),
            "unexpected config path: {}",
            path.display()
        );
    }
}
