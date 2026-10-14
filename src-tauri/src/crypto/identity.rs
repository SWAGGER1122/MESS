use crate::error::{CoreError, CoreResult};
use argon2::{Algorithm, Argon2, Params, Version};
use bip39::{Language, Mnemonic};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

/// Number of words for BIP-39 mnemonic generation (12 = 128-bit entropy, 24 = 256-bit entropy)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WordCount {
    Words12 = 12,
    Words24 = 24,
}

impl WordCount {
    pub fn entropy_bytes(&self) -> usize {
        match self {
            WordCount::Words12 => 16,
            WordCount::Words24 => 32,
        }
    }
}

/// Publicly shareable Zero-Trust identity of a user/node
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicIdentity {
    /// Base58-encoded Ed25519 public key (acts as the user's global PeerId / Account ID)
    pub peer_id: String,
    /// Hex-encoded Ed25519 verifying key (32 bytes)
    pub ed25519_public_hex: String,
    /// Hex-encoded X25519 static Diffie-Hellman key derived birationally from Ed25519 or HKDF (32 bytes)
    pub x25519_public_hex: String,
    /// Human-readable 64-bit BLAKE3 short fingerprint for UI badges
    pub short_fingerprint: String,
    /// Optional display alias signed by the user
    pub display_name: Option<String>,
}

/// Exportable bundle returned to the frontend upon registration or unlock
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityBundle {
    pub mnemonic_phrase: Option<String>,
    pub public_identity: PublicIdentity,
    pub prekey_bundle: PreKeyBundle,
}

/// Asynchronous X3DH PreKey bundle published to DHT / exchanged on first contact
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreKeyBundle {
    pub peer_id: String,
    pub identity_ed25519_hex: String,
    pub identity_x25519_hex: String,
    pub signed_prekey_x25519_hex: String,
    pub signed_prekey_signature_hex: String,
    pub one_time_prekey_x25519_hex: Option<String>,
    pub timestamp_utc: i64,
}

/// Encrypted AEAD envelope payload with Double Ratchet header metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedPayload {
    /// Sender's ephemeral X25519 ratchet public key (32 bytes hex)
    pub dh_ratchet_public_hex: String,
    /// Message index in the current sending chain
    pub msg_index: u32,
    /// Previous chain length
    pub prev_chain_len: u32,
    /// 96-bit random nonce (12 bytes hex)
    pub nonce_hex: String,
    /// ChaCha20-Poly1305 ciphertext + 16-byte Poly1305 authentication tag
    pub ciphertext: Vec<u8>,
}

/// Verification structure for Telegram-style Safety Numbers (QR & 12-group numeric blocks)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyNumberVerify {
    pub local_peer_id: String,
    pub remote_peer_id: String,
    /// 60-digit numeric safety number formatted in 12 groups of 5 digits
    pub numeric_blocks: Vec<String>,
    /// Combined BLAKE3 256-bit digest hex for QR code verification
    pub qr_payload_hex: String,
}

/// Merkle-DAG Channel Post with cryptographic signature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedChannelPost {
    /// BLAKE3 hash of (parent_hash || author_peer_id || sequence || timestamp || content_bytes)
    pub post_hash_hex: String,
    /// Hash of the previous post in the channel's Merkle-DAG (None for genesis post)
    pub parent_hash_hex: Option<String>,
    pub author_peer_id: String,
    pub sequence: u64,
    pub timestamp_ms: i64,
    pub content: Vec<u8>,
    pub ed25519_signature_hex: String,
}

/// Full local cryptographic identity holding private keys in zeroizable memory
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct KeyPairIdentity {
    /// Raw 32-byte Ed25519 secret seed
    secret_seed: [u8; 32],
    /// Raw 32-byte X25519 static secret scalar
    x25519_secret_bytes: [u8; 32],
    /// Raw 32-byte signed prekey secret scalar for X3DH
    signed_prekey_secret_bytes: [u8; 32],
    #[zeroize(skip)]
    display_name: Option<String>,
}

