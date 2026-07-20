use std::fmt;
use std::marker::PhantomData;

use swipl_sys::record_t;

use crate::exception::{take_pending_exception, PrologException};
use crate::runtime::{self, Runtime};
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
/// The lifetime binds the record to the [`Runtime`], not to any scope: for
/// records made by [`Record::of`]/[`Term::record`] the borrow checker
/// guarantees the record is gone before [`Runtime::cleanup`] runs (invariant
/// RC1). Deserialization (the `serde` feature) instead lets the caller choose
/// `'rt` freely, so every record additionally carries the runtime *session*
/// it was made in, and erasing, cloning, and recalling dynamically check that
/// session is still the current one (invariant RC2): a stale recall or clone
/// panics, and a stale drop silently skips the erase. The record carries no
/// engine generation, because it is engine-independent (like
/// [`Atom`](crate::Atom); invariant A2), and it is [`Send`] because
/// SWI-Prolog records are portable across threads and engines. Recalling
/// still goes through an [`FliContext`], which witnesses that an engine is
/// current.
pub struct Record<'rt> {
    /// Invariant: a live record handle owned by this value, erased exactly
    /// once (by `Drop`, unless leaked or its session already ended).
    raw: record_t,
    /// The runtime session current when this record was made — by
    /// [`Record::of`]/[`Term::record`], or by deserialization (stamped under
    /// the deserializing context's live engine). The dynamic backstop for
    /// handles whose `'rt` a `Deserialize` caller chose freely (RC2).
    session: u64,
    _rt: PhantomData<&'rt Runtime>,
}

// SAFETY (RC1): a `PL_record` handle refers to a global, lock-protected store
// that is independent of any engine or thread; recalling and erasing it are
// valid from any thread, so the handle may be moved across threads. It is
// deliberately not `Sync`: `&Record` shared between threads is not needed and
// would require reasoning about concurrent recalls of the same record.
unsafe impl Send for Record<'_> {}

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
/// `raw` is a live handle of the current runtime session.
pub(crate) fn recall_raw_into(raw: record_t, term: Term<'_>) -> Result<(), RecordError> {
    crate::scope::assert_gen(term.gen(), "term");
    // SAFETY: `raw` is a live record handle of the current session (caller's
    // contract); `term` is a live reference on the thread's current engine
    // (the gen assert above), which `PL_recorded` copies the recorded term
    // into.
    let ok = unsafe { swipl_sys::PL_recorded(raw, term.as_raw()) };
    if ok {
        return Ok(());
    }
    match take_pending_exception() {
        Some(exception) => Err(RecordError::Exception(exception)),
        None => Err(RecordError::Failed),
    }
}

impl<'rt> Record<'rt> {
    /// Records `term`, returning a handle bound to `runtime`.
    ///
    /// Equivalent to [`Term::record`]; the `runtime` argument supplies the
    /// `'rt` brand, which is independent of `term`'s own (soon-to-end) scope.
    pub fn of(runtime: &'rt Runtime, term: Term<'_>) -> Result<Record<'rt>, RecordError> {
        term.record(runtime)
    }

    /// Wraps a raw record handle stamped with the session it was made in. The
    /// caller transfers ownership of exactly one erase obligation to the
    /// returned value.
    pub(crate) fn from_raw(raw: record_t, session: u64) -> Record<'rt> {
        Record {
            raw,
            session,
            _rt: PhantomData,
        }
    }

    /// Recalls the recorded term into a fresh reference allocated from `ctx`
    /// (`PL_recorded`).
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3), as for [`FliContext::term`], or if this record was made
    /// in a runtime session that is no longer current (RC2) — possible only
    /// for a record deserialized in a previous session.
    pub fn recall<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Term<'a>, RecordError> {
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
    /// Panics if this record was made in a runtime session that is no longer
    /// current (RC2) — possible only for a record deserialized in a previous
    /// session.
    pub fn recall_into(&self, term: Term<'_>) -> Result<(), RecordError> {
        // No lock is held across the recall: `term` passed its generation
        // check, so an engine is attached, and that engine sits on a borrow
        // chain ending at the one live `Runtime` (R1/R4) — a concurrent
        // `Runtime::cleanup(self)` is statically impossible, so a session
        // that is current here stays current for the whole call.
        runtime::assert_session_current(self.session, "record");
        recall_raw_into(self.raw, term)
    }

    /// The raw record handle. Exposed for tests and escape hatches; erasing it
    /// outside this type's control voids the safety guarantees documented on
    /// [`Record`].
    #[doc(hidden)]
    pub fn as_raw(&self) -> record_t {
        self.raw
    }

    /// The runtime session this record was made in (RC2).
    #[cfg(feature = "serde")]
    pub(crate) fn session(&self) -> u64 {
        self.session
    }
}

impl fmt::Debug for Record<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The recorded term is opaque without an engine to recall it into, so
        // there is nothing meaningful to render beyond the type itself.
        f.debug_struct("Record")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Clone for Record<'_> {
    /// # Panics
    ///
    /// Panics if this record was made in a runtime session that is no longer
    /// current (RC2) — possible only for a record deserialized in a previous
    /// session.
    fn clone(&self) -> Self {
        // The session check and `PL_duplicate_record` run under the runtime
        // state lock: unlike `recall_into`, cloning carries no borrow that
        // could statically exclude a concurrent `Runtime::cleanup` (RC2).
        let raw = runtime::duplicate_record_current(self.raw, self.session);
        Record::from_raw(raw, self.session)
    }
}

impl Drop for Record<'_> {
    fn drop(&mut self) {
        // Erases only if this record's session is still the current one,
        // checked and erased under the runtime state lock so a concurrent
        // `Runtime::cleanup` cannot interleave (RC2). A stale record's store
        // already died with its session, so skipping the erase merely
        // finishes what that session's cleanup started; for records made by
        // `Record::of`/`Term::record` the `'rt` borrow makes staleness
        // unreachable and this always erases (RC1).
        runtime::erase_record_if_current(self.raw, self.session);
    }
}
