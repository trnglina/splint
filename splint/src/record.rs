use std::fmt;

use swipl_sys::record_t;

use crate::exception::{take_pending_exception, PrologException};
use crate::term::{FliContext, Term, TermError};

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
/// A record is bound to no scope at all — it is a plain owned handle, erased
/// on drop (invariant RC1). The runtime it belongs to lives for the rest of
/// the process, so there is nothing for the record to dangle against and it
/// needs neither a lifetime nor any dynamic liveness check. It carries no
/// engine generation either, because its store is engine-independent (like
/// [`Atom`](crate::Atom); invariant A2), and it is [`Send`] because
/// SWI-Prolog records are portable across threads and engines. Recalling
/// still goes through an [`FliContext`], which witnesses that an engine is
/// current.
pub struct Record {
    /// Invariant: a live record handle owned by this value, erased exactly
    /// once (by `Drop`, unless leaked).
    raw: record_t,
}

// SAFETY (RC1): a `PL_record` handle refers to a global, lock-protected store
// that is independent of any engine or thread; recalling and erasing it are
// valid from any thread, so the handle may be moved across threads. It is
// deliberately not `Sync`: `&Record` shared between threads is not needed and
// would require reasoning about concurrent recalls of the same record.
unsafe impl Send for Record {}

/// Records `term`, returning the raw handle carrying one erase obligation.
///
/// Shared by [`Term::record`] and the serde record handoff, which needs the
/// raw handle before it has committed to producing a [`Record`].
pub(crate) fn record_raw(term: Term<'_>) -> Result<record_t, RecordError> {
    crate::scope::assert_gen(term.gen(), "term");
    // SAFETY: C3 assert above; `PL_record` copies the term into the global
    // recorded database and returns a fresh handle carrying one erase
    // obligation, which the caller takes on.
    let raw = unsafe { swipl_sys::PL_record(term.as_raw()) };
    if raw.is_null() {
        return Err(match take_pending_exception() {
            Some(exception) => RecordError::Exception(exception),
            None => RecordError::Failed,
        });
    }
    Ok(raw)
}

/// Recalls the raw record `raw` into `term` (`PL_recorded`).
///
/// Shared by [`Record::recall_into`] and the serde record handoff, which
/// recalls from a raw handle rather than a `&Record`. The caller must ensure
/// `raw` is a live handle.
pub(crate) fn recall_raw_into(raw: record_t, term: Term<'_>) -> Result<(), RecordError> {
    crate::scope::assert_gen(term.gen(), "term");
    // SAFETY: `raw` is a live record handle (caller's contract); `term` is a
    // live reference on the thread's current engine (the gen assert above),
    // which `PL_recorded` copies the recorded term into.
    let ok = unsafe { swipl_sys::PL_recorded(raw, term.as_raw()) };
    if ok {
        return Ok(());
    }
    match take_pending_exception() {
        Some(exception) => Err(RecordError::Exception(exception)),
        None => Err(RecordError::Failed),
    }
}

impl Record {
    /// Wraps a raw record handle. The caller transfers ownership of exactly
    /// one erase obligation to the returned value.
    pub(crate) fn from_raw(raw: record_t) -> Record {
        Record { raw }
    }

    /// Recalls the recorded term into a fresh reference allocated from `ctx`
    /// (`PL_recorded`).
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3), as for [`FliContext::term`].
    pub fn recall<'a, C: FliContext + ?Sized>(&self, ctx: &'a C) -> Result<Term<'a>, RecordError> {
        // `ctx.term()` already captured and cleared any resource exception;
        // preserve it rather than flattening it to the generic `Failed`.
        let dest = ctx.term().map_err(|error| match error {
            TermError::Exception(exception) => RecordError::Exception(exception),
            _ => RecordError::Failed,
        })?;
        self.recall_into(dest)?;
        Ok(dest)
    }

    /// Recalls the recorded term into the existing reference `term`
    /// (`PL_recorded`), overwriting whatever it held.
    ///
    /// Use this to recall into a slot that already exists — for instance a
    /// query argument or a term about to be unified — rather than allocating a
    /// fresh reference as [`Record::recall`] does.
    ///
    /// # Panics
    ///
    /// Panics if `term` does not belong to the thread's current engine (C3).
    pub fn recall_into(&self, term: Term<'_>) -> Result<(), RecordError> {
        recall_raw_into(self.raw, term)
    }

    #[cfg(feature = "serde")]
    pub(crate) fn as_raw(&self) -> record_t {
        self.raw
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The recorded term is opaque without an engine to recall it into, so
        // there is nothing meaningful to render beyond the type itself.
        f.debug_struct("Record").finish_non_exhaustive()
    }
}

impl Clone for Record {
    fn clone(&self) -> Self {
        // SAFETY: `self.raw` is a live record handle (Record invariant), and
        // the recorded database outlives every record because the runtime is
        // never torn down. The copy carries its own erase obligation, which
        // the returned value takes on.
        let duplicate = unsafe { swipl_sys::PL_duplicate_record(self.raw) };
        assert!(
            !duplicate.is_null(),
            "splint: PL_duplicate_record reported failure for a live record"
        );
        Record::from_raw(duplicate)
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        // SAFETY: `self.raw` is a live record handle whose erase obligation
        // this value holds (Record invariant), and the recorded database that
        // issued it outlives every record because the runtime is never torn
        // down (RC1).
        unsafe { swipl_sys::PL_erase(self.raw) };
    }
}
