use std::env;

/// Database configuration.
///
/// Reads from the `GATOR_DATABASE_URL` environment variable, falling back to
/// `postgresql://localhost:5432/gator` when unset.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Full PostgreSQL connection URL.
    pub database_url: String,
}

impl DbConfig {
    /// The default connection URL used when no environment variable is set.
    pub const DEFAULT_URL: &str = "postgresql://localhost:5432/gator";

    /// Build a config from the environment.
    ///
    /// Priority: `GATOR_DATABASE_URL` env var, then the compile-time default.
    pub fn from_env() -> Self {
        let database_url = env::var("GATOR_DATABASE_URL")
            .unwrap_or_else(|_| Self::DEFAULT_URL.to_owned());
        Self { database_url }
    }

    /// Build a config from an explicit URL (useful for tests and CLI flags).
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
        }
    }

    /// Extract the database name from the URL.
    ///
    /// Returns `None` if the URL cannot be parsed or has no path component.
    pub fn database_name(&self) -> Option<&str> {
        // URLs look like: postgresql://host:port/dbname or postgres://host:port/dbname
        self.database_url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
    }

    /// Return a URL pointing at the `postgres` maintenance database on the
    /// same host. Used to issue `CREATE DATABASE` when the target DB does not
    /// yet exist.
    pub fn maintenance_url(&self) -> String {
        match self.database_url.rfind('/') {
            Some(pos) => {
                let mut url = self.database_url[..pos].to_owned();
                url.push_str("/postgres");
                url
            }
            None => self.database_url.clone(),
        }
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
    fn default_url() {
        let cfg = DbConfig::new(DbConfig::DEFAULT_URL);
        assert_eq!(cfg.database_url, "postgresql://localhost:5432/gator");
    }

    #[test]
    fn database_name_extraction() {
        let cfg = DbConfig::new("postgresql://localhost:5432/mydb");
        assert_eq!(cfg.database_name(), Some("mydb"));
    }

    #[test]
    fn maintenance_url_replaces_db() {
        let cfg = DbConfig::new("postgresql://localhost:5432/gator");
        assert_eq!(cfg.maintenance_url(), "postgresql://localhost:5432/postgres");
    }

    #[test]
    fn explicit_new() {
        let cfg = DbConfig::new("postgresql://remotehost:5433/other");
        assert_eq!(cfg.database_url, "postgresql://remotehost:5433/other");
        assert_eq!(cfg.database_name(), Some("other"));
    }
}
