use crate::error::{CoreError, CoreResult};
use crate::storage::EncryptedDatabase;
use dashmap::DashMap;
use loro::{ExportMode, LoroDoc, LoroMap, LoroValue, ToJson};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Conflict-free replicated chat message stored inside a `LoroDoc` CRDT state tree
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrdtChatMessage {
    pub id: String,
    pub room_id: String,
    pub sender_peer_id: String,
    pub text: String,
    pub timestamp_ms: i64,
    pub edited_at_ms: Option<i64>,
    pub deleted: bool,
    pub reply_to_id: Option<String>,
    pub forwarded_from_peer: Option<String>,
    /// `"text"`, `"voice"`, `"video_circle"`, `"file"`
    pub attachment_kind: String,
    pub media_blake3_hash: Option<String>,
    pub media_mime: Option<String>,
    pub media_size_bytes: Option<u64>,
    pub waveform: Vec<u8>,
    /// Map of `emoji -> Vec<peer_id>`
    pub reactions: BTreeMap<String, Vec<String>>,
    /// Merkle-DAG signature hex for broadcast channels
    pub channel_signature_hex: Option<String>,
}

/// Reaction mutation event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionUpdate {
    pub room_id: String,
    pub message_id: String,
    pub emoji: String,
    pub peer_id: String,
    pub added: bool,
}

/// Per-room CRDT document wrapper backed by `loro::LoroDoc`
pub struct CrdtChatDocument {
    room_id: String,
    doc: RwLock<LoroDoc>,
}

impl CrdtChatDocument {
    /// Creates a new CRDT document for `room_id`, optionally importing an existing binary snapshot
    pub fn new(room_id: impl Into<String>, initial_snapshot: Option<&[u8]>) -> CoreResult<Self> {
        let doc = LoroDoc::new();
        if let Some(bytes) = initial_snapshot {
            if !bytes.is_empty() {
                doc.import(bytes)
                    .map_err(|e| CoreError::Serialization(format!("Loro CRDT import error: {e}")))?;
            }
        }
        Ok(Self {
            room_id: room_id.into(),
            doc: RwLock::new(doc),
        })
    }

    /// Inserts or updates a `CrdtChatMessage` in the CRDT map and returns a binary delta
    /// ready to be broadcast over Iroh QUIC / GossipSub to other peers.
    pub fn upsert_message(&self, msg: &CrdtChatMessage) -> CoreResult<Vec<u8>> {
        let doc = self.doc.write();
        let start_vv = doc.oplog_vv();

        let messages_map: LoroMap = doc.get_map("messages");
        let json_str =
            serde_json::to_string(msg).map_err(|e| CoreError::Serialization(e.to_string()))?;
        messages_map
            .insert(&msg.id, LoroValue::from(json_str))
            .map_err(|e| CoreError::Serialization(format!("Loro map insert error: {e}")))?;
        doc.commit();

        doc.export(ExportMode::updates(&start_vv))
            .map_err(|e| CoreError::Serialization(format!("Loro delta export failed: {e}")))
    }

    /// Edits the text of an existing message conflict-free and returns the CRDT delta bytes
    pub fn edit_message_text(
        &self,
        message_id: &str,
        new_text: &str,
        edited_at_ms: i64,
    ) -> CoreResult<Option<Vec<u8>>> {
        let Some(mut msg) = self.get_message(message_id)? else {
            return Ok(None);
        };
        msg.text = new_text.to_string();
        msg.edited_at_ms = Some(edited_at_ms);
        let delta = self.upsert_message(&msg)?;
        Ok(Some(delta))
    }

    /// Tombstones (soft-deletes) a message across all peers via CRDT and returns the delta
    pub fn delete_message(&self, message_id: &str) -> CoreResult<Option<Vec<u8>>> {
        let Some(mut msg) = self.get_message(message_id)? else {
            return Ok(None);
        };
        msg.deleted = true;
        msg.text.clear();
        msg.media_blake3_hash = None;
        let delta = self.upsert_message(&msg)?;
        Ok(Some(delta))
    }

