use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::{Mutex, PoisonError};

use crate::engine::CurrentEngine;
use crate::error::{CleanupError, CleanupErrorKind, InitError};

enum RuntimeState {
    Uninitialized,
    Initialized,
}

/// Serializes all `PL_initialise`/`PL_set_resource_db_mem`/`PL_cleanup`
/// calls (invariant R2). `PL_initialise` performs an unlocked check-then-act
/// on its own initialized flag, so the C side provides no protection against
/// concurrent initialization; holding this mutex across the entire FFI call
/// is what makes it safe. Two states suffice: `Runtime` is the sole,
/// non-clonable token, so `cleanup` cannot race itself from safe code.
static RUNTIME_STATE: Mutex<RuntimeState> = Mutex::new(RuntimeState::Uninitialized);

/// A witness that the SWI-Prolog runtime is initialized in this process.
///
/// At most one `Runtime` exists at a time (invariant R1): it is produced
/// only by a successful [`Runtime::initialize`]/[`initialize_from_state`]
/// and consumed only by a successful [`Runtime::cleanup`]. Engines and other
/// runtime-dependent values borrow the `Runtime`, so the borrow checker
/// guarantees they are gone before cleanup can run (invariant R4).
///
/// Dropping a `Runtime` does *not* shut Prolog down: leaving the runtime
/// initialized for the remainder of the process is normal embedding
/// practice, and `PL_cleanup` outcomes (canceled, failed) could not be
/// reported from a destructor. Call [`Runtime::cleanup`] explicitly if
/// teardown or re-initialization is needed.
///
/// [`initialize_from_state`]: Runtime::initialize_from_state
#[derive(Debug)]
pub struct Runtime {
    _private: (),
}

/// Options for [`Runtime::cleanup`], mirroring the `PL_cleanup` flag word.
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupOptions {
    /// Exit status reported to `at_halt/1` hooks (low 16 bits of the flag
    /// word).
    pub status: u16,
    /// Skip memory reclamation (`PL_CLEANUP_NO_RECLAIM_MEMORY`). Faster, but
    /// the runtime cannot be cleanly re-initialized afterwards.
    pub no_reclaim_memory: bool,
    /// Do not run `at_halt/1` cancellation hooks (`PL_CLEANUP_NO_CANCEL`).
    pub no_cancel: bool,
}

impl CleanupOptions {
    fn into_raw(self) -> c_int {
        let mut flags = c_int::from(self.status);
        if self.no_reclaim_memory {
            flags |= swipl_sys::PL_CLEANUP_NO_RECLAIM_MEMORY as c_int;
        }
        if self.no_cancel {
            flags |= swipl_sys::PL_CLEANUP_NO_CANCEL as c_int;
        }
        flags
    }
}

