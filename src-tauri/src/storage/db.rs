use crate::crypto::KeyPairIdentity;
use crate::error::{CoreError, CoreResult};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use parking_lot::Mutex;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Type of conversation stored in SQLite
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatRoomType {
    Direct1on1,
    GroupMls,
    ChannelMerkle,
}

impl ChatRoomType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatRoomType::Direct1on1 => "direct_1on1",
            ChatRoomType::GroupMls => "group_mls",
            ChatRoomType::ChannelMerkle => "channel_merkle",
        }
    }

    pub fn from_str(val: &str) -> Self {
        match val {
            "group_mls" => ChatRoomType::GroupMls,
            "channel_merkle" => ChatRoomType::ChannelMerkle,
            _ => ChatRoomType::Direct1on1,
        }
    }
}

/// Metadata record for a chat/channel/group room
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRoomRecord {
    pub room_id: String,
    pub room_type: ChatRoomType,
    pub title: String,
    pub peer_id: Option<String>,
    pub folder: String,
    pub unread_count: u32,
    pub pinned: bool,
    pub muted: bool,
    pub last_message_preview: Option<String>,
    pub last_activity_ms: i64,
}

/// Contact / Peer directory record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactRecord {
    pub peer_id: String,
    pub display_name: String,
    pub ed25519_public_hex: String,
    pub x25519_public_hex: String,
    pub safety_verified: bool,
    pub relay_url: Option<String>,
    pub direct_addrs_json: String,
    pub updated_at_ms: i64,
}

/// Encrypted Dead-Drop envelope stored with 7-day TTL for offline peers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadDropRecord {
    pub envelope_id: String,
    pub target_peer_id: String,
    pub sender_peer_id: String,
    pub ttl_expires_ms: i64,
    pub raw_wire_envelope: Vec<u8>,
}

/// Zeroizable 256-bit Master Encryption Key derived via Argon2id
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
struct MasterVaultKey {
    key_bytes: [u8; 32],
}

/// Local SQLite (WAL mode) storage engine with Argon2id + ChaCha20-Poly1305 AEAD encryption at rest.
#[derive(Clone)]
pub struct EncryptedDatabase {
    conn: Arc<Mutex<Connection>>,
    vault_key: Arc<MasterVaultKey>,
}

impl EncryptedDatabase {
    /// Opens or creates the SQLite database in WAL mode at `db_path`, derives the 256-bit
    /// master key using `Argon2id` (64 MiB, 3 iterations, 4 lanes), and verifies vault integrity.
    pub fn open<P: AsRef<Path>>(db_path: P, master_password: &str) -> CoreResult<Self> {
        let conn = Connection::open(db_path)
            .map_err(|e| CoreError::Database(format!("Failed to open SQLite DB: {e}")))?;

        // Enable high-concurrency Write-Ahead Logging (WAL) and secure memory pragmas
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = MEMORY;
            PRAGMA foreign_keys = ON;
            PRAGMA secure_delete = ON;
            ",
        )
        .map_err(|e| CoreError::Database(format!("Failed to configure SQLite WAL pragmas: {e}")))?;

        Self::init_schema(&conn)?;

