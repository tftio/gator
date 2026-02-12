//! Scoped token generation and validation for agent-mode authentication.
//!
//! Tokens are HMAC-SHA256 based, scoped to a (task_id, attempt) pair.
//! Format: `gator_at_<task_id>_<attempt>_<hmac_hex>`

pub mod guard;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Token prefix used to identify gator agent tokens.
const TOKEN_PREFIX: &str = "gator_at_";

/// Errors that can occur during token operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token format: {0}")]
    InvalidFormat(String),

    #[error("invalid task ID in token: {0}")]
    InvalidTaskId(String),

    #[error("invalid attempt number in token: {0}")]
    InvalidAttempt(String),

    #[error("token HMAC verification failed")]
    HmacMismatch,

    #[error("missing token secret")]
    MissingSecret,
}

/// Configuration for token generation and validation.
#[derive(Debug, Clone)]
pub struct TokenConfig {
    /// The HMAC secret key bytes.
    pub secret: Vec<u8>,
}

impl TokenConfig {
    /// Create a new TokenConfig with the given secret.
    pub fn new(secret: Vec<u8>) -> Self {
        Self { secret }
    }

    /// Create a TokenConfig from the `GATOR_TOKEN_SECRET` environment variable.
    ///
    /// The value must be a hex-encoded string (as written by `gator init`
    /// and forwarded by the orchestrator). Returns an error if the variable
    /// is missing or contains invalid hex.
    pub fn from_env() -> Result<Self, TokenError> {
        let secret_hex =
            std::env::var("GATOR_TOKEN_SECRET").map_err(|_| TokenError::MissingSecret)?;
        let secret = hex::decode(&secret_hex).map_err(|e| {
            TokenError::InvalidFormat(format!("GATOR_TOKEN_SECRET is not valid hex: {e}"))
        })?;
        Ok(Self::new(secret))
    }
}

/// Claims extracted from a validated token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    /// The task ID this token is scoped to.
    pub task_id: Uuid,
    /// The attempt number this token is scoped to.
    pub attempt: u32,
}

/// Generate a scoped agent token for a given task and attempt.
///
/// The token format is: `gator_at_<task_id>_<attempt>_<hmac_hex>`
/// where the HMAC-SHA256 is computed over `<task_id>:<attempt>`.
pub fn generate_token(config: &TokenConfig, task_id: Uuid, attempt: u32) -> String {
    let message = format!("{task_id}:{attempt}");
    let mac = compute_hmac(&config.secret, message.as_bytes());
    let hmac_hex = hex::encode(mac);
    format!("{TOKEN_PREFIX}{task_id}_{attempt}_{hmac_hex}")
}

/// Validate a scoped agent token and extract its claims.
///
/// This function:
/// 1. Parses the token format
/// 2. Recomputes the HMAC
/// 3. Uses constant-time comparison to verify the HMAC
/// 4. Returns the extracted claims on success
pub fn validate_token(config: &TokenConfig, token: &str) -> Result<TokenClaims, TokenError> {
    // Strip prefix
    let rest = token.strip_prefix(TOKEN_PREFIX).ok_or_else(|| {
        TokenError::InvalidFormat("token must start with 'gator_at_'".to_string())
    })?;

    // Parse the components: <task_id>_<attempt>_<hmac_hex>
    // A UUID is 36 chars (8-4-4-4-12). We parse the UUID first (36 chars),
    // then expect underscore, then attempt, then underscore, then hmac_hex.
    let (task_id_str, after_task_id) = parse_uuid_prefix(rest)?;

    let task_id =
        Uuid::parse_str(task_id_str).map_err(|e| TokenError::InvalidTaskId(e.to_string()))?;

    // after_task_id should start with '_'
    let after_underscore = after_task_id.strip_prefix('_').ok_or_else(|| {
        TokenError::InvalidFormat("expected underscore after task_id".to_string())
    })?;

    // Split on the next underscore to get attempt and hmac
    let (attempt_str, hmac_hex) = after_underscore.split_once('_').ok_or_else(|| {
        TokenError::InvalidFormat("expected underscore between attempt and hmac".to_string())
    })?;

    let attempt: u32 = attempt_str
        .parse()
        .map_err(|e: std::num::ParseIntError| TokenError::InvalidAttempt(e.to_string()))?;

    // Decode the provided HMAC
    let provided_mac = hex::decode(hmac_hex)
        .map_err(|e| TokenError::InvalidFormat(format!("invalid hex in hmac: {e}")))?;

    // Recompute and verify HMAC using constant-time comparison
    let message = format!("{task_id}:{attempt}");
    verify_hmac_constant_time(&config.secret, message.as_bytes(), &provided_mac)?;

    Ok(TokenClaims { task_id, attempt })
}

