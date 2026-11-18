//! [`CandyriyaStatus`]: the return code of every fallible entry point.

/// The outcome of an entry point call.
///
/// `#[repr(u32)]` and fieldless, so it is FFI-safe and stable across crate
/// versions. The set of variants may grow in later ABI versions — consumers
/// **must** treat any unknown value as [`CandyriyaStatus::Failed`]; match
/// with a wildcard arm, never exhaustively.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CandyriyaStatus {
    /// The call succeeded.
    Ok = 0,
    /// The call failed. The human-readable detail, if any, is in the proxy
    /// logs — failures crossing the ABI carry no message payload.
    Failed = 1,
}

impl CandyriyaStatus {
    /// `true` only for [`CandyriyaStatus::Ok`]. Unknown discriminant values
    /// are conservatively failures.
    #[must_use]
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}