impl KeyPairIdentity {
    /// Generates a brand new cryptographically secure BIP-39 mnemonic phrase (12 or 24 words)
    /// and derives the deterministic Ed25519 + X25519 identity.
    pub fn generate_new(
        word_count: WordCount,
        passphrase: &str,
        display_name: Option<String>,
    ) -> CoreResult<(Self, String)> {
        let mut entropy = vec![0u8; word_count.entropy_bytes()];
        OsRng.fill_bytes(&mut entropy);

        let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
            .map_err(|e| CoreError::InvalidMnemonic(e.to_string()))?;
        entropy.zeroize();

        let phrase = mnemonic.to_string();
        let identity = Self::from_mnemonic(&phrase, passphrase, display_name)?;
        Ok((identity, phrase))
    }

    /// Restores a deterministic `KeyPairIdentity` from an existing 12/24-word BIP-39 mnemonic phrase.
    pub fn from_mnemonic(
        phrase: &str,
        passphrase: &str,
        display_name: Option<String>,
    ) -> CoreResult<Self> {
        let normalized = phrase
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();

        let mnemonic = Mnemonic::parse_in(Language::English, &normalized)
            .map_err(|e| CoreError::InvalidMnemonic(e.to_string()))?;

        // 64-byte BIP-39 seed (PBKDF2-HMAC-SHA512, 2048 rounds)
        let bip39_seed = mnemonic.to_seed(passphrase);

        // Domain-separated HKDF-SHA512 derivation for Ed25519 Identity, X25519 Static DH, and Signed PreKey
        let hk = Hkdf::<Sha512>::new(Some(b"AETHER-ZERO-TRUST-IDENTITY-V1"), &bip39_seed);

        let mut secret_seed = [0u8; 32];
        hk.expand(b"ed25519-identity-signing-key", &mut secret_seed)
            .map_err(|e| CoreError::Crypto(format!("HKDF expand Ed25519 failed: {e}")))?;

        // Convert Ed25519 seed to birationally equivalent X25519 curve25519 scalar
        let x25519_secret_bytes = Self::ed25519_seed_to_x25519_scalar(&secret_seed);

        let mut signed_prekey_secret_bytes = [0u8; 32];
        hk.expand(b"x3dh-signed-prekey-v1", &mut signed_prekey_secret_bytes)
            .map_err(|e| CoreError::Crypto(format!("HKDF expand PreKey failed: {e}")))?;

        Ok(Self {
            secret_seed,
            x25519_secret_bytes,
            signed_prekey_secret_bytes,
            display_name,
        })
    }

    /// Restores directly from a 32-byte Ed25519 secret seed
    pub fn from_secret_seed(
        secret_seed: [u8; 32],
        display_name: Option<String>,
    ) -> CoreResult<Self> {
        let x25519_secret_bytes = Self::ed25519_seed_to_x25519_scalar(&secret_seed);
        let hk = Hkdf::<Sha512>::new(Some(b"AETHER-SEED-DIRECT-V1"), &secret_seed);
        let mut signed_prekey_secret_bytes = [0u8; 32];
        hk.expand(b"x3dh-signed-prekey-v1", &mut signed_prekey_secret_bytes)
            .map_err(|e| CoreError::Crypto(format!("HKDF expand failed: {e}")))?;

        Ok(Self {
            secret_seed,
            x25519_secret_bytes,
            signed_prekey_secret_bytes,
            display_name,
        })
    }

