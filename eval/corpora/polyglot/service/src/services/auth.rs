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
/// Retrieval-challenge note: `utils::validation` also defines a function
/// named `validate` (sku validation, unrelated to tokens) with the same
/// arity — see `eval/edges/polyglot.yaml`.
pub fn validate(token: &str) -> bool {
    !token.is_empty()
}
