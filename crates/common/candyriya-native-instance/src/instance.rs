//! Opening plugin libraries and probing exports.
//!
//! [`LibraryLoader`] is the one owner of the mapping: it maps the library on
//! [`LibraryLoader::open`], hands out probes through [`LibraryLoader::probe_export`],
//! and unloads on drop (or explicitly in [`LibraryLoader::close`] when the
//! caller cares about unload errors). Probe handles borrow the loader, so a
//! probe can never outlive the mapping it was taken from.

use core::ffi::{CStr, c_char};
use std::path::{Path, PathBuf};

use libloading::{Library, Symbol};
use thiserror::Error;

/// How the OS loader binds the library at open time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenFlags {
    /// Eager binding (`RTLD_NOW` on unix) — the recommended default.
    ///
    /// A plugin whose relocations cannot be resolved is refused at open time
    /// instead of failing mid-call, which is the only behaviour compatible
    /// with fail-closed loading.
    #[default]
    Now,
    /// Lazy binding (`RTLD_LAZY` on unix). Kept for diagnostics; plugin work
    /// should use [`OpenFlags::Now`].
    Lazy,
}

/// An open dynamic library handle for one native plugin.
///
/// The mapping stays owned by this value for as long as it lives, and probe
/// handles borrow it. [`Clone`] is deliberately not implemented: two owners
/// of `dlclose` would unmap under each other's symbols.
///
/// # Drop
///
/// Dropping unloads the mapping and ignores errors (matching
/// [`libloading::Library`] semantics). Use [`LibraryLoader::close`] when the
/// unload outcome matters — unloading fails, for example, when some
/// other code still holds the mapping.
#[derive(Debug)]
pub struct LibraryLoader {
    path: PathBuf,
    library: Library,
}

impl LibraryLoader {
    /// Maps the dynamic library at `path`.
    ///
    /// On unix this is `dlopen(path, RTLD_NOW|RTLD_LOCAL)` (or `RTLD_LAZY`
    /// when [`OpenFlags::Lazy`] is passed); symbols stay local to the plugin,
    /// which is what keeps two plugins from interposing on each other. On
    /// Windows this is `LoadLibraryW`; `flags` has no meaning there and is
    /// ignored, because `LoadLibraryW` has no `RTLD_*` equivalents.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::Open`] when the OS refuses the mapping: file
    /// missing, unsupported architecture, unresolvable dependency, a
    /// constructor that aborted, and so on.
    pub fn open(path: &Path, flags: OpenFlags) -> Result<Self, LoaderError> {
        #[cfg(unix)]
        let opened = {
            use libloading::os::unix::{RTLD_LAZY, RTLD_LOCAL, RTLD_NOW};
            let mode = match flags {
                OpenFlags::Now => RTLD_NOW,
                OpenFlags::Lazy => RTLD_LAZY,
            };
            // SAFETY: naming a plugin library *is* the request to run its
            // initialisation routines (see `libloading::Library::new` — the
            // same invariant, restated at our boundary). The caller got the
            // path from its own discovery pass; we own the resulting mapping.
            unsafe { libloading::os::unix::Library::open(Some(path), mode | RTLD_LOCAL) }
                .map(Library::from)
        };

        #[cfg(windows)]
        let opened = {
            // LoadLibraryW has no RTLD_* knobs; `flags` is accepted so call
            // sites stay portable and ignored here by design.
            let _ = flags;
            // SAFETY: same invariant as the unix branch: the caller named a
            // plugin library and therefore opts into running its init
            // routines at load time.
            unsafe { libloading::os::windows::Library::new(path) }.map(Library::from)
        };

        #[cfg(not(any(unix, windows)))]
        compile_error!("candyriya-native-instance supports unix-likes and Windows only");

        let library = opened.map_err(|source| LoaderError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            library,
        })
    }

    /// The path this library was opened from, for logs and error messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves `symbol` to an opaque probe handle.
    ///
    /// The returned [`Symbol`] borrows `self` and pins the mapping's lifetime.
    /// Its pointee type is deliberately unknown here: this crate never calls
    /// a plugin function, and the typed cast to an ABI signature happens at
    /// the call site (e.g. in [`crate::initialize`]), where it can be
    /// reviewed against the contract.
    ///
    /// # Safety
    ///
    /// - `symbol` must be null or point at a valid nul-terminated string that
    ///   stays readable for the duration of this call (exactly the contract
    ///   of `CStr::from_ptr`); [`CStr::as_ptr`] on a live `&CStr` satisfies
    ///   it. A wrong value here is a contract violation by the caller, never a
    ///   condition this function can detect.
    /// - The caller must not use the returned symbol's pointee for anything
    ///   other than the exact signature the library was built against. This
    ///   function resolves names, not types — a correct name with a wrong
    ///   assumed type is still UB one cast later.
    pub unsafe fn probe_export(
        &self,
        symbol: *const c_char,
    ) -> Result<Symbol<'_, *const c_char>, LoaderError> {
        if symbol.is_null() {
            return Err(LoaderError::NullSymbolName);
        }
        // SAFETY: the caller upholds the nul-terminated string contract above;
        // this is the one place we turn the raw pointer back into a &CStr.
        let name = unsafe { CStr::from_ptr(symbol) };
        // SAFETY: `Library::get` requires the caller to know the real type of
        // the symbol — which is precisely the boundary that does *not* exist
        // in this function. We hand out `Symbol<'_, *const c_char>`, an opaque
        // address token, and every typed reinterpretation happens in caller
        // code, next to the signature it assumes.
        unsafe { self.library.get(name) }.map_err(|source| LoaderError::SymbolNotFound {
            library: self.path.clone(),
            name: name.to_string_lossy().into_owned(),
            source,
        })
    }

    /// Unloads the mapping now and reports what the OS said about it.
    ///
    /// Prefer this over relying on drop when teardown ordering is
    /// load-bearing (the unload sequencing of a plugin lifecycle is); drop
    /// swallows the same errors silently.
    ///
    /// # Errors
    ///
    /// Returns [`LoaderError::Close`] when `dlclose`/`FreeLibrary` fails,
    /// which typically means something inside the process still pins the
    /// mapping. The library is detached from this value either way.
    pub fn close(self) -> Result<(), LoaderError> {
        let path = self.path.clone();
        self.library
            .close()
            .map_err(|source| LoaderError::Close { path, source })
    }
}

/// Errors that can happen while opening, probing or closing a plugin
/// library.
///
/// Every variant carries the failing path or symbol name — "the plugin did
/// not load" is never actionable without knowing which file and which
/// export.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoaderError {
    /// The OS refused to map the library.
    #[error("failed to load dynamic library `{}`: {source}", path.display())]
    Open {
        /// The path passed to the OS loader.
        path: PathBuf,
        /// What the loader reported.
        #[source]
        source: libloading::Error,
    },
    /// [`LibraryLoader::probe_export`] was called with a null name pointer.
    #[error("symbol name pointer is null")]
    NullSymbolName,
    /// The export is absent, or the resolver itself failed.
    #[error("failed to resolve `{name}` in `{}`: {source}", library.display())]
    SymbolNotFound {
        /// The library that was probed.
        library: PathBuf,
        /// The symbol name that was looked up.
        name: String,
        /// What the resolver reported.
        #[source]
        source: libloading::Error,
    },
    /// An explicit [`LibraryLoader::close`] failed.
    #[error("failed to unload dynamic library `{}`: {source}", path.display())]
    Close {
        /// The path the library was opened from.
        path: PathBuf,
        /// What the unloader reported.
        #[source]
        source: libloading::Error,
    },
}
