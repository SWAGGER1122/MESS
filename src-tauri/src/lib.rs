pub mod crypto;
pub mod error;
pub mod network;
pub mod storage;

use crate::crypto::{
    IdentityBundle, KeyPairIdentity, SafetyNumberVerify, SignedChannelPost, WordCount,
};
use crate::error::{CoreError, CoreResult};
use crate::network::{
    ChatPayload, NetworkStats, NodeAddrTicket, P2PNode, StealthConfig, StealthTelemetry,
    StealthTransport,
};
use crate::storage::{
    ChatRoomRecord, ChatRoomType, ContactRecord, CrdtChatMessage, CrdtSyncEngine,
    EncryptedDatabase,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Managed application state shared across all Tauri v2 IPC commands
pub struct AppState {
    pub active_identity: RwLock<Option<KeyPairIdentity>>,
    pub p2p_node: RwLock<Option<Arc<P2PNode>>>,
    pub database: RwLock<Option<EncryptedDatabase>>,
    pub crdt_engine: RwLock<CrdtSyncEngine>,
    pub stealth_transport: Arc<StealthTransport>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            active_identity: RwLock::new(None),
            p2p_node: RwLock::new(None),
            database: RwLock::new(None),
            crdt_engine: RwLock::new(CrdtSyncEngine::new(None)),
            stealth_transport: StealthTransport::new(StealthConfig::default()),
        }
    }
}

/// Initializes or unlocks the local SQLite WAL encrypted vault with `master_password` (Argon2id)
#[tauri::command]
pub async fn cmd_unlock_vault(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    master_password: String,
) -> CoreResult<Option<IdentityBundle>> {
    let app_dir = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    std::fs::create_dir_all(&app_dir)?;
    let db_path = app_dir.join("aether_vault.sqlite3");

    let db = EncryptedDatabase::open(&db_path, &master_password)?;
    let crdt_engine = CrdtSyncEngine::new(Some(db.clone()));

    let existing_identity = if let Some((seed, mnemonic)) = db.load_local_identity_secret()? {
        let id = KeyPairIdentity::from_secret_seed(seed, None)?;
        let bundle = IdentityBundle {
            mnemonic_phrase: mnemonic,
            public_identity: id.public_identity(),
            prekey_bundle: id.generate_prekey_bundle(true),
        };
        *state.active_identity.write().await = Some(id);
        Some(bundle)
    } else {
        None
    };

    *state.database.write().await = Some(db);
    *state.crdt_engine.write().await = crdt_engine;

    Ok(existing_identity)
}

#[tauri::command]
pub async fn cmd_generate_identity(
    state: State<'_, AppState>,
    words: u8,
    passphrase: Option<String>,
    display_name: Option<String>,
) -> CoreResult<IdentityBundle> {
    let word_count = match words {
        24 => WordCount::Words24,
        _ => WordCount::Words12,
    };

    let (identity, phrase) = KeyPairIdentity::generate_new(
        word_count,
        passphrase.as_deref().unwrap_or(""),
        display_name,
    )?;

    if let Some(ref db) = *state.database.read().await {
        db.save_local_identity_secret(&identity.secret_seed_bytes(), Some(&phrase))?;
    }

    let bundle = IdentityBundle {
        mnemonic_phrase: Some(phrase),
        public_identity: identity.public_identity(),
        prekey_bundle: identity.generate_prekey_bundle(true),
    };

    *state.active_identity.write().await = Some(identity);
    Ok(bundle)
}

#[tauri::command]
pub async fn cmd_restore_identity(
    state: State<'_, AppState>,
    mnemonic_phrase: String,
    passphrase: Option<String>,
    display_name: Option<String>,
) -> CoreResult<IdentityBundle> {
    let identity = KeyPairIdentity::from_mnemonic(
        &mnemonic_phrase,
        passphrase.as_deref().unwrap_or(""),
        display_name,
    )?;

    if let Some(ref db) = *state.database.read().await {
        db.save_local_identity_secret(&identity.secret_seed_bytes(), Some(&mnemonic_phrase))?;
    }

    let bundle = IdentityBundle {
        mnemonic_phrase: None,
        public_identity: identity.public_identity(),
        prekey_bundle: identity.generate_prekey_bundle(true),
    };

    *state.active_identity.write().await = Some(identity);
    Ok(bundle)
}

