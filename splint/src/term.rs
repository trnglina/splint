use std::marker::PhantomData;
use std::os::raw::c_int;

use swipl_sys::{atom_t, functor_t, term_t, PL_fid_t};

use crate::exception::{take_pending_exception, text_from_term, PrologException};
use crate::handles::{Atom, Functor};
use crate::record::{Record, RecordError};
use crate::scope::{self, Activation};
use crate::ScopedCallError;

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

mod dict;
pub use dict::DictKey;

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

/// Interprets the result of a `PL_put_*`/`PL_cons_*` call. These have no
/// "wrong type" concept, so failure without a pending exception is a contract
/// violation and panics.
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

    /// Runs `body` in a nested foreign frame, keeping its bindings when the
    /// callback returns normally. A panic discards the frame.
    ///
    /// The callback receives the frame by shared reference, so it cannot
    /// close, discard, or leak the frame managed by this method. Use
    /// [`FliContext::try_with_frame`] when a callback error should roll the
    /// frame back.
    ///
    /// Values allocated in the frame cannot escape:
    ///
    /// ```compile_fail
    /// use splint::{FliContext, Term};
    ///
    /// fn escape<'c, C: FliContext>(ctx: &'c C) -> Term<'c> {
    ///     ctx.with_frame(|frame| frame.term().unwrap()).unwrap()
    /// }
    /// ```
    fn with_frame<R>(&self, body: impl for<'a> FnOnce(&'a Frame<'a>) -> R) -> Result<R, FrameError>
    where
        Self: Sized,
    {
        let frame = self.frame()?;
        let result = body(&frame);
        frame.close();
        Ok(result)
    }

    /// Runs a fallible callback in a nested foreign frame.
    ///
    /// `Ok` closes the frame and keeps its bindings. `Err` or a panic
    /// discards it.
    fn try_with_frame<R, E>(
        &self,
        body: impl for<'a> FnOnce(&'a Frame<'a>) -> Result<R, E>,
    ) -> Result<R, ScopedCallError<FrameError, E>>
    where
        Self: Sized,
    {
        let frame = self.frame().map_err(ScopedCallError::Operation)?;
        match body(&frame) {
            Ok(result) => {
                frame.close();
                Ok(result)
            }
            Err(error) => {
                frame.discard();
                Err(ScopedCallError::Body(error))
            }
        }
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
        check_put(
            unsafe { swipl_sys::PL_put_variable(self.raw) },
            "PL_put_variable",
        )
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
        check_put(
            unsafe { swipl_sys::PL_put_int64(self.raw, value) },
            "PL_put_int64",
        )
    }

    /// Makes the term an integer (`PL_put_uint64`).
    pub fn put_u64(&self, value: u64) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(
            unsafe { swipl_sys::PL_put_uint64(self.raw, value) },
            "PL_put_uint64",
        )
    }

    /// Makes the term a float (`PL_put_float`).
    pub fn put_f64(&self, value: f64) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        check_put(
            unsafe { swipl_sys::PL_put_float(self.raw, value) },
            "PL_put_float",
        )
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
        check_put(
            unsafe { swipl_sys::PL_put_term(self.raw, other.raw) },
            "PL_put_term",
        )
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
        let mut raw: atom_t = 0;
        // SAFETY: as for `get_i64`.
        check_get(
            unsafe { swipl_sys::PL_get_atom(self.raw, &mut raw) },
            "an atom",
        )?;
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
        let mut name: atom_t = 0;
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
        let mut raw: functor_t = 0;
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
        // SAFETY: C3 assert above; the `0` tail argument means the tail is
        // not returned, which is not needed here.
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
    /// The record is an independent copy bound to no scope, so it survives
    /// frame close, backtracking, and engine switches.
    pub fn record(&self) -> Result<Record, RecordError> {
        let raw = crate::record::record_raw(*self)?;
        Ok(Record::from_raw(raw))
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
