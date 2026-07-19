use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

use swipl_sys::{term_t, PL_fid_t};

use crate::handles::{Atom, Functor};
use crate::record::{Record, RecordError};
use crate::runtime::Runtime;
use crate::scope::{self, Activation};

mod sealed {
    use crate::scope::Activation;

    pub trait Sealed {
        /// The activation this context's scopes and allocations belong to:
        /// its engine generation, and the scope depth at which it is the
        /// innermost context (C2/C3).
        fn activation(&self) -> Activation;
    }
}

pub(crate) use sealed::Sealed;

/// A Prolog exception captured as text.
///
/// The exception term is rendered (as by `writeq/1`) at the moment it is
/// observed, before the raising query or frame is torn down, and the engine's
/// pending-exception state is cleared. The representation is deliberately a
/// plain [`String`]: the raw exception term dies with the scope that raised
/// it, and carrying a `PL_record` instead would tie every error type to the
/// runtime's lifetime. Structured exception inspection is future work.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PrologException(pub String);

/// An error from a term operation.
#[derive(Debug, thiserror::Error)]
pub enum TermError {
    /// The term does not have the shape the operation requires. This is the
    /// normal, recoverable "wrong type" outcome of the `PL_get_*` family.
    #[error("expected a term convertible to {expected}")]
    TypeMismatch { expected: &'static str },
    /// The number of argument terms does not match the functor's arity.
    #[error("functor of arity {expected} cannot be built from {actual} arguments")]
    ArityMismatch { expected: usize, actual: usize },
    /// A dict was given a different number of keys than values.
    #[error("dict with {keys} key(s) cannot be built from {values} value(s)")]
    DictLengthMismatch { keys: usize, values: usize },
    /// A list operation required a proper, acyclic list ending in `[]`, but
    /// the term's spine was something else (partial, improper, or cyclic).
    #[error("term is not a proper list ({0:?})")]
    NotAProperList(ListShape),
    /// The operation raised a Prolog exception (e.g. a resource or syntax
    /// error); the exception has been cleared from the engine.
    #[error("prolog exception: {0}")]
    Exception(#[source] PrologException),
}

/// An error from opening a foreign frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("PL_open_foreign_frame reported failure")]
    OpenFailed,
    /// Opening the frame raised a Prolog exception (e.g. a resource error);
    /// the exception has been cleared from the engine.
    #[error("prolog exception: {0}")]
    Exception(#[source] PrologException),
}

/// Reads the text of `raw` via `PL_get_nchars`. `flags` must include a
/// `CVT_*` selection; `BUF_MALLOC | REP_UTF8` are always added, so the
/// returned buffer is copied into an owned `String` and freed immediately,
/// decoupling its lifetime from term scoping.
///
/// # Safety
///
/// `raw` must be a live term reference on the thread's current engine.
pub(crate) unsafe fn text_from_term(raw: term_t, cvt: c_uint) -> Option<String> {
    let mut chars: *mut c_char = ptr::null_mut();
    let mut len: usize = 0;
    let flags = cvt | swipl_sys::BUF_MALLOC | swipl_sys::REP_UTF8;
    // SAFETY: `raw` is live per this function's contract; `BUF_MALLOC`
    // makes SWI-Prolog return a PL_malloc'ed buffer we own and free below.
    let ok = unsafe { swipl_sys::PL_get_nchars(raw, &mut len, &mut chars, flags) };
    if !ok {
        return None;
    }
    // SAFETY: on success `chars` points to `len` bytes (plus a NUL) that we
    // own; they are copied out before being freed.
    let bytes = unsafe { std::slice::from_raw_parts(chars.cast::<u8>(), len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: `chars` was allocated by PL_malloc per BUF_MALLOC.
    unsafe { swipl_sys::PL_free(chars.cast::<c_void>()) };
    Some(text)
}

/// Captures and clears the exception pending on the given query (or, with a
/// null `qid`, on the engine itself). Returns `None` if no exception is
/// pending. Must run while the raising query/frame is still open, so the
/// exception term is still live for rendering.
pub(crate) fn take_exception(qid: swipl_sys::qid_t) -> Option<PrologException> {
    // SAFETY: caller context guarantees an engine is current on this thread
    // (all call sites hold an FliContext witness); a null qid means "the
    // engine's pending exception", the sentinel PL_clear_exception pairs
    // with.
    let raw = unsafe { swipl_sys::PL_exception(qid) };
    if raw == 0 {
        return None;
    }
    // SAFETY: `raw` was just returned as a live exception term reference.
    let text = unsafe { text_from_term(raw, swipl_sys::CVT_WRITEQ) }
        .unwrap_or_else(|| "<unrenderable exception>".to_owned());
    // SAFETY: an exception is pending (checked above); clearing it restores
    // a clean engine state, which converting to a Rust error requires.
    unsafe { swipl_sys::PL_clear_exception() };
    Some(PrologException(text))
}

/// Captures and clears the engine's pending exception, if any.
pub(crate) fn take_pending_exception() -> Option<PrologException> {
    take_exception(ptr::null_mut())
}

/// Interprets the result of a `PL_put_*`/`PL_cons_*` call. These have no
/// "wrong type" concept, so failure without a pending exception means
/// SWI-Prolog violated its own contract and panicking is the only honest
/// response (mirroring the crate's treatment of other violated FLI
/// invariants).
fn check_put(ok: bool, operation: &'static str) -> Result<(), TermError> {
    if ok {
        return Ok(());
    }
    match take_pending_exception() {
        Some(exception) => Err(TermError::Exception(exception)),
        None => panic!("splint: {operation} reported failure with no pending exception"),
    }
}

/// Interprets the result of a `PL_get_*` call: failure without a pending
/// exception is the normal "wrong shape" outcome.
fn check_get(ok: bool, expected: &'static str) -> Result<(), TermError> {
    if ok {
        return Ok(());
    }
    match take_pending_exception() {
        Some(exception) => Err(TermError::Exception(exception)),
        None => Err(TermError::TypeMismatch { expected }),
    }
}

/// A sealed witness that an engine is current on the calling thread, through
/// which term references, term lists, and frames are created. Implemented by
/// [`AttachedEngine`](crate::AttachedEngine),
/// [`CurrentEngine`](crate::CurrentEngine), [`Frame`], and
/// [`Query`](crate::Query).
///
/// The provided methods are context-independent at the FFI level —
/// `PL_new_term_ref(s)` and `PL_open_foreign_frame` operate on the calling
/// thread's current engine, not on data stored in `self` — so single default
/// implementations are correct for every implementor. What `self`
/// contributes is the *witness*: the returned values borrow it, so they
/// cannot outlive the scope that allocated them (F1), and its recorded
/// activation lets the crate verify dynamically that `self` is the innermost
/// open scope of the current engine (C2/C3), which the borrow checker cannot
/// see across sibling scopes or engine switches.
pub trait FliContext: Sealed {
    /// Allocates a fresh term reference (`PL_new_term_ref`), initially an
    /// unbound variable, valid until this context's scope ends.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not the innermost open scope of the thread's
    /// current engine (C2/C3): the reference would be allocated in the
    /// innermost open frame and die before its type says it does.
    fn term(&self) -> Result<Term<'_>, TermError> {
        let activation = self.activation();
        scope::assert_innermost(activation, "term reference");
        // SAFETY: `self` witnesses that an engine is current on this thread
        // (F1), and the assert above proves the allocation lands in this
        // context's scope (C2).
        let raw = unsafe { swipl_sys::PL_new_term_ref() };
        if raw == 0 {
            return match take_pending_exception() {
                Some(exception) => Err(TermError::Exception(exception)),
                None => panic!("splint: PL_new_term_ref failed with no pending exception"),
            };
        }
        Ok(Term {
            raw,
            gen: activation.gen,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Allocates `len` contiguous fresh term references (`PL_new_term_refs`)
    /// — the argument vector shape `PL_cons_functor_v` and `PL_open_query`
    /// require.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not the innermost open scope of the thread's
    /// current engine (C2/C3), as for [`FliContext::term`].
    fn terms(&self, len: usize) -> Result<TermList<'_>, TermError> {
        let activation = self.activation();
        scope::assert_innermost(activation, "term reference block");
        // SAFETY: as for `term`; a zero count is accepted by the C API and
        // yields a base reference that is never dereferenced.
        let first = unsafe { swipl_sys::PL_new_term_refs(len) };
        if first == 0 && len > 0 {
            return match take_pending_exception() {
                Some(exception) => Err(TermError::Exception(exception)),
                None => panic!("splint: PL_new_term_refs failed with no pending exception"),
            };
        }
        Ok(TermList {
            first,
            len,
            gen: activation.gen,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Opens a nested foreign frame borrowing `self`; sugar for
    /// [`Frame::open`].
    fn frame(&self) -> Result<Frame<'_>, FrameError> {
        Frame::open(self)
    }
}

impl Sealed for crate::AttachedEngine<'_> {
    fn activation(&self) -> Activation {
        self.activation()
    }
}
impl FliContext for crate::AttachedEngine<'_> {}

impl Sealed for crate::CurrentEngine<'_> {
    fn activation(&self) -> Activation {
        self.activation()
    }
}
impl FliContext for crate::CurrentEngine<'_> {}

impl Sealed for Frame<'_> {
    fn activation(&self) -> Activation {
        Activation {
            gen: self.gen,
            depth: self.depth + 1,
        }
    }
}
impl FliContext for Frame<'_> {}

/// An open foreign frame (`PL_open_foreign_frame`): the unit of term-ref
/// allocation and backtracking for foreign code.
///
/// Term references allocated inside the frame die with it; bindings made
/// inside it survive only [`Frame::close`]. Dropping the frame *discards* it
/// (undoes bindings) — keeping work requires the affirmative `close()` call
/// (F6). Frames must be closed in the reverse order they were opened; the
/// borrow checker enforces this for nested scopes, and a per-thread check
/// panics on sibling-order violations (C2).
pub struct Frame<'c> {
    /// Invariant: a live frame id owned by this value, closed exactly once
    /// by `close`/`discard`/`Drop`.
    fid: PL_fid_t,
    gen: u64,
    /// The thread's scope depth when this frame was opened; the frame is the
    /// innermost scope exactly while the current depth equals `depth + 1`.
    depth: usize,
    _ctx: PhantomData<&'c ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'c> Frame<'c> {
    /// Opens a frame as the new innermost scope of `ctx`'s engine.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's
    /// current engine (C2/C3).
    pub fn open<C: FliContext + ?Sized>(ctx: &'c C) -> Result<Frame<'c>, FrameError> {
        let activation = ctx.activation();
        let depth = scope::open_scope(activation, "frame");
        // SAFETY: `ctx` witnesses that an engine is current on this thread
        // (F1).
        let fid = unsafe { swipl_sys::PL_open_foreign_frame() };
        if fid == 0 {
            scope::close_scope(activation.gen, depth, "frame");
            return Err(match take_pending_exception() {
                Some(exception) => FrameError::Exception(exception),
                None => FrameError::OpenFailed,
            });
        }
        Ok(Frame {
            fid,
            gen: activation.gen,
            depth,
            _ctx: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Closes the frame, keeping bindings made inside it. Term references
    /// allocated inside the frame are freed (their borrows of the frame end
    /// with this consuming call, so none can be live).
    ///
    /// # Panics
    ///
    /// Panics if the frame is not the innermost open scope (C2).
    pub fn close(self) {
        scope::close_scope(self.gen, self.depth, "frame");
        // SAFETY: `self.fid` is a live frame id (frame invariant), this
        // frame is the innermost scope (the close_scope assert above, C2),
        // and consuming `self` proves no borrows of it survive (F1).
        unsafe { swipl_sys::PL_close_foreign_frame(self.fid) };
        std::mem::forget(self);
    }

    /// Discards the frame, undoing all bindings made inside it and freeing
    /// the term references allocated inside it.
    ///
    /// # Panics
    ///
    /// Panics if the frame is not the innermost open scope (C2).
    pub fn discard(self) {
        scope::close_scope(self.gen, self.depth, "frame");
        // SAFETY: as for `close`.
        unsafe { swipl_sys::PL_discard_foreign_frame(self.fid) };
        std::mem::forget(self);
    }

    /// Undoes all bindings and frees all term references made since the
    /// frame was opened (or last rewound), without closing it.
    ///
    /// Takes `&mut self`, so the borrow checker rejects the call while any
    /// term reference or nested scope borrowed from this frame is live —
    /// exactly the handles `PL_rewind_foreign_frame` would silently
    /// invalidate (F4).
    ///
    /// # Panics
    ///
    /// Panics if the frame is not the innermost open scope (C2): a scope
    /// opened through a [`CurrentEngine`](crate::CurrentEngine) witness can
    /// alias this frame's position without borrowing it, and rewinding
    /// across it would invalidate its frame id.
    pub fn rewind(&mut self) {
        scope::assert_innermost(self.activation(), "rewind");
        // SAFETY: `self.fid` is live, this frame is the innermost scope
        // (assert above), and `&mut self` proves no term borrowed from this
        // frame survives the rewind (F4).
        unsafe { swipl_sys::PL_rewind_foreign_frame(self.fid) };
    }

    /// The raw frame id. Exposed for tests and escape hatches; closing or
    /// discarding it outside this type's control voids the safety guarantees
    /// documented on [`Frame`].
    #[doc(hidden)]
    pub fn as_raw(&self) -> PL_fid_t {
        self.fid
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // Discard, not close: undoing bindings is well-defined regardless of
        // what happened inside the frame, including a mid-frame panic (F6).
        if scope::try_close_scope(self.gen, self.depth) {
            // SAFETY: `self.fid` is live and this frame was verified to be
            // the innermost scope (C2); `close`/`discard` did not run (they
            // forget `self`).
            unsafe { swipl_sys::PL_discard_foreign_frame(self.fid) };
        } else if !std::thread::panicking() {
            panic!(
                "splint: frame dropped out of order: frames and queries must \
                 close in exactly the reverse order they were opened"
            );
        }
        // While unwinding from an earlier panic, an inconsistent frame is
        // leaked silently rather than double-panicking into an abort.
    }
}

/// The classification `PL_term_type` assigns to a term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Variable,
    Atom,
    Integer,
    Rational,
    Float,
    String,
    /// A compound term that is not a list cell.
    Compound,
    Nil,
    Blob,
    /// A non-empty list cell (`'[|]'/2`).
    ListPair,
    Dict,
}

/// How [`Term::list_shape`] classifies a term's list spine (`PL_skip_list`),
/// which walks the spine cycle-safely instead of following it blindly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListShape {
    /// A proper list ending in `[]`; `len` is the number of elements (`0` for
    /// the empty list itself).
    Proper { len: usize },
    /// A partial list ending in an unbound variable (e.g. `[a | _]`).
    Partial,
    /// An improper list ending in a non-list, non-`[]` term (e.g. `[a | b]`).
    Improper,
    /// A cyclic list, whose spine would loop forever if followed naively.
    Cyclic,
    /// Not a list at all: neither `[]` nor a list cell.
    NotAList,
}

/// A handle to a Prolog term: a term reference allocated from an
/// [`FliContext`].
///
/// `Term` is a small `Copy` value; copies alias the same underlying
/// reference, which is how the imperative `PL_put_*`/`PL_get_*` model works
/// in C. The lifetime ties the handle to the scope that allocated it (F1),
/// and every operation verifies the allocating engine is still the thread's
/// current engine (C3).
#[derive(Clone, Copy)]
pub struct Term<'f> {
    raw: term_t,
    gen: u64,
    _scope: PhantomData<&'f ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

/// A contiguous block of term references (`PL_new_term_refs`), as required
/// for compound-term arguments (`PL_cons_functor_v`) and query argument
/// vectors (`PL_open_query`).
#[derive(Clone, Copy)]
pub struct TermList<'f> {
    first: term_t,
    len: usize,
    gen: u64,
    _scope: PhantomData<&'f ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'f> TermList<'f> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `index`-th term of the block (0-based).
    ///
    /// # Panics
    ///
    /// Panics if `index >= len`.
    pub fn get(&self, index: usize) -> Term<'f> {
        assert!(
            index < self.len,
            "splint: term index {index} out of bounds for a list of {} references",
            self.len,
        );
        Term {
            raw: self.first + index,
            gen: self.gen,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = Term<'f>> + '_ {
        (0..self.len).map(|index| self.get(index))
    }

    /// The raw base term reference. Exposed for tests and escape hatches.
    #[doc(hidden)]
    pub fn as_raw(&self) -> term_t {
        self.first
    }

    pub(crate) fn gen(&self) -> u64 {
        self.gen
    }
}

impl<'f> Term<'f> {
    /// Resets the term to a fresh unbound variable (`PL_put_variable`).
    pub fn put_variable(&self) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: `self.raw` is a live reference on the current engine (C3
        // assert above); likewise for every FFI call on `self.raw` below.
        check_put(unsafe { swipl_sys::PL_put_variable(self.raw) }, "PL_put_variable")
    }

    /// Makes the term reference the given atom (`PL_put_atom`).
    pub fn put_atom(&self, atom: &Atom<'_>) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above; the atom handle is valid while `atom`
        // holds its registration (A1).
        check_put(
            unsafe { swipl_sys::PL_put_atom(self.raw, atom.as_raw()) },
            "PL_put_atom",
        )
    }

    /// Makes the term an atom with the given text (`PL_put_atom_nchars`).
    pub fn put_atom_text(&self, text: &str) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above; the pointer/length pair is a valid UTF-8
        // buffer for the duration of the call, which copies it.
        check_put(
            unsafe { swipl_sys::PL_put_atom_nchars(self.raw, text.len(), text.as_ptr().cast()) },
            "PL_put_atom_nchars",
        )
    }

    /// Makes the term a Prolog string (`PL_put_string_nchars`).
    pub fn put_string(&self, text: &str) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: as for `put_atom_text`.
        check_put(
            unsafe { swipl_sys::PL_put_string_nchars(self.raw, text.len(), text.as_ptr().cast()) },
            "PL_put_string_nchars",
        )
    }

    /// Makes the term an integer (`PL_put_int64`).
    pub fn put_i64(&self, value: i64) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(unsafe { swipl_sys::PL_put_int64(self.raw, value) }, "PL_put_int64")
    }

    /// Makes the term an integer (`PL_put_uint64`).
    pub fn put_u64(&self, value: u64) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(unsafe { swipl_sys::PL_put_uint64(self.raw, value) }, "PL_put_uint64")
    }

    /// Makes the term a float (`PL_put_float`).
    pub fn put_f64(&self, value: f64) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(unsafe { swipl_sys::PL_put_float(self.raw, value) }, "PL_put_float")
    }