/// Parse a UUID from the beginning of a string.
/// Returns (uuid_str, remainder).
fn parse_uuid_prefix(s: &str) -> Result<(&str, &str), TokenError> {
    // A standard UUID is 36 characters: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    if s.len() < 36 {
        return Err(TokenError::InvalidFormat(
            "token too short to contain a valid UUID".to_string(),
        ));
    }
    Ok(s.split_at(36))
}

/// Compute HMAC-SHA256 over the given message with the given key.
fn compute_hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// Verify HMAC using constant-time comparison.
///
/// This uses the `hmac` crate's `verify_slice` method which is
/// designed to be constant-time to prevent timing attacks.
fn verify_hmac_constant_time(
    key: &[u8],
    message: &[u8],
    expected_mac: &[u8],
) -> Result<(), TokenError> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(message);
    mac.verify_slice(expected_mac)
        .map_err(|_| TokenError::HmacMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> TokenConfig {
        TokenConfig::new(b"test-secret-key-for-gator".to_vec())
    }

    #[test]
    fn generate_token_has_correct_format() {
        let config = test_config();
        let task_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let attempt = 1;

        let token = generate_token(&config, task_id, attempt);

        assert!(
            token.starts_with("gator_at_"),
            "token must start with gator_at_ prefix"
        );
        assert!(
            token.contains(&task_id.to_string()),
            "token must contain task_id"
        );
        assert!(token.contains("_1_"), "token must contain attempt number");

        // Verify the HMAC hex portion is 64 chars (SHA-256 = 32 bytes = 64 hex chars)
        let rest = token.strip_prefix("gator_at_").unwrap();
        let parts_after_uuid = rest[36..].strip_prefix('_').unwrap();
        let (_attempt_str, hmac_hex) = parts_after_uuid.split_once('_').unwrap();
        assert_eq!(hmac_hex.len(), 64, "HMAC-SHA256 hex should be 64 chars");
    }

    #[test]
    fn generate_and_validate_roundtrip() {
        let config = test_config();
        let task_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let attempt = 3;

        let token = generate_token(&config, task_id, attempt);
        let claims = validate_token(&config, &token).unwrap();

        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, attempt);
    }

    #[test]
    fn validate_with_zero_attempt() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let attempt = 0;

        let token = generate_token(&config, task_id, attempt);
        let claims = validate_token(&config, &token).unwrap();

        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, 0);
    }

    #[test]
    fn validate_with_large_attempt() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let attempt = 999;

        let token = generate_token(&config, task_id, attempt);
        let claims = validate_token(&config, &token).unwrap();

        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, 999);
    }

    #[test]
    fn reject_tampered_hmac() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        // Tamper with the last character of the HMAC
        let mut tampered = token.clone();
        let last_char = tampered.pop().unwrap();
        let replacement = if last_char == 'a' { 'b' } else { 'a' };
        tampered.push(replacement);

        let result = validate_token(&config, &tampered);
        assert!(result.is_err(), "tampered token must be rejected");
        assert!(
            matches!(result.unwrap_err(), TokenError::HmacMismatch),
            "error must be HmacMismatch"
        );
    }

    #[test]
    fn reject_tampered_task_id() {
        let config = test_config();
        let task_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let token = generate_token(&config, task_id, 1);

        // Replace task_id in the token with a different one
        let other_id = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440000").unwrap();
        let tampered = token.replace(&task_id.to_string(), &other_id.to_string());

        let result = validate_token(&config, &tampered);
        assert!(
            result.is_err(),
            "token with tampered task_id must be rejected"
        );
    }

    #[test]
    fn reject_tampered_attempt() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        // Replace _1_ with _2_ in the token (after the UUID)
        let prefix_and_uuid = &token[..TOKEN_PREFIX.len() + 36];
        let after_uuid = &token[TOKEN_PREFIX.len() + 36..];
        let tampered_after = after_uuid.replacen("_1_", "_2_", 1);
        let tampered = format!("{prefix_and_uuid}{tampered_after}");

        let result = validate_token(&config, &tampered);
        assert!(
            result.is_err(),
            "token with tampered attempt must be rejected"
        );
    }

    #[test]
    fn reject_wrong_secret() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        let wrong_config = TokenConfig::new(b"wrong-secret-key".to_vec());
        let result = validate_token(&wrong_config, &token);
        assert!(
            result.is_err(),
            "token validated with wrong secret must be rejected"
        );
        assert!(matches!(result.unwrap_err(), TokenError::HmacMismatch));
    }

    #[test]
    fn reject_empty_token() {
        let config = test_config();
        let result = validate_token(&config, "");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::InvalidFormat(_)));
    }

    #[test]
    fn reject_wrong_prefix() {
        let config = test_config();
        let result = validate_token(&config, "wrong_prefix_abc");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::InvalidFormat(_)));
    }

    #[test]
    fn reject_truncated_token() {
        let config = test_config();
        let result = validate_token(&config, "gator_at_short");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::InvalidFormat(_)));
    }

    #[test]
    fn reject_invalid_uuid() {
        let config = test_config();
        let result = validate_token(&config, "gator_at_not-a-valid-uuid-at-all-noooooo_1_abcdef");
        assert!(result.is_err());
    }

    #[test]
    fn reject_invalid_attempt_number() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = format!("gator_at_{task_id}_abc_deadbeef");
        let result = validate_token(&config, &token);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::InvalidAttempt(_)));
    }

    #[test]
    fn reject_invalid_hex_in_hmac() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = format!("gator_at_{task_id}_1_zzzz-not-valid-hex!");
        let result = validate_token(&config, &token);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::InvalidFormat(_)));
    }

    #[test]
    fn different_tasks_produce_different_tokens() {
        let config = test_config();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let token1 = generate_token(&config, id1, 1);
        let token2 = generate_token(&config, id2, 1);

        assert_ne!(token1, token2);
    }

    #[test]
    fn different_attempts_produce_different_tokens() {
        let config = test_config();
        let task_id = Uuid::new_v4();

        let token1 = generate_token(&config, task_id, 1);
        let token2 = generate_token(&config, task_id, 2);

        assert_ne!(token1, token2);
    }

    #[test]
    fn same_inputs_produce_same_token() {
        let config = test_config();
        let task_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let token1 = generate_token(&config, task_id, 1);
        let token2 = generate_token(&config, task_id, 1);

        assert_eq!(
            token1, token2,
            "same inputs must produce deterministic token"
        );
    }

    #[test]
    fn constant_time_verification_path() {
        // Verify that both valid and invalid tokens go through the
        // verify_hmac_constant_time code path (which uses hmac's
        // verify_slice for constant-time comparison).
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        // Valid token should succeed
        assert!(validate_token(&config, &token).is_ok());

        // A token with a completely wrong HMAC (all zeros) should fail
        // through the same constant-time path
        let wrong_hmac = "0".repeat(64);
        let wrong_token = format!("gator_at_{task_id}_1_{wrong_hmac}");
        let result = validate_token(&config, &wrong_token);
        assert!(matches!(result.unwrap_err(), TokenError::HmacMismatch));

        // A token with an HMAC that differs only in the last byte should fail
        // through the same constant-time path
        let rest = token.strip_prefix("gator_at_").unwrap();
        let hmac_start = rest.rfind('_').unwrap() + 1;
        let hmac_hex = &rest[hmac_start..];
        let mut bytes = hex::decode(hmac_hex).unwrap();
        bytes[31] ^= 0x01; // flip one bit in the last byte
        let modified_hmac = hex::encode(bytes);
        let near_miss_token = format!("gator_at_{task_id}_1_{modified_hmac}");
        let result = validate_token(&config, &near_miss_token);
        assert!(matches!(result.unwrap_err(), TokenError::HmacMismatch));
    }

    #[test]
    fn token_config_from_env_missing() {
        // Test that missing env var produces MissingSecret error
        // SAFETY: test-only; env var manipulation is safe in single-threaded tests.
        unsafe { std::env::remove_var("GATOR_TOKEN_SECRET") };
        let result = TokenConfig::from_env();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TokenError::MissingSecret));
    }
}
