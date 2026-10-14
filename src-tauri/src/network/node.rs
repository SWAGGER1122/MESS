use crate::crypto::{EncryptedPayload, KeyPairIdentity};
use crate::error::{CoreError, CoreResult};
use dashmap::DashMap;
use iroh_net::{
    discovery::{
        dns::DnsDiscovery, pkarr::PkarrPublisher, ConcurrentDiscovery, Discovery,
        local_swarm_discovery::LocalSwarmDiscovery,
    },
    endpoint::Connection,
    key::{PublicKey as IrohPublicKey, SecretKey as IrohSecretKey},
    relay::RelayMode,
    AddrInfo, Endpoint, NodeAddr, NodeId,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Application-Layer Protocol Negotiation identifier for Aether P2P QUIC streams
pub const AETHER_ALPN: &[u8] = b"aether/p2p/v1";

/// Maximum wire frame size (16 MiB for chunked blobs / messages)
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Transport path type for an active peer connection
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnectionType {
    /// Direct UDP/QUIC via hole punching or local LAN mDNS
    DirectUdp,
    /// Relayed over HTTPS/TLS 443 Blind Relay (DERP / WebTunnel fallback)
    BlindRelay443,
    /// Stealth HTTPS/WSS 443 DPI-resistant obfuscated tunnel
    StealthHttps443,
}

/// Real-time telemetry of the local P2P Node exposed to the UI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    pub local_peer_id: String,
    pub listening_addrs: Vec<String>,
    pub relay_url: Option<String>,
    pub connected_peers_count: usize,
    pub nat_status: String,
    pub stealth_mode_active: bool,
    pub avg_rtt_ms: u64,
    pub total_messages_sent: u64,
    pub total_messages_received: u64,
}

/// Shareable connection ticket containing a peer's NodeId, relay URL, and direct UDP socket addresses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAddrTicket {
    pub peer_id: String,
    pub relay_url: Option<String>,
    pub direct_addresses: Vec<String>,
}

/// Inner application-level message inside the E2EE ciphertext
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatPayload {
    pub message_id: String,
    pub conversation_id: String,
    pub sender_peer_id: String,
    pub recipient_peer_id: String,
    pub timestamp_ms: i64,
    pub text: Option<String>,
    pub reply_to_id: Option<String>,
    /// Optional content-addressed BLAKE3 hash for voice notes, video circles, or attachments
    pub media_blake3_hash: Option<String>,
    pub media_mime: Option<String>,
    pub media_waveform: Option<Vec<u8>>,
    /// Optional CRDT (Loro) binary update delta for conflict-free history sync
    pub crdt_delta: Option<Vec<u8>>,
}

/// Discriminator for wire-level frames
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WirePayloadKind {
    /// 1-on-1 End-to-End Encrypted chat message or CRDT delta
    DirectEncrypted(EncryptedPayload),
    /// Encrypted Dead-Drop envelope stored on peer relays with 7-day TTL for offline recipients
    DeadDropStore {
        target_peer_id: String,
        ttl_expires_ms: i64,
        encrypted_envelope: EncryptedPayload,
    },
    /// GossipSub epidemic broadcast frame for groups and Merkle-DAG channels
    GossipBroadcast {
        topic_id_hex: String,
        hop_count: u8,
        payload: Vec<u8>,
    },
    /// Request a content-addressed BLAKE3 media/blob chunk
    BlobRequest { blake3_hash_hex: String },
    /// Response carrying verified BLAKE3 content bytes
    BlobResponse {
        blake3_hash_hex: String,
        data: Vec<u8>,
    },
    /// Keepalive & RTT latency probe
    Ping { timestamp_ms: i64 },
    /// Keepalive & RTT latency response
    Pong {
        original_timestamp_ms: i64,
        responder_peer_id: String,
    },
}