    /// Derives a 256-bit master encryption key from a user password and salt using Argon2id
    /// (64 MiB memory, 3 iterations, 4 lanes) for SQLite WAL local storage encryption.
    pub fn derive_storage_key_argon2id(password: &str, salt_16_bytes: &[u8]) -> CoreResult<[u8; 32]> {
        if salt_16_bytes.len() < 16 {
            return Err(CoreError::Crypto(
                "Argon2id salt must be at least 16 bytes".into(),
            ));
        }
        let params = Params::new(64 * 1024, 3, 4, Some(32))
            .map_err(|e| CoreError::Crypto(format!("Argon2id params error: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

        let mut output_key = [0u8; 32];
        argon2
            .hash_password_into(password.as_bytes(), salt_16_bytes, &mut output_key)
            .map_err(|e| CoreError::Crypto(format!("Argon2id key derivation error: {e}")))?;

        Ok(output_key)
    }

    /// Converts an Ed25519 32-byte seed into a clamped Curve25519 / X25519 scalar
    /// matching the birational map between Edwards25519 and Montgomery25519.
    pub fn ed25519_seed_to_x25519_scalar(ed25519_seed: &[u8; 32]) -> [u8; 32] {
        let digest = Sha512::digest(ed25519_seed);
        let mut scalar = [0u8; 32];
        scalar.copy_from_slice(&digest[0..32]);
        scalar[0] &= 248;
        scalar[31] &= 127;
        scalar[31] |= 64;
        scalar
    }

    /// Converts a remote peer's Ed25519 public key (32 bytes) to its birationally equivalent
    /// Montgomery X25519 public key (`u = (1 + y) / (1 - y) mod p`), enabling direct E2EE
    /// encryption to any PeerID without requiring a prior round-trip.
    pub fn ed25519_pk_to_x25519_pk(ed25519_pk_bytes: &[u8; 32]) -> CoreResult<X25519PublicKey> {
        let compressed = CompressedEdwardsY(*ed25519_pk_bytes);
        let edwards_point = compressed.decompress().ok_or_else(|| {
            CoreError::InvalidPeerId("Invalid Edwards25519 curve point".to_string())
        })?;
        let montgomery = edwards_point.to_montgomery();
        Ok(X25519PublicKey::from(montgomery.to_bytes()))
    }

    /// Returns the raw 32-byte Ed25519 seed (used to initialize the Iroh QUIC Endpoint with the exact same NodeId)
    pub fn secret_seed_bytes(&self) -> [u8; 32] {
        self.secret_seed
    }

    pub fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.secret_seed)
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key().verifying_key()
    }

    pub fn x25519_static_secret(&self) -> X25519StaticSecret {
        X25519StaticSecret::from(self.x25519_secret_bytes)
    }

    pub fn x25519_public_key(&self) -> X25519PublicKey {
        X25519PublicKey::from(&self.x25519_static_secret())
    }

    /// Base58-encoded Ed25519 public key (identical to Iroh's NodeId / PeerId representation)
    pub fn peer_id(&self) -> String {
        bs58::encode(self.verifying_key().as_bytes()).into_string()
    }

    /// Builds the `PublicIdentity` descriptor
    pub fn public_identity(&self) -> PublicIdentity {
        let vk_bytes = self.verifying_key().to_bytes();
        let x_bytes = self.x25519_public_key().to_bytes();
        let b3 = blake3::hash(&vk_bytes);
        let short_fp = hex::encode(&b3.as_bytes()[0..8]).to_uppercase();

        PublicIdentity {
            peer_id: bs58::encode(vk_bytes).into_string(),
            ed25519_public_hex: hex::encode(vk_bytes),
            x25519_public_hex: hex::encode(x_bytes),
            short_fingerprint: short_fp,
            display_name: self.display_name.clone(),
        }
    }

    /// Generates a signed X3DH `PreKeyBundle` for asynchronous session initiation
    pub fn generate_prekey_bundle(&self, include_one_time_key: bool) -> PreKeyBundle {
        let spk_secret = X25519StaticSecret::from(self.signed_prekey_secret_bytes);
        let spk_public = X25519PublicKey::from(&spk_secret);
        let spk_signature = self.sign(spk_public.as_bytes());

        let one_time_prekey_x25519_hex = if include_one_time_key {
            let mut otpk_bytes = [0u8; 32];
            OsRng.fill_bytes(&mut otpk_bytes);
            let otpk_secret = X25519StaticSecret::from(otpk_bytes);
            let otpk_pub = X25519PublicKey::from(&otpk_secret);
            otpk_bytes.zeroize();
            Some(hex::encode(otpk_pub.as_bytes()))
        } else {
            None
        };

        PreKeyBundle {
            peer_id: self.peer_id(),
            identity_ed25519_hex: hex::encode(self.verifying_key().as_bytes()),
            identity_x25519_hex: hex::encode(self.x25519_public_key().as_bytes()),
            signed_prekey_x25519_hex: hex::encode(spk_public.as_bytes()),
            signed_prekey_signature_hex: hex::encode(spk_signature.to_bytes()),
            one_time_prekey_x25519_hex,
            timestamp_utc: chrono::Utc::now().timestamp(),
        }
    }

    /// Signs arbitrary bytes using the node's Ed25519 private key
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key().sign(message)
    }

