//! Private Postgres for `clankrd --dev` and tests. Binaries download once into ~/.theseus.

use std::path::PathBuf;

use postgresql_embedded::{PostgreSQL, Settings};

/// A running server; stopped (and deleted, if temporary) on drop.
pub struct DevPostgres {
    pg: PostgreSQL,
}

impl DevPostgres {
    /// `data_dir: None` gives a throwaway server whose data is removed on drop.
    pub async fn start(data_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut settings = Settings::default();
        if let Some(dir) = data_dir {
            settings.data_dir = dir;
            settings.temporary = false;
            // persistent clusters keep the password chosen at initdb; the default is random per run
            settings.password = "clankr-dev".into();
        }
        settings.timeout = Some(std::time::Duration::from_secs(30));
        let mut pg = PostgreSQL::new(settings);
        pg.setup().await?;
        pg.start().await?;
        Ok(Self { pg })
    }

    /// Where this server's binaries (pg_dump, pg_restore, …) live.
    pub fn bin_dir(&self) -> PathBuf {
        self.pg.settings().binary_dir()
    }

    /// Creates `name` if missing and returns its connection URL.
    pub async fn database(&self, name: &str) -> anyhow::Result<String> {
        if !self.pg.database_exists(name).await? {
            self.pg.create_database(name).await?;
        }
        Ok(self.pg.settings().url(name))
    }
}