impl Runtime {
    /// Initializes the SWI-Prolog runtime.
    ///
    /// `args` is the argument vector passed to `PL_initialise`; the first
    /// element is the program name (`argv[0]`), and the rest are SWI-Prolog
    /// commandline options (e.g. `-q`, `-x state`). The calling thread
    /// becomes Prolog's main thread and gets the main engine attached (see
    /// [`Runtime::current_engine`]).
    ///
    /// The argument strings are copied into storage that is intentionally
    /// leaked: SWI-Prolog retains the `argv` pointers for the lifetime of
    /// the process (invariant R3).
    ///
    /// Fails with [`InitError::AlreadyInitialized`] if a `Runtime` already
    /// exists. After a successful [`Runtime::cleanup`], a new `Runtime` may
    /// be initialized, but note that foreign extensions loaded by the
    /// previous session may not support re-initialization cleanly.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), splint::InitError> {
    /// let runtime = splint::Runtime::initialize(["splint", "-q"])?;
    /// # Ok(()) }
    /// ```
    pub fn initialize<I, S>(args: I) -> Result<Runtime, InitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        Self::initialize_inner(args, None)
    }

    /// Initializes the SWI-Prolog runtime, booting from an in-memory saved
    /// state produced by `qsave_program/2` (e.g. embedded with
    /// `include_bytes!`) instead of the default state search.
    ///
    /// The buffer must be `'static`: SWI-Prolog does not copy it, but reads
    /// from it lazily for the entire session — during boot and for any later
    /// `open_resource/3` access (invariant R5). Cleanup severs the
    /// association without freeing the buffer, so re-initializing from the
    /// same state simply means calling this function again.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), splint::InitError> {
    /// // In practice: `static STATE: &[u8] = include_bytes!("app.state");`
    /// static STATE: &[u8] = &[];
    /// let runtime = splint::Runtime::initialize_from_state(["splint", "-q"], STATE)?;
    /// # Ok(()) }
    /// ```
    pub fn initialize_from_state<I, S>(args: I, state: &'static [u8]) -> Result<Runtime, InitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        Self::initialize_inner(args, Some(state))
    }

    fn initialize_inner<I, S>(args: I, state: Option<&'static [u8]>) -> Result<Runtime, InitError>
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        let mut guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*guard, RuntimeState::Initialized) {
            return Err(InitError::AlreadyInitialized);
        }

        let (argc, argv) = leak_argv(args)?;

        if let Some(state) = state {
            // SAFETY: `state` is a live `'static` buffer; SWI-Prolog stores
            // the pointer and reads from it lazily for the whole session
            // without copying or freeing it, which `'static` accommodates
            // (R5). The function writes an unsynchronized global, which is
            // serialized by holding RUNTIME_STATE across this call (R2).
            let ok = unsafe { swipl_sys::PL_set_resource_db_mem(state.as_ptr(), state.len()) };
            if !ok {
                return Err(InitError::InvalidSavedState);
            }
        }

        // SAFETY: `argv` points to `argc` NUL-terminated C strings plus a
        // trailing null sentinel, all leaked to `'static` because SWI-Prolog
        // retains the pointers (R3). `PL_initialise`'s internal
        // check-then-act on its initialized flag is not thread-safe; holding
        // RUNTIME_STATE across this call serializes it, and this is the only
        // call site in the crate (R2).
        let ok = unsafe { swipl_sys::PL_initialise(argc, argv) };
        if !ok {
            // argv (and any partially-consumed saved state) is deliberately
            // not reclaimed: PL_initialise may have stashed the pointers in
            // global state before failing.
            return Err(InitError::InitialiseFailed);
        }

        *guard = RuntimeState::Initialized;
        Ok(Runtime { _private: () })
    }

    /// Tears down the SWI-Prolog runtime via `PL_cleanup`.
    ///
    /// Callable from any thread, though cleanup initiated off the
    /// initializing thread relies on Prolog's cross-thread halt signalling
    /// and may take longer or fail if the main thread is unresponsive.
    ///
    /// On success the `Runtime` is consumed and a new one may be
    /// initialized. On cancellation or failure the runtime is still fully
    /// initialized, and the token is handed back inside the
    /// [`CleanupError`] so it can be kept in use or the cleanup retried.
    ///
    /// Engines created from this runtime borrow it, so all of them must be
    /// dropped before `cleanup` can be called; a leaked (forgotten) engine
    /// will typically surface as [`CleanupErrorKind::Failed`].
    pub fn cleanup(self, options: CleanupOptions) -> Result<(), CleanupError> {
        let mut guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
        debug_assert!(matches!(*guard, RuntimeState::Initialized));

        // SAFETY: `self` proves the runtime is initialized (R1).
        // `PL_cleanup` is internally guarded against concurrent and
        // recursive invocation and is documented as callable from any
        // thread; the flag word is built from documented bit masks.
        let rc = unsafe { swipl_sys::PL_cleanup(options.into_raw()) };

        if rc == swipl_sys::PL_CLEANUP_SUCCESS as c_int {
            *guard = RuntimeState::Uninitialized;
            return Ok(());
        }

        let kind = if rc == swipl_sys::PL_CLEANUP_CANCELED as c_int {
            CleanupErrorKind::Canceled
        } else if rc == swipl_sys::PL_CLEANUP_FAILED {
            CleanupErrorKind::Failed
        } else if rc == swipl_sys::PL_CLEANUP_RECURSIVE {
            CleanupErrorKind::Recursive
        } else {
            CleanupErrorKind::Unknown(rc)
        };
        Err(CleanupError { runtime: self, kind })
    }

    /// Returns a witness for the engine currently attached to the calling
    /// thread, if any — for example the main engine on the thread that
    /// called [`Runtime::initialize`].
    pub fn current_engine(&self) -> Option<CurrentEngine<'_>> {
        // SAFETY: `self` proves the runtime is initialized (R1); this is a
        // pure read of the calling thread's TLS engine slot.
        let raw = unsafe { swipl_sys::PL_current_engine() };
        if raw.is_null() {
            None
        } else {
            Some(CurrentEngine::new(raw))
        }
    }
}

fn leak_argv<I, S>(args: I) -> Result<(c_int, *mut *mut c_char), InitError>
where
    I: IntoIterator<Item = S>,
    S: Into<Vec<u8>>,
{
    let mut owned = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        let arg = CString::new(arg).map_err(|source| InitError::InvalidArgument { index, source })?;
        owned.push(arg);
    }
    let argc: c_int = owned
        .len()
        .try_into()
        .map_err(|_| InitError::TooManyArguments)?;
    let mut ptrs: Vec<*mut c_char> = owned.into_iter().map(CString::into_raw).collect();
    ptrs.push(std::ptr::null_mut());
    let leaked: &'static mut [*mut c_char] = ptrs.leak();
    Ok((argc, leaked.as_mut_ptr()))
}

#[cfg(test)]
mod tests {
    use super::CleanupOptions;
    use std::os::raw::c_int;

    #[test]
    fn cleanup_options_flag_word() {
        assert_eq!(CleanupOptions::default().into_raw(), 0);
        let options = CleanupOptions {
            status: 7,
            no_reclaim_memory: true,
            no_cancel: true,
        };
        let expected = 7 | swipl_sys::PL_CLEANUP_NO_RECLAIM_MEMORY as c_int
            | swipl_sys::PL_CLEANUP_NO_CANCEL as c_int;
        assert_eq!(options.into_raw(), expected);
    }
}