    /// Verifies an Ed25519 signature against a PeerID (Base58 or 32-byte Hex)
    pub fn verify_peer_signature(
        peer_id: &str,
        message: &[u8],
        signature_bytes: &[u8],
    ) -> CoreResult<()> {
        let vk_bytes = Self::decode_peer_id_bytes(peer_id)?;
        let verifying_key = VerifyingKey::from_bytes(&vk_bytes)
            .map_err(|e| CoreError::InvalidPeerId(e.to_string()))?;
        let signature = Signature::from_slice(signature_bytes)
            .map_err(|e| CoreError::SignatureVerificationFailed(e.to_string()))?;

        verifying_key
            .verify(message, &signature)
            .map_err(|_| CoreError::SignatureVerificationFailed(peer_id.to_string()))
    }

    /// Decodes a PeerID from either Base58 or Hex into a 32-byte Ed25519 public key
    pub fn decode_peer_id_bytes(peer_id: &str) -> CoreResult<[u8; 32]> {
        let trimmed = peer_id.trim();
        let raw = if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            hex::decode(trimmed).map_err(|e| CoreError::InvalidPeerId(e.to_string()))?
        } else {
            bs58::decode(trimmed)
                .into_vec()
                .map_err(|e| CoreError::InvalidPeerId(e.to_string()))?
        };

        if raw.len() != 32 {
            return Err(CoreError::InvalidPeerId(format!(
                "Expected 32-byte public key, got {} bytes",
                raw.len()
            )));
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        Ok(arr)
    }

    /// Computes a deterministic 60-digit Safety Number (12 blocks of 5 digits) + QR digest
    /// for verifying 1-on-1 E2EE authenticity against MITM attacks.
    pub fn compute_safety_numbers(&self, remote_peer_id: &str) -> CoreResult<SafetyNumberVerify> {
        let local_bytes = self.verifying_key().to_bytes();
        let remote_bytes = Self::decode_peer_id_bytes(remote_peer_id)?;

        // Sort keys lexicographically so both peers compute the exact same Safety Number
        let (first, second) = if local_bytes <= remote_bytes {
            (local_bytes, remote_bytes)
        } else {
            (remote_bytes, local_bytes)
        };

        let mut hasher = blake3::Hasher::new_derive_key("AETHER-SAFETY-NUMBERS-V1");
        hasher.update(&first);
        hasher.update(&second);

        let mut xof = hasher.finalize_xof();
        let mut extended = [0u8; 64];
        xof.fill(&mut extended);

        let mut numeric_blocks = Vec::with_capacity(12);
        for chunk in extended.chunks_exact(5).take(12) {
            let mut val: u64 = 0;
            for &b in chunk {
                val = (val << 8) | (b as u64);
            }
            numeric_blocks.push(format!("{:05}", val % 100_000));
        }

        let qr_payload_hex = hex::encode(&extended[0..32]);

        Ok(SafetyNumberVerify {
            local_peer_id: self.peer_id(),
            remote_peer_id: remote_peer_id.to_string(),
            numeric_blocks,
            qr_payload_hex,
        })
    }

