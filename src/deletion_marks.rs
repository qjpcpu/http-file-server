use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};

#[derive(Clone)]
pub(crate) enum DeletionMarks {
    Memory(Arc<Mutex<HashSet<PathBuf>>>),
    Disk(PathBuf),
}

impl DeletionMarks {
    pub(crate) fn new(cache_root: Option<&Path>) -> io::Result<Self> {
        match cache_root {
            Some(root) => {
                let directory = root.join("deletion-marks");
                fs::create_dir_all(&directory)?;
                Ok(Self::Disk(directory))
            }
            None => Ok(Self::Memory(Arc::default())),
        }
    }

    pub(crate) fn contains(&self, path: &Path) -> io::Result<bool> {
        match self {
            Self::Memory(paths) => Ok(paths.lock().unwrap().contains(path)),
            Self::Disk(directory) => index_path(directory, path).try_exists(),
        }
    }

    pub(crate) fn set(&self, path: &Path, marked: bool) -> io::Result<()> {
        match self {
            Self::Memory(paths) => {
                let mut paths = paths.lock().unwrap();
                if marked {
                    paths.insert(path.to_owned());
                } else {
                    paths.remove(path);
                }
                Ok(())
            }
            Self::Disk(directory) => {
                let index = index_path(directory, path);
                if marked {
                    let mut file = tempfile::NamedTempFile::new_in(directory)?;
                    serde_json::to_writer(&mut file, &serde_json::json!({"path": path}))?;
                    file.write_all(b"\n")?;
                    file.persist(index).map_err(|error| error.error)?;
                    Ok(())
                } else {
                    match fs::remove_file(index) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                        _ => Ok(()),
                    }
                }
            }
        }
    }
}

fn index_path(directory: &Path, path: &Path) -> PathBuf {
    let key = crate::hex_digest(&Sha1::digest(path.as_os_str().as_encoded_bytes()));
    directory.join(format!("{key}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_marks_are_shared_and_can_be_cleared() {
        let marks = DeletionMarks::new(None).unwrap();
        let path = Path::new("/photos/cat.png");
        marks.set(path, true).unwrap();
        assert!(marks.clone().contains(path).unwrap());
        marks.set(path, false).unwrap();
        assert!(!marks.contains(path).unwrap());
    }

    #[test]
    fn disk_marks_survive_reopening() {
        let cache = tempfile::tempdir().unwrap();
        let path = Path::new("/photos/cat.png");
        DeletionMarks::new(Some(cache.path()))
            .unwrap()
            .set(path, true)
            .unwrap();
        let reopened = DeletionMarks::new(Some(cache.path())).unwrap();
        assert!(reopened.contains(path).unwrap());
        reopened.set(path, false).unwrap();
        assert!(!DeletionMarks::new(Some(cache.path()))
            .unwrap()
            .contains(path)
            .unwrap());
    }
}
