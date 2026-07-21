use std::ffi::CString;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::os::raw::c_int;
use std::ptr;

use swipl_sys::{atom_t, functor_t, module_t, predicate_t};

use crate::exception::{take_pending_exception, PrologException};
use crate::term::FliContext;

/// An error from constructing a handle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandleError {
    #[error("PL_new_functor_sz reported failure")]
    FunctorConstruction,
    #[error("PL_pred/PL_predicate reported failure")]
    PredicateConstruction,
    #[error("predicate arity {arity} exceeds the C API's integer range")]
    PredicateArityOutOfRange { arity: usize },
    #[error("name contains an interior NUL byte")]
    InteriorNul(#[source] std::ffi::NulError),
    /// Constructing the handle raised a Prolog exception (e.g. a
    /// program-space resource error); the exception has been cleared from the
    /// engine.
    #[error("prolog exception: {0}")]
    Exception(#[source] PrologException),
}

/// A handle to a Prolog atom, holding one reference on the atom's reference
/// count.
///
/// Atoms are engine-independent global values. Every construction path
/// registers the handle unconditionally and `Drop` unregisters it, so the
/// count is self-contained regardless of how the raw atom was obtained (A1).
pub struct Atom {
    raw: atom_t,
}

impl Atom {
    /// Creates (or finds) the atom with the given text
    /// (`PL_new_atom_nchars`).
    pub fn new<C: FliContext + ?Sized>(_ctx: &C, text: &str) -> Atom {
        // SAFETY: `_ctx` witnesses the runtime is initialized with an engine
        // current on this thread; the pointer/length pair is a valid UTF-8
        // buffer that the call copies. A freshly returned atom already
        // carries one reference for the caller (A1).
        let raw = unsafe { swipl_sys::PL_new_atom_nchars(text.len(), text.as_ptr().cast()) };
        Atom { raw }
    }

    /// Wraps a raw atom handle, taking a fresh reference on it (A1).
    ///
    /// # Safety
    ///
    /// `raw` must be a live atom handle and the process-global runtime must
    /// have been initialized.
    pub(crate) unsafe fn from_raw(raw: atom_t) -> Atom {
        // SAFETY: `raw` is live per this function's contract; registering
        // keeps it live for this handle's lifetime (A1).
        unsafe { swipl_sys::PL_register_atom(raw) };
        Atom { raw }
    }

    /// The atom's text (`PL_atom_nchars`).
    pub fn text(&self) -> String {
        let mut len: usize = 0;
        // SAFETY: `self.raw` is registered and therefore live (A1); the
        // returned buffer belongs to the atom and is copied before this
        // handle can be released.
        let chars = unsafe { swipl_sys::PL_atom_nchars(self.raw, &mut len) };
        assert!(
            !chars.is_null(),
            "splint: PL_atom_nchars reported failure for a live atom"
        );
        // SAFETY: on success `chars` points to `len` valid bytes.
        let bytes = unsafe { std::slice::from_raw_parts(chars.cast::<u8>(), len) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    pub(crate) fn as_raw(&self) -> atom_t {
        self.raw
    }
}

impl Clone for Atom {
    fn clone(&self) -> Self {
        // SAFETY: `self.raw` is live (A1); the clone takes its own
        // reference.
        unsafe { swipl_sys::PL_register_atom(self.raw) };
        Atom { raw: self.raw }
    }
}

impl Drop for Atom {
    fn drop(&mut self) {
        // SAFETY: this handle holds exactly one reference (A1), and the
        // process-global runtime is never torn down (R1).
        unsafe { swipl_sys::PL_unregister_atom(self.raw) };
    }
}

/// Atoms are interned: two atoms with the same text always share the same
/// handle, so comparing the raw handles is exact value equality.
impl PartialEq for Atom {
    fn eq(&self, other: &Atom) -> bool {
        self.raw == other.raw
    }
}

impl Eq for Atom {}

impl Hash for Atom {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Consistent with `PartialEq`: equal atoms share a handle, so hashing
        // the handle keeps `a == b => hash(a) == hash(b)`.
        self.raw.hash(state);
    }
}

impl fmt::Debug for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Atom").field(&self.text()).finish()
    }
}

