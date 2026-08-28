use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use serde::Deserialize;
use tungstenite::error::Error as WebSocketError;
use tungstenite::protocol::{Message, Role, WebSocket, WebSocketConfig};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

const TEXT_NAME: &str = "source";
const SYNC_STATE_VECTOR: u8 = 0;
const SYNC_UPDATE: u8 = 1;
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone, Default)]
pub struct CollaborationHub {
    registry: Arc<RoomRegistry>,
}

#[derive(Default)]
struct RoomRegistry {
    rooms: Mutex<HashMap<PathBuf, Arc<Room>>>,
}

impl CollaborationHub {
    pub fn connect(&self, path: &Path, initial: &str) -> io::Result<RoomConnection> {
        let mut rooms = self.registry.rooms.lock().unwrap();
        let room = match rooms.get(path) {
            Some(room) => room.clone(),
            None => {
                let room = Room::new(path.to_owned(), initial, Arc::downgrade(&self.registry))?;
                rooms.insert(path.to_owned(), room.clone());
                room
            }
        };
        Ok(room.connect())
    }

    pub fn replace_if_active(&self, path: &Path, content: &str) -> io::Result<bool> {
        let room = self.active_room(path);
        if let Some(room) = room {
            room.replace_text(content)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn active_room(&self, path: &Path) -> Option<Arc<Room>> {
        self.registry.rooms.lock().unwrap().get(path).cloned()
    }
}

enum Outbound {
    Binary(Vec<u8>),
    Text(String),
}

enum SaveCommand {
    Dirty,
    Flush(mpsc::Sender<io::Result<()>>),
}

struct Room {
    path: PathBuf,
    registry: Weak<RoomRegistry>,
    inner: Mutex<RoomState>,
    save_tx: mpsc::Sender<SaveCommand>,
}

struct RoomState {
    doc: Doc,
    clients: HashMap<u64, mpsc::Sender<Outbound>>,
    next_client_id: u64,
    generation: u64,
    saved_generation: u64,
}

pub struct RoomConnection {
    room: Arc<Room>,
    client_id: u64,
    receiver: mpsc::Receiver<Outbound>,
}

#[derive(Deserialize)]
struct ClientControl {
    #[serde(rename = "type")]
    kind: String,
}

impl Room {
    fn new(path: PathBuf, initial: &str, registry: Weak<RoomRegistry>) -> io::Result<Arc<Self>> {
        let doc = Doc::new();
        let text = doc.get_or_insert_text(TEXT_NAME);
        if !initial.is_empty() {
            text.insert(&mut doc.transact_mut(), 0, initial);
        }
        let (save_tx, save_rx) = mpsc::channel();
        let room = Arc::new(Self {
            path,
            registry,
            inner: Mutex::new(RoomState {
                doc,
                clients: HashMap::new(),
                next_client_id: 1,
                generation: 0,
                saved_generation: 0,
            }),
            save_tx,
        });
        let weak = Arc::downgrade(&room);
        std::thread::spawn(move || save_worker(weak, save_rx));
        Ok(room)
    }

    fn connect(self: &Arc<Self>) -> RoomConnection {
        let (sender, receiver) = mpsc::channel();
        let (client_id, state_vector) = {
            let mut state = self.inner.lock().unwrap();
            let client_id = state.next_client_id;
            state.next_client_id += 1;
            state.clients.insert(client_id, sender);
            let vector = state.doc.transact().state_vector().encode_v1();
            (client_id, vector)
        };
        self.send_to(
            client_id,
            Outbound::Binary(frame(SYNC_STATE_VECTOR, &state_vector)),
        );
        self.broadcast_presence();
        RoomConnection {
            room: self.clone(),
            client_id,
            receiver,
        }
    }

    fn disconnect(&self, client_id: u64) {
        let empty = {
            let mut state = self.inner.lock().unwrap();
            state.clients.remove(&client_id);
            state.clients.is_empty()
        };
        self.broadcast_presence();
        if empty && self.flush().is_ok() {
            self.remove_if_idle();
        }
    }

    fn remove_if_idle(&self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut rooms = registry.rooms.lock().unwrap();
        let idle = self.inner.lock().unwrap().clients.is_empty();
        let is_current = rooms
            .get(&self.path)
            .is_some_and(|room| std::ptr::eq(Arc::as_ptr(room), self));
        if idle && is_current {
            rooms.remove(&self.path);
        }
    }

    fn handle_binary(&self, client_id: u64, payload: &[u8], max_document_size: usize) {
        let Some((&kind, body)) = payload.split_first() else {
            self.send_error(client_id, "空的协作消息");
            return;
        };
        match kind {
            SYNC_STATE_VECTOR => {
                let vector = match StateVector::decode_v1(body) {
                    Ok(vector) => vector,
                    Err(_) => {
                        self.send_error(client_id, "无效的状态向量");
                        return;
                    }
                };
                let update = {
                    let state = self.inner.lock().unwrap();
                    let update = state.doc.transact().encode_state_as_update_v1(&vector);
                    update
                };
                self.send_to(client_id, Outbound::Binary(frame(SYNC_UPDATE, &update)));
            }
            SYNC_UPDATE if body.len() >= 4 => {
                let sequence = u32::from_be_bytes(body[..4].try_into().unwrap());
                let update_bytes = &body[4..];
                let update = match Update::decode_v1(update_bytes) {
                    Ok(update) => update,
                    Err(_) => {
                        self.send_error(client_id, "无效的协作更新");
                        return;
                    }
                };
                let result = self.apply_update(update, update_bytes, client_id, max_document_size);
                match result {
                    Ok(generation) => self.send_to(
                        client_id,
                        Outbound::Text(format!(
                            "{{\"type\":\"ack\",\"sequence\":{sequence},\"generation\":{generation}}}"
                        )),
                    ),
                    Err(error) => self.send_error(client_id, &error),
                }
            }
            _ => self.send_error(client_id, "未知的协作消息"),
        }
    }

    fn handle_text(&self, client_id: u64, payload: &str) {
        match serde_json::from_str::<ClientControl>(payload) {
            Ok(control) if control.kind == "save" => {
                if let Err(error) = self.flush() {
                    self.send_error(client_id, &format!("保存失败：{error}"));
                }
            }
            _ => self.send_error(client_id, "未知的控制消息"),
        }
    }

    fn apply_update(
        &self,
        update: Update,
        encoded: &[u8],
        sender_id: u64,
        max_document_size: usize,
    ) -> Result<u64, String> {
        let (generation, recipients) = {
            let mut state = self.inner.lock().unwrap();
            let backup = state
                .doc
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            state
                .doc
                .transact_mut()
                .apply_update(update)
                .map_err(|_| "无法应用协作更新".to_string())?;
            if document_text(&state.doc).len() > max_document_size {
                state.doc =
                    document_from_update(&backup).map_err(|_| "协作文档恢复失败".to_string())?;
                return Err("Markdown 内容不能超过 16 MB".into());
            }
            state.generation += 1;
            let recipients = state
                .clients
                .iter()
                .filter(|(id, _)| **id != sender_id)
                .map(|(_, sender)| sender.clone())
                .collect::<Vec<_>>();
            (state.generation, recipients)
        };
        let message = Outbound::Binary(frame(SYNC_UPDATE, encoded));
        broadcast_to(recipients, message);
        let _ = self.save_tx.send(SaveCommand::Dirty);
        Ok(generation)
    }

    fn replace_text(&self, content: &str) -> io::Result<()> {
        let (update, recipients) = {
            let mut state = self.inner.lock().unwrap();
            let text = state.doc.get_or_insert_text(TEXT_NAME);
            let mut txn = state.doc.transact_mut();
            let length = text.len(&txn);
            if length > 0 {
                text.remove_range(&mut txn, 0, length);
            }
            if !content.is_empty() {
                text.insert(&mut txn, 0, content);
            }
            let update = txn.encode_update_v1();
            drop(txn);
            state.generation += 1;
            let recipients = state.clients.values().cloned().collect::<Vec<_>>();
            (update, recipients)
        };
        broadcast_to(recipients, Outbound::Binary(frame(SYNC_UPDATE, &update)));
        let _ = self.save_tx.send(SaveCommand::Dirty);
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        let (sender, receiver) = mpsc::channel();
        self.save_tx
            .send(SaveCommand::Flush(sender))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "保存线程已停止"))?;
        receiver
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "保存线程未响应"))?
    }

    fn save(&self) -> io::Result<()> {
        let (content, generation, already_saved) = {
            let state = self.inner.lock().unwrap();
            (
                document_text(&state.doc),
                state.generation,
                state.saved_generation == state.generation,
            )
        };
        if already_saved {
            self.broadcast_saved(generation);
            return Ok(());
        }
        atomic_write(&self.path, content.as_bytes(), generation)?;
        {
            let mut state = self.inner.lock().unwrap();
            state.saved_generation = state.saved_generation.max(generation);
        }
        self.broadcast_saved(generation);
        Ok(())
    }

    fn send_to(&self, client_id: u64, message: Outbound) {
        let sender = self.inner.lock().unwrap().clients.get(&client_id).cloned();
        if let Some(sender) = sender {
            let _ = sender.send(message);
        }
    }

    fn send_error(&self, client_id: u64, error: &str) {
        let message = serde_json::json!({"type": "error", "message": error}).to_string();
        self.send_to(client_id, Outbound::Text(message));
    }

    fn broadcast_presence(&self) {
        let (count, recipients) = {
            let state = self.inner.lock().unwrap();
            (
                state.clients.len(),
                state.clients.values().cloned().collect(),
            )
        };
        broadcast_to(
            recipients,
            Outbound::Text(format!("{{\"type\":\"presence\",\"count\":{count}}}")),
        );
    }

    fn broadcast_saved(&self, generation: u64) {
        let recipients = self
            .inner
            .lock()
            .unwrap()
            .clients
            .values()
            .cloned()
            .collect();
        broadcast_to(
            recipients,
            Outbound::Text(format!(
                "{{\"type\":\"saved\",\"generation\":{generation}}}"
            )),
        );
    }

    fn broadcast_save_error(&self, error: &io::Error) {
        let recipients = self
            .inner
            .lock()
            .unwrap()
            .clients
            .values()
            .cloned()
            .collect();
        let message = serde_json::json!({
            "type": "error",
            "message": format!("自动保存失败：{error}")
        })
        .to_string();
        broadcast_to(recipients, Outbound::Text(message));
    }
}

