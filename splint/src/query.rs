use std::convert::Infallible;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::ptr;

use swipl_sys::qid_t;

use crate::exception::{take_exception, take_pending_exception, PrologException};
use crate::handles::Predicate;
use crate::scope::{self, Activation};
use crate::term::{FliContext, Sealed, TermList};
use crate::ScopedCallError;

/// User-facing options for the [`Query`] helper methods, mirroring the
/// exposed subset of the `PL_Q_*` flag word. `PL_Q_EXT_STATUS` and
/// `PL_Q_CATCH_EXCEPTION` are always set internally so queries can enumerate
/// solutions and surface caught exceptions as [`QueryError::Exception`].
/// Create this non-exhaustive options value with [`QueryOptions::default`]
/// and then set the desired public fields.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
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
    /// Advancing a query failed, and closing it then failed independently.
    #[error("query operation failed ({operation}); cleanup also failed ({cleanup})")]
    OperationAndCleanup {
        operation: Box<QueryError>,
        cleanup: Box<QueryError>,
    },
}

/// A callback-scoped view of the current solution of a Prolog query.
///
/// Values of this type are supplied to the callbacks accepted by
/// [`Query::once`], [`Query::try_once`], [`Query::solutions`], and
/// [`Query::try_solutions`]. The query is also an [`FliContext`], allowing
/// solution-local scratch terms to be allocated without letting them escape
/// into the next solution.
pub struct Query<'c> {
    /// Invariant: a live query id owned by this value, ended exactly once by
    /// `cut`/`close`/`Drop`.
    qid: qid_t,
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
    fn open<C: FliContext + ?Sized>(
        ctx: &'c C,
        predicate: &Predicate,
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
            return Err(match take_pending_exception() {
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

    /// Runs `body` against the first solution of a newly opened query.
    ///
    /// A successful callback cuts the query, keeping that solution's
    /// bindings. No solution closes the query and returns `Ok(None)`. A panic
    /// closes the query through its destructor.
    pub fn once<C, R>(
        ctx: &'c C,
        predicate: &Predicate,
        args: &TermList<'_>,
        options: QueryOptions,
        body: impl for<'a> FnOnce(&'a Query<'c>) -> R,
    ) -> Result<Option<R>, QueryError>
    where
        C: FliContext + ?Sized,
    {
        let mut query = Query::open(ctx, predicate, args, options)?;
        match query.next_solution() {
            Ok(true) => {
                let result = body(&query);
                query.cut()?;
                Ok(Some(result))
            }
            Ok(false) => {
                query.close()?;
                Ok(None)
            }
            Err(operation) => Err(cleanup_after_operation(query, operation)),
        }
    }

    /// Fallible counterpart to [`Query::once`].
    ///
    /// `Ok` cuts the query and keeps the first solution. `Err` or a panic
    /// closes it and rolls its bindings back.
    pub fn try_once<C, R, E>(
        ctx: &'c C,
        predicate: &Predicate,
        args: &TermList<'_>,
        options: QueryOptions,
        body: impl for<'a> FnOnce(&'a Query<'c>) -> Result<R, E>,
    ) -> Result<Option<R>, ScopedCallError<QueryError, E>>
    where
        C: FliContext + ?Sized,
    {
        let mut query =
            Query::open(ctx, predicate, args, options).map_err(ScopedCallError::Operation)?;
        match query.next_solution() {
            Ok(true) => match body(&query) {
                Ok(result) => {
                    query.cut().map_err(ScopedCallError::Operation)?;
                    Ok(Some(result))
                }
                Err(body) => Err(close_after_body(query, body)),
            },
            Ok(false) => {
                query.close().map_err(ScopedCallError::Operation)?;
                Ok(None)
            }
            Err(operation) => Err(ScopedCallError::Operation(cleanup_after_operation(
                query, operation,
            ))),
        }
    }

    /// Opens a query and maps each solution to an owned value.
    ///
    /// The returned iterator closes the query on exhaustion or drop. Its
    /// mapper is invoked while each solution is current and may allocate
    /// scratch terms through the `&Query` it receives, but its output cannot
    /// borrow those terms. Use [`Solutions::cut`] after stopping early to
    /// keep the current solution; simply dropping the iterator rolls it back.
    ///
    /// ```compile_fail
    /// use splint::{FliContext, Predicate, Query, QueryOptions, TermList};
    ///
    /// fn escaping_mapper<'c, C: FliContext + ?Sized>(
    ///     ctx: &'c C,
    ///     predicate: &Predicate,
    ///     args: &TermList<'_>,
    /// ) {
    ///     let _ = Query::solutions(ctx, predicate, args, QueryOptions::default(), |query| {
    ///         query.term().unwrap()
    ///     });
    /// }
    /// ```
    pub fn solutions<C, R, F>(
        ctx: &'c C,
        predicate: &Predicate,
        args: &TermList<'_>,
        options: QueryOptions,
        mut mapper: F,
    ) -> Result<Solutions<'c, R>, QueryError>
    where
        C: FliContext + ?Sized,
        F: for<'a> FnMut(&'a Query<'c>) -> R + 'c,
    {
        let query = Query::open(ctx, predicate, args, options)?;
        Ok(Solutions {
            inner: SolutionIter {
                query: Some(query),
                mapper: Box::new(move |query| Ok(mapper(query))),
            },
        })
    }

    /// Fallible counterpart to [`Query::solutions`].
    ///
    /// A mapper error closes the query before the error is yielded. If both
    /// the mapper and closing fail, the iterator preserves both errors in
    /// [`ScopedCallError::BodyAndCleanup`].
    pub fn try_solutions<C, R, E, F>(
        ctx: &'c C,
        predicate: &Predicate,
        args: &TermList<'_>,
        options: QueryOptions,
        mapper: F,
    ) -> Result<TrySolutions<'c, R, E>, QueryError>
    where
        C: FliContext + ?Sized,
        F: for<'a> FnMut(&'a Query<'c>) -> Result<R, E> + 'c,
    {
        let query = Query::open(ctx, predicate, args, options)?;
        Ok(TrySolutions {
            inner: SolutionIter {
                query: Some(query),
                mapper: Box::new(mapper),
            },
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
    fn next_solution(&mut self) -> Result<bool, QueryError> {
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
    fn cut(self) -> Result<(), QueryError> {
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
    fn close(self) -> Result<(), QueryError> {
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
        match take_pending_exception() {
            Some(exception) => Err(QueryError::Exception(exception)),
            None => Ok(()),
        }
    }
}

type SolutionMapper<'c, R, E> = dyn for<'a> FnMut(&'a Query<'c>) -> Result<R, E> + 'c;

struct SolutionIter<'c, R, E> {
    query: Option<Query<'c>>,
    mapper: Box<SolutionMapper<'c, R, E>>,
}

impl<R, E> SolutionIter<'_, R, E> {
    fn cut(mut self) -> Result<(), QueryError> {
        match self.query.take() {
            Some(query) => query.cut(),
            None => Ok(()),
        }
    }

    fn close(mut self) -> Result<(), QueryError> {
        match self.query.take() {
            Some(query) => query.close(),
            None => Ok(()),
        }
    }

    fn operation_error(&mut self, operation: QueryError) -> ScopedCallError<QueryError, E> {
        let query = self
            .query
            .take()
            .expect("splint: solution iterator lost its open query");
        ScopedCallError::Operation(cleanup_after_operation(query, operation))
    }

    fn body_error(&mut self, body: E) -> ScopedCallError<QueryError, E> {
        let query = self
            .query
            .take()
            .expect("splint: solution iterator lost its open query");
        close_after_body(query, body)
    }
}

impl<'c, R, E> Iterator for SolutionIter<'c, R, E> {
    type Item = Result<R, ScopedCallError<QueryError, E>>;

    fn next(&mut self) -> Option<Self::Item> {
        let query = self.query.as_mut()?;
        match query.next_solution() {
            Ok(true) => {
                let mapped = catch_unwind(AssertUnwindSafe(|| {
                    (self.mapper)(
                        self.query
                            .as_ref()
                            .expect("splint: solution iterator lost its open query"),
                    )
                }));
                let mapped = match mapped {
                    Ok(mapped) => mapped,
                    Err(payload) => {
                        // `Solutions` may survive if the caller catches this
                        // panic around `next`, so close here rather than
                        // relying on the iterator itself being dropped.
                        drop(self.query.take());
                        resume_unwind(payload);
                    }
                };
                Some(match mapped {
                    Ok(value) => Ok(value),
                    Err(body) => Err(self.body_error(body)),
                })
            }
            Ok(false) => {
                let query = self
                    .query
                    .take()
                    .expect("splint: solution iterator lost its open query");
                match query.close() {
                    Ok(()) => None,
                    Err(error) => Some(Err(ScopedCallError::Operation(error))),
                }
            }
            Err(operation) => Some(Err(self.operation_error(operation))),
        }
    }
}

/// A mapped iterator over the solutions of an infallible [`Query`] callback.
///
/// The query stays open between yielded items. Calling `next` backtracks from
/// the previous solution before finding another one. Exhaustion and drop
/// close the query and discard bindings; [`Solutions::cut`] is the explicit
/// early-success path that keeps the current solution.
pub struct Solutions<'c, R> {
    inner: SolutionIter<'c, R, Infallible>,
}

impl<R> Solutions<'_, R> {
    /// Ends iteration, keeping the current solution's bindings.
    ///
    /// Calling this before a solution has been yielded simply ends the query.
    /// If iteration has already finished, it is a no-op.
    pub fn cut(self) -> Result<(), QueryError> {
        self.inner.cut()
    }

    /// Ends iteration early and discards the query's bindings.
    ///
    /// Unlike dropping the iterator, this reports an error raised while
    /// closing the query.
    pub fn close(self) -> Result<(), QueryError> {
        self.inner.close()
    }
}

impl<'c, R> Iterator for Solutions<'c, R> {
    type Item = Result<R, QueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|result| {
            result.map_err(|error| match error {
                ScopedCallError::Operation(error) => error,
                ScopedCallError::Body(body) | ScopedCallError::BodyAndCleanup { body, .. } => {
                    match body {}
                }
            })
        })
    }
}

/// A mapped iterator over the solutions of a fallible [`Query`] callback.
///
/// It has the same query-lifetime behavior as [`Solutions`], while preserving
/// mapper failures in [`ScopedCallError`].
pub struct TrySolutions<'c, R, E> {
    inner: SolutionIter<'c, R, E>,
}