/// A handle to a name/arity pair (`PL_new_functor_sz`). Functors are global
/// and never garbage collected, so the handle carries no reference count.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Functor {
    raw: functor_t,
    arity: usize,
}

impl Functor {
    /// Creates (or finds) the functor `name/arity`.
    pub fn new<C: FliContext + ?Sized>(
        _ctx: &C,
        name: &Atom,
        arity: usize,
    ) -> Result<Functor, HandleError> {
        // SAFETY: `_ctx` witnesses the runtime is initialized; `name` is a
        // live atom (A1).
        let raw = unsafe { swipl_sys::PL_new_functor_sz(name.as_raw(), arity) };
        if raw == 0 {
            // A zero return can carry a pending resource exception; capture and
            // clear it so it cannot leak onto a later, unrelated operation.
            return Err(match take_pending_exception() {
                Some(exception) => HandleError::Exception(exception),
                None => HandleError::FunctorConstruction,
            });
        }
        Ok(Functor { raw, arity })
    }

    /// Creates (or finds) the functor `name/arity` from text.
    pub fn from_name<C: FliContext + ?Sized>(
        ctx: &C,
        name: &str,
        arity: usize,
    ) -> Result<Functor, HandleError> {
        Functor::new(ctx, &Atom::new(ctx, name), arity)
    }

    /// Wraps a raw functor handle (e.g. read from a term), recovering its
    /// arity with `PL_functor_arity_sz`.
    ///
    /// # Safety
    ///
    /// `raw` must be a live functor handle and the process-global runtime
    /// must have been initialized.
    pub(crate) unsafe fn from_raw(raw: functor_t) -> Functor {
        // SAFETY: `raw` is a live functor handle per this function's contract;
        // functors are global and never garbage collected, so no registration
        // is needed (A2).
        let arity = unsafe { swipl_sys::PL_functor_arity_sz(raw) };
        Functor { raw, arity }
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    /// The functor's name atom (`PL_functor_name`).
    pub fn name(&self) -> Atom {
        // SAFETY: `self.raw` is a live functor handle; its name is a live atom
        // handle, and `from_raw` takes its own registration (A1).
        unsafe { Atom::from_raw(swipl_sys::PL_functor_name(self.raw)) }
    }

    pub(crate) fn as_raw(&self) -> functor_t {
        self.raw
    }
}

impl fmt::Debug for Functor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Functor")
            .field("name", &self.name())
            .field("arity", &self.arity)
            .finish()
    }
}

/// A handle to a Prolog module (`PL_new_module`, find-or-create).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    raw: module_t,
}

// SAFETY: modules are process-global, stable handles whose operations are
// supported independently of a thread's current engine (A2).
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    /// Finds or creates the module with the given name.
    pub fn new<C: FliContext + ?Sized>(_ctx: &C, name: &Atom) -> Module {
        // SAFETY: `_ctx` witnesses the runtime is initialized; `name` is a
        // live atom (A1). PL_new_module finds-or-creates and has no failure
        // sentinel (A3).
        let raw = unsafe { swipl_sys::PL_new_module(name.as_raw()) };
        Module { raw }
    }

    /// Finds or creates the module with the given name from text.
    pub fn from_name<C: FliContext + ?Sized>(ctx: &C, name: &str) -> Module {
        Module::new(ctx, &Atom::new(ctx, name))
    }

    /// Wraps a raw module handle (e.g. read from a predicate). Modules are
    /// global and never garbage collected, so the handle carries no reference
    /// count.
    ///
    /// # Safety
    ///
    /// `raw` must be a live module handle and the process-global runtime must
    /// have been initialized.
    pub(crate) unsafe fn from_raw(raw: module_t) -> Module {
        Module { raw }
    }

    /// The module's name atom (`PL_module_name`).
    pub fn name(&self) -> Atom {
        // SAFETY: `self.raw` is a live module handle; its name is a live atom,
        // and `from_raw` takes its own registration (A1).
        unsafe { Atom::from_raw(swipl_sys::PL_module_name(self.raw)) }
    }

    pub(crate) fn as_raw(&self) -> module_t {
        self.raw
    }
}

impl fmt::Debug for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Module").field(&self.name()).finish()
    }
}