impl Drop for RoomConnection {
    fn drop(&mut self) {
        self.room.disconnect(self.client_id);
    }
}

impl RoomConnection {
    pub fn run(
        self,
        stream: TcpStream,
        partially_read: Vec<u8>,
        max_message_size: usize,
        max_document_size: usize,
    ) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_millis(75)))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_message_size))
            .max_frame_size(Some(max_message_size));
        let mut socket =
            WebSocket::from_partially_read(stream, partially_read, Role::Server, Some(config));

        loop {
            while let Ok(message) = self.receiver.try_recv() {
                let result = match message {
                    Outbound::Binary(bytes) => socket.send(Message::Binary(bytes.into())),
                    Outbound::Text(text) => socket.send(Message::Text(text.into())),
                };
                if result.is_err() {
                    return Ok(());
                }
            }

            match socket.read() {
                Ok(Message::Binary(bytes)) => {
                    self.room
                        .handle_binary(self.client_id, &bytes, max_document_size);
                }
                Ok(Message::Text(text)) => self.room.handle_text(self.client_id, &text),
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

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 1);
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

fn broadcast_to(recipients: Vec<mpsc::Sender<Outbound>>, message: Outbound) {
    for recipient in recipients {
        let copy = match &message {
            Outbound::Binary(bytes) => Outbound::Binary(bytes.clone()),
            Outbound::Text(text) => Outbound::Text(text.clone()),
        };
        let _ = recipient.send(copy);
    }
}

fn document_text(doc: &Doc) -> String {
    let txn = doc.transact();
    txn.get_text(TEXT_NAME)
        .map(|text| text.get_string(&txn))
        .unwrap_or_default()
}

fn document_from_update(bytes: &[u8]) -> Result<Doc, ()> {
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).map_err(|_| ())?)
        .map_err(|_| ())?;
    Ok(doc)
}

