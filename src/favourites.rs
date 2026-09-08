use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};

#[derive(Clone)]
pub(crate) enum Favourites {
    Memory(Arc<Mutex<HashSet<PathBuf>>>),
    Disk(PathBuf),
}

impl Favourites {
    pub(crate) fn new(cache_root: Option<&Path>) -> io::Result<Self> {
        match cache_root {
            Some(root) => {
                let directory = root.join("favourites");
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

    pub(crate) fn set(&self, path: &Path, favourite: bool) -> io::Result<()> {
        match self {
            Self::Memory(paths) => {
                let mut paths = paths.lock().unwrap();
                if favourite {
                    paths.insert(path.to_owned());
                } else {
                    paths.remove(path);
                }
                Ok(())
            }
            Self::Disk(directory) => {
                let index = index_path(directory, path);
                if favourite {
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
    fn memory_is_shared_until_the_store_is_recreated() {
        let store = Favourites::new(None).unwrap();
        let path = Path::new("/photos/cat.png");
        store.set(path, true).unwrap();
        assert!(store.clone().contains(path).unwrap());
        store.clone().set(path, false).unwrap();
        assert!(!store.contains(path).unwrap());
        store.set(path, true).unwrap();
        assert!(!Favourites::new(None).unwrap().contains(path).unwrap());
    }

    #[test]
    fn disk_persists_likes_and_unlikes_with_separate_absolute_paths() {
        let cache = tempfile::tempdir().unwrap();
        let first = Path::new("/photos/猫.png");
        let second = Path::new("/other/猫.png");
        let store = Favourites::new(Some(cache.path())).unwrap();
        store.set(first, true).unwrap();
        store.set(second, true).unwrap();
        let index: serde_json::Value = serde_json::from_slice(
            &fs::read(index_path(&cache.path().join("favourites"), first)).unwrap(),
        )
        .unwrap();
        assert_eq!(index["path"], first.to_str().unwrap());
        let reopened = Favourites::new(Some(cache.path())).unwrap();
        assert!(reopened.contains(first).unwrap());
        assert!(reopened.contains(second).unwrap());
        reopened.set(first, false).unwrap();
        let reopened = Favourites::new(Some(cache.path())).unwrap();
        assert!(!reopened.contains(first).unwrap());
        assert!(reopened.contains(second).unwrap());
    }
}
