//! Agent mode guards for CLI command access control.
//!
//! The guard system enforces the trust model:
//! - Operator mode (default): full command surface, no agent token set
//! - Agent mode (GATOR_AGENT_TOKEN set): restricted to task/check/progress/done

use super::{TokenClaims, TokenConfig, TokenError, validate_token};

/// Environment variable name for the agent token.
pub const AGENT_TOKEN_ENV: &str = "GATOR_AGENT_TOKEN";

/// Errors from mode guard checks.
#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    #[error("this command is not available in agent mode")]
    AgentModeBlocked,

    #[error("this command requires agent mode (GATOR_AGENT_TOKEN must be set)")]
    NotInAgentMode,

    #[error("invalid agent token: {0}")]
    InvalidToken(#[from] TokenError),
}

/// Require that we are NOT in agent mode (i.e., GATOR_AGENT_TOKEN is not set).
///
/// This is used to guard operator-only commands. If an agent token is
/// present in the environment, this function returns an error to prevent
/// agents from executing operator commands.
pub fn require_operator_mode() -> Result<(), GuardError> {
    require_operator_mode_with_value(std::env::var(AGENT_TOKEN_ENV).ok())
}

/// Check operator mode given an explicit token value (testable without env vars).
///
/// Returns `Err(GuardError::AgentModeBlocked)` if `token_value` is `Some`.
pub fn require_operator_mode_with_value(token_value: Option<String>) -> Result<(), GuardError> {
    if token_value.is_some() {
        return Err(GuardError::AgentModeBlocked);
    }
    Ok(())
}

/// Require that we ARE in agent mode with a valid token.
///
/// This reads `GATOR_AGENT_TOKEN` from the environment, validates it
/// against the provided TokenConfig, and returns the extracted claims
/// (task_id and attempt) on success.
///
/// Returns an error if:
/// - The agent token environment variable is not set
/// - The token is malformed or has an invalid HMAC
pub fn require_agent_mode(config: &TokenConfig) -> Result<TokenClaims, GuardError> {
    let token = std::env::var(AGENT_TOKEN_ENV).map_err(|_| GuardError::NotInAgentMode)?;
    require_agent_mode_with_value(config, &token)
}

/// Check agent mode given an explicit token string (testable without env vars).
///
/// Validates the token against the provided `TokenConfig` and returns claims
/// on success.
pub fn require_agent_mode_with_value(
    config: &TokenConfig,
    token: &str,
) -> Result<TokenClaims, GuardError> {
    let claims = validate_token(config, token)?;
    Ok(claims)
}

/// Check whether we are currently in agent mode (token is set in env).
///
/// This does NOT validate the token; it only checks for its presence.
/// Use `require_agent_mode` for full validation.
pub fn is_agent_mode() -> bool {
    std::env::var(AGENT_TOKEN_ENV).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{TokenConfig, generate_token};
    use std::sync::Mutex;
    use uuid::Uuid;

    fn test_config() -> TokenConfig {
        TokenConfig::new(b"guard-test-secret".to_vec())
    }

    // Mutex to serialize tests that touch environment variables.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // --- Tests using the inner (env-free) functions ---

    #[test]
    fn operator_mode_succeeds_when_no_token() {
        let result = require_operator_mode_with_value(None);
        assert!(result.is_ok());
    }

    #[test]
    fn operator_mode_fails_when_token_present() {
        let result = require_operator_mode_with_value(Some("any-value".to_string()));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::AgentModeBlocked));
    }

    #[test]
    fn agent_mode_succeeds_with_valid_token() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let attempt = 2;
        let token = generate_token(&config, task_id, attempt);

        let claims = require_agent_mode_with_value(&config, &token).unwrap();
        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, attempt);
    }

    #[test]
    fn agent_mode_fails_with_invalid_token() {
        let config = test_config();
        let result = require_agent_mode_with_value(&config, "gator_at_bogus_token");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::InvalidToken(_)));
    }

    #[test]
    fn agent_mode_fails_with_tampered_token() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let mut token = generate_token(&config, task_id, 1);

        // Tamper with the token
        let last = token.pop().unwrap();
        token.push(if last == 'a' { 'b' } else { 'a' });

        let result = require_agent_mode_with_value(&config, &token);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::InvalidToken(_)));
    }

    #[test]
    fn agent_mode_fails_with_wrong_secret() {
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        let wrong_config = TokenConfig::new(b"completely-different-secret".to_vec());
        let result = require_agent_mode_with_value(&wrong_config, &token);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::InvalidToken(_)));
    }

    // --- Tests that exercise the actual env-reading public API ---
    // These are serialized behind ENV_MUTEX to avoid race conditions.

    #[test]
    fn env_require_operator_mode_succeeds_without_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        let result = require_operator_mode();
        assert!(result.is_ok());
    }

    #[test]
    fn env_require_operator_mode_fails_with_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let token = generate_token(&config, task_id, 1);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        let result = require_operator_mode();
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::AgentModeBlocked));
    }

    #[test]
    fn env_require_agent_mode_succeeds_with_valid_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        let task_id = Uuid::new_v4();
        let attempt = 2;
        let token = generate_token(&config, task_id, attempt);

        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, &token) };
        let result = require_agent_mode(&config);
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };

        let claims = result.unwrap();
        assert_eq!(claims.task_id, task_id);
        assert_eq!(claims.attempt, attempt);
    }

    #[test]
    fn env_require_agent_mode_fails_without_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let config = test_config();
        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };

        let result = require_agent_mode(&config);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), GuardError::NotInAgentMode));
    }

    #[test]
    fn env_is_agent_mode_true_when_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::set_var(AGENT_TOKEN_ENV, "any-value") };
        let result = is_agent_mode();
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };

        assert!(result);
    }

    #[test]
    fn env_is_agent_mode_false_when_unset() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: serialized by mutex, test-only code.
        unsafe { std::env::remove_var(AGENT_TOKEN_ENV) };
        assert!(!is_agent_mode());
    }

    // --- Error display tests (no env vars needed) ---

    #[test]
    fn guard_error_display_messages() {
        let blocked = GuardError::AgentModeBlocked;
        assert_eq!(
            blocked.to_string(),
            "this command is not available in agent mode"
        );

        let not_agent = GuardError::NotInAgentMode;
        assert_eq!(
            not_agent.to_string(),
            "this command requires agent mode (GATOR_AGENT_TOKEN must be set)"
        );

        let invalid = GuardError::InvalidToken(TokenError::HmacMismatch);
        assert_eq!(
            invalid.to_string(),
            "invalid agent token: token HMAC verification failed"
        );
    }
}
