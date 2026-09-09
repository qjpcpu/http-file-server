use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sha1::{Digest, Sha1};

#[derive(Clone)]
pub(crate) struct ImageCache {
    directory: PathBuf,
    estimated_size: Arc<AtomicU64>,
    cleanup_running: Arc<AtomicBool>,
}

const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CACHE_TARGET_BYTES: u64 = CACHE_MAX_BYTES * 4 / 5;
const CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CACHE_TOUCH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const CACHE_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const TEMP_FILE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

impl ImageCache {
    pub(crate) fn new(cache_root: &Path) -> io::Result<Self> {
        let directory = cache_root.join("thumbnails");
        fs::create_dir_all(&directory)?;
        Ok(Self {
            directory,
            estimated_size: Arc::new(AtomicU64::new(0)),
            cleanup_running: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn start_maintenance(&self) {
        let cache = self.clone();
        std::thread::spawn(move || loop {
            cache.cleanup_in_background();
            std::thread::sleep(CACHE_MAINTENANCE_INTERVAL);
        });
    }

    pub(crate) fn thumbnail(
        &self,
        absolute_path: &Path,
        source_version: &str,
        max_edge: u32,
        render: impl FnOnce() -> io::Result<(Vec<u8>, &'static str)>,
    ) -> io::Result<(Vec<u8>, &'static str)> {
        let key = thumbnail_key(absolute_path, source_version, max_edge);
        for (extension, content_type) in [("jpg", "image/jpeg"), ("png", "image/png")] {
            let path = self.directory.join(format!("{key}.{extension}"));
            match fs::read(&path) {
                Ok(bytes) => {
                    touch_if_stale(&path, CACHE_TOUCH_INTERVAL);
                    return Ok((bytes, content_type));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        let (bytes, content_type) = render()?;
        let extension = if content_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        // Publish only complete images when requests generate the same entry.
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&bytes)?;
        file.persist(self.directory.join(format!("{key}.{extension}")))
            .map_err(|error| error.error)?;
        let size = bytes.len() as u64;
        if self.estimated_size.fetch_add(size, Ordering::Relaxed) + size > CACHE_MAX_BYTES {
            self.cleanup_in_background();
        }
        Ok((bytes, content_type))
    }

    fn cleanup_in_background(&self) {
        if self
            .cleanup_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let cache = self.clone();
        std::thread::spawn(move || {
            match prune_directory(
                &cache.directory,
                SystemTime::now(),
                CACHE_MAX_AGE,
                CACHE_MAX_BYTES,
                CACHE_TARGET_BYTES,
            ) {
                Ok(size) => cache.estimated_size.store(size, Ordering::Relaxed),
                Err(error) => eprintln!("清理图片缓存失败：{error}"),
            }
            cache.cleanup_running.store(false, Ordering::Release);
        });
    }
}

fn touch_if_stale(path: &Path, interval: Duration) {
    let Ok(metadata) = path.metadata() else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    if SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        < interval
    {
        return;
    }
    if let Ok(file) = fs::File::open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}

fn prune_directory(
    directory: &Path,
    now: SystemTime,
    max_age: Duration,
    max_bytes: u64,
    target_bytes: u64,
) -> io::Result<u64> {
    let mut cached = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(directory)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();
        let extension = path.extension().and_then(|value| value.to_str());
        if !matches!(extension, Some("jpg" | "png")) {
            if age >= TEMP_FILE_MAX_AGE {
                let _ = fs::remove_file(path);
            }
            continue;
        }
        if age >= max_age && fs::remove_file(&path).is_ok() {
            continue;
        }
        total = total.saturating_add(metadata.len());
        cached.push((modified, metadata.len(), path));
    }
    if total > max_bytes {
        cached.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in cached {
            if total <= target_bytes {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
    Ok(total)
}

pub(crate) fn thumbnail_key(absolute_path: &Path, source_version: &str, max_edge: u32) -> String {
    let path_digest = Sha1::digest(absolute_path.as_os_str().as_encoded_bytes());
    format!("{}-{max_edge}-{}", hex_digest(&path_digest), source_version)
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn write_at(path: &Path, size: usize, modified: SystemTime) {
        fs::write(path, vec![7; size]).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    #[test]
    fn prunes_expired_temporary_and_oldest_cached_images() {
        let root = tempfile::tempdir().unwrap();
        let thumbnails = root.path().join("thumbnails");
        let database = root.path().join("http.sqlite");
        fs::create_dir_all(&thumbnails).unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        write_at(
            &thumbnails.join("expired.jpg"),
            8,
            now - Duration::from_secs(100),
        );
        write_at(
            &thumbnails.join("old.jpg"),
            8,
            now - Duration::from_secs(20),
        );
        write_at(
            &thumbnails.join("middle.png"),
            8,
            now - Duration::from_secs(10),
        );
        write_at(
            &thumbnails.join("recent.jpg"),
            8,
            now - Duration::from_secs(2),
        );
        write_at(&thumbnails.join(".tmp-crash"), 4, now - TEMP_FILE_MAX_AGE);
        write_at(&database, 3, now - Duration::from_secs(100));

        let remaining = prune_directory(&thumbnails, now, Duration::from_secs(30), 15, 10).unwrap();

        assert_eq!(remaining, 8);
        assert!(!thumbnails.join("expired.jpg").exists());
        assert!(!thumbnails.join("old.jpg").exists());
        assert!(!thumbnails.join("middle.png").exists());
        assert!(thumbnails.join("recent.jpg").exists());
        assert!(!thumbnails.join(".tmp-crash").exists());
        assert!(database.exists());
    }

    #[test]
    fn reuses_persisted_thumbnails_with_separate_sizes_and_formats() {
        let directory = tempfile::tempdir().unwrap();
        let cache_root = directory.path().join("cache");
        let cache_path = cache_root.join("thumbnails");
        let cache = ImageCache::new(&cache_root).unwrap();
        let source_path = directory.path().join("photo.png");
        let alpha_path = directory.path().join("alpha.png");
        let mut source = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(800, 200, image::Rgb([20, 40, 60]))
            .write_to(&mut source, image::ImageFormat::Png)
            .unwrap();
        let source = source.into_inner();
        let small = cache
            .thumbnail(&source_path, "source-v1", 128, || {
                super::super::render_image_thumbnail(&source, 128)
            })
            .unwrap();
        let large = cache
            .thumbnail(&source_path, "source-v1", 512, || {
                super::super::render_image_thumbnail(&source, 512)
            })
            .unwrap();
        assert_eq!(image::load_from_memory(&small.0).unwrap().width(), 128);
        assert_eq!(image::load_from_memory(&large.0).unwrap().width(), 512);
        assert_eq!(small.1, "image/jpeg");

        let mut alpha = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(32, 32, image::Rgba([20, 40, 60, 80]))
            .write_to(&mut alpha, image::ImageFormat::Png)
            .unwrap();
        let transparent = cache
            .thumbnail(&alpha_path, "alpha-v1", 128, || {
                super::super::render_image_thumbnail(alpha.get_ref(), 128)
            })
            .unwrap();
        assert_eq!(transparent.1, "image/png");
        assert!(image::load_from_memory(&transparent.0)
            .unwrap()
            .color()
            .has_alpha());

        let old_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        for entry in fs::read_dir(&cache_path).unwrap() {
            fs::File::options()
                .write(true)
                .open(entry.unwrap().path())
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old_time))
                .unwrap();
        }
        drop(cache);
        let cache = ImageCache::new(&cache_root).unwrap();
        assert_eq!(
            cache
                .thumbnail(&source_path, "source-v1", 128, || panic!("cache miss"))
                .unwrap(),
            small
        );
        assert_eq!(
            cache
                .thumbnail(&source_path, "source-v1", 512, || panic!("cache miss"))
                .unwrap(),
            large
        );
        assert_eq!(
            cache
                .thumbnail(&alpha_path, "alpha-v1", 128, || panic!("cache miss"))
                .unwrap(),
            transparent
        );
        let entries: Vec<_> = fs::read_dir(&cache_path).unwrap().collect();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert!(entry.unwrap().metadata().unwrap().modified().unwrap() > old_time);
        }
    }

    #[test]
    fn indexes_identical_images_in_different_directories_separately() {
        let directory = tempfile::tempdir().unwrap();
        let cache_root = directory.path().join("cache");
        let cache_path = cache_root.join("thumbnails");
        let cache = ImageCache::new(&cache_root).unwrap();
        let mut source = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(32, 32, image::Rgb([20, 40, 60]))
            .write_to(&mut source, image::ImageFormat::Png)
            .unwrap();

        let mut cached_files = Vec::new();
        for folder in ["a", "b"] {
            let parent = directory.path().join(folder);
            fs::create_dir(&parent).unwrap();
            let path = parent.join("photo.png");
            fs::write(&path, source.get_ref()).unwrap();
            let path = fs::canonicalize(path).unwrap();
            cache
                .thumbnail(&path, "source-v1", 128, || {
                    super::super::render_image_thumbnail(source.get_ref(), 128)
                })
                .unwrap();
            let files: Vec<_> = fs::read_dir(&cache_path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(files.len(), cached_files.len() + 1);
            assert!(cached_files.iter().all(|path| files.contains(path)));
            cached_files = files;
        }
        assert_eq!(
            fs::read(&cached_files[0]).unwrap(),
            fs::read(&cached_files[1]).unwrap()
        );
    }
}
