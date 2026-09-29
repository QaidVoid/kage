//! The bearer token guarding the remote endpoint.
//!
//! The token is 256 random bits, hex encoded. It lives in a private
//! file next to the other credentials, survives restarts, and is
//! replaced whole by rotation. Comparison against a presented value is
//! constant time, and the value is never rendered: [`Token::as_str`]
//! is the only way out, reserved for the startup connect line.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use kage_core::fsutil::atomic_write_private;

/// Raw token length: 256 random bits.
const TOKEN_BYTES: usize = 32;

/// The secret a remote client must present to connect.
pub struct Token {
    value: String,
}

impl Token {
    /// Loads the token stored at `path`, generating and storing a new
    /// one with private permissions when the file is missing or empty.
    ///
    /// # Errors
    ///
    /// Fails when a fresh token cannot be generated or written.
    pub fn load_or_create(path: &Path) -> io::Result<Token> {
        if let Ok(stored) = fs::read_to_string(path) {
            let stored = stored.trim();
            if !stored.is_empty() {
                return Ok(Token {
                    value: stored.to_owned(),
                });
            }
        }
        Self::rotate(path)
    }

    /// Generates a fresh token, replaces the file at `path` with it,
    /// and returns it.
    ///
    /// # Errors
    ///
    /// Fails when no random source is available or the file cannot be
    /// written.
    pub fn rotate(path: &Path) -> io::Result<Token> {
        let token = Self::generate()?;
        atomic_write_private(path, token.value.as_bytes())?;
        Ok(token)
    }

    /// Compares a presented value against the token. Equal lengths
    /// compare in time independent of the content; a length mismatch
    /// reports `false` without scanning.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let presented = presented.as_bytes();
        let actual = self.value.as_bytes();
        if presented.len() != actual.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in presented.iter().zip(actual) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// The token itself. Never log the result; the one intended use is
    /// the startup connect line.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    fn generate() -> io::Result<Token> {
        let mut raw = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(format!("random source: {e}")))?;
        Ok(Token {
            value: hex::encode(raw),
        })
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token([redacted])")
    }
}

#[cfg(test)]
pub(crate) fn with_value(value: String) -> Token {
    Token { value }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn token_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-token");
        (dir, path)
    }

    fn mode(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn create_reuse_and_rotate() {
        let (_dir, path) = token_file();
        let first = Token::load_or_create(&path).unwrap();
        assert_eq!(first.as_str().len(), 64);
        assert!(
            first.as_str().bytes().all(|b| b.is_ascii_hexdigit()),
            "{first:?}"
        );
        assert_eq!(mode(&path), 0o600);

        let second = Token::load_or_create(&path).unwrap();
        assert_eq!(first.as_str(), second.as_str(), "restart must reuse");

        let third = Token::rotate(&path).unwrap();
        assert_ne!(first.as_str(), third.as_str(), "rotate must replace");
        assert_eq!(mode(&path), 0o600);
        let fourth = Token::load_or_create(&path).unwrap();
        assert_eq!(third.as_str(), fourth.as_str());
    }

    #[test]
    fn empty_file_is_replaced() {
        let (_dir, path) = token_file();
        fs::write(&path, b"  \n").unwrap();
        let token = Token::load_or_create(&path).unwrap();
        assert_eq!(token.as_str().len(), 64);
    }

    #[test]
    fn debug_redacts_the_value() {
        let (_dir, path) = token_file();
        let token = Token::load_or_create(&path).unwrap();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(token.as_str()), "{rendered}");
    }

    #[test]
    fn matches_in_constant_time_shape() {
        let token = Token {
            value: "ab".repeat(32),
        };
        assert!(token.matches(&"ab".repeat(32)));
        assert!(!token.matches(&format!("{}ac", "ab".repeat(31))));
        assert!(!token.matches("shorter"));
        assert!(!token.matches(&"ab".repeat(33)));
    }
}