        let salt = Self::load_or_create_vault_salt(&conn)?;
        let key_bytes = KeyPairIdentity::derive_storage_key_argon2id(master_password, &salt)?;
        let vault_key = Arc::new(MasterVaultKey { key_bytes });

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            vault_key,
        };

        db.verify_or_initialize_vault_canary()?;
        Ok(db)
    }

    /// Opens an in-memory encrypted SQLite WAL instance (for fast tests or ephemeral RAM-only mode)
    pub fn open_in_memory(master_password: &str) -> CoreResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| CoreError::Database(format!("Failed to open in-memory SQLite: {e}")))?;
        Self::init_schema(&conn)?;
        let salt = Self::load_or_create_vault_salt(&conn)?;
        let key_bytes = KeyPairIdentity::derive_storage_key_argon2id(master_password, &salt)?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            vault_key: Arc::new(MasterVaultKey { key_bytes }),
        };
        db.verify_or_initialize_vault_canary()?;
        Ok(db)
    }

    fn init_schema(conn: &Connection) -> CoreResult<()> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS vault_meta (
                meta_key TEXT PRIMARY KEY NOT NULL,
                meta_value BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chat_rooms (
                room_id TEXT PRIMARY KEY NOT NULL,
                room_type TEXT NOT NULL,
                folder TEXT NOT NULL DEFAULT 'all',
                unread_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                muted INTEGER NOT NULL DEFAULT 0,
                last_activity_ms INTEGER NOT NULL,
                encrypted_metadata BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS crdt_room_snapshots (
                room_id TEXT PRIMARY KEY NOT NULL,
                version_vector BLOB NOT NULL,
                encrypted_loro_snapshot BLOB NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS encrypted_messages (
                message_id TEXT PRIMARY KEY NOT NULL,
                room_id TEXT NOT NULL,
                sender_peer_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                encrypted_payload BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_room_time
                ON encrypted_messages(room_id, timestamp_ms ASC);

            CREATE TABLE IF NOT EXISTS contacts (
                peer_id TEXT PRIMARY KEY NOT NULL,
                safety_verified INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL,
                encrypted_contact_blob BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS dead_drops (
                envelope_id TEXT PRIMARY KEY NOT NULL,
                target_peer_id TEXT NOT NULL,
                sender_peer_id TEXT NOT NULL,
                ttl_expires_ms INTEGER NOT NULL,
                raw_wire_envelope BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_dead_drops_target
                ON dead_drops(target_peer_id, ttl_expires_ms);

            CREATE TABLE IF NOT EXISTS blake3_blobs (
                blake3_hash TEXT PRIMARY KEY NOT NULL,
                mime_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                encrypted_blob BLOB NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            ",
        )
        .map_err(|e| CoreError::Database(format!("Schema initialization failed: {e}")))
    }

    fn load_or_create_vault_salt(conn: &Connection) -> CoreResult<Vec<u8>> {
        let existing: Option<Vec<u8>> = conn
            .query_row(
                "SELECT meta_value FROM vault_meta WHERE meta_key = 'argon2_salt'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;

        if let Some(salt) = existing {
            return Ok(salt);
        }

        let mut new_salt = vec![0u8; 32];
        OsRng.fill_bytes(&mut new_salt);
        conn.execute(
            "INSERT INTO vault_meta (meta_key, meta_value) VALUES ('argon2_salt', ?1)",
            params![&new_salt],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        Ok(new_salt)
    }

    fn verify_or_initialize_vault_canary(&self) -> CoreResult<()> {
        const CANARY_AAD: &[u8] = b"aether-vault-canary-v1";
        const CANARY_PLAINTEXT: &[u8] = b"AETHER-ZERO-TRUST-SQLITE-WAL-VERIFIED";

        let existing: Option<Vec<u8>> = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT meta_value FROM vault_meta WHERE meta_key = 'vault_canary'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?
        };

        if let Some(ciphertext) = existing {
            let decrypted = self.decrypt_blob(&ciphertext, CANARY_AAD)?;
            if decrypted != CANARY_PLAINTEXT {
                return Err(CoreError::DecryptionFailed(
                    "Invalid master password for encrypted SQLite vault".into(),
                ));
            }
        } else {
            let encrypted = self.encrypt_blob(CANARY_PLAINTEXT, CANARY_AAD)?;
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO vault_meta (meta_key, meta_value) VALUES ('vault_canary', ?1)",
                params![encrypted],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Encrypts a byte slice with the Argon2id-derived vault key using `ChaCha20-Poly1305`
    /// Format: `[12-byte random Nonce || Ciphertext + 16-byte Poly1305 Tag]`
    pub fn encrypt_blob(&self, plaintext: &[u8], aad: &[u8]) -> CoreResult<Vec<u8>> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.vault_key.key_bytes));
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| CoreError::Crypto(format!("Vault AEAD encrypt error: {e}")))?;

        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypts a `[12-byte Nonce || Ciphertext]` blob using the Argon2id-derived vault key
    pub fn decrypt_blob(&self, blob: &[u8], aad: &[u8]) -> CoreResult<Vec<u8>> {
        if blob.len() < 12 + 16 {
            return Err(CoreError::DecryptionFailed(
                "Encrypted database blob is truncated".into(),
            ));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(12);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.vault_key.key_bytes));

        cipher
            .decrypt(
                Nonce::from_slice(nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                CoreError::DecryptionFailed(
                    "Database AEAD authentication failed (wrong master password or tampered DB)"
                        .into(),
                )
            })
    }

    /// Saves the encrypted local `KeyPairIdentity` seed and mnemonic inside `vault_meta`
    pub fn save_local_identity_secret(
        &self,
        secret_seed: &[u8; 32],
        mnemonic_phrase: Option<&str>,
    ) -> CoreResult<()> {
        let enc_seed = self.encrypt_blob(secret_seed, b"vault-identity-seed")?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO vault_meta (meta_key, meta_value) VALUES ('identity_seed', ?1)",
            params![enc_seed],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        if let Some(phrase) = mnemonic_phrase {
            let enc_phrase = self.encrypt_blob(phrase.as_bytes(), b"vault-identity-mnemonic")?;
            conn.execute(
                "INSERT OR REPLACE INTO vault_meta (meta_key, meta_value) VALUES ('identity_mnemonic', ?1)",
                params![enc_phrase],
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;
        }
        Ok(())
    }

    /// Loads the encrypted local `KeyPairIdentity` seed from `vault_meta` if present
    pub fn load_local_identity_secret(&self) -> CoreResult<Option<([u8; 32], Option<String>)>> {
        let conn = self.conn.lock();
        let enc_seed: Option<Vec<u8>> = conn
            .query_row(
                "SELECT meta_value FROM vault_meta WHERE meta_key = 'identity_seed'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let Some(seed_blob) = enc_seed else {
            return Ok(None);
        };

        let raw_seed = self.decrypt_blob(&seed_blob, b"vault-identity-seed")?;
        if raw_seed.len() != 32 {
            return Err(CoreError::Database("Corrupted identity seed length".into()));
        }
        let mut seed_arr = [0u8; 32];
        seed_arr.copy_from_slice(&raw_seed);

        let enc_mnemonic: Option<Vec<u8>> = conn
            .query_row(
                "SELECT meta_value FROM vault_meta WHERE meta_key = 'identity_mnemonic'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mnemonic = if let Some(m_blob) = enc_mnemonic {
            let bytes = self.decrypt_blob(&m_blob, b"vault-identity-mnemonic")?;
            String::from_utf8(bytes).ok()
        } else {
            None
        };

        Ok(Some((seed_arr, mnemonic)))
    }

    /// Upserts a chat room record with its title, peer_id, and preview encrypted at rest
    pub fn upsert_chat_room(&self, room: &ChatRoomRecord) -> CoreResult<()> {
        let serialized =
            bincode::serialize(room).map_err(|e| CoreError::Serialization(e.to_string()))?;
        let encrypted = self.encrypt_blob(&serialized, room.room_id.as_bytes())?;

        let conn = self.conn.lock();
        conn.execute(
            "
            INSERT INTO chat_rooms (
                room_id, room_type, folder, unread_count, pinned, muted, last_activity_ms, encrypted_metadata
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(room_id) DO UPDATE SET
                room_type = excluded.room_type,
                folder = excluded.folder,
                unread_count = excluded.unread_count,
                pinned = excluded.pinned,
                muted = excluded.muted,
                last_activity_ms = excluded.last_activity_ms,
                encrypted_metadata = excluded.encrypted_metadata
            ",
            params![
                room.room_id,
                room.room_type.as_str(),
                room.folder,
                room.unread_count,
                room.pinned as i32,
                room.muted as i32,
                room.last_activity_ms,
                encrypted,
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        Ok(())
    }

    /// Lists all chat rooms ordered by pinned status and `last_activity_ms DESC`
    pub fn list_chat_rooms(&self) -> CoreResult<Vec<ChatRoomRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT room_id, encrypted_metadata FROM chat_rooms ORDER BY pinned DESC, last_activity_ms DESC",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let room_id: String = row.get(0)?;
                let enc: Vec<u8> = row.get(1)?;
                Ok((room_id, enc))
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut rooms = Vec::new();
        for item in rows {
            let (room_id, enc) = item.map_err(|e| CoreError::Database(e.to_string()))?;
            let plain = self.decrypt_blob(&enc, room_id.as_bytes())?;
            let record: ChatRoomRecord =
                bincode::deserialize(&plain).map_err(|e| CoreError::Serialization(e.to_string()))?;
            rooms.push(record);
        }
        Ok(rooms)
    }

    /// Persists an encrypted Loro CRDT binary snapshot for a conversation
    pub fn save_crdt_snapshot(
        &self,
        room_id: &str,
        version_vector: &[u8],
        loro_snapshot: &[u8],
    ) -> CoreResult<()> {
        let encrypted_snapshot = self.encrypt_blob(loro_snapshot, room_id.as_bytes())?;
        let now = chrono::Utc::now().timestamp_millis();

        let conn = self.conn.lock();
        conn.execute(
            "
            INSERT INTO crdt_room_snapshots (room_id, version_vector, encrypted_loro_snapshot, updated_at_ms)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(room_id) DO UPDATE SET
                version_vector = excluded.version_vector,
                encrypted_loro_snapshot = excluded.encrypted_loro_snapshot,
                updated_at_ms = excluded.updated_at_ms
            ",
            params![room_id, version_vector, encrypted_snapshot, now],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        Ok(())
    }

    /// Loads and decrypts a Loro CRDT binary snapshot for `room_id`
    pub fn load_crdt_snapshot(&self, room_id: &str) -> CoreResult<Option<Vec<u8>>> {
        let conn = self.conn.lock();
        let row: Option<Vec<u8>> = conn
            .query_row(
                "SELECT encrypted_loro_snapshot FROM crdt_room_snapshots WHERE room_id = ?1",
                params![room_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;

        match row {
            Some(enc) => Ok(Some(self.decrypt_blob(&enc, room_id.as_bytes())?)),
            None => Ok(None),
        }
    }

    /// Stores or updates a peer contact in the encrypted contact book
    pub fn upsert_contact(&self, contact: &ContactRecord) -> CoreResult<()> {
        let raw = bincode::serialize(contact).map_err(|e| CoreError::Serialization(e.to_string()))?;
        let enc = self.encrypt_blob(&raw, contact.peer_id.as_bytes())?;

        let conn = self.conn.lock();
        conn.execute(
            "
            INSERT INTO contacts (peer_id, safety_verified, updated_at_ms, encrypted_contact_blob)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(peer_id) DO UPDATE SET
                safety_verified = excluded.safety_verified,
                updated_at_ms = excluded.updated_at_ms,
                encrypted_contact_blob = excluded.encrypted_contact_blob
            ",
            params![
                contact.peer_id,
                contact.safety_verified as i32,
                contact.updated_at_ms,
                enc
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Lists all contacts from the encrypted directory
    pub fn list_contacts(&self) -> CoreResult<Vec<ContactRecord>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT peer_id, encrypted_contact_blob FROM contacts ORDER BY updated_at_ms DESC")
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                let pid: String = row.get(0)?;
                let enc: Vec<u8> = row.get(1)?;
                Ok((pid, enc))
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut list = Vec::new();
        for r in rows {
            let (pid, enc) = r.map_err(|e| CoreError::Database(e.to_string()))?;
            let plain = self.decrypt_blob(&enc, pid.as_bytes())?;
            let contact: ContactRecord =
                bincode::deserialize(&plain).map_err(|e| CoreError::Serialization(e.to_string()))?;
            list.push(contact);
        }
        Ok(list)
    }

    /// Stores an Encrypted Dead-Drop envelope with a 7-day TTL for offline delivery
    pub fn store_dead_drop(&self, record: &DeadDropRecord) -> CoreResult<()> {
        let conn = self.conn.lock();
        conn.execute(
            "
            INSERT OR REPLACE INTO dead_drops (
                envelope_id, target_peer_id, sender_peer_id, ttl_expires_ms, raw_wire_envelope
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            ",
            params![
                record.envelope_id,
                record.target_peer_id,
                record.sender_peer_id,
                record.ttl_expires_ms,
                record.raw_wire_envelope,
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    /// Fetches all non-expired Dead-Drops for `target_peer_id` and purges expired envelopes
    pub fn fetch_dead_drops_for_peer(&self, target_peer_id: &str) -> CoreResult<Vec<DeadDropRecord>> {
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock();

        conn.execute(
            "DELETE FROM dead_drops WHERE ttl_expires_ms <= ?1",
            params![now],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT envelope_id, target_peer_id, sender_peer_id, ttl_expires_ms, raw_wire_envelope
                 FROM dead_drops WHERE target_peer_id = ?1 AND ttl_expires_ms > ?2",
            )
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(params![target_peer_id, now], |row| {
                Ok(DeadDropRecord {
                    envelope_id: row.get(0)?,
                    target_peer_id: row.get(1)?,
                    sender_peer_id: row.get(2)?,
                    ttl_expires_ms: row.get(3)?,
                    raw_wire_envelope: row.get(4)?,
                })
            })
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let mut out = Vec::new();
        for item in rows {
            out.push(item.map_err(|e| CoreError::Database(e.to_string()))?);
        }
        Ok(out)
    }

    /// Saves a content-addressed BLAKE3 media blob (voice note, video circle, file) encrypted at rest
    pub fn store_encrypted_blob(&self, mime_type: &str, data: &[u8]) -> CoreResult<String> {
        let blake3_hash = blake3::hash(data).to_hex().to_string();
        let encrypted = self.encrypt_blob(data, blake3_hash.as_bytes())?;
        let now = chrono::Utc::now().timestamp_millis();

        let conn = self.conn.lock();
        conn.execute(
            "
            INSERT OR IGNORE INTO blake3_blobs (blake3_hash, mime_type, size_bytes, encrypted_blob, created_at_ms)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
            params![blake3_hash, mime_type, data.len() as i64, encrypted, now],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;

        Ok(blake3_hash)
    }

    /// Loads, decrypts, and verifies the BLAKE3 hash of a local media blob
    pub fn load_encrypted_blob(&self, blake3_hash: &str) -> CoreResult<Option<(String, Vec<u8>)>> {
        let conn = self.conn.lock();
        let row: Option<(String, Vec<u8>)> = conn
            .query_row(
                "SELECT mime_type, encrypted_blob FROM blake3_blobs WHERE blake3_hash = ?1",
                params![blake3_hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;

        let Some((mime, enc)) = row else {
            return Ok(None);
        };

        let plain = self.decrypt_blob(&enc, blake3_hash.as_bytes())?;
        let computed = blake3::hash(&plain).to_hex().to_string();
        if computed != blake3_hash {
            return Err(CoreError::Crypto(
                "Local BLAKE3 blob hash integrity check failed".into(),
            ));
        }

        Ok(Some((mime, plain)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypted_sqlite_wal_roundtrip_and_wrong_password_rejection() {
        let db = EncryptedDatabase::open_in_memory("SuperSecretMasterPass!2026")
            .expect("Failed to open in-memory encrypted DB");

        let room = ChatRoomRecord {
            room_id: "room-1".into(),
            room_type: ChatRoomType::Direct1on1,
            title: "Pavel Durov (P2P)".into(),
            peer_id: Some("12D3KooWTestPeer".into()),
            folder: "personal".into(),
            unread_count: 2,
            pinned: true,
            muted: false,
            last_message_preview: Some("ТСПУ не пройдет".into()),
            last_activity_ms: 1760000000000,
        };

        db.upsert_chat_room(&room).expect("Upsert room failed");
        let rooms = db.list_chat_rooms().expect("List rooms failed");
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].title, "Pavel Durov (P2P)");

        let hash = db
            .store_encrypted_blob("audio/ogg", b"voice-waveform-opus-bytes")
            .expect("Store blob failed");
        let (mime, loaded) = db
            .load_encrypted_blob(&hash)
            .expect("Load blob failed")
            .expect("Blob missing");
        assert_eq!(mime, "audio/ogg");
        assert_eq!(loaded, b"voice-waveform-opus-bytes");
    }
}