#[tauri::command]
pub async fn cmd_spawn_p2p_node(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    bind_port: Option<u16>,
) -> CoreResult<NetworkStats> {
    let identity = state
        .active_identity
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Crypto("Identity not initialized yet".into()))?;

    if let Some(existing_node) = state.p2p_node.write().await.take() {
        existing_node.shutdown().await;
    }

    let node = P2PNode::spawn(identity, bind_port.unwrap_or(0)).await?;
    let mut incoming_rx = node.subscribe_incoming();
    let crdt_engine = state.crdt_engine.read().await.clone();

    // Forward incoming decrypted P2P messages + merge CRDT deltas automatically
    let app_handle = app.clone();
    tokio::spawn(async move {
        while let Ok(msg) = incoming_rx.recv().await {
            if let Some(ref delta) = msg.chat_payload.crdt_delta {
                let _ = crdt_engine
                    .apply_remote_delta(&msg.chat_payload.conversation_id, delta);
            } else {
                let crdt_msg = CrdtChatMessage {
                    id: msg.chat_payload.message_id.clone(),
                    room_id: msg.chat_payload.conversation_id.clone(),
                    sender_peer_id: msg.sender_peer_id.clone(),
                    text: msg.chat_payload.text.clone().unwrap_or_default(),
                    timestamp_ms: msg.chat_payload.timestamp_ms,
                    edited_at_ms: None,
                    deleted: false,
                    reply_to_id: msg.chat_payload.reply_to_id.clone(),
                    forwarded_from_peer: None,
                    attachment_kind: if msg.chat_payload.media_blake3_hash.is_some() {
                        "media".to_string()
                    } else {
                        "text".to_string()
                    },
                    media_blake3_hash: msg.chat_payload.media_blake3_hash.clone(),
                    media_mime: msg.chat_payload.media_mime.clone(),
                    media_size_bytes: None,
                    waveform: msg.chat_payload.media_waveform.clone().unwrap_or_default(),
                    reactions: BTreeMap::new(),
                    channel_signature_hex: None,
                };
                let _ = crdt_engine.record_message(&crdt_msg);
            }

            let _ = app_handle.emit("p2p://incoming-message", &msg);
        }
    });

    let stats = node.network_stats().await;
    *state.p2p_node.write().await = Some(node);
    Ok(stats)
}

#[tauri::command]
pub async fn cmd_get_network_stats(state: State<'_, AppState>) -> CoreResult<NetworkStats> {
    let node = state
        .p2p_node
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Network("P2P Node is not running".into()))?;
    Ok(node.network_stats().await)
}

#[tauri::command]
pub async fn cmd_get_local_ticket(state: State<'_, AppState>) -> CoreResult<NodeAddrTicket> {
    let node = state
        .p2p_node
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Network("P2P Node is not running".into()))?;
    node.local_ticket().await
}

#[tauri::command]
pub async fn cmd_register_peer_ticket(
    state: State<'_, AppState>,
    ticket: NodeAddrTicket,
    display_name: Option<String>,
) -> CoreResult<()> {
    let node = state
        .p2p_node
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Network("P2P Node is not running".into()))?;

    node.register_peer_hints(
        &ticket.peer_id,
        ticket.relay_url.as_deref(),
        &ticket.direct_addresses,
    )?;

    if let Some(ref db) = *state.database.read().await {
        let now = chrono::Utc::now().timestamp_millis();
        let pk_bytes = KeyPairIdentity::decode_peer_id_bytes(&ticket.peer_id)?;
        let x_pk = KeyPairIdentity::ed25519_pk_to_x25519_pk(&pk_bytes)?;

        let contact = ContactRecord {
            peer_id: ticket.peer_id.clone(),
            display_name: display_name.unwrap_or_else(|| {
                format!("Peer {}", &ticket.peer_id[..8.min(ticket.peer_id.len())])
            }),
            ed25519_public_hex: hex::encode(pk_bytes),
            x25519_public_hex: hex::encode(x_pk.as_bytes()),
            safety_verified: false,
            relay_url: ticket.relay_url.clone(),
            direct_addrs_json: serde_json::to_string(&ticket.direct_addresses)
                .unwrap_or_else(|_| "[]".into()),
            updated_at_ms: now,
        };
        db.upsert_contact(&contact)?;
    }

    Ok(())
}

