use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr;

use swipl_sys::{PL_engine_t, PL_thread_attr_t};

use crate::error::{AttachError, EngineCreateError};
use crate::Runtime;

/// Creation attributes for an [`Engine`], mirroring the numeric knobs of
/// `PL_thread_attr_t`. `alias`, the cancel hook, `thread_class`, and `flags`
/// are deliberately not exposed yet: wrapping them safely (string lifetimes,
/// callback ABI) is a separate design problem.
#[derive(Debug, Clone, Copy, Default)]
pub struct EngineAttributes {
    /// Total stack limit in bytes (`stack_limit`). `None` uses the default.
    pub stack_limit: Option<usize>,
    /// Total tabling space limit in bytes (`table_space`). `None` uses the
    /// default.
    pub table_space: Option<usize>,
}

/// An owned SWI-Prolog engine, created via `PL_create_engine`.
///
/// A freshly created engine is not attached to any thread. Use
/// [`Engine::attach`] to bind it to the calling thread for the lifetime of
/// the returned guard. Between attachments the engine may be moved to and
/// attached from a different thread — SWI-Prolog explicitly supports
/// resuming a detached engine elsewhere — which is why `Engine` is [`Send`].
///
/// It is not [`Sync`]: every operation takes `&mut self`, so at most one
/// thread can interact with a given engine at any time (invariant E2).
pub struct Engine<'r> {
    /// Invariant E1: non-null, uniquely owned; destroyed exactly once in
    /// `Drop`.
    raw: PL_engine_t,
    _runtime: PhantomData<&'r Runtime>,
}

// SAFETY: an engine created by `PL_create_engine` starts unattached, and
// SWI-Prolog documents that a detached engine may later be attached to a
// *different* OS thread (`PL_set_engine` tracks attachment via `has_tid`
// under its own L_THREAD lock, at attach time, not creation time).
// Transferring the owning value across threads and attaching there is
// therefore a supported operation, which is all `Send` grants. `Engine` is
// deliberately not `Sync` (E2): with `&mut`-only methods, no two threads can
// ever call into SWI-Prolog with the same engine handle concurrently.
unsafe impl Send for Engine<'_> {}

impl<'r> Engine<'r> {
    /// Creates a new, unattached engine.
    ///
    /// Callable from any thread; `PL_create_engine` internally saves and
    /// restores the calling thread's current engine.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let runtime = splint::Runtime::initialize(["splint", "-q"])?;
    /// let mut engine = splint::Engine::new(&runtime, Default::default())?;
    /// let guard = engine.attach()?;
    /// // ... engine-scoped work on this thread ...
    /// drop(guard); // restores the previously attached engine
    /// # Ok(()) }
    /// ```
    pub fn new(
        _runtime: &'r Runtime,
        attributes: EngineAttributes,
    ) -> Result<Self, EngineCreateError> {
        let mut storage;
        let attr_ptr = if attributes.stack_limit.is_none() && attributes.table_space.is_none() {
            ptr::null_mut()
        } else {
            storage = PL_thread_attr_t {
                stack_limit: attributes.stack_limit.unwrap_or(0),
                table_space: attributes.table_space.unwrap_or(0),
                alias: ptr::null_mut(),
                cancel: None,
                flags: 0,
                max_queue_size: 0,
                thread_class: ptr::null_mut(),
                reserved: [ptr::null_mut(); 2],
            };
            &mut storage as *mut PL_thread_attr_t
        };

        // SAFETY: `_runtime` proves the runtime is initialized (R1).
        // `attr_ptr` is null or points to a fully-initialized stack-local
        // struct that is only read during this call (PL_create_engine
        // forwards it to PL_thread_attach_engine, which copies the values it
        // needs before returning).
        let raw = unsafe { swipl_sys::PL_create_engine(attr_ptr) };
        if raw.is_null() {
            return Err(EngineCreateError::Failed);
        }
        Ok(Engine {
            raw,
            _runtime: PhantomData,
        })
    }

