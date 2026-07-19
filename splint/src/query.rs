use std::marker::PhantomData;
use std::os::raw::c_int;
use std::ptr;

use crate::handles::Predicate;
use crate::scope::{self, Activation};
use crate::term::{take_exception, FliContext, PrologException, Sealed, TermList};

/// User-facing options for [`Query::open`], mirroring the exposed subset of
/// the `PL_Q_*` flag word. `PL_Q_EXT_STATUS` and `PL_Q_CATCH_EXCEPTION` are
/// always set internally: extended status codes drive the
/// [`Query::next_solution`] state machine, and caught exceptions are how
/// errors surface as [`QueryError::Exception`] instead of propagating into
/// an enclosing (possibly nonexistent) query.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryOptions {
    /// Run the goal with the debugger disabled (`PL_Q_NODEBUG`).
    pub nodebug: bool,
}

impl QueryOptions {
    fn into_raw(self) -> c_int {
        let mut flags = (swipl_sys::PL_Q_EXT_STATUS | swipl_sys::PL_Q_CATCH_EXCEPTION) as c_int;
        if self.nodebug {
            flags |= swipl_sys::PL_Q_NODEBUG as c_int;
        }
        flags
    }
}

/// An error from opening or running a query.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The argument block's length does not match the predicate's arity.
    #[error("predicate of arity {expected} cannot be called with {actual} argument terms")]
    ArityMismatch { expected: usize, actual: usize },
    #[error("PL_open_query reported failure")]
    OpenFailed,
    /// The goal (or closing it) raised a Prolog exception; the exception has
    /// been cleared from the engine.
    #[error("prolog exception: {0}")]
    Exception(#[source] PrologException),
    #[error("PL_next_solution returned an unrecognized status code {0}")]
    Unknown(c_int),
}

