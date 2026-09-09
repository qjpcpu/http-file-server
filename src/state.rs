use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone)]
pub(crate) struct StateStore {
    connection: Arc<Mutex<Connection>>,
}

impl StateStore {
    pub(crate) fn new(cache_root: Option<&Path>) -> io::Result<Self> {
        let connection = match cache_root {
            Some(root) => {
                fs::create_dir_all(root)?;
                Connection::open(root.join("http.sqlite")).map_err(sqlite_error)?
            }
            None => Connection::open_in_memory().map_err(sqlite_error)?,
        };
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS favourites (
                    path BLOB PRIMARY KEY NOT NULL
                );
                CREATE TABLE IF NOT EXISTS deletion_marks (
                    path BLOB PRIMARY KEY NOT NULL
                );",
            )
            .map_err(sqlite_error)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(crate) fn is_favourite(&self, path: &Path) -> io::Result<bool> {
        self.contains("favourites", path)
    }

    pub(crate) fn set_favourite(&self, path: &Path, value: bool) -> io::Result<()> {
        self.set("favourites", path, value)
    }

    pub(crate) fn is_deletion_marked(&self, path: &Path) -> io::Result<bool> {
        self.contains("deletion_marks", path)
    }

    pub(crate) fn set_deletion_mark(&self, path: &Path, value: bool) -> io::Result<()> {
        self.set("deletion_marks", path, value)
    }

    pub(crate) fn clear_image_state(&self, path: &Path) -> io::Result<()> {
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection.transaction().map_err(sqlite_error)?;
        transaction
            .execute(
                "DELETE FROM favourites WHERE path = ?1",
                params![path_bytes(path)],
            )
            .map_err(sqlite_error)?;
        transaction
            .execute(
                "DELETE FROM deletion_marks WHERE path = ?1",
                params![path_bytes(path)],
            )
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)
    }

    fn contains(&self, table: &str, path: &Path) -> io::Result<bool> {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                &format!("SELECT 1 FROM {table} WHERE path = ?1"),
                params![path_bytes(path)],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(sqlite_error)
    }

    fn set(&self, table: &str, path: &Path, value: bool) -> io::Result<()> {
        let connection = self.connection.lock().unwrap();
        let statement = if value {
            format!("INSERT OR IGNORE INTO {table} (path) VALUES (?1)")
        } else {
            format!("DELETE FROM {table} WHERE path = ?1")
        };
        connection
            .execute(&statement, params![path_bytes(path)])
            .map(|_| ())
            .map_err(sqlite_error)
    }
}

fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

fn sqlite_error(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_database_is_shared_by_clones_and_isolated_by_instance() {
        let state = StateStore::new(None).unwrap();
        let path = Path::new("/photos/cat.png");
        state.set_favourite(path, true).unwrap();
        state.set_deletion_mark(path, true).unwrap();

        assert!(state.clone().is_favourite(path).unwrap());
        assert!(state.clone().is_deletion_marked(path).unwrap());
        assert!(!StateStore::new(None).unwrap().is_favourite(path).unwrap());

        state.clear_image_state(path).unwrap();
        assert!(!state.is_favourite(path).unwrap());
        assert!(!state.is_deletion_marked(path).unwrap());
    }

    #[test]
    fn disk_database_persists_both_state_tables() {
        let cache = tempfile::tempdir().unwrap();
        let first = Path::new("/photos/猫.png");
        let second = Path::new("/other/猫.png");
        let state = StateStore::new(Some(cache.path())).unwrap();
        state.set_favourite(first, true).unwrap();
        state.set_deletion_mark(second, true).unwrap();
        drop(state);

        let reopened = StateStore::new(Some(cache.path())).unwrap();
        assert!(reopened.is_favourite(first).unwrap());
        assert!(reopened.is_deletion_marked(second).unwrap());
        assert!(cache.path().join("http.sqlite").is_file());
        assert!(!cache.path().join("favourites").exists());
        assert!(!cache.path().join("deletion-marks").exists());
    }
}