    /// Performs direct E2EE encryption to a target `peer_id` using ephemeral-static X25519 +
    /// local static-static X25519 (X3DH-lite) + HKDF-SHA256 + ChaCha20-Poly1305.
    pub fn seal_for_peer(
        &self,
        recipient_peer_id: &str,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> CoreResult<EncryptedPayload> {
        let recipient_ed_pk = Self::decode_peer_id_bytes(recipient_peer_id)?;
        let recipient_x_pk = Self::ed25519_pk_to_x25519_pk(&recipient_ed_pk)?;

        // Generate fresh ephemeral X25519 keypair for Forward Secrecy
        let mut eph_secret_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut eph_secret_bytes);
        let eph_secret = X25519StaticSecret::from(eph_secret_bytes);
        let eph_public = X25519PublicKey::from(&eph_secret);
        eph_secret_bytes.zeroize();

        // Combine DH1 (Sender Static x Recipient Static) || DH2 (Sender Ephemeral x Recipient Static)
        let dh_static = self.x25519_static_secret().diffie_hellman(&recipient_x_pk);
        let dh_ephemeral = eph_secret.diffie_hellman(&recipient_x_pk);

        let mut ikm = [0u8; 64];
        ikm[0..32].copy_from_slice(dh_static.as_bytes());
        ikm[32..64].copy_from_slice(dh_ephemeral.as_bytes());

        let hk = Hkdf::<Sha256>::new(Some(b"AETHER-E2EE-ENVELOPE-V1"), &ikm);
        let mut aead_key = [0u8; 32];
        hk.expand(b"chacha20poly1305-message-key", &mut aead_key)
            .map_err(|e| CoreError::Crypto(e.to_string()))?;
        ikm.zeroize();

        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&aead_key));
        aead_key.zeroize();

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|e| CoreError::Crypto(format!("AEAD encryption failed: {e}")))?;

        Ok(EncryptedPayload {
            dh_ratchet_public_hex: hex::encode(eph_public.as_bytes()),
            msg_index: 0,
            prev_chain_len: 0,
            nonce_hex: hex::encode(nonce_bytes),
            ciphertext,
        })
    }

    /// Opens and authenticates an `EncryptedPayload` sent by `sender_peer_id`.
    pub fn open_from_peer(
        &self,
        sender_peer_id: &str,
        encrypted: &EncryptedPayload,
        associated_data: &[u8],
    ) -> CoreResult<Vec<u8>> {
        let sender_ed_pk = Self::decode_peer_id_bytes(sender_peer_id)?;
        let sender_x_pk = Self::ed25519_pk_to_x25519_pk(&sender_ed_pk)?;

        let eph_pub_bytes = hex::decode(&encrypted.dh_ratchet_public_hex)
            .map_err(|e| CoreError::DecryptionFailed(format!("Invalid ephemeral DH hex: {e}")))?;
        if eph_pub_bytes.len() != 32 {
            return Err(CoreError::DecryptionFailed(
                "Invalid ephemeral X25519 public key length".into(),
            ));
        }
        let mut eph_arr = [0u8; 32];
        eph_arr.copy_from_slice(&eph_pub_bytes);
        let sender_eph_pk = X25519PublicKey::from(eph_arr);

        let my_x_secret = self.x25519_static_secret();
        let dh_static = my_x_secret.diffie_hellman(&sender_x_pk);
        let dh_ephemeral = my_x_secret.diffie_hellman(&sender_eph_pk);

        let mut ikm = [0u8; 64];
        ikm[0..32].copy_from_slice(dh_static.as_bytes());
        ikm[32..64].copy_from_slice(dh_ephemeral.as_bytes());

        let hk = Hkdf::<Sha256>::new(Some(b"AETHER-E2EE-ENVELOPE-V1"), &ikm);
        let mut aead_key = [0u8; 32];
        hk.expand(b"chacha20poly1305-message-key", &mut aead_key)
            .map_err(|e| CoreError::Crypto(e.to_string()))?;
        ikm.zeroize();

        let nonce_bytes = hex::decode(&encrypted.nonce_hex)
            .map_err(|e| CoreError::DecryptionFailed(format!("Invalid nonce hex: {e}")))?;
        if nonce_bytes.len() != 12 {
            return Err(CoreError::DecryptionFailed("Invalid 96-bit nonce".into()));
        }

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&aead_key));
        aead_key.zeroize();

        cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &encrypted.ciphertext,
                    aad: associated_data,
                },
            )
            .map_err(|_| {
                CoreError::DecryptionFailed(
                    "ChaCha20-Poly1305 authentication tag mismatch".to_string(),
                )
            })
    }

    /// Creates and signs a Merkle-DAG post for decentralized broadcast channels
    pub fn sign_channel_post(
        &self,
        parent_hash_hex: Option<String>,
        sequence: u64,
        content: Vec<u8>,
    ) -> SignedChannelPost {
        let timestamp_ms = chrono::Utc::now().timestamp_millis();
        let author_peer_id = self.peer_id();

        let mut hasher = blake3::Hasher::new();
        if let Some(ref parent) = parent_hash_hex {
            hasher.update(parent.as_bytes());
        }
        hasher.update(author_peer_id.as_bytes());
        hasher.update(&sequence.to_be_bytes());
        hasher.update(&timestamp_ms.to_be_bytes());
        hasher.update(&content);

        let post_hash = hasher.finalize();
        let post_hash_hex = post_hash.to_hex().to_string();
        let signature = self.sign(post_hash.as_bytes());

        SignedChannelPost {
            post_hash_hex,
            parent_hash_hex,
            author_peer_id,
            sequence,
            timestamp_ms,
            content,
            ed25519_signature_hex: hex::encode(signature.to_bytes()),
        }
    }

    /// Verifies the cryptographic integrity and Ed25519 signature of a Merkle-DAG channel post
    pub fn verify_channel_post(post: &SignedChannelPost) -> CoreResult<()> {
        let mut hasher = blake3::Hasher::new();
        if let Some(ref parent) = post.parent_hash_hex {
            hasher.update(parent.as_bytes());
        }
        hasher.update(post.author_peer_id.as_bytes());
        hasher.update(&post.sequence.to_be_bytes());
        hasher.update(&post.timestamp_ms.to_be_bytes());
        hasher.update(&post.content);

        let expected_hash = hasher.finalize();
        if expected_hash.to_hex().as_str() != post.post_hash_hex {
            return Err(CoreError::Crypto(
                "Merkle-DAG post BLAKE3 hash mismatch".into(),
            ));
        }

        let sig_bytes = hex::decode(&post.ed25519_signature_hex)
            .map_err(|e| CoreError::SignatureVerificationFailed(e.to_string()))?;

        Self::verify_peer_signature(&post.author_peer_id, expected_hash.as_bytes(), &sig_bytes)
    }
}