    /// Makes the term `true` or `false` (`PL_put_bool`).
    pub fn put_bool(&self, value: bool) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(
            unsafe { swipl_sys::PL_put_bool(self.raw, c_int::from(value)) },
            "PL_put_bool",
        )
    }

    /// Makes the term the empty list (`PL_put_nil`).
    pub fn put_nil(&self) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(unsafe { swipl_sys::PL_put_nil(self.raw) }, "PL_put_nil")
    }

    /// Makes the term reference the same value as `other` (`PL_put_term`).
    pub fn put_term(&self, other: Term<'_>) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(other.gen, "term");
        // SAFETY: C3 asserts above cover both references.
        check_put(unsafe { swipl_sys::PL_put_term(self.raw, other.raw) }, "PL_put_term")
    }

    /// Parses `text` as a Prolog term and stores it in this reference
    /// (`PL_put_term_from_chars`). Syntax errors surface as
    /// [`TermError::Exception`].
    pub fn put_term_from_text(&self, text: &str) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // CVT_EXCEPTION makes syntax errors raise a normal Prolog exception;
        // without it the error is stored *as a term* in `self` with no
        // exception pending, which would be indistinguishable from success
        // at this layer.
        let flags = (swipl_sys::REP_UTF8 | swipl_sys::CVT_EXCEPTION) as c_int;
        // SAFETY: C3 assert above; the pointer/length pair is a valid buffer
        // for the duration of the call, which parses (copies) it.
        let ok = unsafe {
            swipl_sys::PL_put_term_from_chars(self.raw, flags, text.len(), text.as_ptr().cast())
        };
        check_put(ok, "PL_put_term_from_chars")
    }

    /// Builds the compound `functor(args...)` in this reference
    /// (`PL_cons_functor_v`). `args` must have exactly the functor's arity.
    pub fn cons_functor(
        &self,
        functor: &Functor<'_>,
        args: &TermList<'_>,
    ) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(args.gen(), "term");
        if args.len() != functor.arity() {
            return Err(TermError::ArityMismatch {
                expected: functor.arity(),
                actual: args.len(),
            });
        }
        // SAFETY: C3 asserts above; `args` is the base of `arity` contiguous
        // live references (TermList invariant, checked against the functor
        // arity just above).
        check_put(
            unsafe { swipl_sys::PL_cons_functor_v(self.raw, functor.as_raw(), args.as_raw()) },
            "PL_cons_functor_v",
        )
    }

    /// Builds the list cell `[head | tail]` in this reference
    /// (`PL_cons_list`).
    pub fn cons_list(&self, head: Term<'_>, tail: Term<'_>) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(head.gen, "term");
        scope::assert_gen(tail.gen, "term");
        // SAFETY: C3 asserts above cover all three references.
        check_put(
            unsafe { swipl_sys::PL_cons_list(self.raw, head.raw, tail.raw) },
            "PL_cons_list",
        )
    }

    /// Reads the term as an `i64` (`PL_get_int64`).
    pub fn get_i64(&self) -> Result<i64, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut value: i64 = 0;
        // SAFETY: C3 assert above; the out-pointer is a live stack local.
        check_get(
            unsafe { swipl_sys::PL_get_int64(self.raw, &mut value) },
            "an integer fitting i64",
        )?;
        Ok(value)
    }

    /// Reads the term as a `u64` (`PL_get_uint64`).
    pub fn get_u64(&self) -> Result<u64, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut value: u64 = 0;
        // SAFETY: as for `get_i64`.
        check_get(
            unsafe { swipl_sys::PL_get_uint64(self.raw, &mut value) },
            "an integer fitting u64",
        )?;
        Ok(value)
    }

    /// Reads the term as an `f64` (`PL_get_float`).
    pub fn get_f64(&self) -> Result<f64, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut value: f64 = 0.0;
        // SAFETY: as for `get_i64`.
        check_get(
            unsafe { swipl_sys::PL_get_float(self.raw, &mut value) },
            "a float",
        )?;
        Ok(value)
    }

    /// Reads the term as a boolean (`PL_get_bool`): the atoms `true`/`on` and
    /// `false`/`off`.
    pub fn get_bool(&self) -> Result<bool, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut value: c_int = 0;
        // SAFETY: as for `get_i64`.
        check_get(
            unsafe { swipl_sys::PL_get_bool(self.raw, &mut value) },
            "a boolean",
        )?;
        Ok(value != 0)
    }

    /// Reads the term as an atom handle (`PL_get_atom`).
    pub fn get_atom(&self) -> Result<Atom<'f>, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut raw: swipl_sys::atom_t = 0;
        // SAFETY: as for `get_i64`.
        check_get(unsafe { swipl_sys::PL_get_atom(self.raw, &mut raw) }, "an atom")?;
        // SAFETY: `raw` is a live atom handle just read from a term;
        // `from_raw` takes its own registration (A1).
        Ok(unsafe { Atom::from_raw(raw) })
    }

    /// Reads the text an atomic term denotes (`PL_get_nchars` with
    /// `CVT_ATOMIC`): atoms, strings, and numbers. Compounds and variables
    /// are a [`TermError::TypeMismatch`]; use [`Term::write_to_string`] to
    /// render arbitrary terms.
    pub fn get_text(&self) -> Result<String, TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        match unsafe { text_from_term(self.raw, swipl_sys::CVT_ATOMIC) } {
            Some(text) => Ok(text),
            None => Err(match take_pending_exception() {
                Some(exception) => TermError::Exception(exception),
                None => TermError::TypeMismatch {
                    expected: "an atomic term with a text representation",
                },
            }),
        }
    }

    /// Renders any term to text as `writeq/1` would (`PL_get_nchars` with
    /// `CVT_WRITEQ`).
    pub fn write_to_string(&self) -> Result<String, TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        match unsafe { text_from_term(self.raw, swipl_sys::CVT_WRITEQ) } {
            Some(text) => Ok(text),
            None => Err(match take_pending_exception() {
                Some(exception) => TermError::Exception(exception),
                None => TermError::TypeMismatch {
                    expected: "a writable term",
                },
            }),
        }
    }

    /// Reads a compound term's name and arity
    /// (`PL_get_compound_name_arity_sz`).
    pub fn name_arity(&self) -> Result<(Atom<'f>, usize), TermError> {
        scope::assert_gen(self.gen, "term");
        let mut name: swipl_sys::atom_t = 0;
        let mut arity: usize = 0;
        // SAFETY: C3 assert above; the out-pointers are live stack locals.
        check_get(
            unsafe { swipl_sys::PL_get_compound_name_arity_sz(self.raw, &mut name, &mut arity) },
            "a compound term",
        )?;
        // SAFETY: `name` is a live atom handle just read from a compound.
        Ok((unsafe { Atom::from_raw(name) }, arity))
    }

    /// Reads the functor of a compound term — or of an atom, as its arity-0
    /// functor — into a [`Functor`] handle branded to `ctx` (`PL_get_functor`).
    /// Unlike [`Term::name_arity`], the result is a reusable handle (e.g. for
    /// [`Term::cons_functor`] or [`Predicate::new`](crate::Predicate::new)).
    pub fn get_functor<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Functor<'a>, TermError> {
        scope::assert_gen(self.gen, "term");
        let mut raw: swipl_sys::functor_t = 0;
        // SAFETY: C3 assert above; the out-pointer is a live stack local.
        check_get(
            unsafe { swipl_sys::PL_get_functor(self.raw, &mut raw) },
            "a compound term or atom",
        )?;
        // SAFETY: `raw` is a live functor handle just read from the term;
        // `ctx` pins the runtime for the returned handle's lifetime (A2).
        Ok(unsafe { Functor::from_raw(ctx, raw) })
    }

    /// Reads the `index`-th argument (0-based) of a compound term into a
    /// fresh reference allocated from `ctx` (`PL_get_arg_sz`).
    pub fn get_arg<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
        index: usize,
    ) -> Result<Term<'a>, TermError> {
        scope::assert_gen(self.gen, "term");
        let dest = ctx.term()?;
        // SAFETY: C3 assert above; `dest` is a live fresh reference; the C
        // API's argument index is 1-based.
        check_get(
            unsafe { swipl_sys::PL_get_arg_sz(index + 1, self.raw, dest.raw) },
            "a compound term with enough arguments",
        )?;
        Ok(dest)
    }

    /// Decomposes a non-empty list into head and tail references allocated
    /// from `ctx` (`PL_get_list`).
    pub fn get_list<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<(Term<'a>, Term<'a>), TermError> {
        scope::assert_gen(self.gen, "term");
        let head = ctx.term()?;
        let tail = ctx.term()?;
        // SAFETY: C3 assert above; `head`/`tail` are live fresh references.
        check_get(
            unsafe { swipl_sys::PL_get_list(self.raw, head.raw, tail.raw) },
            "a non-empty list",
        )?;
        Ok((head, tail))
    }

    /// Classifies the term's list spine (`PL_skip_list`), distinguishing a
    /// proper list from a partial (`[a|_]`), improper (`[a|b]`), or cyclic
    /// one. Unlike following [`Term::get_list`] in a loop, this terminates on
    /// a cyclic list rather than spinning forever.
    pub fn list_shape(&self) -> ListShape {
        scope::assert_gen(self.gen, "term");
        let mut len: usize = 0;
        // SAFETY: C3 assert above; passing `0` (no term reference) for the
        // tail tells PL_skip_list not to return the tail, which is not needed
        // here. PL_skip_list is cycle-safe by construction.
        let status = unsafe { swipl_sys::PL_skip_list(self.raw, 0, &mut len) };
        match status as u32 {
            swipl_sys::PL_LIST => ListShape::Proper { len },
            swipl_sys::PL_PARTIAL_LIST => ListShape::Partial,
            swipl_sys::PL_CYCLIC_TERM => ListShape::Cyclic,
            swipl_sys::PL_NOT_A_LIST => {
                // PL_skip_list reports NOT_A_LIST both for a term that is not a
                // list cell at all and for an improper list whose spine ends
                // in a bound non-`[]` term; the starting term being a pair
                // distinguishes the latter.
                // SAFETY: C3 asserted above still holds.
                if unsafe { swipl_sys::PL_is_pair(self.raw) } {
                    ListShape::Improper
                } else {
                    ListShape::NotAList
                }
            }
            other => panic!("splint: PL_skip_list returned an unrecognized status code {other}"),
        }
    }

    /// Collects a proper, acyclic list into a vector of element references
    /// allocated from `ctx`. A non-proper spine (partial, improper, or cyclic)
    /// is a [`TermError::NotAProperList`]; because the spine is verified with
    /// [`Term::list_shape`] first, this never loops on a cyclic list.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3): the element references are allocated through it.
    pub fn collect_list<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<Term<'a>>, TermError> {
        scope::assert_gen(self.gen, "term");
        let len = match self.list_shape() {
            ListShape::Proper { len } => len,
            other => return Err(TermError::NotAProperList(other)),
        };
        let mut items = Vec::with_capacity(len);
        // The spine is a verified proper list of `len` cells, so exactly `len`
        // decompositions reach `[]` — a bounded, cycle-free walk.
        let mut cursor = self.copy_term_ref(ctx)?;
        for _ in 0..len {
            let (head, tail) = cursor.get_list(ctx)?;
            items.push(head);
            cursor = tail;
        }
        Ok(items)
    }

    /// Whether the term is the empty list (`PL_get_nil`).
    pub fn is_nil(&self) -> bool {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        unsafe { swipl_sys::PL_get_nil(self.raw) }
    }

    /// Allocates a fresh reference from `ctx` pointing at the same term
    /// (`PL_copy_term_ref`).
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope (C2/C3), as for
    /// [`FliContext::term`].
    pub fn copy_term_ref<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Term<'a>, TermError> {
        scope::assert_gen(self.gen, "term");
        let activation = ctx.activation();
        scope::assert_innermost(activation, "term reference");
        // SAFETY: C3/C2 asserts above; PL_copy_term_ref allocates a new
        // reference in the innermost frame, which the asserts prove is
        // `ctx`'s scope.
        let raw = unsafe { swipl_sys::PL_copy_term_ref(self.raw) };
        if raw == 0 {
            return match take_pending_exception() {
                Some(exception) => Err(TermError::Exception(exception)),
                None => panic!("splint: PL_copy_term_ref failed with no pending exception"),
            };
        }
        Ok(Term {
            raw,
            gen: activation.gen,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Unifies the two terms (`PL_unify`). `Ok(false)` is clean
    /// non-unification; bindings made by a partially successful unification
    /// are only undone by the enclosing frame/query, as in C.
    pub fn unify(&self, other: Term<'_>) -> Result<bool, TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(other.gen, "term");
        // SAFETY: C3 asserts above cover both references.
        let ok = unsafe { swipl_sys::PL_unify(self.raw, other.raw) };
        if ok {
            return Ok(true);
        }
        match take_pending_exception() {
            Some(exception) => Err(TermError::Exception(exception)),
            None => Ok(false),
        }
    }

    /// Classifies the term (`PL_term_type`).
    pub fn kind(&self) -> TermKind {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        let raw = unsafe { swipl_sys::PL_term_type(self.raw) };
        match raw as u32 {
            swipl_sys::PL_VARIABLE => TermKind::Variable,
            swipl_sys::PL_ATOM => TermKind::Atom,
            swipl_sys::PL_INTEGER => TermKind::Integer,
            swipl_sys::PL_RATIONAL => TermKind::Rational,
            swipl_sys::PL_FLOAT => TermKind::Float,
            swipl_sys::PL_STRING => TermKind::String,
            swipl_sys::PL_TERM => TermKind::Compound,
            swipl_sys::PL_NIL => TermKind::Nil,
            swipl_sys::PL_BLOB => TermKind::Blob,
            swipl_sys::PL_LIST_PAIR => TermKind::ListPair,
            swipl_sys::PL_DICT => TermKind::Dict,
            other => panic!("splint: PL_term_type returned an unrecognized type code {other}"),
        }
    }

    /// Whether the term is an unbound variable (`PL_is_variable`).
    pub fn is_variable(&self) -> bool {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        unsafe { swipl_sys::PL_is_variable(self.raw) }
    }

    /// Records this term into SWI-Prolog's engine-independent recorded
    /// database (`PL_record`), returning a [`Record`] that outlives this
    /// term's scope.
    ///
    /// `runtime` supplies the returned record's lifetime brand, which is
    /// independent of this term's own scope — that independence is the point:
    /// the record survives frame close, backtracking, and engine switches.
    pub fn record<'rt>(&self, _runtime: &'rt Runtime) -> Result<Record<'rt>, RecordError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above; `PL_record` copies the term into the global
        // recorded database and returns a fresh handle carrying one erase
        // obligation, which the `Record` takes on.
        let raw = unsafe { swipl_sys::PL_record(self.raw) };
        if raw.is_null() {
            return Err(match take_pending_exception() {
                Some(exception) => RecordError::Exception(exception),
                None => RecordError::Failed,
            });
        }
        Ok(Record::from_raw(raw))
    }

    /// Builds a dict `tag{key: value, ...}` in this reference
    /// (`PL_put_dict`). `keys` and `values` must have equal length; `values`
    /// is a contiguous block (as from [`FliContext::terms`]).
    ///
    /// `PL_put_dict` sorts the key/value pairs in place — reordering the
    /// `values` block — and rejects duplicate keys (surfaced as a
    /// [`TermError::Exception`]), so `values` should not be relied on to keep
    /// its original order after this call.
    pub fn put_dict(
        &self,
        tag: &Atom<'_>,
        keys: &[&Atom<'_>],
        values: &TermList<'_>,
    ) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(values.gen(), "term");
        if keys.len() != values.len() {
            return Err(TermError::DictLengthMismatch {
                keys: keys.len(),
                values: values.len(),
            });
        }
        let raw_keys: Vec<swipl_sys::atom_t> = keys.iter().map(|atom| atom.as_raw()).collect();
        // SAFETY: C3 asserts above cover `self` and the `values` block; `tag`
        // and the key atoms are live handles (A1); `raw_keys` and `values` are
        // both `keys.len()` long (checked above), matching the `len` argument.
        check_put(
            unsafe {
                swipl_sys::PL_put_dict(
                    self.raw,
                    tag.as_raw(),
                    raw_keys.len(),
                    raw_keys.as_ptr(),
                    values.as_raw(),
                )
            },
            "PL_put_dict",
        )
    }

    /// Reads the value stored under `key` in this dict into a fresh reference
    /// allocated from `ctx` (`PL_get_dict_key`). A missing key or a non-dict
    /// term is a [`TermError::TypeMismatch`].
    pub fn get_dict<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
        key: &Atom<'_>,
    ) -> Result<Term<'a>, TermError> {
        scope::assert_gen(self.gen, "term");
        let dest = ctx.term()?;
        // SAFETY: C3 assert above; `key` is a live atom handle (A1); `dest` is
        // a fresh live reference the value is read into.
        check_get(
            unsafe { swipl_sys::PL_get_dict_key(key.as_raw(), self.raw, dest.raw) },
            "a dict containing the key",
        )?;
        Ok(dest)
    }

    /// Enumerates this dict's key/value pairs (`PL_for_dict`), in the dict's
    /// sorted key order. Each value is copied into a fresh reference allocated
    /// from `ctx`. A non-dict term is a [`TermError::TypeMismatch`].
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3): the value copies are allocated in the innermost open
    /// frame, which the check proves is `ctx`'s scope.
    pub fn dict_entries<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<(DictKey<'a>, Term<'a>)>, TermError> {
        scope::assert_gen(self.gen, "term");
        let activation = ctx.activation();
        scope::assert_innermost(activation, "dict value reference");
        // SAFETY: C3 assert above; classifying is always valid.
        if !unsafe { swipl_sys::PL_is_dict(self.raw) } {
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        let mut collector = DictCollector {
            entries: Vec::new(),
            error: None,
            resource_failed: false,
        };
        // SAFETY: `self.raw` is a live dict on the current engine (C3 assert +
        // `PL_is_dict` above); the callback allocates value copies in the
        // innermost open frame, which the innermost assert above proves is
        // `ctx`'s scope, so they live exactly as long as `'a`.
        unsafe {
            swipl_sys::PL_for_dict(
                self.raw,
                Some(collect_dict_entry),
                (&mut collector as *mut DictCollector).cast(),
                swipl_sys::PL_FOR_DICT_SORTED as c_int,
            );
        }
        if collector.resource_failed {
            // Copying a value failed mid-iteration under resource exhaustion;
            // capture and clear the pending exception now (matching
            // `FliContext::term`'s handling of the same failure) so it cannot
            // leak onto a later operation.
            return Err(match take_pending_exception() {
                Some(exception) => TermError::Exception(exception),
                None => panic!("splint: a dict value copy failed with no pending exception"),
            });
        }
        if let Some(error) = collector.error {
            return Err(error);
        }
        Ok(collector
            .entries
            .into_iter()
            .map(|(key, value)| {
                let value = Term {
                    raw: value,
                    gen: activation.gen,
                    _scope: PhantomData,
                    _not_send_sync: PhantomData,
                };
                let key = match key {
                    RawDictKey::Atom(raw) => {
                        // SAFETY: `raw` is a live atom handle read from the
                        // dict during iteration; `from_raw` takes its own
                        // registration (A1).
                        DictKey::Atom(unsafe { Atom::from_raw(raw) })
                    }
                    RawDictKey::Int(value) => DictKey::Int(value),
                };
                (key, value)
            })
            .collect())
    }

    /// Reads this dict's tag: `Some(atom)` for an atom tag, `None` for an
    /// unbound (variable) tag. A non-dict term is a
    /// [`TermError::TypeMismatch`].
    ///
    /// The C API exposes no native tag accessor, so the tag is read through
    /// the `is_dict/2` builtin — the sanctioned path, as reading the internal
    /// dict layout would be fragile.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3): a query is opened through it.
    pub fn dict_tag<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Option<Atom<'a>>, TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        if !unsafe { swipl_sys::PL_is_dict(self.raw) } {
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        let is_dict = crate::Predicate::resolve(ctx, "is_dict", 2, None)
            .map_err(|_| TermError::TypeMismatch { expected: "a dict" })?;
        let args = ctx.terms(2)?;
        args.get(0).put_term(*self)?;
        let tag = args.get(1);
        let mut query = crate::Query::open(ctx, &is_dict, &args, crate::QueryOptions::default())
            .map_err(dict_query_error)?;
        let matched = query.next_solution().map_err(dict_query_error)?;
        if !matched {
            // No solution: discard and report the (already checked, but be
            // defensive) non-dict shape.
            query.close().map_err(dict_query_error)?;
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        // Keep the solution's bindings so the tag stays bound in `args`;
        // `close` would undo them.
        query.cut().map_err(dict_query_error)?;
        if tag.is_variable() {
            Ok(None)
        } else {
            Ok(Some(tag.get_atom()?))
        }
    }

    /// The raw term reference. Exposed for tests and escape hatches; using
    /// it after this term's scope ends voids the safety guarantees
    /// documented on [`Term`].
    #[doc(hidden)]
    pub fn as_raw(&self) -> term_t {
        self.raw
    }

    pub(crate) fn gen(&self) -> u64 {
        self.gen
    }
}

