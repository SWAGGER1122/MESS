use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("Cryptographic error: {0}")]
    Crypto(String),

    #[error("Invalid BIP-39 mnemonic phrase: {0}")]
    InvalidMnemonic(String),

    #[error("Invalid PeerId / PublicKey format: {0}")]
    InvalidPeerId(String),

    #[error("Signature verification failed for envelope from peer {0}")]
    SignatureVerificationFailed(String),

    #[error("Decryption error (AEAD ChaCha20-Poly1305 authentication failed): {0}")]
    DecryptionFailed(String),

    #[error("P2P Network error: {0}")]
    Network(String),

    #[error("NAT Traversal / Connection timeout reaching peer {0}")]
    PeerUnreachable(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Storage / Database error: {0}")]
    Database(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Serialize for CoreError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type CoreResult<T> = Result<T, CoreError>;
