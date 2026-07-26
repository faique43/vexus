//! HTTP-adjacent handlers for user endpoints.

use crate::services::auth::register_user;
use crate::utils::validation::validate_display_name;

/// Handle a "register user" request.
pub fn register_user_endpoint(email: &str, display_name: &str) -> String {
    if !validate_display_name(display_name) {
        panic!("invalid display name: {display_name}");
    }
    register_user(email, display_name)
}
