use std::ffi::{CString, NulError};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::engine::CurrentEngine;

/// Errors from [`Runtime::initialize`] and [`Runtime::initialize_from_state`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// A [`Runtime`] already exists in this process.
    #[error("the SWI-Prolog runtime is already initialized in this process")]
    AlreadyInitialized,
    /// `PL_initialise` reported failure (invalid options, missing boot state,
    /// or a resource error).
    #[error("PL_initialise reported failure")]
    InitialiseFailed,
    /// `PL_set_resource_db_mem` rejected the saved-state buffer (not a valid
    /// zip archive produced by `qsave_program/2`).
    #[error("the saved state is not a valid SWI-Prolog resource archive")]
    InvalidSavedState,
    /// An argument contained an interior NUL byte and cannot be passed as a
    /// C string.
    #[error("argument at index {index} contains an interior NUL byte")]
    InvalidArgument {
        index: usize,
        #[source]
        source: NulError,
    },
    /// More arguments were supplied than fit in a C `int`.
    #[error("too many arguments to fit in a C int")]
    TooManyArguments,
}

/// Why a [`Runtime::cleanup`] call did not tear the runtime down.
#[derive(Debug, thiserror::Error)]
pub enum CleanupErrorKind {
    /// An `at_halt/1` hook vetoed the cleanup (`PL_CLEANUP_CANCELED`). The
    /// runtime remains fully initialized.
    #[error("cleanup was canceled by an at-halt hook")]
    Canceled,
    /// `PL_cleanup` failed (`PL_CLEANUP_FAILED`), e.g. because engines or
    /// threads are still outstanding. The runtime remains initialized.
    #[error("PL_cleanup failed (e.g. outstanding engines or threads)")]
    Failed,
    /// Another cleanup was already in progress (`PL_CLEANUP_RECURSIVE`).
    #[error("a cleanup was already in progress")]
    Recursive,
    /// `PL_cleanup` returned a status code this crate does not know.
    #[error("PL_cleanup returned an unrecognized status code {0}")]
    Unknown(c_int),
}

/// A failed [`Runtime::cleanup`]. Carries the still-valid [`Runtime`] back to
/// the caller: a non-successful cleanup leaves Prolog fully initialized, so
/// the runtime can be kept in use or the cleanup retried.
#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct CleanupError {
    pub runtime: Runtime,
    pub kind: CleanupErrorKind,
}

enum RuntimeState {
    Uninitialized,
    Initialized { session: u64 },
}

/// Mints process-unique runtime sessions, one per successful `initialize`
/// (mirroring `scope.rs`'s generation mint for engine attachments). A
/// [`Record`](crate::Record)'s session stamp is how its dynamic checks tell
/// "this handle's store is still the live one" from "this handle's store
/// already died with an earlier session" — necessary because a record's
/// `'rt` lifetime can be chosen freely by a `Deserialize` caller rather than
/// tied to a real `&'rt Runtime` borrow (invariant RC2).
static SESSION_MINT: AtomicU64 = AtomicU64::new(1);

/// The current runtime session, if the runtime is initialized.
///
/// `None` is unreachable for callers holding an [`FliContext`](crate::FliContext)
/// witness: a live engine keeps its [`Runtime`] borrowed and un-cleaned-up
/// (R4), and at most one runtime exists (R1), so the state it stamped stays
/// current for as long as the witness lives.
pub(crate) fn current_session() -> Option<u64> {
    let guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
    match *guard {
        RuntimeState::Initialized { session } => Some(session),
        RuntimeState::Uninitialized => None,
    }
}

/// Panics unless `session` is the current runtime session.
///
/// Used by [`Record::recall`](crate::Record::recall)/`recall_into`, which may
/// release the lock before the `PL_recorded` call that follows: the recall
/// destination sits on a borrow chain (frame/query → attached engine →
/// [`Engine`](crate::Engine) → [`Runtime`], or
/// [`CurrentEngine`] → [`Runtime`]) that statically prevents a concurrent
/// `Runtime::cleanup(self)`, so a session that is current here stays current
/// for the whole call.
pub(crate) fn assert_session_current(session: u64, what: &str) {
    assert!(
        current_session() == Some(session),
        "splint: {what} belongs to a runtime session that is no longer current",
    );
}

/// Whether `session` is the current runtime session (a brief locked read).
#[cfg(feature = "serde")]
pub(crate) fn session_is_current(session: u64) -> bool {
    current_session() == Some(session)
}

/// Erases the record `raw` if `session` is still current; otherwise a silent
/// no-op — the record's store already died with its session (its memory was
/// reclaimed by that session's cleanup), so there is nothing left to erase.
///
/// The lock is held across both the check and the `PL_erase`: a record being
/// dropped carries no borrow that could statically exclude a concurrent
/// [`Runtime::cleanup`], so the check-then-erase must be atomic against the
/// cleanup path, which mutates the state under this same lock (R2).
pub(crate) fn erase_record_if_current(raw: swipl_sys::record_t, session: u64) {
    let guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
    if matches!(*guard, RuntimeState::Initialized { session: current } if current == session) {
        // SAFETY: `raw` is a live record handle whose erase obligation the
        // caller holds; the session check under the held lock proves the
        // recorded database that issued it is still the live one, and the
        // lock keeps `PL_cleanup` from tearing it down mid-erase (RC2).
        unsafe { swipl_sys::PL_erase(raw) };
    }
}

/// Duplicates the record `raw`, panicking unless `session` is still current.
///
/// As for [`erase_record_if_current`], the lock is held across the check and
/// the `PL_duplicate_record`, because cloning a record carries no borrow that
/// could statically exclude a concurrent [`Runtime::cleanup`].
pub(crate) fn duplicate_record_current(
    raw: swipl_sys::record_t,
    session: u64,
) -> swipl_sys::record_t {
    let guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(
        matches!(*guard, RuntimeState::Initialized { session: current } if current == session),
        "splint: record belongs to a runtime session that is no longer current",
    );
    // SAFETY: `raw` is a live record handle (Record invariant); the session
    // check under the held lock proves its recorded database is still the
    // live one, and the lock keeps `PL_cleanup` from racing the duplication
    // (RC2). The returned copy carries its own erase obligation.
    let duplicate = unsafe { swipl_sys::PL_duplicate_record(raw) };
    assert!(
        !duplicate.is_null(),
        "splint: PL_duplicate_record reported failure for a live record"
    );
    duplicate
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
    /// be initialized, though foreign extensions loaded by the previous
    /// session may not re-initialize cleanly.
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
    /// association without freeing the buffer, so the same buffer can be
    /// reused on a later init.
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
        if matches!(*guard, RuntimeState::Initialized { .. }) {
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

        let session = SESSION_MINT.fetch_add(1, Ordering::Relaxed);
        *guard = RuntimeState::Initialized { session };
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
    /// dropped before `cleanup` can be called. A leaked (forgotten) engine
    /// leaves the outcome up to SWI-Prolog and the platform: `PL_cleanup`
    /// waits in `exitPrologThreads` for outstanding engines to be destroyed,
    /// which on some builds surfaces as [`CleanupErrorKind::Failed`] but on
    /// others blocks indefinitely. Do not rely on a leaked engine producing
    /// any particular result.
    pub fn cleanup(self, options: CleanupOptions) -> Result<(), CleanupError> {
        let mut guard = RUNTIME_STATE.lock().unwrap_or_else(PoisonError::into_inner);
        debug_assert!(matches!(*guard, RuntimeState::Initialized { .. }));

        // SAFETY: `self` proves the runtime is initialized (R1); `PL_cleanup`
        // guards itself against concurrent and recursive calls.
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
