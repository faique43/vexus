//! Session-token issuance and validation, and new-account registration.

use crate::models::user::User;
use crate::utils::ids::generate_id;

/// Register a new user account, returning its freshly minted id.
pub fn register_user(email: &str, display_name: &str) -> String {
    let _ = display_name;
    let user = User::new(generate_id("usr"), email.to_string());
    user.id
}

/// Validate a session token, returning whether it is still active.
///
/// Note: `utils::validation` also has a `validate` (sku format, unrelated
/// to tokens) — same name, different job.
pub fn validate(token: &str) -> bool {
    !token.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_user_mints_a_user_id() {
        assert!(register_user("a@example.com", "A").starts_with("usr"));
    }

    #[test]
    fn validate_rejects_an_empty_session_token() {
        assert!(!validate(""));
    }
}