    /// Toggles an emoji reaction for `peer_id` on `message_id` and returns the CRDT delta
    pub fn toggle_reaction(
        &self,
        message_id: &str,
        emoji: &str,
        peer_id: &str,
    ) -> CoreResult<Option<Vec<u8>>> {
        let Some(mut msg) = self.get_message(message_id)? else {
            return Ok(None);
        };

        let peers = msg.reactions.entry(emoji.to_string()).or_default();
        if let Some(pos) = peers.iter().position(|p| p == peer_id) {
            peers.remove(pos);
            if peers.is_empty() {
                msg.reactions.remove(emoji);
            }
        } else {
            peers.push(peer_id.to_string());
        }

        let delta = self.upsert_message(&msg)?;
        Ok(Some(delta))
    }

    /// Merges an incoming remote CRDT update or snapshot from a peer without conflicts
    pub fn merge_remote_bytes(&self, remote_bytes: &[u8]) -> CoreResult<()> {
        if remote_bytes.is_empty() {
            return Ok(());
        }
        let doc = self.doc.write();
        doc.import(remote_bytes)
            .map_err(|e| CoreError::Serialization(format!("CRDT remote merge failed: {e}")))?;
        doc.commit();
        Ok(())
    }

    /// Exports a full compacted CRDT snapshot for local SQLite storage or new-device sync
    pub fn export_full_snapshot(&self) -> CoreResult<Vec<u8>> {
        let doc = self.doc.read();
        doc.export(ExportMode::Snapshot)
            .map_err(|e| CoreError::Serialization(format!("CRDT snapshot export failed: {e}")))
    }

    /// Reads a single message by ID from the CRDT state
    pub fn get_message(&self, message_id: &str) -> CoreResult<Option<CrdtChatMessage>> {
        let doc = self.doc.read();
        let messages_map = doc.get_map("messages");
        let val = messages_map.get_deep_value();
        let json_val = val.to_json_value();

        if let Some(obj) = json_val.as_object() {
            if let Some(raw_str) = obj.get(message_id).and_then(|v| v.as_str()) {
                let parsed: CrdtChatMessage = serde_json::from_str(raw_str)
                    .map_err(|e| CoreError::Serialization(e.to_string()))?;
                return Ok(Some(parsed));
            }
        }
        Ok(None)
    }

