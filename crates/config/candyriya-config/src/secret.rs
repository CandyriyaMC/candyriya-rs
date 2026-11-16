//! Forwarding secret handling.
//!
//! The proxy needs a shared 32-byte secret to prove to backends that the
//! player connection really came from us (modern forwarding). The secret is
//! stored base64-encoded in a small text file next to the configuration.
//! This module owns the file dance: read an existing secret, or generate one
//! on first run and lock it down.

use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rand::RngCore;
use zeroize::Zeroize;

use crate::error::ConfigError;

/// Length of the randomly generated forwarding secret, in bytes.
pub const FORWARDING_SECRET_LEN: usize = 32;

/// The forwarding secret.
///
/// Decodes to exactly the bytes read from (or written to) the secret file.
/// The value is zeroized when dropped and its `Debug` implementation prints
/// `ForwardingSecret(***)`, so the secret can never leak into logs or crash
/// reports.
#[derive(Clone, PartialEq, Eq)]
pub struct ForwardingSecret(Vec<u8>);

impl ForwardingSecret {
    /// Wraps raw secret bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Raw secret bytes. Handle with care; never log them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Secret length in bytes (32 for generated secrets).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when no secret is held (only possible on [`crate::Config::default`]).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for ForwardingSecret {
    /// Empty placeholder secret. Loading via [`crate::Config::load`] always
    /// resolves a real secret from disk (or generates one).
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl fmt::Debug for ForwardingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the bytes, not even their length in debug output
        f.write_str("ForwardingSecret(***)")
    }
}

impl Drop for ForwardingSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Generates a fresh 32-byte secret.
fn generate_secret_bytes() -> [u8; FORWARDING_SECRET_LEN] {
    let mut bytes = [0u8; FORWARDING_SECRET_LEN];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Decodes a secret file's contents. Tolerates surrounding whitespace and a
/// trailing newline; requires exactly [`FORWARDING_SECRET_LEN`] bytes.
fn decode_secret_file(contents: &str) -> Result<ForwardingSecret, ConfigError> {
    let trimmed = contents.trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|error| ConfigError::Secret(format!("invalid base64 in secret file: {error}")))?;
    if bytes.len() != FORWARDING_SECRET_LEN {
        return Err(ConfigError::Secret(format!(
            "expected {FORWARDING_SECRET_LEN} secret bytes, found {}",
            bytes.len()
        )));
    }
    Ok(ForwardingSecret::new(bytes))
}

/// Restricts the just-written secret file to the owner.
///
/// On Unix this sets mode `0o600`. On Windows the file is created with the
/// directory's default ACL: a strictly owner-only ACL cannot be attached
/// from safe code here, so we log a loud warning instead of pretending.
fn protect_secret_file(path: &Path, _file: &std::fs::File) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, permissions).map_err(|error| {
            ConfigError::Secret(format!(
                "failed to set permissions 0600 on {}: {error}",
                path.display()
            ))
        })?;
    }
    #[cfg(not(unix))]
    {
        tracing::warn!(
            "secret file {} was created with inherited directory permissions; \
             on Unix it would be chmod 0600. Store it in a user-private directory or \
             tighten the ACL manually.",
            path.display()
        );
        let _ = _file;
    }
    Ok(())
}

/// Writes a new secret file with restrictive permissions.
///
/// Uses `create_new` so an existing secret is never silently overwritten;
/// if another process won the race and created the file, we re-read it
/// instead of clobbering it.
fn write_secret_file(path: &Path) -> Result<ForwardingSecret, ConfigError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            ConfigError::Secret(format!(
                "failed to create directory {}: {error}",
                parent.display()
            ))
        })?;
    }

    let bytes = generate_secret_bytes();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);

    // on unix the file is born with mode 0600; nothing sensitive ever
    // touches disk with wider permissions
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(path) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(encoded.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .map_err(|error| {
                    ConfigError::Secret(format!(
                        "failed to write secret file {}: {error}",
                        path.display()
                    ))
                })?;
            protect_secret_file(path, &file)?;
            tracing::warn!(
                "no forwarding secret found at {}; generated a new one (share it with your backends)",
                path.display()
            );
            Ok(ForwardingSecret::new(bytes.to_vec()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // someone else created it between the check and now; use theirs
            let contents = std::fs::read_to_string(path).map_err(|read_error| {
                ConfigError::Secret(format!(
                    "failed to read secret file {}: {read_error}",
                    path.display()
                ))
            })?;
            decode_secret_file(&contents)
        }
        Err(error) => Err(ConfigError::Secret(format!(
            "failed to create secret file {}: {error}",
            path.display()
        ))),
    }
}

/// Resolves the configured secret file relative to the config file's
/// directory (absolute paths are used as-is).
pub(crate) fn secret_file_path(config_path: &Path, forwarding_secret_file: &str) -> PathBuf {
    let configured = Path::new(forwarding_secret_file);
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    let base = match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    base.join(configured)
}

/// Reads the forwarding secret, or generates and writes one when the file
/// is missing. An existing but malformed secret is a hard error: silently
/// replacing it would strand every backend that already trusts it.
pub(crate) fn load_or_create_secret(
    config_path: &Path,
    forwarding_secret_file: &str,
) -> Result<ForwardingSecret, ConfigError> {
    let path = secret_file_path(config_path, forwarding_secret_file);
    if path.exists() {
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            ConfigError::Secret(format!(
                "failed to read secret file {}: {error}",
                path.display()
            ))
        })?;
        return decode_secret_file(&contents);
    }
    write_secret_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_is_32_bytes_and_base64() {
        let bytes = generate_secret_bytes();
        assert_eq!(bytes.len(), FORWARDING_SECRET_LEN);
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded.len(), FORWARDING_SECRET_LEN);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        // 16 bytes, valid base64 but not the required 32
        let encoded = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
        let error = decode_secret_file(&encoded).unwrap_err();
        assert!(error.to_string().contains("expected 32"));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_secret_file("!!! not base64 !!!").is_err());
    }

    #[test]
    fn debug_redacts_bytes() {
        let secret = ForwardingSecret::new(vec![1, 2, 3, 4]);
        assert_eq!(format!("{secret:?}"), "ForwardingSecret(***)");
    }
}
