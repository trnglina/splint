//! Safe, low-level bindings to the SWI-Prolog C API.
//!
//! This crate wraps the raw [`swipl_sys`] bindings in types whose ownership
//! and borrowing rules encode SWI-Prolog's threading model, so that the safe
//! surface cannot cause undefined behavior. Currently covered: the
//! process-global [`Runtime`], thread-movable [`Engine`]s, foreign
//! [`Frame`]s, [`Term`] references, and [`Query`] execution.
//!
//! # Soundness invariants
//!
//! Runtime (see `runtime.rs`):
//!
//! - **R1** — At most one [`Runtime`] exists per process; its existence is
//!   equivalent to "`PL_initialise` succeeded and no successful cleanup has
//!   happened since". It is created only by [`Runtime::initialize`] /
//!   [`Runtime::initialize_from_state`] and consumed only by a successful
//!   [`Runtime::cleanup`] (a failed cleanup hands the token back).
//! - **R2** — All calls to `PL_initialise`, `PL_set_resource_db_mem`, and
//!   `PL_cleanup` are serialized under one private mutex, held across the
//!   entire FFI call. This compensates for `PL_initialise`'s unlocked
//!   check-then-act on its initialized flag in the C implementation.
//! - **R3** — The argv strings handed to `PL_initialise` are leaked to
//!   `'static`, because SWI-Prolog retains the pointers.
//! - **R4** — Everything created from the runtime ([`Engine`],
//!   [`CurrentEngine`]) borrows it, so `cleanup(self)` statically requires
//!   that no engines are alive.
//! - **R5** — Saved-state buffers are `&'static [u8]`: SWI-Prolog stores the
//!   pointer and reads from it lazily for the whole session, never copying
//!   or freeing it.
//!
//! Engines (see `engine.rs`):
//!
//! - **E1** — `Engine.raw` is non-null, uniquely owned, and destroyed
//!   exactly once (in `Drop`).
//! - **E2** — [`Engine`] is `Send` (a detached engine may be re-attached
//!   from any thread, per the SWI-Prolog manual) but not `Sync`; all methods
//!   take `&mut self`, so at most one thread ever uses a given engine.
//! - **E3** — [`AttachedEngine`] and [`CurrentEngine`] are `!Send + !Sync`:
//!   they describe the calling thread's TLS current-engine slot, and a
//!   guard's detach-on-drop must run on the thread that attached.
//! - **E4** — An attach guard exclusively borrows its [`Engine`], which
//!   borrows the [`Runtime`]: guard ⊆ engine ⊆ runtime, enforced by the
//!   borrow checker.
//! - **E5** — A guard's drop re-attaches the previously attached engine, so
//!   that engine must outlive the guard. [`Engine::attach`] therefore
//!   refuses a thread that already has a crate-managed engine attached: its
//!   `previous` is then null or an unmanaged engine (e.g. the main engine),
//!   pinned for the [`Runtime`]'s lifetime. Nesting goes through
//!   [`Engine::attach_within`], whose guard borrows the outer guard: the
//!   outer engine stays exclusively borrowed — alive and unattachable
//!   elsewhere — until the nested guard drops, and drops are LIFO by
//!   construction. Dynamic backstops cover what borrows cannot: a guard
//!   whose generation is no longer current (a leaked inner guard) panics on
//!   drop instead of restoring through the leak, and the restore's
//!   `PL_set_engine` status is checked, never assumed.
//!
//! Contexts and scopes (see `scope.rs`, `term.rs`, `query.rs`):
//!
//! - **C1** — Every thread carries an *activation record* `(generation,
//!   scope depth)`. Attaching an engine mints a process-unique generation
//!   and saves the previous record, which the guard's drop restores in
//!   lockstep with its *verified* `PL_set_engine` restore — and only after
//!   checking the guard is still the thread's innermost attachment (E5) —
//!   so the record always describes the engine actually attached.
//!   [`CurrentEngine`] snapshots the record at creation.
//! - **C2** — Frames and queries record the depth they were opened at and
//!   must be the innermost open scope to close (LIFO): the borrow checker
//!   enforces this for parent/child nesting, and the activation record
//!   closes the gap it cannot see — sibling scopes opened from a shared
//!   context — by panicking on out-of-order closes instead of letting
//!   `PL_close_foreign_frame`'s unchecked pointer arithmetic corrupt the
//!   stacks. Term allocation likewise asserts its context is the innermost
//!   scope, because `PL_new_term_ref` always allocates in the innermost
//!   open frame regardless of the handle used.
//! - **C3** — [`Term`], [`TermList`], [`Frame`], and [`Query`] record their
//!   generation and every FFI-touching operation asserts it is still the
//!   thread's current one: a `term_t` indexes the *current* engine's
//!   stacks, so using a handle after another engine was attached would be
//!   out-of-bounds access. Violations panic; they are never UB.
//!
//! Frames and terms (see `term.rs`):
//!
//! - **F1** — [`FliContext`] is sealed and implemented only by
//!   [`AttachedEngine`], [`CurrentEngine`], [`Frame`], and [`Query`]
//!   (a query is a scope for inspecting its current solution; see Q1 for
//!   why that is sound). Values allocated
//!   through it borrow the context (`&self`-elided lifetimes), so no
//!   [`Term`]/[`TermList`]/nested [`Frame`]/[`Query`] can outlive — or see
//!   the closing of — the scope it was allocated in.
//! - **F4** — [`Frame::rewind`] takes `&mut self`, so the borrow checker
//!   rejects it while terms borrowed from the frame are live — exactly the
//!   handles `PL_rewind_foreign_frame` would silently invalidate; an
//!   innermost check (C2) covers scopes that alias the frame's position via
//!   a [`CurrentEngine`] witness without borrowing it.
//! - **F5** — All term/frame/query/handle types are `!Send + !Sync`: they
//!   describe state of the engine currently attached to the calling thread
//!   (same reasoning as E3).
//! - **F6** — Dropping a [`Frame`] *discards* it (undoes bindings), which is
//!   well-defined regardless of what happened inside, including a panic;
//!   keeping bindings requires the affirmative [`Frame::close`] call.
//!
//! Queries (see `query.rs`):
//!
//! - **Q1** — A [`Query`] follows the same borrow (F1), LIFO (C2), and
//!   generation (C3) discipline as a frame, sharing the same activation
//!   record. Ending it is explicit — [`Query::cut`] keeps the current
//!   solution's bindings, [`Query::close`] discards them — and exhaustion
//!   (`Ok(false)`) still requires ending it. Exceptions are captured and
//!   cleared eagerly as rendered text ([`PrologException`]), never carried
//!   as raw engine state. [`Query::next_solution`] takes `&mut self`
//!   because backtracking destroys term references and frames created since
//!   the previous solution: the exclusive borrow forces those values, which
//!   borrow the query shared, to be dead before it runs; an innermost check
//!   (C2) covers scopes that alias the query's position via a
//!   [`CurrentEngine`] witness without borrowing it, exactly as F4 does for
//!   [`Frame::rewind`].
//!
//! Handles (see `handles.rs`):
//!
//! - **A1** — Every [`Atom`] construction path takes its own
//!   `PL_register_atom` reference and its drop releases exactly that
//!   reference, so the count is self-contained and never relies on
//!   undocumented protection of freshly created atoms.
//! - **A2** — [`Atom`]/[`Functor`]/[`Module`]/[`Predicate`] are
//!   engine-independent global handles (no generation check); their context
//!   borrow pins the [`Runtime`] alive, which is all their use and drop
//!   require. Bounding them by a context borrow is conservative; relaxing
//!   to a runtime-lifetime bound is future work.
//! - **A3** — `PL_new_module` and `PL_pred` find-or-create and have no
//!   failure sentinel; `PL_new_functor_sz` and `PL_predicate` do and are
//!   checked.
//!
//! Leaking values ([`std::mem::forget`]) never causes undefined behavior:
//! a leaked guard leaves an engine attached (and eventually leaked), its
//! generation current — so the thread refuses further plain attaches and
//! dropping an outer guard panics rather than restoring through the leak
//! (E5) — a leaked engine leaves a C engine outstanding (making a later
//! cleanup report failure), and a leaked runtime merely prevents cleanup. A
//! leaked frame or query leaves its C-side scope open and its depth
//! registered, so closing any outer scope afterwards panics (C2) rather
//! than corrupting the stacks.

mod engine;
mod error;
mod handles;
mod query;
mod runtime;
mod scope;
mod term;

pub use engine::{AttachedEngine, CurrentEngine, Engine, EngineAttributes};
pub use error::{AttachError, CleanupError, CleanupErrorKind, EngineCreateError, InitError};
pub use handles::{Atom, Functor, HandleError, Module, Predicate};
pub use query::{Query, QueryError, QueryOptions};
pub use runtime::{CleanupOptions, Runtime};
pub use term::{FliContext, Frame, FrameError, PrologException, Term, TermError, TermKind, TermList};
