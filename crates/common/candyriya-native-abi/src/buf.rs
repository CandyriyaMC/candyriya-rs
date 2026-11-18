//! [`CandyriyaBuf`]: the `(ptr, len)` byte buffer of the ABI.

use core::fmt;

/// A borrowed byte buffer crossing the ABI boundary.
///
/// Layout is `(*const u8, u32)` — no ownership, no allocator, nothing about
/// allocation is implied on either side. `len` is a `u32` so the buffer is
/// 16 bytes on 64-bit targets and 8 on 32-bit ones; a buffer larger than
/// `u32::MAX` bytes is not representable and is rejected up front.
///
/// # Lifetime rules
///
/// - the producer owns the memory and must keep it alive for the whole call;
/// - the consumer **must not** retain `ptr` past the call or free it;
/// - an empty buffer may be `ptr: null, len: 0` — always safe to read as an
///   empty slice;
/// - a null `ptr` with a non-zero `len` is a contract violation. Reading it
///   through [`CandyriyaBuf::as_slice`] is *not* performed; the slice is
///   empty instead of dereferencing null.
///
/// # Examples
///
/// ```
/// use candyriya_native_abi::CandyriyaBuf;
///
/// let buf = CandyriyaBuf::new(b"candyriya");
/// assert_eq!(buf.len, 9);
/// // SAFETY: `buf` was built from a live slice; reading it back is sound.
/// let bytes = unsafe { buf.as_slice() };
/// assert_eq!(bytes, b"candyriya");
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandyriyaBuf {
    /// Pointer to the first byte, or null for an empty buffer.
    pub ptr: *const u8,
    /// Number of bytes at `ptr`. Never read when `ptr` is null.
    pub len: u32,
}

impl CandyriyaBuf {
    /// The canonical empty buffer: null pointer, zero length.
    pub const EMPTY: Self = Self {
        ptr: core::ptr::null(),
        len: 0,
    };

    /// Builds a buffer from a live slice.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` is longer than `u32::MAX` bytes — the ABI cannot
    /// represent it. This is a programming error, never a runtime condition.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Self {
        let len = u32::try_from(bytes.len())
            .expect("buffers larger than u32::MAX are not representable in the ABI");
        Self {
            ptr: bytes.as_ptr(),
            len,
        }
    }

    /// `true` when the buffer holds no bytes (null pointer or `len == 0`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads the buffer as a slice.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that, at the moment of the call, `ptr` is
    /// null or points at `len` readable bytes and stays valid for the
    /// duration of the returned slice. A null `ptr` yields `&[]` instead of
    /// dereferencing null; a non-null dangling or truncated `ptr` is the
    /// caller's contract violation and is UB.
    #[must_use]
    pub unsafe fn as_slice(&self) -> &[u8] {
        if self.ptr.is_null() {
            return &[];
        }
        // SAFETY: the caller asserts `ptr` points at `len` readable bytes.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len as usize) }
    }
}

impl fmt::Display for CandyriyaBuf {
    /// Renders the buffer as UTF-8, lossily. Buffers are not required to be
    /// UTF-8 — metadata fields in practice are, but the ABI does not enforce
    /// it, so display is diagnostics-only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: called only for diagnostics; buffer rules apply — the
        // producer must keep the bytes alive, which holds here since the
        // buffer itself is borrowed.
        let bytes = unsafe { self.as_slice() };
        write!(f, "{}", String::from_utf8_lossy(bytes))
    }
}