/// An open query (`PL_open_query`): an active call of a [`Predicate`] whose
/// solutions are enumerated with [`Query::next_solution`].
///
/// The argument block passed to [`Query::open`] *is* the query's argument
/// vector — SWI-Prolog does not copy it — so solution bindings are read back
/// through the same [`TermList`]. Ending the query is explicit:
/// [`Query::cut`] keeps the current solution's bindings, [`Query::close`]
/// discards them; dropping an open query closes it. Queries participate in
/// the same LIFO scope discipline as frames (C2).
pub struct Query<'c> {
    /// Invariant: a live query id owned by this value, ended exactly once by
    /// `cut`/`close`/`Drop`.
    qid: swipl_sys::qid_t,
    gen: u64,
    /// The thread's scope depth when this query was opened (C2).
    depth: usize,
    /// Set once `PL_next_solution` reported the query finished (last or
    /// failed) or raised; further `next_solution` calls short-circuit. The
    /// query must still be ended explicitly.
    exhausted: bool,
    _ctx: PhantomData<&'c ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'c> Query<'c> {
    /// Opens a query calling `predicate` with the argument block `args`
    /// (which must match the predicate's arity) in the predicate's own
    /// module.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's
    /// current engine (C2/C3).
    pub fn open<C: FliContext + ?Sized>(
        ctx: &'c C,
        predicate: &Predicate<'_>,
        args: &TermList<'_>,
        options: QueryOptions,
    ) -> Result<Query<'c>, QueryError> {
        if args.len() != predicate.arity() {
            return Err(QueryError::ArityMismatch {
                expected: predicate.arity(),
                actual: args.len(),
            });
        }
        scope::assert_gen(args.gen(), "term reference block");
        let activation = ctx.activation();
        let depth = scope::open_scope(activation, "query");
        // SAFETY: `ctx` witnesses that an engine is current on this thread
        // (F1); `predicate` is a valid handle (A3); `args` is the base of
        // `arity` contiguous live references on the current engine (checked
        // above); a null module selects the predicate's own module.
        let qid = unsafe {
            swipl_sys::PL_open_query(
                ptr::null_mut(),
                options.into_raw(),
                predicate.as_raw(),
                args.as_raw(),
            )
        };
        if qid.is_null() {
            scope::close_scope(activation.gen, depth, "query");
            return Err(match crate::term::take_pending_exception() {
                Some(exception) => QueryError::Exception(exception),
                None => QueryError::OpenFailed,
            });
        }
        Ok(Query {
            qid,
            gen: activation.gen,
            depth,
            exhausted: false,
            _ctx: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Advances to the next solution (`PL_next_solution`).
    ///
    /// `Ok(true)` means the argument block now holds a solution; `Ok(false)`
    /// means the goal has no (more) solutions. Both a finished and a failed
    /// query must still be ended with [`Query::cut`], [`Query::close`], or
    /// by dropping.
    ///
    /// # Panics
    ///
    /// Panics if the query is not the innermost open scope (C2): as with
    /// [`Frame::rewind`](crate::Frame::rewind), a scope opened through a
    /// `CurrentEngine` witness can alias this query's position without
    /// borrowing it, and backtracking across it would invalidate its
    /// frame id.
    pub fn next_solution(&mut self) -> Result<bool, QueryError> {
        if self.exhausted {
            return Ok(false);
        }
        scope::assert_innermost(self.activation(), "next_solution");
        // SAFETY: `self.qid` is a live query id (query invariant) on the
        // current engine, with no scope open above it (C2/C3 assert above)
        // that its internal backtracking could invalidate; `&mut self` gives
        // exclusive access.
        let status = unsafe { swipl_sys::PL_next_solution(self.qid) };
        match status {
            _ if status == swipl_sys::PL_S_TRUE as c_int => Ok(true),
            _ if status == swipl_sys::PL_S_LAST as c_int => {
                // Deliberately not cutting here: PL_cut_query runs cleanup
                // handlers, a side effect that must stay an explicit call.
                self.exhausted = true;
                Ok(true)
            }
            _ if status == swipl_sys::PL_S_FALSE as c_int => {
                self.exhausted = true;
                Ok(false)
            }
            _ if status == swipl_sys::PL_S_EXCEPTION as c_int => {
                self.exhausted = true;
                match take_exception(self.qid) {
                    Some(exception) => Err(QueryError::Exception(exception)),
                    None => Err(QueryError::Unknown(status)),
                }
            }
            other => Err(QueryError::Unknown(other)),
        }
    }

    /// Ends the query, keeping the bindings of the current solution
    /// (`PL_cut_query`). Pending choice points are pruned, running any
    /// cleanup handlers.
    ///
    /// # Panics
    ///
    /// Panics if the query is not the innermost open scope (C2).
    pub fn cut(self) -> Result<(), QueryError> {
        scope::close_scope(self.gen, self.depth, "query");
        // SAFETY: `self.qid` is live and this query is the innermost scope
        // (close_scope assert above, C2); consuming `self` (with the forget
        // in `finish`) ensures it is ended exactly once.
        let rc = unsafe { swipl_sys::PL_cut_query(self.qid) };
        self.finish(rc)
    }

    /// Ends the query, discarding the bindings made since it was opened
    /// (`PL_close_query`).
    ///
    /// # Panics
    ///
    /// Panics if the query is not the innermost open scope (C2).
    pub fn close(self) -> Result<(), QueryError> {
        scope::close_scope(self.gen, self.depth, "query");
        // SAFETY: as for `cut`.
        let rc = unsafe { swipl_sys::PL_close_query(self.qid) };
        self.finish(rc)
    }

    /// Interprets `PL_cut_query`/`PL_close_query`'s result now that the
    /// scope bookkeeping is already updated: `PL_S_NOT_INNER` means the
    /// engine disagrees with this crate's LIFO record — an internal bug, so
    /// it panics; `false` means the query *was* ended but ending it raised a
    /// fresh exception (e.g. from undo handlers), surfaced normally.
    fn finish(self, rc: c_int) -> Result<(), QueryError> {
        std::mem::forget(self);
        if rc == swipl_sys::PL_S_NOT_INNER as c_int {
            panic!(
                "splint: PL_cut_query/PL_close_query reported PL_S_NOT_INNER; \
                 the crate's LIFO bookkeeping disagrees with the engine \
                 (this is a bug in splint — please report it)"
            );
        }
        if rc != 0 {
            return Ok(());
        }
        // The query id is already freed at this point; a fresh exception
        // raised while ending the query is pending on the engine itself.
        match crate::term::take_pending_exception() {
            Some(exception) => Err(QueryError::Exception(exception)),
            None => Ok(()),
        }
    }

    /// The raw query id. Exposed for tests and escape hatches; ending the
    /// query outside this type's control voids the safety guarantees
    /// documented on [`Query`].
    #[doc(hidden)]
    pub fn as_raw(&self) -> swipl_sys::qid_t {
        self.qid
    }
}

/// An open query is a scope: scratch term references for inspecting the
/// current solution are allocated from it (`query.term()`, `query.frame()`).
///
/// This is sound because [`Query::next_solution`] takes `&mut self`:
/// backtracking to the next solution destroys term references (and frames)
/// created since the previous one, and the exclusive borrow forces all such
/// values — which borrow the query shared — to be dead before it runs (F1).
/// An innermost check in `next_solution` (C2) covers scopes that alias the
/// query's position via a `CurrentEngine` witness without borrowing it.
impl Sealed for Query<'_> {
    fn activation(&self) -> Activation {
        Activation {
            gen: self.gen,
            depth: self.depth + 1,
        }
    }
}
impl FliContext for Query<'_> {}

impl Drop for Query<'_> {
    fn drop(&mut self) {
        if scope::try_close_scope(self.gen, self.depth) {
            // SAFETY: `self.qid` is live and this query was verified to be
            // the innermost scope (C2); `cut`/`close` did not run (they
            // forget `self`).
            let rc = unsafe { swipl_sys::PL_close_query(self.qid) };
            if rc == swipl_sys::PL_S_NOT_INNER as c_int && !std::thread::panicking() {
                panic!(
                    "splint: PL_close_query reported PL_S_NOT_INNER; the \
                     crate's LIFO bookkeeping disagrees with the engine \
                     (this is a bug in splint — please report it)"
                );
            }
            // A fresh exception raised by closing cannot be propagated from
            // a destructor; clear it (from the engine — the query id is
            // already freed) so the engine is left in a clean state.
            let _ = crate::term::take_pending_exception();
        } else if !std::thread::panicking() {
            panic!(
                "splint: query dropped out of order: frames and queries must \
                 close in exactly the reverse order they were opened"
            );
        }
        // While unwinding from an earlier panic, an inconsistent query is
        // leaked silently rather than double-panicking into an abort.
    }
}
