use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

#[derive(Clone)]
pub(crate) struct ImageCache {
    directory: PathBuf,
}

impl ImageCache {
    pub(crate) fn new(cache_root: &Path) -> io::Result<Self> {
        let directory = cache_root.join("thumbnails");
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub(crate) fn thumbnail(
        &self,
        absolute_path: &Path,
        source: &[u8],
        max_edge: u32,
    ) -> io::Result<(Vec<u8>, &'static str)> {
        let key = thumbnail_key(absolute_path, source, max_edge);
        for (extension, content_type) in [("jpg", "image/jpeg"), ("png", "image/png")] {
            match fs::read(self.directory.join(format!("{key}.{extension}"))) {
                Ok(bytes) => return Ok((bytes, content_type)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        let (bytes, content_type) = super::render_image_thumbnail(source, max_edge)?;
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
        Ok((bytes, content_type))
    }
}

pub(crate) fn thumbnail_key(absolute_path: &Path, source: &[u8], max_edge: u32) -> String {
    // Index by the canonical absolute source path; the content digest
    // distinguishes replacements even when size and mtime stay the same.
    let path_digest = Sha1::digest(absolute_path.as_os_str().as_encoded_bytes());
    format!(
        "{}-{max_edge}-{}",
        hex_digest(&path_digest),
        hex_digest(&Sha1::digest(source))
    )
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
        let small = cache.thumbnail(&source_path, &source, 128).unwrap();
        let large = cache.thumbnail(&source_path, &source, 512).unwrap();
        assert_eq!(image::load_from_memory(&small.0).unwrap().width(), 128);
        assert_eq!(image::load_from_memory(&large.0).unwrap().width(), 512);
        assert_eq!(small.1, "image/jpeg");

        let mut alpha = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(32, 32, image::Rgba([20, 40, 60, 80]))
            .write_to(&mut alpha, image::ImageFormat::Png)
            .unwrap();
        let transparent = cache.thumbnail(&alpha_path, alpha.get_ref(), 128).unwrap();
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
        assert_eq!(cache.thumbnail(&source_path, &source, 128).unwrap(), small);
        assert_eq!(cache.thumbnail(&source_path, &source, 512).unwrap(), large);
        assert_eq!(
            cache.thumbnail(&alpha_path, alpha.get_ref(), 128).unwrap(),
            transparent
        );
        let entries: Vec<_> = fs::read_dir(&cache_path).unwrap().collect();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert_eq!(
                entry.unwrap().metadata().unwrap().modified().unwrap(),
                old_time
            );
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
            cache.thumbnail(&path, source.get_ref(), 128).unwrap();
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