    /// Returns all non-deleted messages sorted chronologically by `(timestamp_ms, id)`
    pub fn list_messages(&self) -> CoreResult<Vec<CrdtChatMessage>> {
        let doc = self.doc.read();
        let messages_map = doc.get_map("messages");
        let json_val = messages_map.get_deep_value().to_json_value();

        let mut list = Vec::new();
        if let Some(obj) = json_val.as_object() {
            for (_key, val) in obj {
                if let Some(raw_str) = val.as_str() {
                    if let Ok(msg) = serde_json::from_str::<CrdtChatMessage>(raw_str) {
                        if !msg.deleted {
                            list.push(msg);
                        }
                    }
                }
            }
        }

        list.sort_by(|a, b| {
            a.timestamp_ms
                .cmp(&b.timestamp_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(list)
    }

    pub fn room_id(&self) -> &str {
        &self.room_id
    }
}

/// High-level CRDT synchronization engine coordinating in-memory `LoroDoc` instances
/// with the encrypted SQLite WAL database.
#[derive(Clone)]
pub struct CrdtSyncEngine {
    rooms: Arc<DashMap<String, Arc<CrdtChatDocument>>>,
    db: Option<EncryptedDatabase>,
}

impl CrdtSyncEngine {
    pub fn new(db: Option<EncryptedDatabase>) -> Self {
        Self {
            rooms: Arc::new(DashMap::new()),
            db,
        }
    }

    /// Gets or loads the `CrdtChatDocument` for `room_id` from SQLite WAL storage
    pub fn room_doc(&self, room_id: &str) -> CoreResult<Arc<CrdtChatDocument>> {
        if let Some(existing) = self.rooms.get(room_id) {
            return Ok(Arc::clone(existing.value()));
        }

        let initial_snapshot = match &self.db {
            Some(db) => db.load_crdt_snapshot(room_id)?,
            None => None,
        };

        let doc = Arc::new(CrdtChatDocument::new(
            room_id.to_string(),
            initial_snapshot.as_deref(),
        )?);
        self.rooms.insert(room_id.to_string(), Arc::clone(&doc));
        Ok(doc)
    }

    /// Appends or updates a message in `room_id`, persists the compacted CRDT snapshot to SQLite,
    /// and returns the delta bytes for P2P transmission.
    pub fn record_message(&self, msg: &CrdtChatMessage) -> CoreResult<Vec<u8>> {
        let doc = self.room_doc(&msg.room_id)?;
        let delta = doc.upsert_message(msg)?;
        self.persist_room(doc.as_ref())?;
        Ok(delta)
    }

    /// Applies a remote CRDT delta received from a peer and persists the merged state
    pub fn apply_remote_delta(
        &self,
        room_id: &str,
        delta_bytes: &[u8],
    ) -> CoreResult<Vec<CrdtChatMessage>> {
        let doc = self.room_doc(room_id)?;
        doc.merge_remote_bytes(delta_bytes)?;
        self.persist_room(doc.as_ref())?;
        doc.list_messages()
    }

    /// Toggles a reaction on a message, persists to SQLite, and returns the CRDT delta
    pub fn toggle_reaction(
        &self,
        room_id: &str,
        message_id: &str,
        emoji: &str,
        peer_id: &str,
    ) -> CoreResult<Option<Vec<u8>>> {
        let doc = self.room_doc(room_id)?;
        let delta = doc.toggle_reaction(message_id, emoji, peer_id)?;
        if delta.is_some() {
            self.persist_room(doc.as_ref())?;
        }
        Ok(delta)
    }

    /// Edits a message's text, persists to SQLite, and returns the CRDT delta
    pub fn edit_message(
        &self,
        room_id: &str,
        message_id: &str,
        new_text: &str,
    ) -> CoreResult<Option<Vec<u8>>> {
        let doc = self.room_doc(room_id)?;
        let now = chrono::Utc::now().timestamp_millis();
        let delta = doc.edit_message_text(message_id, new_text, now)?;
        if delta.is_some() {
            self.persist_room(doc.as_ref())?;
        }
        Ok(delta)
    }

    /// Deletes a message across all replicas, persists to SQLite, and returns the CRDT delta
    pub fn delete_message(&self, room_id: &str, message_id: &str) -> CoreResult<Option<Vec<u8>>> {
        let doc = self.room_doc(room_id)?;
        let delta = doc.delete_message(message_id)?;
        if delta.is_some() {
            self.persist_room(doc.as_ref())?;
        }
        Ok(delta)
    }

    fn persist_room(&self, doc: &CrdtChatDocument) -> CoreResult<()> {
        if let Some(ref db) = self.db {
            let snapshot = doc.export_full_snapshot()?;
            db.save_crdt_snapshot(doc.room_id(), b"v1", &snapshot)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loro_crdt_concurrent_message_and_reaction_convergence() {
        let doc_alice = CrdtChatDocument::new("room-alpha", None).unwrap();
        let doc_bob = CrdtChatDocument::new("room-alpha", None).unwrap();

        let msg1 = CrdtChatMessage {
            id: "msg-001".into(),
            room_id: "room-alpha".into(),
            sender_peer_id: "AlicePeer".into(),
            text: "Привет через Loro CRDT!".into(),
            timestamp_ms: 1000,
            edited_at_ms: None,
            deleted: false,
            reply_to_id: None,
            forwarded_from_peer: None,
            attachment_kind: "text".into(),
            media_blake3_hash: None,
            media_mime: None,
            media_size_bytes: None,
            waveform: vec![],
            reactions: BTreeMap::new(),
            channel_signature_hex: None,
        };

        let delta_from_alice = doc_alice.upsert_message(&msg1).unwrap();
        doc_bob.merge_remote_bytes(&delta_from_alice).unwrap();

        let bob_messages = doc_bob.list_messages().unwrap();
        assert_eq!(bob_messages.len(), 1);
        assert_eq!(bob_messages[0].text, "Привет через Loro CRDT!");

        // Bob adds a fire reaction and sends delta back to Alice
        let reaction_delta = doc_bob
            .toggle_reaction("msg-001", "🔥", "BobPeer")
            .unwrap()
            .unwrap();
        doc_alice.merge_remote_bytes(&reaction_delta).unwrap();

        let alice_msg = doc_alice.get_message("msg-001").unwrap().unwrap();
        assert_eq!(
            alice_msg.reactions.get("🔥"),
            Some(&vec!["BobPeer".to_string()])
        );
    }
}