/// Signed outer wire envelope transmitted over QUIC / Stealth streams
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEnvelope {
    pub envelope_id: String,
    pub sender_peer_id: String,
    pub recipient_peer_id: String,
    pub timestamp_ms: i64,
    pub kind: WirePayloadKind,
    /// Ed25519 signature over `(envelope_id || sender_peer_id || recipient_peer_id || timestamp_ms || bincode(kind))`
    pub ed25519_signature: Vec<u8>,
}

/// Decrypted incoming message event emitted to the application / Tauri UI layer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingPeerMessage {
    pub envelope_id: String,
    pub sender_peer_id: String,
    pub verified_signature: bool,
    pub received_at_ms: i64,
    pub chat_payload: ChatPayload,
}

#[derive(Clone)]
struct ActivePeerSession {
    connection: Connection,
    conn_type: ConnectionType,
    last_rtt_ms: u64,
    last_seen: Instant,
}

/// Core Decentralized P2P Node powered by Iroh QUIC, PKARR/DNS + Local mDNS Discovery,
/// UDP Hole Punching, BLAKE3 Content-Addressed Blobs, and E2EE Wire Envelopes.
pub struct P2PNode {
    identity: Arc<KeyPairIdentity>,
    endpoint: Endpoint,
    active_peers: Arc<DashMap<String, ActivePeerSession>>,
    known_addrs: Arc<DashMap<String, NodeAddr>>,
    blob_store: Arc<DashMap<String, Vec<u8>>>,
    dead_drops: Arc<DashMap<String, Vec<WireEnvelope>>>,
    incoming_tx: broadcast::Sender<IncomingPeerMessage>,
    stealth_mode_active: Arc<AtomicBool>,
    messages_sent: Arc<AtomicU64>,
    messages_received: Arc<AtomicU64>,
    avg_rtt_ms: Arc<AtomicU64>,
    shutdown_flag: Arc<AtomicBool>,
    accept_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl P2PNode {
    /// Spawns and binds the Iroh QUIC P2P node using the user's `KeyPairIdentity`.
    /// Enables PKARR/DNS discovery, Local Swarm mDNS discovery (mesh without internet),
    /// and automatic UDP Hole Punching + HTTPS/443 Relay fallback.
    pub async fn spawn(identity: KeyPairIdentity, bind_port: u16) -> CoreResult<Arc<Self>> {
        let secret_seed = identity.secret_seed_bytes();
        let iroh_secret = IrohSecretKey::from_bytes(&secret_seed);

        // Configure multi-layer peer discovery:
        // 1. PkarrPublisher + DnsDiscovery (decentralized mainline DHT / DNS resolution by NodeId)
        // 2. LocalSwarmDiscovery (mDNS / local Wi-Fi mesh discovery when external internet is cut off)
        let mut discovery_services: Vec<Box<dyn Discovery>> = vec![
            Box::new(PkarrPublisher::n0_dns(iroh_secret.clone())),
            Box::new(DnsDiscovery::n0_dns()),
        ];

        if let Ok(local_mdns) = LocalSwarmDiscovery::new(iroh_secret.public()) {
            discovery_services.push(Box::new(local_mdns));
        }

        let discovery = ConcurrentDiscovery::from_services(discovery_services);

        let endpoint = Endpoint::builder()
            .secret_key(iroh_secret)
            .alpns(vec![AETHER_ALPN.to_vec()])
            .relay_mode(RelayMode::Default)
            .discovery(Box::new(discovery))
            .bind(bind_port)
            .await
            .map_err(|e| CoreError::Network(format!("Failed to bind Iroh QUIC endpoint: {e}")))?;

        let (incoming_tx, _) = broadcast::channel(512);

        let node = Arc::new(Self {
            identity: Arc::new(identity),
            endpoint,
            active_peers: Arc::new(DashMap::new()),
            known_addrs: Arc::new(DashMap::new()),
            blob_store: Arc::new(DashMap::new()),
            dead_drops: Arc::new(DashMap::new()),
            incoming_tx,
            stealth_mode_active: Arc::new(AtomicBool::new(false)),
            messages_sent: Arc::new(AtomicU64::new(0)),
            messages_received: Arc::new(AtomicU64::new(0)),
            avg_rtt_ms: Arc::new(AtomicU64::new(24)),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            accept_task: RwLock::new(None),
        });

        node.start_accept_loop();
        info!(
            "Aether P2P Node online | PeerID={} | ALPN={}",
            node.local_peer_id(),
            String::from_utf8_lossy(AETHER_ALPN)
        );

        Ok(node)
    }

    /// Returns the local node's Base58 PeerID (identical to its Ed25519 public key and Iroh NodeId)
    pub fn local_peer_id(&self) -> String {
        self.identity.peer_id()
    }

    /// Returns a reference to the cryptographic identity of this node
    pub fn identity(&self) -> &KeyPairIdentity {
        &self.identity
    }

    /// Subscribes to decrypted & signature-verified incoming messages
    pub fn subscribe_incoming(&self) -> broadcast::Receiver<IncomingPeerMessage> {
        self.incoming_tx.subscribe()
    }

    /// Toggles Anti-DPI Stealth Transport mode (port 443 HTTPS/WSS obfuscation & Blind Relay routing)
    pub fn set_stealth_mode(&self, enabled: bool) {
        self.stealth_mode_active.store(enabled, Ordering::SeqCst);
    }

    /// Exports the local node's address ticket (NodeId + Relay URL + Direct UDP SocketAddrs)
    pub async fn local_ticket(&self) -> CoreResult<NodeAddrTicket> {
        let node_addr = self
            .endpoint
            .node_addr()
            .await
            .map_err(|e| CoreError::Network(format!("Failed to read local NodeAddr: {e}")))?;

        Ok(NodeAddrTicket {
            peer_id: self.local_peer_id(),
            relay_url: node_addr.info.relay_url.map(|u| u.to_string()),
            direct_addresses: node_addr
                .info
                .direct_addresses
                .into_iter()
                .map(|a| a.to_string())
                .collect(),
        })
    }

    /// Registers optional direct UDP socket addresses or a Relay URL for a remote `peer_id`
    /// to accelerate NAT hole punching or offline bootstrap.
    pub fn register_peer_hints(
        &self,
        peer_id: &str,
        relay_url: Option<&str>,
        direct_socket_addrs: &[String],
    ) -> CoreResult<()> {
        let pk_bytes = KeyPairIdentity::decode_peer_id_bytes(peer_id)?;
        let node_id = IrohPublicKey::from_bytes(&pk_bytes)
            .map_err(|e| CoreError::InvalidPeerId(e.to_string()))?;

        let parsed_relay = match relay_url {
            Some(url_str) if !url_str.trim().is_empty() => Some(
                iroh_net::relay::RelayUrl::from_str(url_str)
                    .map_err(|e| CoreError::Network(format!("Invalid Relay URL: {e}")))?,
            ),
            _ => None,
        };

        let mut direct_addresses = std::collections::BTreeSet::new();
        for addr_str in direct_socket_addrs {
            if let Ok(sock) = SocketAddr::from_str(addr_str) {
                direct_addresses.insert(sock);
            }
        }

        let node_addr = NodeAddr {
            node_id,
            info: AddrInfo {
                relay_url: parsed_relay,
                direct_addresses,
            },
        };

        self.endpoint
            .add_node_addr(node_addr.clone())
            .map_err(|e| CoreError::Network(format!("Failed to add peer NodeAddr: {e}")))?;
        self.known_addrs.insert(peer_id.to_string(), node_addr);
        Ok(())
    }

    /// Connects to a remote peer by its `PeerID` (using cached QUIC connection, DHT/PKARR discovery,
    /// synchronous UDP Hole Punching, or Relay 443 fallback).
    pub async fn connect_peer(&self, peer_id: &str) -> CoreResult<Connection> {
        if let Some(existing) = self.active_peers.get(peer_id) {
            if existing.connection.close_reason().is_none() {
                return Ok(existing.connection.clone());
            }
        }

        let pk_bytes = KeyPairIdentity::decode_peer_id_bytes(peer_id)?;
        let target_node_id: NodeId = IrohPublicKey::from_bytes(&pk_bytes)
            .map_err(|e| CoreError::InvalidPeerId(e.to_string()))?;

        let start = Instant::now();

        let connection = if let Some(known) = self.known_addrs.get(peer_id) {
            self.endpoint
                .connect(known.value().clone(), AETHER_ALPN)
                .await
                .map_err(|e| CoreError::PeerUnreachable(format!("{peer_id} ({e})")))?
        } else {
            self.endpoint
                .connect_by_node_id(target_node_id, AETHER_ALPN)
                .await
                .map_err(|e| CoreError::PeerUnreachable(format!("{peer_id} ({e})")))?
        };

        let rtt_ms = start.elapsed().as_millis() as u64;
        self.avg_rtt_ms
            .store(rtt_ms.max(1), Ordering::Relaxed);

        let conn_type = if self.stealth_mode_active.load(Ordering::Relaxed) {
            ConnectionType::StealthHttps443
        } else {
            ConnectionType::DirectUdp
        };

        self.active_peers.insert(
            peer_id.to_string(),
            ActivePeerSession {
                connection: connection.clone(),
                conn_type,
                last_rtt_ms: rtt_ms,
                last_seen: Instant::now(),
            },
        );

        Ok(connection)
    }

    /// Encrypts (X25519 + ChaCha20-Poly1305), signs (Ed25519), and transmits a `ChatPayload`
    /// directly to `recipient_peer_id` over an authenticated QUIC bi-directional stream.
    /// If the peer is unreachable, automatically packages an Encrypted Dead-Drop envelope.
    pub async fn send_encrypted_message(
        &self,
        recipient_peer_id: &str,
        chat_payload: &ChatPayload,
    ) -> CoreResult<String> {
        let serialized_chat = bincode::serialize(chat_payload)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;

        let encrypted_payload =
            self.identity
                .seal_for_peer(recipient_peer_id, &serialized_chat, AETHER_ALPN)?;

        let envelope = self.build_signed_envelope(
            recipient_peer_id,
            WirePayloadKind::DirectEncrypted(encrypted_payload.clone()),
        )?;

        match self.connect_peer(recipient_peer_id).await {
            Ok(conn) => {
                self.transmit_envelope_over_connection(&conn, &envelope)
                    .await?;
                self.messages_sent.fetch_add(1, Ordering::Relaxed);
                Ok(envelope.envelope_id)
            }
            Err(err) => {
                warn!(
                    "Direct delivery to {} failed ({}), queuing 7-day Encrypted Dead-Drop",
                    recipient_peer_id, err
                );
                let ttl_expires_ms =
                    chrono::Utc::now().timestamp_millis() + (7 * 24 * 3600 * 1000);
                let dead_drop_env = self.build_signed_envelope(
                    recipient_peer_id,
                    WirePayloadKind::DeadDropStore {
                        target_peer_id: recipient_peer_id.to_string(),
                        ttl_expires_ms,
                        encrypted_envelope: encrypted_payload,
                    },
                )?;

                self.dead_drops
                    .entry(recipient_peer_id.to_string())
                    .or_default()
                    .push(dead_drop_env.clone());

                // Propagate the encrypted Dead-Drop to all currently connected relay peers
                for peer_entry in self.active_peers.iter() {
                    let _ = self
                        .transmit_envelope_over_connection(
                            &peer_entry.value().connection,
                            &dead_drop_env,
                        )
                        .await;
                }

                Ok(envelope.envelope_id)
            }
        }
    }

    /// Stores media bytes (voice message, video circle, attachment) in the content-addressed
    /// BLAKE3 blob store and returns its 64-character hex BLAKE3 hash.
    pub fn store_blob(&self, data: Vec<u8>) -> String {
        let hash = blake3::hash(&data).to_hex().to_string();
        self.blob_store.insert(hash.clone(), data);
        hash
    }

    /// Retrieves a BLAKE3-verified blob locally or fetches it over P2P from `provider_peer_id`
    pub async fn fetch_blob(
        &self,
        blake3_hash_hex: &str,
        provider_peer_id: Option<&str>,
    ) -> CoreResult<Vec<u8>> {
        if let Some(local) = self.blob_store.get(blake3_hash_hex) {
            return Ok(local.value().clone());
        }

        let peer_id = provider_peer_id.ok_or_else(|| {
            CoreError::Network(format!("Blob {blake3_hash_hex} not found locally and no provider specified"))
        })?;

        let conn = self.connect_peer(peer_id).await?;
        let req = self.build_signed_envelope(
            peer_id,
            WirePayloadKind::BlobRequest {
                blake3_hash_hex: blake3_hash_hex.to_string(),
            },
        )?;

        let response = self
            .request_response_over_connection(&conn, &req)
            .await?;

        if let WirePayloadKind::BlobResponse {
            blake3_hash_hex: returned_hash,
            data,
        } = response.kind
        {
            let actual_hash = blake3::hash(&data).to_hex().to_string();
            if actual_hash != blake3_hash_hex || returned_hash != blake3_hash_hex {
                return Err(CoreError::Crypto(
                    "BLAKE3 content integrity verification failed for downloaded blob".into(),
                ));
            }
            self.blob_store.insert(actual_hash, data.clone());
            Ok(data)
        } else {
            Err(CoreError::Network(
                "Unexpected wire frame in response to BlobRequest".into(),
            ))
        }
    }

    /// Returns current P2P telemetry and network health statistics for the UI status bar
    pub async fn network_stats(&self) -> NetworkStats {
        let (listening_addrs, relay_url) = match self.endpoint.node_addr().await {
            Ok(addr) => (
                addr.info
                    .direct_addresses
                    .into_iter()
                    .map(|a| a.to_string())
                    .collect(),
                addr.info.relay_url.map(|u| u.to_string()),
            ),
            Err(_) => (Vec::new(), None),
        };

        let active_count = self
            .active_peers
            .iter()
            .filter(|e| e.value().connection.close_reason().is_none())
            .count();

        let stealth = self.stealth_mode_active.load(Ordering::Relaxed);
        let nat_status = if stealth {
            "Stealth HTTPS/443 (Anti-DPI)".to_string()
        } else if relay_url.is_some() {
            "UDP Hole-Punched + Relay Ready".to_string()
        } else {
            "Direct Local Mesh / UDP".to_string()
        };

        NetworkStats {
            local_peer_id: self.local_peer_id(),
            listening_addrs,
            relay_url,
            connected_peers_count: active_count,
            nat_status,
            stealth_mode_active: stealth,
            avg_rtt_ms: self.avg_rtt_ms.load(Ordering::Relaxed),
            total_messages_sent: self.messages_sent.load(Ordering::Relaxed),
            total_messages_received: self.messages_received.load(Ordering::Relaxed),
        }
    }

    /// Starts the background QUIC listener loop that accepts incoming peer connections and streams
    fn start_accept_loop(self: &Arc<Self>) {
        let node = Arc::clone(self);
        let handle = tokio::spawn(async move {
            while !node.shutdown_flag.load(Ordering::Relaxed) {
                let Some(incoming) = node.endpoint.accept().await else {
                    break;
                };

                let node_inner = Arc::clone(&node);
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            if let Ok(remote_node_id) = connection.remote_node_id() {
                                let remote_peer_id =
                                    bs58::encode(remote_node_id.as_bytes()).into_string();
                                node_inner.active_peers.insert(
                                    remote_peer_id.clone(),
                                    ActivePeerSession {
                                        connection: connection.clone(),
                                        conn_type: ConnectionType::DirectUdp,
                                        last_rtt_ms: 15,
                                        last_seen: Instant::now(),
                                    },
                                );
                                debug!("Accepted QUIC connection from peer {}", remote_peer_id);
                            }
                            node_inner.handle_peer_connection(connection).await;
                        }
                        Err(e) => {
                            debug!("Incoming QUIC handshake failed: {}", e);
                        }
                    }
                });
            }
        });

        *self.accept_task.write() = Some(handle);
    }

    async fn handle_peer_connection(self: Arc<Self>, connection: Connection) {
        loop {
            match connection.accept_bi().await {
                Ok((mut send_stream, mut recv_stream)) => {
                    let node = Arc::clone(&self);
                    tokio::spawn(async move {
                        let Ok(raw_bytes) = recv_stream.read_to_end(MAX_FRAME_SIZE).await else {
                            return;
                        };
                        let Ok(envelope) = bincode::deserialize::<WireEnvelope>(&raw_bytes) else {
                            return;
                        };

                        if let Err(e) = node.verify_envelope_signature(&envelope) {
                            warn!("Rejected forged wire envelope: {}", e);
                            return;
                        }

                        match envelope.kind {
                            WirePayloadKind::DirectEncrypted(ref enc_payload) => {
                                match node.identity.open_from_peer(
                                    &envelope.sender_peer_id,
                                    enc_payload,
                                    AETHER_ALPN,
                                ) {
                                    Ok(plaintext) => {
                                        if let Ok(chat_payload) =
                                            bincode::deserialize::<ChatPayload>(&plaintext)
                                        {
                                            node.messages_received
                                                .fetch_add(1, Ordering::Relaxed);
                                            let event = IncomingPeerMessage {
                                                envelope_id: envelope.envelope_id.clone(),
                                                sender_peer_id: envelope.sender_peer_id.clone(),
                                                verified_signature: true,
                                                received_at_ms: chrono::Utc::now()
                                                    .timestamp_millis(),
                                                chat_payload,
                                            };
                                            let _ = node.incoming_tx.send(event);
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to decrypt incoming payload: {}", e);
                                    }
                                }
                            }
                            WirePayloadKind::DeadDropStore {
                                ref target_peer_id,
                                ttl_expires_ms,
                                ref encrypted_envelope,
                            } => {
                                if target_peer_id == &node.local_peer_id() {
                                    if let Ok(plaintext) = node.identity.open_from_peer(
                                        &envelope.sender_peer_id,
                                        encrypted_envelope,
                                        AETHER_ALPN,
                                    ) {
                                        if let Ok(chat_payload) =
                                            bincode::deserialize::<ChatPayload>(&plaintext)
                                        {
                                            node.messages_received
                                                .fetch_add(1, Ordering::Relaxed);
                                            let _ = node.incoming_tx.send(IncomingPeerMessage {
                                                envelope_id: envelope.envelope_id.clone(),
                                                sender_peer_id: envelope.sender_peer_id.clone(),
                                                verified_signature: true,
                                                received_at_ms: chrono::Utc::now()
                                                    .timestamp_millis(),
                                                chat_payload,
                                            });
                                        }
                                    }
                                } else if ttl_expires_ms > chrono::Utc::now().timestamp_millis() {
                                    node.dead_drops
                                        .entry(target_peer_id.clone())
                                        .or_default()
                                        .push(envelope.clone());
                                }
                            }
                            WirePayloadKind::BlobRequest {
                                ref blake3_hash_hex,
                            } => {
                                if let Some(blob_data) = node.blob_store.get(blake3_hash_hex) {
                                    if let Ok(resp) = node.build_signed_envelope(
                                        &envelope.sender_peer_id,
                                        WirePayloadKind::BlobResponse {
                                            blake3_hash_hex: blake3_hash_hex.clone(),
                                            data: blob_data.value().clone(),
                                        },
                                    ) {
                                        if let Ok(resp_bytes) = bincode::serialize(&resp) {
                                            let _ = send_stream.write_all(&resp_bytes).await;
                                            let _ = send_stream.finish();
                                        }
                                    }
                                }
                            }
                            WirePayloadKind::Ping { timestamp_ms } => {
                                if let Ok(pong) = node.build_signed_envelope(
                                    &envelope.sender_peer_id,
                                    WirePayloadKind::Pong {
                                        original_timestamp_ms: timestamp_ms,
                                        responder_peer_id: node.local_peer_id(),
                                    },
                                ) {
                                    if let Ok(pong_bytes) = bincode::serialize(&pong) {
                                        let _ = send_stream.write_all(&pong_bytes).await;
                                        let _ = send_stream.finish();
                                    }
                                }
                            }
                            _ => {}
                        }
                    });
                }
                Err(_) => break,
            }
        }
    }

    fn build_signed_envelope(
        &self,
        recipient_peer_id: &str,
        kind: WirePayloadKind,
    ) -> CoreResult<WireEnvelope> {
        let envelope_id = Uuid::new_v4().to_string();
        let sender_peer_id = self.local_peer_id();
        let timestamp_ms = chrono::Utc::now().timestamp_millis();

        let signable = Self::compute_envelope_signing_bytes(
            &envelope_id,
            &sender_peer_id,
            recipient_peer_id,
            timestamp_ms,
            &kind,
        )?;
        let signature = self.identity.sign(&signable);

        Ok(WireEnvelope {
            envelope_id,
            sender_peer_id,
            recipient_peer_id: recipient_peer_id.to_string(),
            timestamp_ms,
            kind,
            ed25519_signature: signature.to_bytes().to_vec(),
        })
    }

    fn verify_envelope_signature(&self, envelope: &WireEnvelope) -> CoreResult<()> {
        let signable = Self::compute_envelope_signing_bytes(
            &envelope.envelope_id,
            &envelope.sender_peer_id,
            &envelope.recipient_peer_id,
            envelope.timestamp_ms,
            &envelope.kind,
        )?;

        KeyPairIdentity::verify_peer_signature(
            &envelope.sender_peer_id,
            &signable,
            &envelope.ed25519_signature,
        )
    }

    fn compute_envelope_signing_bytes(
        envelope_id: &str,
        sender_peer_id: &str,
        recipient_peer_id: &str,
        timestamp_ms: i64,
        kind: &WirePayloadKind,
    ) -> CoreResult<Vec<u8>> {
        let kind_bytes =
            bincode::serialize(kind).map_err(|e| CoreError::Serialization(e.to_string()))?;
        let mut hasher = blake3::Hasher::new_derive_key("AETHER-WIRE-ENVELOPE-SIG-V1");
        hasher.update(envelope_id.as_bytes());
        hasher.update(sender_peer_id.as_bytes());
        hasher.update(recipient_peer_id.as_bytes());
        hasher.update(&timestamp_ms.to_be_bytes());
        hasher.update(&kind_bytes);
        Ok(hasher.finalize().as_bytes().to_vec())
    }

    async fn transmit_envelope_over_connection(
        &self,
        connection: &Connection,
        envelope: &WireEnvelope,
    ) -> CoreResult<()> {
        let payload_bytes =
            bincode::serialize(envelope).map_err(|e| CoreError::Serialization(e.to_string()))?;

        let (mut send_stream, _recv_stream) = connection
            .open_bi()
            .await
            .map_err(|e| CoreError::Network(format!("Failed to open QUIC bi-stream: {e}")))?;

        send_stream
            .write_all(&payload_bytes)
            .await
            .map_err(|e| CoreError::Network(format!("QUIC write failed: {e}")))?;
        send_stream
            .finish()
            .map_err(|e| CoreError::Network(format!("QUIC stream finish failed: {e}")))?;

        Ok(())
    }

    async fn request_response_over_connection(
        &self,
        connection: &Connection,
        request: &WireEnvelope,
    ) -> CoreResult<WireEnvelope> {
        let payload_bytes =
            bincode::serialize(request).map_err(|e| CoreError::Serialization(e.to_string()))?;

        let (mut send_stream, mut recv_stream) = connection
            .open_bi()
            .await
            .map_err(|e| CoreError::Network(format!("Failed to open QUIC bi-stream: {e}")))?;

        send_stream
            .write_all(&payload_bytes)
            .await
            .map_err(|e| CoreError::Network(format!("QUIC write failed: {e}")))?;
        send_stream
            .finish()
            .map_err(|e| CoreError::Network(format!("QUIC finish failed: {e}")))?;

        let resp_bytes = tokio::time::timeout(
            Duration::from_secs(10),
            recv_stream.read_to_end(MAX_FRAME_SIZE),
        )
        .await
        .map_err(|_| CoreError::Network("Timed out waiting for peer QUIC response".into()))?
        .map_err(|e| CoreError::Network(format!("QUIC read failed: {e}")))?;

        let envelope: WireEnvelope =
            bincode::deserialize(&resp_bytes).map_err(|e| CoreError::Serialization(e.to_string()))?;
        self.verify_envelope_signature(&envelope)?;
        Ok(envelope)
    }

    /// Gracefully closes the QUIC endpoint and terminates background listeners
    pub async fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_task.write().take() {
            handle.abort();
        }
        let _ = self.endpoint.close(0u32.into(), b"graceful-shutdown").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::WordCount;

    #[tokio::test]
    async fn test_two_p2p_nodes_local_quic_encrypted_exchange() {
        let (alice_id, _) =
            KeyPairIdentity::generate_new(WordCount::Words12, "", Some("Alice".into())).unwrap();
        let (bob_id, _) =
            KeyPairIdentity::generate_new(WordCount::Words12, "", Some("Bob".into())).unwrap();

        let alice_node = P2PNode::spawn(alice_id, 0).await.expect("Alice spawn failed");
        let bob_node = P2PNode::spawn(bob_id, 0).await.expect("Bob spawn failed");

        let mut bob_rx = bob_node.subscribe_incoming();
        let bob_ticket = bob_node.local_ticket().await.unwrap();

        alice_node
            .register_peer_hints(
                &bob_ticket.peer_id,
                bob_ticket.relay_url.as_deref(),
                &bob_ticket.direct_addresses,
            )
            .unwrap();

        let payload = ChatPayload {
            message_id: Uuid::new_v4().to_string(),
            conversation_id: "direct-alice-bob".to_string(),
            sender_peer_id: alice_node.local_peer_id(),
            recipient_peer_id: bob_node.local_peer_id(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            text: Some("Encrypted QUIC packet over Iroh P2P!".to_string()),
            reply_to_id: None,
            media_blake3_hash: None,
            media_mime: None,
            media_waveform: None,
            crdt_delta: None,
        };

        alice_node
            .send_encrypted_message(&bob_node.local_peer_id(), &payload)
            .await
            .expect("Failed to send encrypted message");

        let received = tokio::time::timeout(Duration::from_secs(5), bob_rx.recv())
            .await
            .expect("Timed out waiting for P2P message")
            .expect("Broadcast channel closed");

        assert!(received.verified_signature);
        assert_eq!(received.sender_peer_id, alice_node.local_peer_id());
        assert_eq!(
            received.chat_payload.text.as_deref(),
            Some("Encrypted QUIC packet over Iroh P2P!")
        );

        alice_node.shutdown().await;
        bob_node.shutdown().await;
    }
}