impl<R, E> TrySolutions<'_, R, E> {
    /// Ends iteration, keeping the current solution's bindings.
    pub fn cut(self) -> Result<(), QueryError> {
        self.inner.cut()
    }

    /// Ends iteration early and discards the query's bindings, reporting an
    /// error raised while closing it.
    pub fn close(self) -> Result<(), QueryError> {
        self.inner.close()
    }
}

impl<'c, R, E> Iterator for TrySolutions<'c, R, E> {
    type Item = Result<R, ScopedCallError<QueryError, E>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

fn cleanup_after_operation(query: Query<'_>, operation: QueryError) -> QueryError {
    match query.close() {
        Ok(()) => operation,
        Err(cleanup) => QueryError::OperationAndCleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        },
    }
}

fn close_after_body<E>(query: Query<'_>, body: E) -> ScopedCallError<QueryError, E> {
    match query.close() {
        Ok(()) => ScopedCallError::Body(body),
        Err(cleanup) => ScopedCallError::BodyAndCleanup { body, cleanup },
    }
}

/// An open query is a scope: scratch term references for inspecting the
/// current solution are allocated from it (`query.term()`, `query.frame()`).
///
/// This is sound because advancing the query requires exclusive access:
/// backtracking to the next solution destroys term references (and frames)
/// created since the previous one, and the exclusive borrow forces all such
/// values — which borrow the query shared — to be dead before it runs (F1).
/// An innermost check while advancing (C2) covers scopes that alias the
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
            let _ = take_pending_exception();
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