/// Stateful Double Ratchet session (Signal Protocol specification: Root KDF + Symmetric Chain KDF + DH Ratchet)
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DoubleRatchetSession {
    root_key: [u8; 32],
    sending_chain_key: [u8; 32],
    receiving_chain_key: [u8; 32],
    dh_sending_secret: [u8; 32],
    #[zeroize(skip)]
    dh_remote_public: Option<[u8; 32]>,
    send_count: u32,
    recv_count: u32,
    prev_send_count: u32,
}

impl DoubleRatchetSession {
    /// Initializes a Double Ratchet session from a shared 32-byte X3DH master secret
    pub fn new_from_shared_secret(
        shared_secret: [u8; 32],
        remote_dh_public: [u8; 32],
    ) -> CoreResult<Self> {
        let mut dh_sending_secret = [0u8; 32];
        OsRng.fill_bytes(&mut dh_sending_secret);

        let local_secret = X25519StaticSecret::from(dh_sending_secret);
        let remote_pub = X25519PublicKey::from(remote_dh_public);
        let dh_out = local_secret.diffie_hellman(&remote_pub);

        let (root_key, sending_chain_key) =
            Self::kdf_rk(&shared_secret, dh_out.as_bytes())?;

        Ok(Self {
            root_key,
            sending_chain_key,
            receiving_chain_key: shared_secret,
            dh_sending_secret,
            dh_remote_public: Some(remote_dh_public),
            send_count: 0,
            recv_count: 0,
            prev_send_count: 0,
        })
    }

