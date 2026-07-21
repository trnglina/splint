use std::ffi::{CString, NulError};
use std::os::raw::{c_char, c_int};
use std::sync::{Mutex, PoisonError};

use crate::engine::{CurrentEngine, EngineCreateError};

/// Errors from [`Runtime::initialize`] and [`Runtime::initialize_from_state`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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

enum RuntimeState {
    Uninitialized,
    Initialized,
}

/// Serializes the `PL_set_resource_db_mem`/`PL_initialise` pair (invariant
/// R2). `PL_initialise` performs an unlocked check-then-act on its own
/// initialized flag, so the C side provides no protection against concurrent
/// initialization; holding this mutex across the entire FFI call is what
/// makes it safe.
static RUNTIME_STATE: Mutex<RuntimeState> = Mutex::new(RuntimeState::Uninitialized);

/// A witness that the SWI-Prolog runtime is initialized in this process.
///
/// At most one `Runtime` exists (invariant R1): it is produced only by a
/// successful [`Runtime::initialize`]/[`initialize_from_state`], and once
/// initialized the runtime stays initialized for the remainder of the
/// process. Engines and other runtime-dependent values borrow the `Runtime`,
/// which orders them within its lifetime (invariant R4).
///
/// Dropping a `Runtime` does *not* shut Prolog down, and this crate offers no
/// way to shut it down at all: leaving it up for the life of the process is
/// normal embedding practice, and it is what lets handles like
/// [`Record`](crate::Record) be plain owned values rather than carrying
/// dynamic liveness checks.
///
/// This has a consequence worth knowing before embedding Prolog code that
/// relies on orderly shutdown. Because SWI-Prolog is never asked to clean up,
/// `at_halt/1` hooks never run, Prolog's own stream buffers are not flushed,
/// and Prolog threads are not joined when the process exits. Prolog's I/O
/// layer is separate from libc's, so a plain `exit` does not flush it either.
/// Code that must durably write on shutdown should flush explicitly rather
/// than rely on a halt hook.
///
/// [`initialize_from_state`]: Runtime::initialize_from_state
#[derive(Debug)]
pub struct Runtime {
    _private: (),
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
    /// exists; the runtime is initialized at most once per process, so a
    /// second call is a programming error rather than a no-op.
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
    /// from it lazily for the entire life of the runtime — during boot and
    /// for any later `open_resource/3` access (invariant R5).
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
            // the pointer and reads from it lazily for the whole life of the
            // runtime without copying or freeing it, which `'static`
            // accommodates (R5). The function writes an unsynchronized
            // global, which is serialized by holding RUNTIME_STATE across
            // this call (R2).
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

    /// Returns a witness for the calling thread's current engine, creating
    /// and attaching one with default attributes if the thread has none.
    ///
    /// An engine created by this method remains attached to the native thread
    /// after the returned witness is dropped. Later calls on the same thread
    /// reuse it. This persistent lifecycle matches the main engine installed
    /// on the thread that called [`Runtime::initialize`].
    ///
    /// Fails if SWI-Prolog cannot create the engine or was built without
    /// native-thread support.
    pub fn engine(&self) -> Result<CurrentEngine<'_>, EngineCreateError> {
        if let Some(current) = self.current_engine() {
            return Ok(current);
        }

        // SAFETY: `self` proves the runtime is initialized (R1); a null
        // attribute pointer requests fully default thread-engine attributes.
        // The calling thread has no engine (checked above), so success creates
        // and attaches one. The crate intentionally leaves that engine
        // attached for reuse rather than balancing this call with
        // PL_thread_destroy_engine (E7).
        match unsafe { swipl_sys::PL_thread_attach_engine(std::ptr::null_mut()) } {
            id if id >= 0 => Ok(CurrentEngine::new()),
            -2 => Err(EngineCreateError::ThreadingUnavailable),
            _ => Err(EngineCreateError::Failed),
        }
    }

    /// Returns a witness for the engine currently attached to the calling
    /// thread, if any, without creating one.
    ///
    /// This is the optional, side-effect-free probe. Prefer
    /// [`Runtime::engine`] when the caller requires an engine.
    pub fn current_engine(&self) -> Option<CurrentEngine<'_>> {
        // SAFETY: `self` proves the runtime is initialized (R1); this is a
        // pure read of the calling thread's TLS engine slot.
        let raw = unsafe { swipl_sys::PL_current_engine() };
        if raw.is_null() {
            None
        } else {
            Some(CurrentEngine::new())
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
        let arg =
            CString::new(arg).map_err(|source| InitError::InvalidArgument { index, source })?;
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