fn save_worker(room: Weak<Room>, receiver: mpsc::Receiver<SaveCommand>) {
    while let Ok(command) = receiver.recv() {
        match command {
            SaveCommand::Flush(reply) => {
                let result = room.upgrade().map_or_else(
                    || Err(io::Error::new(io::ErrorKind::BrokenPipe, "协作房间已关闭")),
                    |room| room.save(),
                );
                let _ = reply.send(result);
            }
            SaveCommand::Dirty => loop {
                match receiver.recv_timeout(SAVE_DEBOUNCE) {
                    Ok(SaveCommand::Dirty) => continue,
                    Ok(SaveCommand::Flush(reply)) => {
                        let result = room.upgrade().map_or_else(
                            || Err(io::Error::new(io::ErrorKind::BrokenPipe, "协作房间已关闭")),
                            |room| room.save(),
                        );
                        let _ = reply.send(result);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(room) = room.upgrade() {
                            if let Err(error) = room.save() {
                                room.broadcast_save_error(&error);
                            }
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            },
        }
    }
}

fn atomic_write(path: &Path, content: &[u8], generation: u64) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("markdown");
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut last_error = None;

    for attempt in 0..16_u8 {
        let temporary = parent.join(format!(
            ".{name}.http-file-server-{}-{generation}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(mut file) => {
                if let Some(permissions) = permissions.clone() {
                    file.set_permissions(permissions)?;
                }
                if let Err(error) = file.write_all(content).and_then(|_| file.sync_all()) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
                if let Err(error) = fs::rename(&temporary, path) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "无法创建临时文件")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn update_from(doc: &Doc, vector: &StateVector) -> Vec<u8> {
        doc.transact().encode_state_as_update_v1(vector)
    }

    #[test]
    fn concurrent_updates_converge() {
        let left = Doc::with_client_id(1);
        let right = Doc::with_client_id(2);
        let left_text = left.get_or_insert_text(TEXT_NAME);
        left_text.insert(&mut left.transact_mut(), 0, "start");
        let initial = update_from(&left, &StateVector::default());
        right
            .transact_mut()
            .apply_update(Update::decode_v1(&initial).unwrap())
            .unwrap();

        let common = left.transact().state_vector();
        left_text.insert(&mut left.transact_mut(), 5, " left");
        right
            .get_or_insert_text(TEXT_NAME)
            .insert(&mut right.transact_mut(), 0, "right ");
        let left_update = update_from(&left, &common);
        let right_update = update_from(&right, &common);
        left.transact_mut()
            .apply_update(Update::decode_v1(&right_update).unwrap())
            .unwrap();
        right
            .transact_mut()
            .apply_update(Update::decode_v1(&left_update).unwrap())
            .unwrap();

        assert_eq!(document_text(&left), document_text(&right));
        assert!(document_text(&left).contains("left"));
        assert!(document_text(&left).contains("right"));
    }

    #[test]
    fn active_room_replaces_and_persists_text() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("notes.md");
        fs::write(&path, "old").unwrap();
        let hub = CollaborationHub::default();
        let connection = hub.connect(&path, "old").unwrap();

        assert!(hub.replace_if_active(&path, "new value").unwrap());
        connection.room.flush().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "new value");
    }
}
