use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tungstenite::error::Error as WebSocketError;
use tungstenite::protocol::{Message, Role, WebSocket, WebSocketConfig};

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WRITE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewDocument {
    #[serde(default = "review_version")]
    pub version: u32,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for ReviewDocument {
    fn default() -> Self {
        Self {
            version: review_version(),
            comments: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewComment {
    pub id: String,
    pub scope: Value,
    pub status: String,
    #[serde(default)]
    pub messages: Vec<ReviewMessage>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReviewMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub author: String,
    pub body: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ReviewAction {
    AddComment {
        comment: ReviewComment,
    },
    AddMessage {
        comment_id: String,
        message: ReviewMessage,
        #[serde(default)]
        status: Option<String>,
    },
    SetStatus {
        comment_id: String,
        status: String,
    },
    EditMessage {
        comment_id: String,
        #[serde(default)]
        message_id: Option<String>,
        message_index: usize,
        body: String,
        edited_at: String,
    },
    DeleteMessage {
        comment_id: String,
        #[serde(default)]
        message_id: Option<String>,
        message_index: usize,
    },
    DeleteComment {
        comment_id: String,
    },
}

#[derive(Clone, Default)]
pub struct ReviewHub {
    inner: Arc<ReviewHubInner>,
}

#[derive(Default)]
struct ReviewHubInner {
    clients: Mutex<HashMap<PathBuf, HashMap<u64, ReviewClient>>>,
    operations: Mutex<()>,
}

struct ReviewClient {
    user: Option<String>,
    sender: mpsc::Sender<String>,
}

pub struct ReviewConnection {
    hub: ReviewHub,
    path: PathBuf,
    client_id: u64,
    receiver: mpsc::Receiver<String>,
}

#[derive(Deserialize)]
struct ReviewControl {
    #[serde(rename = "type")]
    kind: String,
    user: Option<String>,
}

fn review_version() -> u32 {
    1
}

pub fn sidecar_path(markdown_path: &Path) -> PathBuf {
    let mut name = markdown_path.file_name().unwrap_or_default().to_os_string();
    name.push(".review.json");
    markdown_path.with_file_name(name)
}

pub fn is_review_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".md.review.json"))
}

pub fn read_review(markdown_path: &Path) -> io::Result<ReviewDocument> {
    let path = sidecar_path(markdown_path);
    match fs::read_to_string(&path) {
        Ok(source) => serde_json::from_str(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("无法解析 {}：{error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ReviewDocument::default()),
        Err(error) => Err(error),
    }
}

impl ReviewHub {
    pub fn connect(&self, path: &Path) -> ReviewConnection {
        let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.inner
            .clients
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .insert(client_id, ReviewClient { user: None, sender });
        self.broadcast_presence(path);
        ReviewConnection {
            hub: self.clone(),
            path: path.to_owned(),
            client_id,
            receiver,
        }
    }

    pub fn apply_action(
        &self,
        markdown_path: &Path,
        action: ReviewAction,
    ) -> Result<ReviewDocument, String> {
        let _operation = self.inner.operations.lock().unwrap();
        let mut review = read_review(markdown_path).map_err(|error| error.to_string())?;
        match action {
            ReviewAction::AddComment { comment } => {
                if review.comments.iter().any(|item| item.id == comment.id) {
                    return Err("评论 ID 已存在".into());
                }
                review.comments.push(comment);
            }
            ReviewAction::AddMessage {
                comment_id,
                message,
                status,
            } => {
                let comment = review
                    .comments
                    .iter_mut()
                    .find(|comment| comment.id == comment_id)
                    .ok_or_else(|| "评论不存在".to_string())?;
                comment.messages.push(message);
                if let Some(status) = status {
                    comment.status = checked_status(status)?;
                }
            }
            ReviewAction::SetStatus { comment_id, status } => {
                let comment = review
                    .comments
                    .iter_mut()
                    .find(|comment| comment.id == comment_id)
                    .ok_or_else(|| "评论不存在".to_string())?;
                comment.status = checked_status(status)?;
            }
            ReviewAction::EditMessage {
                comment_id,
                message_id,
                message_index,
                body,
                edited_at,
            } => {
                let comment = review
                    .comments
                    .iter_mut()
                    .find(|comment| comment.id == comment_id)
                    .ok_or_else(|| "评论不存在".to_string())?;
                let index = find_message_index(comment, message_id.as_deref(), message_index)?;
                comment.messages[index].body = body;
                comment.messages[index].edited_at = Some(edited_at);
            }
            ReviewAction::DeleteMessage {
                comment_id,
                message_id,
                message_index,
            } => {
                let comment_index = review
                    .comments
                    .iter()
                    .position(|comment| comment.id == comment_id)
                    .ok_or_else(|| "评论不存在".to_string())?;
                let message_index = find_message_index(
                    &review.comments[comment_index],
                    message_id.as_deref(),
                    message_index,
                )?;
                review.comments[comment_index]
                    .messages
                    .remove(message_index);
                if review.comments[comment_index].messages.is_empty() {
                    review.comments.remove(comment_index);
                }
            }
            ReviewAction::DeleteComment { comment_id } => {
                let comment_index = review
                    .comments
                    .iter()
                    .position(|comment| comment.id == comment_id)
                    .ok_or_else(|| "评论不存在".to_string())?;
                review.comments.remove(comment_index);
            }
        }
        write_review(markdown_path, &review).map_err(|error| error.to_string())?;
        drop(_operation);
        self.broadcast(markdown_path, r#"{"type":"review-updated"}"#.into());
        Ok(review)
    }

    fn set_user(&self, path: &Path, client_id: u64, user: String) {
        if let Some(client) = self
            .inner
            .clients
            .lock()
            .unwrap()
            .get_mut(path)
            .and_then(|clients| clients.get_mut(&client_id))
        {
            client.user = Some(user);
        }
        self.broadcast_presence(path);
    }

    fn disconnect(&self, path: &Path, client_id: u64) {
        {
            let mut rooms = self.inner.clients.lock().unwrap();
            if let Some(clients) = rooms.get_mut(path) {
                clients.remove(&client_id);
                if clients.is_empty() {
                    rooms.remove(path);
                }
            }
        }
        self.broadcast_presence(path);
    }

    fn send_to(&self, path: &Path, client_id: u64, message: String) {
        let sender = self
            .inner
            .clients
            .lock()
            .unwrap()
            .get(path)
            .and_then(|clients| clients.get(&client_id))
            .map(|client| client.sender.clone());
        if let Some(sender) = sender {
            let _ = sender.send(message);
        }
    }

    fn broadcast(&self, path: &Path, message: String) {
        let recipients = self
            .inner
            .clients
            .lock()
            .unwrap()
            .get(path)
            .map(|clients| {
                clients
                    .values()
                    .map(|client| client.sender.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for recipient in recipients {
            let _ = recipient.send(message.clone());
        }
    }

    fn broadcast_presence(&self, path: &Path) {
        let users = self
            .inner
            .clients
            .lock()
            .unwrap()
            .get(path)
            .map(|clients| {
                clients
                    .values()
                    .filter_map(|client| client.user.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let message = serde_json::json!({"type": "review-presence", "users": users}).to_string();
        self.broadcast(path, message);
    }
}

impl ReviewConnection {
    pub fn run(
        self,
        stream: TcpStream,
        partially_read: Vec<u8>,
        max_message_size: usize,
    ) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_millis(75)))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message_size))
            .max_frame_size(Some(max_message_size));
        let mut socket =
            WebSocket::from_partially_read(stream, partially_read, Role::Server, Some(config));

        loop {
            while let Ok(message) = self.receiver.try_recv() {
                if socket.send(Message::Text(message.into())).is_err() {
                    return Ok(());
                }
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let control = serde_json::from_str::<ReviewControl>(&text);
                    match control {
                        Ok(control) if control.kind == "join" => {
                            if let Some(user) = control.user.filter(|user| !user.trim().is_empty())
                            {
                                self.hub.set_user(&self.path, self.client_id, user);
                            }
                        }
                        _ => self.hub.send_to(
                            &self.path,
                            self.client_id,
                            serde_json::json!({
                                "type": "review-error",
                                "message": "未知的审阅消息"
                            })
                            .to_string(),
                        ),
                    }
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(WebSocketError::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => {
                    return Ok(())
                }
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        error.to_string(),
                    ))
                }
            }
        }
    }
}

impl Drop for ReviewConnection {
    fn drop(&mut self) {
        self.hub.disconnect(&self.path, self.client_id);
    }
}

fn checked_status(status: String) -> Result<String, String> {
    if matches!(status.as_str(), "open" | "addressed" | "resolved") {
        Ok(status)
    } else {
        Err("未知的评论状态".into())
    }
}

fn find_message_index(
    comment: &ReviewComment,
    message_id: Option<&str>,
    fallback_index: usize,
) -> Result<usize, String> {
    if let Some(message_id) = message_id {
        return comment
            .messages
            .iter()
            .position(|message| message.id.as_deref() == Some(message_id))
            .ok_or_else(|| "消息不存在".to_string());
    }
    comment
        .messages
        .get(fallback_index)
        .map(|_| fallback_index)
        .ok_or_else(|| "消息不存在".to_string())
}

fn write_review(markdown_path: &Path, review: &ReviewDocument) -> io::Result<()> {
    let path = sidecar_path(markdown_path);
    let mut content = serde_json::to_vec_pretty(review)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    content.push(b'\n');
    atomic_write(&path, &content)
}

fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("review");
    let write_id = NEXT_WRITE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.http-file-server-{}-{write_id}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(author: &str, body: &str) -> ReviewMessage {
        ReviewMessage {
            id: Some(format!("message-{body}")),
            author: author.into(),
            body: body.into(),
            created_at: "2026-09-04T10:00:00.000Z".into(),
            edited_at: None,
            extra: BTreeMap::new(),
        }
    }

    fn comment(id: &str) -> ReviewComment {
        ReviewComment {
            id: id.into(),
            scope: serde_json::json!({"type": "document"}),
            status: "open".into(),
            messages: vec![message("alice@laptop", "补充失败策略")],
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn uses_an_adjacent_review_sidecar() {
        assert_eq!(
            sidecar_path(Path::new("docs/spec.md")),
            Path::new("docs/spec.md.review.json")
        );
        assert!(is_review_sidecar(Path::new("SPEC.MD.REVIEW.JSON")));
        assert!(!is_review_sidecar(Path::new("review.json")));
    }

    #[test]
    fn applies_sequential_actions_without_losing_comments() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        let hub = ReviewHub::default();

        hub.apply_action(
            &markdown,
            ReviewAction::AddComment {
                comment: comment("one"),
            },
        )
        .unwrap();
        hub.apply_action(
            &markdown,
            ReviewAction::AddComment {
                comment: comment("two"),
            },
        )
        .unwrap();
        hub.apply_action(
            &markdown,
            ReviewAction::AddMessage {
                comment_id: "one".into(),
                message: message("codex@workstation", "已补充"),
                status: Some("addressed".into()),
            },
        )
        .unwrap();

        let review = read_review(&markdown).unwrap();
        assert_eq!(review.comments.len(), 2);
        assert_eq!(review.comments[0].messages.len(), 2);
        assert_eq!(review.comments[0].status, "addressed");
    }

    #[test]
    fn concurrent_actions_are_serialized_and_broadcast() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        let hub = ReviewHub::default();
        let connection = hub.connect(&markdown);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for id in ["one", "two"] {
            let hub = hub.clone();
            let markdown = markdown.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                hub.apply_action(
                    &markdown,
                    ReviewAction::AddComment {
                        comment: comment(id),
                    },
                )
                .unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let review = read_review(&markdown).unwrap();
        assert_eq!(review.comments.len(), 2);
        let updates = connection.receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(
            updates
                .iter()
                .filter(|message| message.contains("review-updated"))
                .count(),
            2
        );
    }

    #[test]
    fn presence_uses_unique_user_names() {
        let hub = ReviewHub::default();
        let path = Path::new("spec.md");
        let first = hub.connect(path);
        let second = hub.connect(path);

        hub.set_user(path, first.client_id, "alice@laptop".into());
        hub.set_user(path, second.client_id, "alice@laptop".into());

        let messages = first.receiver.try_iter().collect::<Vec<_>>();
        let presence: Value = serde_json::from_str(messages.last().unwrap()).unwrap();
        assert_eq!(presence["users"], serde_json::json!(["alice@laptop"]));
    }

    #[test]
    fn preserves_unknown_fields_when_an_action_is_applied() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        fs::write(
            sidecar_path(&markdown),
            r#"{"version":1,"workflow":"draft","comments":[{"id":"one","scope":{"type":"document"},"status":"open","messages":[],"agent_note":"keep"}]}"#,
        )
        .unwrap();
        let hub = ReviewHub::default();

        hub.apply_action(
            &markdown,
            ReviewAction::SetStatus {
                comment_id: "one".into(),
                status: "resolved".into(),
            },
        )
        .unwrap();

        let value: Value =
            serde_json::from_str(&fs::read_to_string(sidecar_path(&markdown)).unwrap()).unwrap();
        assert_eq!(value["workflow"], "draft");
        assert_eq!(value["comments"][0]["agent_note"], "keep");
    }

    #[test]
    fn edits_and_deletes_thread_messages() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        let hub = ReviewHub::default();
        hub.apply_action(
            &markdown,
            ReviewAction::AddComment {
                comment: comment("one"),
            },
        )
        .unwrap();

        let review = hub
            .apply_action(
                &markdown,
                ReviewAction::EditMessage {
                    comment_id: "one".into(),
                    message_id: Some("message-补充失败策略".into()),
                    message_index: 0,
                    body: "补充超时后的失败策略".into(),
                    edited_at: "2026-09-04T11:00:00.000Z".into(),
                },
            )
            .unwrap();
        assert_eq!(review.comments[0].messages[0].body, "补充超时后的失败策略");
        assert!(review.comments[0].messages[0].edited_at.is_some());

        let review = hub
            .apply_action(
                &markdown,
                ReviewAction::DeleteMessage {
                    comment_id: "one".into(),
                    message_id: Some("message-补充失败策略".into()),
                    message_index: 0,
                },
            )
            .unwrap();
        assert!(review.comments.is_empty());
    }

    #[test]
    fn legacy_messages_without_ids_can_be_changed_by_index() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        fs::write(
            sidecar_path(&markdown),
            r#"{"version":1,"comments":[{"id":"one","scope":{"type":"document"},"status":"open","messages":[{"author":"Alice","body":"旧消息","created_at":"2026-09-04T08:00:00.000Z"}]}]}"#,
        )
        .unwrap();
        let hub = ReviewHub::default();

        let review = hub
            .apply_action(
                &markdown,
                ReviewAction::EditMessage {
                    comment_id: "one".into(),
                    message_id: None,
                    message_index: 0,
                    body: "修改后的旧消息".into(),
                    edited_at: "2026-09-04T11:00:00.000Z".into(),
                },
            )
            .unwrap();

        assert_eq!(review.comments[0].messages[0].body, "修改后的旧消息");
    }

    #[test]
    fn deletes_an_entire_comment_thread() {
        let directory = tempfile::tempdir().unwrap();
        let markdown = directory.path().join("spec.md");
        fs::write(&markdown, "# Spec").unwrap();
        let hub = ReviewHub::default();
        hub.apply_action(
            &markdown,
            ReviewAction::AddComment {
                comment: comment("one"),
            },
        )
        .unwrap();
        hub.apply_action(
            &markdown,
            ReviewAction::AddComment {
                comment: comment("two"),
            },
        )
        .unwrap();

        let review = hub
            .apply_action(
                &markdown,
                ReviewAction::DeleteComment {
                    comment_id: "one".into(),
                },
            )
            .unwrap();

        assert_eq!(review.comments.len(), 1);
        assert_eq!(review.comments[0].id, "two");
    }
}
