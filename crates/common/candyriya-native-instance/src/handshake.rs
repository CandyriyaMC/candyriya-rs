//! Fail-closed validation of the init handshake.
//!
//! Two checks live here, one for each side of the boundary:
//!
//! - [`validate_handshake`] — *before* the plugin is asked anything: the
//!   caller speaks V1 and the table pointer is a genuine V1 table;
//! - [`validate_init_output`] — *after* the plugin answered: the status is
//!   `Ok`, the ABI echo matches, and the reported identity is not empty.
//!
//! Both are deliberately tiny and total: every failure is a typed error,
//! never a panic, and there is no partial "best effort" handshake — anything
//! unexpected is a refusal.

use core::ffi::c_void;

use candyriya_native_abi::{
    CANDYRIYA_NATIVE_ABI_VERSION_V1, CandyriyaPluginApiV1, PluginInitOutput,
};
use thiserror::Error;

/// Validates that the proxy side of the handshake is sound before the plugin
/// is called.
///
/// `expected_abi` must be [`CANDYRIYA_NATIVE_ABI_VERSION_V1`] (`1`) and
/// `table_ptr` must be null or point at a live [`CandyriyaPluginApiV1`] whose
/// `abi_version == 1` and `size == size_of::<CandyriyaPluginApiV1>()` — the
/// only two things [`CandyriyaPluginApiV1::is_valid`] checks.
///
/// # Safety
///
/// `table_ptr` must be null or point at a [`CandyriyaPluginApiV1`] that is
/// valid for reads for the duration of this call, and stays valid for as
/// long as the pointed-at table is used by the plugin. This is exactly what
/// the proxy guarantees when it hands out a `PluginInitArgs` built by
/// `PluginInitArgs::new`; a fabricated pointer here is a contract
/// violation by the caller, not a condition this function can detect.
pub unsafe fn validate_handshake(
    table_ptr: *const c_void,
    expected_abi: u32,
) -> Result<(), HandshakeError> {
    if expected_abi != CANDYRIYA_NATIVE_ABI_VERSION_V1 {
        return Err(HandshakeError::UnsupportedAbiVersion {
            expected: CANDYRIYA_NATIVE_ABI_VERSION_V1,
            got: expected_abi,
        });
    }
    if table_ptr.is_null() {
        return Err(HandshakeError::NullTablePointer);
    }
    // SAFETY: the caller upholds the live-table contract above; the
    // `expected_abi` gate guarantees the table is the V1 layout we compile
    // against, so no other interpretation of these bytes is legal here.
    let table = unsafe { &*(table_ptr as *const CandyriyaPluginApiV1) };
    if !table.is_valid() {
        return Err(HandshakeError::TableInvalid {
            reported_abi_version: table.abi_version,
            reported_size: table.size,
        });
    }
    Ok(())
}

/// Validates the plugin's answer to the init call.
///
/// Three gates, in the order the plugin contract mandates:
///
/// 1. `status` must be `Ok` — the status is authoritative, and plugins that
///    fail early are not inspected further;
/// 2. `abi_version` must echo `expected_abi` — a plugin built against a
///    different contract is refused;
/// 3. the identity must be complete — `id`, `version` and `name` buffers all
///    non-empty; the proxy's manifest rules do the stricter validation later.
pub fn validate_init_output(
    output: &PluginInitOutput,
    expected_abi: u32,
) -> Result<(), HandshakeError> {
    if !output.status.is_ok() {
        // the status carries no payload by design; there is nothing to log
        // except the refusal itself
        return Err(HandshakeError::InitRejected);
    }
    if output.abi_version != expected_abi {
        return Err(HandshakeError::AbiEchoMismatch {
            expected: expected_abi,
            got: output.abi_version,
        });
    }
    if !output.metadata.is_complete() {
        return Err(HandshakeError::MetadataIncomplete);
    }
    Ok(())
}

/// Why the handshake was refused.
///
/// Every variant is a refusal — none of them is recoverable in place; the
/// caller decides what the failure means for the plugin's lifecycle record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HandshakeError {
    /// The caller passed an `expected_abi` other than V1.
    #[error(
        "the caller speaks ABI version {got}, but this build of candyriya speaks V1 ({expected})"
    )]
    UnsupportedAbiVersion {
        /// The ABI version this crate speaks.
        expected: u32,
        /// The ABI version the caller asked for.
        got: u32,
    },
    /// `table_ptr` was null.
    #[error("the API table pointer is null")]
    NullTablePointer,
    /// The table failed the `abi_version`/`size` tripwire.
    #[error(
        "the API table is not a genuine V1 table (reported abi_version {reported_abi_version}, reported size {reported_size} bytes)"
    )]
    TableInvalid {
        /// The `abi_version` field read from the table.
        reported_abi_version: u32,
        /// The `size` field read from the table.
        reported_size: u32,
    },
    /// The plugin answered with a non-`Ok` status; it refuses to load.
    #[error("the plugin refused the init handshake (status is not Ok)")]
    InitRejected,
    /// The plugin's ABI echo does not match the version asked of it.
    #[error("the plugin echoes ABI version {got}, but {expected} was requested")]
    AbiEchoMismatch {
        /// The ABI version that was requested.
        expected: u32,
        /// The ABI version the plugin answered with.
        got: u32,
    },
    /// The plugin answered `Ok` with an empty `id`/`version`/`name`.
    #[error(
        "the plugin reported Ok with an incomplete identity (id/version/name must be non-empty)"
    )]
    MetadataIncomplete,
}