    /// Attaches this engine to the calling thread.
    ///
    /// The returned guard keeps the engine exclusively borrowed and, on
    /// drop, restores whatever engine the thread had attached before
    /// (possibly none). The guard is neither [`Send`] nor [`Sync`]: the
    /// current-engine slot is per-OS-thread storage, so the detach must
    /// happen on the thread that attached (invariant E3).
    ///
    /// Leaking the guard (e.g. [`std::mem::forget`]) is safe but leaves the
    /// engine attached to this thread indefinitely; the engine can then no
    /// longer be destroyed from another thread and will be leaked.
    pub fn attach(&mut self) -> Result<AttachedEngine<'_>, AttachError> {
        let mut previous: PL_engine_t = ptr::null_mut();
        // SAFETY: `self.raw` is a valid engine handle owned by `self` (E1).
        // `&mut self` guarantees no other call is using this handle
        // concurrently; concurrent PL_set_engine calls for *other* engines
        // on other threads are serialized by SWI-Prolog's L_THREAD lock.
        let rc = unsafe { swipl_sys::PL_set_engine(self.raw, &mut previous) };
        match rc {
            _ if rc == swipl_sys::PL_ENGINE_SET as c_int => Ok(AttachedEngine {
                previous,
                _borrow: PhantomData,
                _not_send_sync: PhantomData,
            }),
            _ if rc == swipl_sys::PL_ENGINE_INVAL as c_int => Err(AttachError::Invalid),
            _ if rc == swipl_sys::PL_ENGINE_INUSE as c_int => Err(AttachError::InUse),
            _ => Err(AttachError::Unknown(rc)),
        }
    }

    /// The raw engine handle. Exposed for tests and escape hatches; using it
    /// to attach or destroy the engine outside this type's control voids the
    /// safety guarantees documented on [`Engine`].
    #[doc(hidden)]
    pub fn as_ptr(&self) -> PL_engine_t {
        self.raw
    }
}

impl Drop for Engine<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is valid and uniquely owned (E1).
        // `PL_destroy_engine` is valid from any thread when the engine is
        // unattached or attached to the calling thread — the only states
        // reachable here, because `attach` keeps `self` exclusively borrowed
        // while a guard exists. If a guard was leaked on another thread, the
        // call is rejected by SWI-Prolog (returns false, no double-free) and
        // the engine leaks; that is a resource leak, not a soundness issue.
        let _ = unsafe { swipl_sys::PL_destroy_engine(self.raw) };
    }
}

/// RAII guard for an engine attached to the current thread; produced by
/// [`Engine::attach`].
///
/// While alive, the underlying [`Engine`] is exclusively borrowed (invariant
/// E4): it cannot be moved, dropped, or re-attached. On drop, the thread's
/// previously attached engine is restored (or the thread is detached if
/// there was none). Guards for *different* engines may be nested on one
/// thread; dropping them in LIFO order restores the chain correctly.
pub struct AttachedEngine<'e> {
    /// The engine that was current before the attach, restored verbatim on
    /// drop. Null means "no engine": `PL_set_engine(NULL, ..)` is the
    /// documented detach path (`PL_ENGINE_NONE` must NOT be passed — the C
    /// implementation would dereference it).
    previous: PL_engine_t,
    _borrow: PhantomData<&'e mut ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl Drop for AttachedEngine<'_> {
    fn drop(&mut self) {
        // SAFETY: this runs on the thread that performed the attach — the
        // guard is !Send, making that a compile-time guarantee (E3).
        // `previous` is either null (detach) or the handle that was current
        // on this thread at attach time; engines cannot have been destroyed
        // in the interim because destroying requires ownership or an
        // exclusive borrow that outer guards on this thread still hold.
        let mut old: PL_engine_t = ptr::null_mut();
        let _ = unsafe { swipl_sys::PL_set_engine(self.previous, &mut old) };
    }
}

/// A non-owning witness that some engine is attached to the calling thread,
/// obtained from [`Runtime::current_engine`] — for example the main engine
/// on the thread that initialized the runtime.
///
/// Unlike [`AttachedEngine`] it restores nothing on drop; it only observes
/// an attachment it does not own. It is `!Send + !Sync` because it describes
/// the calling thread's own TLS state (invariant E3).
pub struct CurrentEngine<'a> {
    raw: PL_engine_t,
    _borrow: PhantomData<&'a Runtime>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl CurrentEngine<'_> {
    pub(crate) fn new(raw: PL_engine_t) -> Self {
        CurrentEngine {
            raw,
            _borrow: PhantomData,
            _not_send_sync: PhantomData,
        }
    }

    /// The raw engine handle. Exposed for tests and escape hatches.
    #[doc(hidden)]
    pub fn as_ptr(&self) -> PL_engine_t {
        self.raw
    }
}