#[tauri::command]
pub async fn cmd_send_p2p_message(
    state: State<'_, AppState>,
    recipient_peer_id: String,
    conversation_id: String,
    text: String,
    reply_to_id: Option<String>,
    attachment_kind: Option<String>,
    media_bytes: Option<Vec<u8>>,
    media_mime: Option<String>,
    waveform: Option<Vec<u8>>,
) -> CoreResult<CrdtChatMessage> {
    let node = state
        .p2p_node
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Network("P2P Node is not running".into()))?;

    let message_id = Uuid::new_v4().to_string();
    let timestamp_ms = chrono::Utc::now().timestamp_millis();

    let (media_blake3_hash, media_size_bytes) = if let Some(bytes) = media_bytes {
        let size = bytes.len() as u64;
        let hash = node.store_blob(bytes.clone());
        if let Some(ref db) = *state.database.read().await {
            let _ = db.store_encrypted_blob(
                media_mime.as_deref().unwrap_or("application/octet-stream"),
                &bytes,
            );
        }
        (Some(hash), Some(size))
    } else {
        (None, None)
    };

    let crdt_msg = CrdtChatMessage {
        id: message_id.clone(),
        room_id: conversation_id.clone(),
        sender_peer_id: node.local_peer_id(),
        text: text.clone(),
        timestamp_ms,
        edited_at_ms: None,
        deleted: false,
        reply_to_id: reply_to_id.clone(),
        forwarded_from_peer: None,
        attachment_kind: attachment_kind.unwrap_or_else(|| "text".to_string()),
        media_blake3_hash: media_blake3_hash.clone(),
        media_mime: media_mime.clone(),
        media_size_bytes,
        waveform: waveform.clone().unwrap_or_default(),
        reactions: BTreeMap::new(),
        channel_signature_hex: None,
    };

    let crdt_delta = state
        .crdt_engine
        .read()
        .await
        .record_message(&crdt_msg)?;

    let payload = ChatPayload {
        message_id,
        conversation_id: conversation_id.clone(),
        sender_peer_id: node.local_peer_id(),
        recipient_peer_id: recipient_peer_id.clone(),
        timestamp_ms,
        text: Some(text.clone()),
        reply_to_id,
        media_blake3_hash,
        media_mime,
        media_waveform: waveform,
        crdt_delta: Some(crdt_delta),
    };

    let _ = node
        .send_encrypted_message(&recipient_peer_id, &payload)
        .await?;

    Ok(crdt_msg)
}

#[tauri::command]
pub async fn cmd_list_room_messages(
    state: State<'_, AppState>,
    room_id: String,
) -> CoreResult<Vec<CrdtChatMessage>> {
    let engine = state.crdt_engine.read().await;
    let doc = engine.room_doc(&room_id)?;
    doc.list_messages()
}

#[tauri::command]
pub async fn cmd_toggle_reaction(
    state: State<'_, AppState>,
    room_id: String,
    recipient_peer_id: Option<String>,
    message_id: String,
    emoji: String,
) -> CoreResult<Vec<CrdtChatMessage>> {
    let local_peer = state
        .active_identity
        .read()
        .await
        .as_ref()
        .map(|i| i.peer_id())
        .ok_or_else(|| CoreError::Crypto("Identity not initialized".into()))?;

    let engine = state.crdt_engine.read().await;
    let delta_opt = engine.toggle_reaction(&room_id, &message_id, &emoji, &local_peer)?;

    if let (Some(delta), Some(peer_id)) = (delta_opt, recipient_peer_id) {
        if let Some(ref node) = *state.p2p_node.read().await {
            let payload = ChatPayload {
                message_id: Uuid::new_v4().to_string(),
                conversation_id: room_id.clone(),
                sender_peer_id: local_peer,
                recipient_peer_id: peer_id.clone(),
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
                text: None,
                reply_to_id: None,
                media_blake3_hash: None,
                media_mime: None,
                media_waveform: None,
                crdt_delta: Some(delta),
            };
            let _ = node.send_encrypted_message(&peer_id, &payload).await;
        }
    }

    engine.room_doc(&room_id)?.list_messages()
}

