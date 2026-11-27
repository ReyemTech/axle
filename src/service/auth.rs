//! Bearer tokens (spec §7): one per host / harness, stored as SHA-256 hashes, revocable
//! individually, with a last-used time.

use std::fmt::Write as _;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::clock::Clock;
use crate::domain::{TokenId, TokenInfo};
use crate::store::{Store, StoreError};

/// Every token's plaintext starts with this, so leaked tokens are recognisable.
pub const TOKEN_PREFIX: &str = "axl_";

/// Random bytes per token (256 bits).
const TOKEN_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("a token label must not be empty")]
    EmptyLabel,
    #[error("no system randomness available: {0}")]
    Random(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub struct AuthService {
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
}

impl AuthService {
    pub fn new(store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Creates a token and returns its id and plaintext. The plaintext is never stored:
    /// this is the only time it is available.
    pub async fn create(&self, label: &str) -> Result<(TokenId, String), AuthError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(AuthError::EmptyLabel);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|e| AuthError::Random(e.to_string()))?;
        let plaintext = format!("{TOKEN_PREFIX}{}", hex(&bytes));
        let id = TokenId::new();
        self.store
            .insert_token(id, label, &hash(&plaintext), self.clock.now())
            .await?;
        Ok((id, plaintext))
    }

    /// The live token matching `plaintext` (recording that it was used at `now`); `None` for
    /// unknown, malformed or revoked tokens.
    pub async fn verify(
        &self,
        plaintext: &str,
        now: OffsetDateTime,
    ) -> Result<Option<TokenId>, AuthError> {
        if !plaintext.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        Ok(self.store.use_token(&hash(plaintext), now).await?)
    }

    /// Revokes the token; already-revoked tokens stay revoked at their original time.
    pub async fn revoke(&self, id: TokenId) -> Result<(), AuthError> {
        Ok(self.store.revoke_token(id, self.clock.now()).await?)
    }

    pub async fn list(&self) -> Result<Vec<TokenInfo>, AuthError> {
        Ok(self.store.list_tokens().await?)
    }
}

/// Hex SHA-256 of the full plaintext token (prefix included).
fn hash(plaintext: &str) -> String {
    hex(&Sha256::digest(plaintext.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_hex_sha256() {
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