    fn kdf_rk(root_key: &[u8; 32], dh_out: &[u8]) -> CoreResult<([u8; 32], [u8; 32])> {
        let hk = Hkdf::<Sha256>::new(Some(root_key), dh_out);
        let mut okm = [0u8; 64];
        hk.expand(b"AETHER-DOUBLE-RATCHET-RK", &mut okm)
            .map_err(|e| CoreError::Crypto(e.to_string()))?;
        let mut new_rk = [0u8; 32];
        let mut new_ck = [0u8; 32];
        new_rk.copy_from_slice(&okm[0..32]);
        new_ck.copy_from_slice(&okm[32..64]);
        okm.zeroize();
        Ok((new_rk, new_ck))
    }

    fn kdf_ck(chain_key: &[u8; 32]) -> CoreResult<([u8; 32], [u8; 32])> {
        let mut mac_mk = <HmacSha256 as Mac>::new_from_slice(chain_key)
            .map_err(|e| CoreError::Crypto(e.to_string()))?;
        mac_mk.update(&[0x01]);
        let mk_bytes = mac_mk.finalize().into_bytes();

        let mut mac_ck = <HmacSha256 as Mac>::new_from_slice(chain_key)
            .map_err(|e| CoreError::Crypto(e.to_string()))?;
        mac_ck.update(&[0x02]);
        let ck_bytes = mac_ck.finalize().into_bytes();

        let mut next_ck = [0u8; 32];
        let mut msg_key = [0u8; 32];
        next_ck.copy_from_slice(&ck_bytes);
        msg_key.copy_from_slice(&mk_bytes);
        Ok((next_ck, msg_key))
    }

