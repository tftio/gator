use std::env;
use std::path::PathBuf;

/// Database configuration.
///
/// Points at a SQLite database file. Reads from the `GATOR_DATABASE_URL`
/// environment variable (as a file path), falling back to
/// `~/.config/gator/gator.db` when unset.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
}

impl DbConfig {
    /// Build a config from the environment.
    ///
    /// Priority: `GATOR_DATABASE_URL` env var (interpreted as a file path),
    /// then `~/.config/gator/gator.db`.
    pub fn from_env() -> Self {
        if let Ok(url) = env::var("GATOR_DATABASE_URL") {
            return Self {
                db_path: PathBuf::from(url),
            };
        }
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("gator");
        Self {
            db_path: config_dir.join("gator.db"),
        }
    }

    /// Build a config from an explicit path (useful for tests and CLI flags).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// Return the SQLite connection URL for this config.
    pub fn database_url(&self) -> String {
        format!("sqlite://{}?mode=rwc", self.db_path.display())
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_from_path() {
        let cfg = DbConfig::new("/tmp/test.db");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/test.db"));
    }

    #[test]
    fn database_url_format() {
        let cfg = DbConfig::new("/tmp/test.db");
        assert_eq!(cfg.database_url(), "sqlite:///tmp/test.db?mode=rwc");
    }
}