/// A dict key, which SWI-Prolog represents as either an atom or a small
/// integer.
pub enum DictKey<'c> {
    Atom(Atom<'c>),
    Int(i64),
}

/// The key form collected during `PL_for_dict` iteration, before atoms are
/// given owning registrations. Kept separate from [`DictKey`] so the callback
/// stays free of [`Atom`] construction (which the `?Sized` context lifetimes
/// make awkward inside an `extern "C"` function).
enum RawDictKey {
    Atom(swipl_sys::atom_t),
    Int(i64),
}

/// Accumulates dict entries across [`collect_dict_entry`] invocations.
struct DictCollector {
    entries: Vec<(RawDictKey, term_t)>,
    /// A *final* error with no pending Prolog exception (e.g. a malformed
    /// key), returned to the caller as-is.
    error: Option<TermError>,
    /// Set when copying a value failed under resource exhaustion
    /// (`PL_new_term_ref` returning 0, or `PL_put_term` returning false): a
    /// Prolog exception is pending and must be captured *after* `PL_for_dict`
    /// returns (rendering it inside the callback, while already out of stack,
    /// is best avoided).
    resource_failed: bool,
}

/// Records one dict entry per invocation (`PL_for_dict` callback). Returns
/// non-zero to abort iteration.
///
/// The `key` and `value` term references are only valid for the duration of
/// this call, so the key is read eagerly and the value is copied into a fresh
/// reference on the innermost open frame — which the caller has asserted is the
/// scope the collected values are attributed to.
unsafe extern "C" fn collect_dict_entry(
    key: term_t,
    value: term_t,
    closure: *mut std::os::raw::c_void,
) -> c_int {
    // SAFETY: `closure` is the `DictCollector` pointer passed to `PL_for_dict`,
    // exclusively borrowed here for the duration of this synchronous callback.
    let collector = unsafe { &mut *closure.cast::<DictCollector>() };
    let mut atom: swipl_sys::atom_t = 0;
    // SAFETY: `key`/`value` are live references for this call. Dict keys are
    // atoms or small integers; try each.
    let dict_key = if unsafe { swipl_sys::PL_get_atom(key, &mut atom) } {
        RawDictKey::Atom(atom)
    } else {
        let mut int: i64 = 0;
        if unsafe { swipl_sys::PL_get_int64(key, &mut int) } {
            RawDictKey::Int(int)
        } else {
            collector.error = Some(TermError::TypeMismatch {
                expected: "a dict key (atom or integer)",
            });
            return 1;
        }
    };
    // SAFETY: allocates in the innermost open frame (the caller's asserted
    // `ctx` scope); the copy outlives this call, unlike `value`.
    let copied = unsafe { swipl_sys::PL_new_term_ref() };
    if copied == 0 {
        // Resource exhaustion: a Prolog exception is pending. Flag it so the
        // caller captures and clears it after iteration, rather than leaving
        // it to surface on a later, unrelated operation.
        collector.resource_failed = true;
        return 1;
    }
    // SAFETY: `copied` and `value` are both live references on the current
    // engine. PL_put_term can fail under global-stack exhaustion (e.g.
    // globalizing a variable value), leaving a pending exception; treat that
    // like the allocation failure above rather than recording a bogus value.
    if !unsafe { swipl_sys::PL_put_term(copied, value) } {
        collector.resource_failed = true;
        return 1;
    }
    collector.entries.push((dict_key, copied));
    0
}

/// Maps a [`QueryError`](crate::QueryError) raised while reading a dict tag
/// back onto [`TermError`], preserving a captured Prolog exception.
fn dict_query_error(error: crate::QueryError) -> TermError {
    match error {
        crate::QueryError::Exception(exception) => TermError::Exception(exception),
        _ => TermError::TypeMismatch { expected: "a dict" },
    }
}