    /// Ratchets the sending chain forward and encrypts a message with Forward Secrecy
    pub fn ratchet_encrypt(
        &mut self,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> CoreResult<EncryptedPayload> {
        let (next_ck, mut message_key) = Self::kdf_ck(&self.sending_chain_key)?;
        self.sending_chain_key = next_ck;

        let local_secret = X25519StaticSecret::from(self.dh_sending_secret);
        let local_public = X25519PublicKey::from(&local_secret);

        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&message_key));
        message_key.zeroize();

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: associated_data,
                },
            )
            .map_err(|e| CoreError::Crypto(format!("Double Ratchet encrypt failed: {e}")))?;

        let msg_index = self.send_count;
        self.send_count = self.send_count.wrapping_add(1);

        Ok(EncryptedPayload {
            dh_ratchet_public_hex: hex::encode(local_public.as_bytes()),
            msg_index,
            prev_chain_len: self.prev_send_count,
            nonce_hex: hex::encode(nonce_bytes),
            ciphertext,
        })
    }

    /// Ratchets the receiving chain (performing a DH ratchet step if a new remote public key is observed)
    /// and decrypts the incoming payload.
    pub fn ratchet_decrypt(
        &mut self,
        payload: &EncryptedPayload,
        associated_data: &[u8],
    ) -> CoreResult<Vec<u8>> {
        let remote_dh_vec = hex::decode(&payload.dh_ratchet_public_hex)
            .map_err(|e| CoreError::DecryptionFailed(e.to_string()))?;
        if remote_dh_vec.len() != 32 {
            return Err(CoreError::DecryptionFailed(
                "Invalid DH ratchet header length".into(),
            ));
        }
        let mut remote_dh = [0u8; 32];
        remote_dh.copy_from_slice(&remote_dh_vec);

        if self.dh_remote_public != Some(remote_dh) {
            self.prev_send_count = self.send_count;
            self.send_count = 0;
            self.recv_count = 0;
            self.dh_remote_public = Some(remote_dh);

            let local_secret = X25519StaticSecret::from(self.dh_sending_secret);
            let remote_pub = X25519PublicKey::from(remote_dh);
            let dh_recv = local_secret.diffie_hellman(&remote_pub);
            let (rk1, ck_recv) = Self::kdf_rk(&self.root_key, dh_recv.as_bytes())?;
            self.root_key = rk1;
            self.receiving_chain_key = ck_recv;

            OsRng.fill_bytes(&mut self.dh_sending_secret);
            let new_local_secret = X25519StaticSecret::from(self.dh_sending_secret);
            let dh_send = new_local_secret.diffie_hellman(&remote_pub);
            let (rk2, ck_send) = Self::kdf_rk(&self.root_key, dh_send.as_bytes())?;
            self.root_key = rk2;
            self.sending_chain_key = ck_send;
        }

        let (next_ck, mut message_key) = Self::kdf_ck(&self.receiving_chain_key)?;
        self.receiving_chain_key = next_ck;
        self.recv_count = self.recv_count.wrapping_add(1);

        let nonce_bytes = hex::decode(&payload.nonce_hex)
            .map_err(|e| CoreError::DecryptionFailed(e.to_string()))?;
        if nonce_bytes.len() != 12 {
            return Err(CoreError::DecryptionFailed("Invalid nonce length".into()));
        }

        let cipher = ChaCha20Poly1305::new(Key::from_slice(&message_key));
        message_key.zeroize();

        cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &payload.ciphertext,
                    aad: associated_data,
                },
            )
            .map_err(|_| {
                CoreError::DecryptionFailed(
                    "Double Ratchet Poly1305 tag verification failed".into(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mnemonic_generation_and_deterministic_restore() {
        let (alice1, phrase) =
            KeyPairIdentity::generate_new(WordCount::Words12, "", Some("Alice".into()))
                .expect("Failed to generate 12-word identity");
        assert_eq!(phrase.split_whitespace().count(), 12);

        let alice2 = KeyPairIdentity::from_mnemonic(&phrase, "", Some("Alice".into()))
            .expect("Failed to restore identity");
        assert_eq!(alice1.peer_id(), alice2.peer_id());
        assert_eq!(
            alice1.public_identity().x25519_public_hex,
            alice2.public_identity().x25519_public_hex
        );
    }

    #[test]
    fn test_e2ee_seal_and_open_between_peers() {
        let (alice, _) = KeyPairIdentity::generate_new(WordCount::Words12, "", None).unwrap();
        let (bob, _) = KeyPairIdentity::generate_new(WordCount::Words24, "", None).unwrap();

        let msg = b"Zero-Trust P2P message resistant to DPI";
        let aad = b"aether/p2p/v1";

        let encrypted = alice
            .seal_for_peer(&bob.peer_id(), msg, aad)
            .expect("Encryption failed");
        let decrypted = bob
            .open_from_peer(&alice.peer_id(), &encrypted, aad)
            .expect("Decryption failed");

        assert_eq!(decrypted, msg);
    }

    #[test]
    fn test_safety_numbers_symmetry_and_merkle_channel_post() {
        let (alice, _) = KeyPairIdentity::generate_new(WordCount::Words12, "", None).unwrap();
        let (bob, _) = KeyPairIdentity::generate_new(WordCount::Words12, "", None).unwrap();

        let sn_alice = alice.compute_safety_numbers(&bob.peer_id()).unwrap();
        let sn_bob = bob.compute_safety_numbers(&alice.peer_id()).unwrap();
        assert_eq!(sn_alice.numeric_blocks, sn_bob.numeric_blocks);
        assert_eq!(sn_alice.qr_payload_hex, sn_bob.qr_payload_hex);

        let post = alice.sign_channel_post(None, 1, b"Hello Decentralized Channel".to_vec());
        KeyPairIdentity::verify_channel_post(&post).expect("Channel post verification failed");
    }
}
