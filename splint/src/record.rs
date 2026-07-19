use std::fmt;
use std::marker::PhantomData;

use crate::runtime::Runtime;
use crate::term::{take_pending_exception, FliContext, PrologException, Term};

/// An error from recording or recalling a term.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// `PL_record`/`PL_recorded` reported failure with no pending exception —
    /// a resource exhaustion the C API signalled only through its return
    /// value.
    #[error("PL_record/PL_recorded reported failure")]
    Failed,
    /// Recording or recalling raised a Prolog exception (e.g. a resource
    /// error); the exception has been cleared from the engine.
    #[error("prolog exception: {0}")]
    Exception(#[source] PrologException),
}

/// A term copied into SWI-Prolog's engine-independent *recorded database*
/// (`PL_record`).
///
/// A [`Record`] is the one handle in this crate designed to *escape* the scope
/// that produced it: unlike a [`Term`], whose value dies with its frame or
/// query and whose reference is tied to the current engine, a record holds an
/// independent copy that survives frame close, backtracking, and engine
/// switches, and may be recalled into any engine on any thread.
///
/// The lifetime binds the record to the [`Runtime`], not to any scope: it may
/// outlive every frame, query, and engine, but the borrow checker still
/// guarantees it is gone before [`Runtime::cleanup`] runs — which is what
/// makes the `PL_erase` in [`Record`]'s `Drop` sound (invariant RC1). The
/// record carries no engine generation, because it is engine-independent (like
/// [`Atom`](crate::Atom); invariant A2), and it is [`Send`] because SWI-Prolog
/// records are portable across threads and engines. Recalling still goes
/// through an [`FliContext`], which witnesses that an engine is current.
pub struct Record<'rt> {
    /// Invariant: a live record handle owned by this value, erased exactly
    /// once (by `Drop`, unless leaked).
    raw: swipl_sys::record_t,
    _rt: PhantomData<&'rt Runtime>,
}

// SAFETY (RC1): a `PL_record` handle refers to a global, lock-protected store
// that is independent of any engine or thread; recalling and erasing it are
// valid from any thread, so the handle may be moved across threads. It is
// deliberately not `Sync`: `&Record` shared between threads is not needed and
// would require reasoning about concurrent recalls of the same record.
unsafe impl Send for Record<'_> {}

impl<'rt> Record<'rt> {
    /// Records `term`, returning a handle bound to `runtime`.
    ///
    /// Equivalent to [`Term::record`]; the `runtime` argument supplies the
    /// `'rt` brand, which is independent of `term`'s own (soon-to-end) scope.
    pub fn of(runtime: &'rt Runtime, term: Term<'_>) -> Result<Record<'rt>, RecordError> {
        term.record(runtime)
    }

    /// Wraps a raw record handle. The caller transfers ownership of exactly
    /// one erase obligation to the returned value.
    pub(crate) fn from_raw(raw: swipl_sys::record_t) -> Record<'rt> {
        Record {
            raw,
            _rt: PhantomData,
        }
    }

    /// Recalls the recorded term into a fresh reference allocated from `ctx`
    /// (`PL_recorded`).
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3), as for [`FliContext::term`].
    pub fn recall<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Term<'a>, RecordError> {
        let dest = ctx.term().map_err(|_| RecordError::Failed)?;
        self.recall_into(dest)?;
        Ok(dest)
    }

    /// Recalls the recorded term into the existing reference `term`
    /// (`PL_recorded`), overwriting whatever it held.
    ///
    /// Use this to recall into a slot that already exists — for instance a
    /// query argument or a term about to be unified — rather than allocating a
    /// fresh reference as [`Record::recall`] does.
    pub fn recall_into(&self, term: Term<'_>) -> Result<(), RecordError> {
        crate::scope::assert_gen(term.gen(), "term");
        // SAFETY: `self.raw` is a live record handle (Record invariant);
        // `term` is a live reference on the thread's current engine (the gen
        // assert above), which `PL_recorded` copies the recorded term into.
        let ok = unsafe { swipl_sys::PL_recorded(self.raw, term.as_raw()) };
        if ok {
            return Ok(());
        }
        match take_pending_exception() {
            Some(exception) => Err(RecordError::Exception(exception)),
            None => Err(RecordError::Failed),
        }
    }

    /// The raw record handle. Exposed for tests and escape hatches; erasing it
    /// outside this type's control voids the safety guarantees documented on
    /// [`Record`].
    #[doc(hidden)]
    pub fn as_raw(&self) -> swipl_sys::record_t {
        self.raw
    }
}

impl fmt::Debug for Record<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The recorded term is opaque without an engine to recall it into, so
        // there is nothing meaningful to render beyond the type itself.
        f.debug_struct("Record").finish_non_exhaustive()
    }
}

impl Clone for Record<'_> {
    fn clone(&self) -> Self {
        // SAFETY: `self.raw` is a live record handle (Record invariant);
        // `PL_duplicate_record` returns an independent copy carrying its own
        // erase obligation, which the returned value takes on.
        let raw = unsafe { swipl_sys::PL_duplicate_record(self.raw) };
        assert!(
            !raw.is_null(),
            "splint: PL_duplicate_record reported failure for a live record"
        );
        Record::from_raw(raw)
    }
}

impl Drop for Record<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is a live record handle erased exactly once here
        // (Record invariant); the `'rt` borrow guarantees the runtime is still
        // initialized, and erasing a record is engine-independent (RC1).
        unsafe { swipl_sys::PL_erase(self.raw) };
    }
}
