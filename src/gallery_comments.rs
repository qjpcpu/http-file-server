use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const COMMENTS_FILE_NAME: &str = ".gallery-comments.json";
static OPERATIONS: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct GalleryComments {
    #[serde(default = "version")]
    pub version: u32,
    #[serde(default)]
    pub comments: Vec<GalleryComment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct GalleryComment {
    pub id: String,
    pub image: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum GalleryCommentAction {
    Add {
        comment: GalleryComment,
    },
    Edit {
        comment_id: String,
        body: String,
        edited_at: String,
    },
    Delete {
        comment_id: String,
    },
    DeleteAll,
}

pub(crate) fn comments_path(image_path: &Path) -> PathBuf {
    image_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(COMMENTS_FILE_NAME)
}

pub(crate) fn is_comments_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == COMMENTS_FILE_NAME)
}

pub(crate) fn read(image_path: &Path) -> io::Result<GalleryComments> {
    let path = comments_path(image_path);
    match fs::read_to_string(&path) {
        Ok(source) => serde_json::from_str(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("无法解析 {}：{error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(GalleryComments {
            version: version(),
            comments: Vec::new(),
        }),
        Err(error) => Err(error),
    }
}

pub(crate) fn apply(
    image_path: &Path,
    action: GalleryCommentAction,
) -> Result<GalleryComments, String> {
    let _operation = OPERATIONS.lock().unwrap();
    let mut document = read(image_path).map_err(|error| error.to_string())?;
    match action {
        GalleryCommentAction::Add { comment } => {
            if document.comments.iter().any(|item| item.id == comment.id) {
                return Err("评论 ID 已存在".into());
            }
            document.comments.push(comment);
        }
        GalleryCommentAction::Edit {
            comment_id,
            body,
            edited_at,
        } => {
            let comment = document
                .comments
                .iter_mut()
                .find(|comment| comment.id == comment_id)
                .ok_or_else(|| "评论不存在".to_string())?;
            comment.body = body;
            comment.edited_at = Some(edited_at);
        }
        GalleryCommentAction::Delete { comment_id } => {
            let index = document
                .comments
                .iter()
                .position(|comment| comment.id == comment_id)
                .ok_or_else(|| "评论不存在".to_string())?;
            document.comments.remove(index);
        }
        GalleryCommentAction::DeleteAll => {
            let image = image_path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            document.comments.retain(|comment| comment.image != image);
        }
    }
    write(image_path, &document).map_err(|error| error.to_string())?;
    Ok(document)
}

fn write(image_path: &Path, document: &GalleryComments) -> io::Result<()> {
    let path = comments_path(image_path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, document)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, image: &str, body: &str) -> GalleryComment {
        GalleryComment {
            id: id.into(),
            image: image.into(),
            author: "Alice".into(),
            body: body.into(),
            created_at: "2026-09-09T10:00:00.000Z".into(),
            edited_at: None,
        }
    }

    #[test]
    fn stores_all_image_comments_in_one_adjacent_file() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.jpg");
        let second = directory.path().join("second.png");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        apply(
            &first,
            GalleryCommentAction::Add {
                comment: comment("one", "first.jpg", "调整天空"),
            },
        )
        .unwrap();
        apply(
            &second,
            GalleryCommentAction::Add {
                comment: comment("two", "second.png", "降低饱和度"),
            },
        )
        .unwrap();

        let document = read(&first).unwrap();
        assert_eq!(document.comments.len(), 2);
        assert_eq!(read(&second).unwrap().comments.len(), 2);
        assert_eq!(
            comments_path(&first),
            directory.path().join(COMMENTS_FILE_NAME)
        );
    }

    #[test]
    fn edits_and_deletes_comments() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image.jpg");
        fs::write(&image, b"image").unwrap();
        apply(
            &image,
            GalleryCommentAction::Add {
                comment: comment("one", "image.jpg", "旧评论"),
            },
        )
        .unwrap();
        let edited = apply(
            &image,
            GalleryCommentAction::Edit {
                comment_id: "one".into(),
                body: "新评论".into(),
                edited_at: "2026-09-09T11:00:00.000Z".into(),
            },
        )
        .unwrap();
        assert_eq!(edited.comments[0].body, "新评论");
        assert!(edited.comments[0].edited_at.is_some());

        let deleted = apply(
            &image,
            GalleryCommentAction::Delete {
                comment_id: "one".into(),
            },
        )
        .unwrap();
        assert!(deleted.comments.is_empty());
    }

    #[test]
    fn deletes_only_the_current_images_comments() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.jpg");
        let second = directory.path().join("second.jpg");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        apply(
            &first,
            GalleryCommentAction::Add {
                comment: comment("one", "first.jpg", "第一条"),
            },
        )
        .unwrap();
        apply(
            &second,
            GalleryCommentAction::Add {
                comment: comment("two", "first.jpg", "第二条"),
            },
        )
        .unwrap();
        apply(
            &second,
            GalleryCommentAction::Add {
                comment: comment("three", "second.jpg", "保留"),
            },
        )
        .unwrap();

        let document = apply(&first, GalleryCommentAction::DeleteAll).unwrap();

        assert_eq!(document.comments.len(), 1);
        assert_eq!(document.comments[0].id, "three");
    }
}
