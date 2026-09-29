//! Password hashing for the HTTP server's `passwd.json`.
//!
//! Passwords are stored as Argon2id PHC strings (`$argon2id$v=19$...`), never
//! in plain text. `vcrs passwd` produces them.

use argon2::Argon2;
use password_hash::rand_core::OsRng;
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

use crate::error::{Result, VcsError};

/// Hash `password` with Argon2id and a fresh random salt.
pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| VcsError::ServerMisconfigured(format!("password hashing failed: {e}")))
}

/// True when `stored` is a password hash this module can verify.
pub fn is_password_hash(stored: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|h| h.algorithm.as_str().starts_with("argon2"))
}

/// Check `password` against a stored hash (constant time inside argon2).
pub fn verify_password(stored: &str, password: &str) -> bool {
    PasswordHash::new(stored).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_are_salted() {
        let a = hash_password("secret").unwrap();
        let b = hash_password("secret").unwrap();
        assert_ne!(a, b, "salted");
        assert!(is_password_hash(&a));
        assert!(verify_password(&a, "secret"));
        assert!(!verify_password(&a, "Secret"));
        assert!(!is_password_hash("secret"));
        assert!(
            !verify_password("secret", "secret"),
            "plain text never verifies"
        );
    }
}
