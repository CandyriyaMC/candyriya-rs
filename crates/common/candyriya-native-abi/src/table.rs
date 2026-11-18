//! [`CandyriyaPluginApiV1`]: the versioned function table, and the opaque
//! instance handle it carries.

use core::fmt;

/// An opaque token identifying one plugin instance to the proxy.
///
/// The proxy allocates and owns this; the plugin only ever receives one
/// (inside [`CandyriyaPluginApiV1::instance`]) and passes it back to proxy
/// calls — in V1 there is no such call yet, but the token is what V2's host
/// functions will be scoped to. Plugins must never fabricate, copy, mutate or
/// forge a handle, and must not dereference it: it is not a pointer into any
/// plugin-meaningful object.
///
/// It is `*mut c_void` under the hood so the ABI stays pointer-width and
/// `repr(transparent)`, which keeps it identical across every C compiler we
/// could ever meet.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct CandyriyaPluginHandle(*mut core::ffi::c_void);

impl CandyriyaPluginHandle {
    /// Wraps a raw proxy-side pointer as a handle.
    ///
    /// # Safety
    ///
    /// The pointer must denote the proxy-owned plugin instance this token
    /// stands for, and must stay valid for as long as the plugin may be
    /// called. This constructor exists for the proxy; plugins never call it.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut core::ffi::c_void) -> Self {
        Self(ptr)
    }

    /// The raw pointer, for the proxy's own bookkeeping.
    #[must_use]
    pub fn as_raw(self) -> *mut core::ffi::c_void {
        self.0
    }
}

impl fmt::Debug for CandyriyaPluginHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // do not print what this is: the value is meaningless outside the
        // proxy and leaking addresses into plugin-side logs helps nobody
        write!(f, "CandyriyaPluginHandle(..)")
    }
}

/// The V1 API table the proxy hands out with every entry point.
///
/// # Layout and size
///
/// Two `u32`s and one pointer — deliberately tiny. `abi_version` is always
/// [`crate::CANDYRIYA_NATIVE_ABI_VERSION_V1`]; `size` is
/// `size_of::<CandyriyaPluginApiV1>()` as measured on the building machine.
/// Both are bootstrap sanity checks: a plugin that sees anything else in
/// either field must refuse to run. Because both sides compile against this
/// exact type, `size` can only disagree when something is fundamentally
/// broken (mixed ABIs, LTO across crates, undefined behaviour) — it is a
/// tripwire, not a negotiation mechanism.
///
/// # The function set
///
/// V1 deliberately ships **zero** host functions: the proxy→plugin calls
/// travel through the plugin's own exports, and every plugin→proxy call
/// (configuration, cache, events, …) is defined by later ABI versions —
/// phase 2 of the epic introduces the first ones through a new
/// `CandyriyaPluginApiV2` table type — this one is never mutated. The table
/// exists now so that:
///
/// - every entry point already receives the same, stable shape;
/// - the instance handle already flows to the plugin;
/// - the versioning rules are real and tested, not aspirational.
///
/// # Stability
///
/// This type is **frozen**. No field is added, removed or reordered; its
/// `abi_version` is fixed at 1. Any extension is a new `CandyriyaPluginApiV2`
/// with its own table type — never a mutation of this one.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CandyriyaPluginApiV1 {
    /// Always [`crate::CANDYRIYA_NATIVE_ABI_VERSION_V1`] (`1`).
    pub abi_version: u32,
    /// Byte size of this struct, so both sides can assert they agree on the
    /// layout before reading anything else.
    pub size: u32,
    /// Opaque token for this plugin instance; see [`CandyriyaPluginHandle`].
    pub instance: CandyriyaPluginHandle,
}

impl CandyriyaPluginApiV1 {
    /// Builds a V1 table for the given instance handle. The hazard lives in
    /// [`CandyriyaPluginHandle::from_raw`]; building the table around it is
    /// safe.
    #[must_use]
    pub fn new(instance: CandyriyaPluginHandle) -> Self {
        Self {
            abi_version: crate::CANDYRIYA_NATIVE_ABI_VERSION_V1,
            size: size_of::<Self>() as u32,
            instance,
        }
    }

    /// Checks the table is a genuine V1 table this crate produced.
    ///
    /// `true` means `abi_version == 1` and `size == size_of::<Self>()` —
    /// everything a plugin needs to decide that the proxy built the table
    /// against the same layout.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.abi_version == crate::CANDYRIYA_NATIVE_ABI_VERSION_V1
            && self.size as usize == size_of::<Self>()
    }
}