/// A handle to a predicate, the callable unit [`Query`](crate::Query)
/// executes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Predicate {
    raw: predicate_t,
    arity: usize,
}

// SAFETY: predicate references are process-global stable handles explicitly
// intended to be cached and reused across engines and threads (A2).
unsafe impl Send for Predicate {}
unsafe impl Sync for Predicate {}

impl Predicate {
    /// The predicate for `functor` in `module` (`PL_pred`, find-or-create).
    ///
    /// Fails if the predicate does not exist and cannot be created — e.g. the
    /// module's `program_space` limit is exhausted, which makes `PL_pred`
    /// return null and raise a resource exception.
    pub fn new<C: FliContext + ?Sized>(
        _ctx: &C,
        functor: &Functor,
        module: &Module,
    ) -> Result<Predicate, HandleError> {
        // SAFETY: `_ctx` witnesses the runtime is initialized; the functor
        // and module handles are valid for the runtime's lifetime.
        let raw = unsafe { swipl_sys::PL_pred(functor.as_raw(), module.as_raw()) };
        if raw.is_null() {
            // A null return can carry a pending resource exception; capture and
            // clear it so it cannot leak onto a later, unrelated operation.
            return Err(match take_pending_exception() {
                Some(exception) => HandleError::Exception(exception),
                None => HandleError::PredicateConstruction,
            });
        }
        Ok(Predicate {
            raw,
            arity: functor.arity(),
        })
    }

    /// Finds or creates `module:name/arity` from text (`PL_predicate`); a
    /// `None` module means the current (typically `user`) module.
    pub fn from_name<C: FliContext + ?Sized>(
        _ctx: &C,
        name: &str,
        arity: usize,
        module: Option<&str>,
    ) -> Result<Predicate, HandleError> {
        let name = CString::new(name).map_err(HandleError::InteriorNul)?;
        let module = module
            .map(|module| CString::new(module).map_err(HandleError::InteriorNul))
            .transpose()?;
        let arity_int =
            c_int::try_from(arity).map_err(|_| HandleError::PredicateArityOutOfRange { arity })?;
        // SAFETY: `_ctx` witnesses the runtime is initialized; both strings
        // are NUL-terminated and live across the call, which copies what it
        // needs.
        let raw = unsafe {
            swipl_sys::PL_predicate(
                name.as_ptr(),
                arity_int,
                module
                    .as_ref()
                    .map_or(ptr::null(), |module| module.as_ptr()),
            )
        };
        if raw.is_null() {
            // As in `new`: a null return can carry a pending resource
            // exception; capture and clear it rather than leaving it pending.
            return Err(match take_pending_exception() {
                Some(exception) => HandleError::Exception(exception),
                None => HandleError::PredicateConstruction,
            });
        }
        Ok(Predicate { raw, arity })
    }

    pub fn arity(&self) -> usize {
        self.arity
    }

    /// Reads the predicate's name, arity, and defining module in one call
    /// (`PL_predicate_info`).
    fn info(&self) -> (atom_t, usize, module_t) {
        let mut name: atom_t = 0;
        let mut arity: usize = 0;
        let mut module: module_t = ptr::null_mut();
        // SAFETY: `self.raw` is a valid predicate handle (A3); the out-pointers
        // are live stack locals. PL_predicate_info succeeds for a live
        // predicate.
        let ok =
            unsafe { swipl_sys::PL_predicate_info(self.raw, &mut name, &mut arity, &mut module) };
        assert!(ok, "splint: PL_predicate_info failed for a live predicate");
        (name, arity, module)
    }

    /// The predicate's name atom (`PL_predicate_info`).
    pub fn name(&self) -> Atom {
        // SAFETY: `name` is a live atom handle just read; `from_raw` takes its
        // own registration (A1).
        unsafe { Atom::from_raw(self.info().0) }
    }

    /// The predicate's defining module (`PL_predicate_info`).
    pub fn module(&self) -> Module {
        // SAFETY: `module` is a live process-global module handle just read.
        unsafe { Module::from_raw(self.info().2) }
    }

    pub(crate) fn as_raw(&self) -> predicate_t {
        self.raw
    }
}

impl fmt::Debug for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Predicate")
            .field("module", &self.module())
            .field("name", &self.name())
            .field("arity", &self.arity)
            .finish()
    }
}
