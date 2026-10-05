//! The bearer token guarding the remote endpoint.
//!
//! The token is 256 random bits, hex encoded. It lives in a private
//! file next to the other credentials, survives restarts, and is
//! replaced whole by rotation. A stored value that is not 64 ASCII
//! hex characters is refused on load, so a truncated or edited file
//! fails loudly instead of answering 401 to every paired client.
//! Comparison against a presented value is constant time, and the
//! value is never rendered: [`Token::as_str`] is the only way out,
//! reserved for the startup connect line.

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
    /// Fails when the stored value is not 64 ASCII hex characters
    /// after trimming, naming the path and suggesting
    /// `--rotate-token`, and when a fresh token cannot be generated or
    /// written.
    pub fn load_or_create(path: &Path) -> io::Result<Token> {
        if let Ok(stored) = fs::read_to_string(path) {
            let stored = stored.trim();
            if stored.is_empty() {
                return Self::rotate(path);
            }
            if !is_token_shape(stored) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds {} characters that are not a 64-character hex token; \
                         replace it with `kage serve --rotate-token`",
                        path.display(),
                        stored.chars().count()
                    ),
                ));
            }
            return Ok(Token {
                value: stored.to_owned(),
            });
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

/// Whether `value` is the shape [`Token::generate`] writes: exactly
/// [`TOKEN_BYTES`] hex-encoded characters.
fn is_token_shape(value: &str) -> bool {
    value.len() == TOKEN_BYTES * 2 && value.bytes().all(|b| b.is_ascii_hexdigit())
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
    use super::*;

    fn token_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-token");
        (dir, path)
    }

    /// Only the owner may read the file, where the platform has modes.
    fn assert_private(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        #[cfg(not(unix))]
        let _ = path;
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
        assert_private(&path);

        let second = Token::load_or_create(&path).unwrap();
        assert_eq!(first.as_str(), second.as_str(), "restart must reuse");

        let third = Token::rotate(&path).unwrap();
        assert_ne!(first.as_str(), third.as_str(), "rotate must replace");
        assert_private(&path);
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
    fn a_valid_file_loads_unchanged_and_padding_is_trimmed() {
        let (_dir, path) = token_file();
        fs::write(&path, format!("{}\n", "ab".repeat(32))).unwrap();
        let token = Token::load_or_create(&path).unwrap();
        assert_eq!(token.as_str(), "ab".repeat(32));
    }

    #[test]
    fn a_short_file_is_refused_and_names_the_path() {
        let (dir, path) = token_file();
        fs::write(&path, "abcd\n").unwrap();
        let err = Token::load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let text = err.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("--rotate-token"), "{text}");
        assert_eq!(
            fs::read_to_string(dir.path().join("remote-token")).unwrap(),
            "abcd\n",
            "a refused file is left for rotation to replace"
        );
    }

    #[test]
    fn a_non_hex_file_is_refused() {
        let (_dir, path) = token_file();
        fs::write(&path, "z".repeat(64)).unwrap();
        let err = Token::load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("--rotate-token"));
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
