//! The `User` record: a registered account holder.

/// A registered account holder.
pub struct User {
    pub id: String,
    pub email: String,
}

impl User {
    /// Construct a new user record.
    pub fn new(id: String, email: String) -> Self {
        Self { id, email }
    }
}