#[tauri::command]
pub async fn cmd_upsert_chat_room(
    state: State<'_, AppState>,
    room_id: String,
    room_type: String,
    title: String,
    peer_id: Option<String>,
    folder: String,
    pinned: bool,
) -> CoreResult<ChatRoomRecord> {
    let record = ChatRoomRecord {
        room_id,
        room_type: ChatRoomType::from_str(&room_type),
        title,
        peer_id,
        folder,
        unread_count: 0,
        pinned,
        muted: false,
        last_message_preview: None,
        last_activity_ms: chrono::Utc::now().timestamp_millis(),
    };

    if let Some(ref db) = *state.database.read().await {
        db.upsert_chat_room(&record)?;
    }

    Ok(record)
}

#[tauri::command]
pub async fn cmd_list_chat_rooms(state: State<'_, AppState>) -> CoreResult<Vec<ChatRoomRecord>> {
    if let Some(ref db) = *state.database.read().await {
        db.list_chat_rooms()
    } else {
        Ok(vec![])
    }
}

#[tauri::command]
pub async fn cmd_configure_stealth(
    state: State<'_, AppState>,
    config: StealthConfig,
) -> CoreResult<StealthTelemetry> {
    let is_stealth = config.mode == crate::network::StealthTransportMode::ForceStealth443;
    state.stealth_transport.update_config(config);

    if let Some(ref node) = *state.p2p_node.read().await {
        node.set_stealth_mode(is_stealth);
    }

    Ok(state.stealth_transport.telemetry())
}

#[tauri::command]
pub async fn cmd_get_stealth_telemetry(state: State<'_, AppState>) -> CoreResult<StealthTelemetry> {
    Ok(state.stealth_transport.telemetry())
}

#[tauri::command]
pub async fn cmd_probe_stealth_sni(state: State<'_, AppState>) -> CoreResult<u64> {
    state.stealth_transport.probe_sni_camouflage_latency().await
}

#[tauri::command]
pub async fn cmd_publish_channel_post(
    state: State<'_, AppState>,
    channel_room_id: String,
    parent_hash_hex: Option<String>,
    sequence: u64,
    text: String,
) -> CoreResult<SignedChannelPost> {
    let identity = state
        .active_identity
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Crypto("Identity not initialized".into()))?;

    let post = identity.sign_channel_post(parent_hash_hex, sequence, text.as_bytes().to_vec());

    let crdt_msg = CrdtChatMessage {
        id: post.post_hash_hex.clone(),
        room_id: channel_room_id,
        sender_peer_id: post.author_peer_id.clone(),
        text,
        timestamp_ms: post.timestamp_ms,
        edited_at_ms: None,
        deleted: false,
        reply_to_id: None,
        forwarded_from_peer: None,
        attachment_kind: "channel_post".to_string(),
        media_blake3_hash: None,
        media_mime: None,
        media_size_bytes: None,
        waveform: vec![],
        reactions: BTreeMap::new(),
        channel_signature_hex: Some(post.ed25519_signature_hex.clone()),
    };

    let _ = state
        .crdt_engine
        .read()
        .await
        .record_message(&crdt_msg)?;

    Ok(post)
}

#[tauri::command]
pub async fn cmd_verify_safety_numbers(
    state: State<'_, AppState>,
    remote_peer_id: String,
) -> CoreResult<SafetyNumberVerify> {
    let identity = state
        .active_identity
        .read()
        .await
        .clone()
        .ok_or_else(|| CoreError::Crypto("Identity not initialized".into()))?;

    identity.compute_safety_numbers(&remote_peer_id)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aether_core_lib=debug,iroh=warn".into()),
        )
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            cmd_unlock_vault,
            cmd_generate_identity,
            cmd_restore_identity,
            cmd_spawn_p2p_node,
            cmd_get_network_stats,
            cmd_get_local_ticket,
            cmd_register_peer_ticket,
            cmd_send_p2p_message,
            cmd_list_room_messages,
            cmd_toggle_reaction,
            cmd_upsert_chat_room,
            cmd_list_chat_rooms,
            cmd_configure_stealth,
            cmd_get_stealth_telemetry,
            cmd_probe_stealth_sni,
            cmd_publish_channel_post,
            cmd_verify_safety_numbers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Aether P2P Tauri application");
}
