//! Safe, low-level bindings to the SWI-Prolog C API.
//!
//! This crate wraps the raw [`swipl_sys`] bindings in types whose ownership
//! and borrowing rules encode SWI-Prolog's threading model, so that the safe
//! surface cannot cause undefined behavior. Currently covered: the
//! process-global [`Runtime`], thread-movable [`Engine`]s, foreign
//! [`Frame`]s, [`Term`] references, [`Query`] execution, dicts, and
//! [`Record`]ed terms.
//!
//! # Soundness invariants
//!
//! Runtime (see `runtime.rs`):
//!
//! - **R1** — At most one [`Runtime`] exists per process; its existence is
//!   equivalent to "`PL_initialise` succeeded". It is created only by
//!   [`Runtime::initialize`] / [`Runtime::initialize_from_state`], and the
//!   runtime is never torn down: this crate exposes no cleanup, halt, or
//!   re-initialization, so once initialized the runtime stays initialized for
//!   the remainder of the process. Every handle in this crate may therefore
//!   assume its SWI-Prolog-side store outlives it. The tradeoff is that
//!   SWI-Prolog is never asked to shut down cleanly: `at_halt/1` hooks do not
//!   run, Prolog's stream buffers are not flushed, and Prolog threads are not
//!   joined at process exit.
//! - **R2** — The `PL_set_resource_db_mem`/`PL_initialise` pair is serialized
//!   under one private mutex, held across the entire FFI call. This
//!   compensates for `PL_initialise`'s unlocked check-then-act on its own
//!   initialized flag.
//! - **R3** — The argv strings handed to `PL_initialise` are leaked to
//!   `'static`, because SWI-Prolog retains the pointers.
//! - **R4** — Everything created from the runtime ([`Engine`],
//!   [`CurrentEngine`]) borrows it, which roots the engine borrow chain (E4).
//!   Nothing consumes the [`Runtime`], so this no longer gates a teardown; it
//!   remains the anchor that gives engine-derived values a lifetime to be
//!   ordered within. Any future operation that reclaims runtime state must
//!   take the `Runtime` by value to inherit that exclusion.
//! - **R5** — Saved-state buffers are `&'static [u8]`: SWI-Prolog stores the
//!   pointer and reads from it lazily for the whole life of the runtime,
//!   never copying or freeing it.
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
//!   that engine must outlive the guard. The internal plain attach path
//!   refuses a thread that already has a crate-managed engine attached: its
//!   `previous` is then null or an unmanaged engine (e.g. the main engine),
//!   pinned for the [`Runtime`]'s lifetime. Nesting goes through
//!   [`Engine::with_attached_within`], whose guard borrows the outer guard: the
//!   outer engine stays exclusively borrowed — alive and unattachable
//!   elsewhere — until the nested guard drops, and drops are LIFO by
//!   construction. Dynamic backstops cover what borrows cannot: a guard
//!   whose generation is no longer current (a leaked inner guard) panics on
//!   drop instead of restoring through the leak, and the restore's
//!   `PL_set_engine` status is checked, never assumed.
//! - **E6** — [`Engine::with_attached`] and its fallible/nested counterparts
//!   lend the attach guard through a higher-ranked callback. The attachment
//!   and anything borrowing it cannot escape, so restoration runs before the
//!   helper returns; unwinding restores through the guard's `Drop`.
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
//! - **F5** — [`Term`], [`TermList`], [`Frame`], and [`Query`] are
//!   `!Send + !Sync`: they describe state of the engine currently attached
//!   to the calling thread (same reasoning as E3). Process-global handles are
//!   covered separately by A2.
//! - **F6** — Dropping a [`Frame`] *discards* it (undoes bindings), which is
//!   well-defined regardless of what happened inside, including a panic;
//!   keeping bindings requires the affirmative [`Frame::close`] call.
//!   [`FliContext::with_frame`] confines the frame to a callback and closes
//!   after a normal return; [`FliContext::try_with_frame`] closes on `Ok` and
//!   discards on `Err`, while unwinding discards through `Drop`.
//!
//! Queries (see `query.rs`):
//!
//! - **Q1** — A [`Query`] follows the same borrow (F1), LIFO (C2), and
//!   generation (C3) discipline as a frame, sharing the same activation
//!   record. Its private state machine explicitly cuts to keep bindings or
//!   closes to discard them. Exceptions are captured and cleared eagerly as
//!   rendered text ([`PrologException`]), never carried as raw engine state.
//!   Advancing requires exclusive access because backtracking destroys term
//!   references and frames created since the previous solution; the
//!   exclusive borrow forces values borrowing the query to be dead first.
//!   An innermost check (C2) covers scopes that alias the query's position
//!   via a [`CurrentEngine`] witness without borrowing it, exactly as F4 does
//!   for [`Frame::rewind`].
//! - **Q2** — [`Query::once`] and [`Query::solutions`] lend each current
//!   solution through a higher-ranked callback, so solution-local references
//!   cannot escape across a cut, close, or the next backtracking step.
//!   Solution iteration yields owned mapped values; exhaustion, callback
//!   failure, panic, and an abandoned iterator close the query, while
//!   [`Solutions::cut`] is the explicit early-commit path.
//!
//! Handles (see `handles.rs`):
//!
//! - **A1** — Every [`Atom`] construction path takes its own
//!   `PL_register_atom` reference and its drop releases exactly that
//!   reference, so the count is self-contained and never relies on
//!   undocumented protection of freshly created atoms. Atoms are interned
//!   (equal text ⇒ equal handle), so `Eq`/`Hash` compare the raw handle,
//!   which is exact value equality regardless of the borrowed context.
//! - **A2** — [`Atom`]/[`Functor`]/[`Module`]/[`Predicate`] are
//!   engine-independent, `Send + Sync` process-global handles with no
//!   generation or Rust lifetime bound. SWI-Prolog keeps functor, module, and
//!   predicate handles valid for the whole initialized session; an [`Atom`]
//!   owns a registration that keeps its atom live. R1 guarantees that session
//!   lasts until process exit. Constructors still accept an [`FliContext`] as
//!   proof that initialization has occurred and, for fallible constructors,
//!   to provide a current engine from which pending exceptions can be taken.
//! - **A3** — `PL_new_module` find-or-creates and has no failure sentinel.
//!   `PL_new_functor_sz`, `PL_pred`, and `PL_predicate` can each fail (e.g. a
//!   `program_space` resource error), so their zero/null returns are checked;
//!   any pending Prolog exception is captured and cleared into
//!   [`HandleError::Exception`], never left to surface on a later operation.
//!
//! Records (see `record.rs`):
//!
//! - **RC1** — A [`Record`] is a copy of a term in SWI-Prolog's global,
//!   lock-protected recorded database. It is a plain owned handle, erased
//!   exactly once on drop, and carries neither a lifetime nor an engine
//!   generation: its store is engine-independent (like an [`Atom`], A2) and
//!   outlives every record, because the runtime is never torn down (R1). A
//!   record may therefore outlive every frame, query, and engine, and be
//!   minted with no `&Runtime` in scope at all — which is what lets
//!   deserialization (S2) produce one. It is [`Send`] (records are portable
//!   across threads and engines) but not `Sync`. Producing one
//!   ([`Term::record`]) checks the source term's generation (C3); recalling it
//!   ([`Record::recall`]) allocates the destination through an
//!   [`FliContext`], which witnesses that an engine is current.
//!
//! Dicts (see `term.rs`):
//!
//! - **D1** — Dicts are ordinary [`Term`]s (`TermKind::Dict`); constructing
//!   ([`Term::put_dict`]) and reading them ([`Term::get_dict`],
//!   [`Term::dict_tag`]) are generation-checked term operations like any
//!   other. [`Term::dict_entries`] additionally asserts its context is the
//!   innermost open scope (C2) before iterating with `PL_for_dict`, because
//!   each value is copied into a reference allocated in the innermost open
//!   frame during the callback; the assert proves that frame is the context
//!   the returned [`Term`]s borrow.
//!
//! Serde (see `serde/`, behind the `serde` feature):
//!
//! - **S1** — The serde serializer and deserializer allocate scratch term
//!   references only through the caller-supplied [`FliContext`] and open no
//!   scopes of their own (dict reads go through the same public term
//!   operations as direct use), so they add no scoping rules: the caller's
//!   context must be the thread's innermost open scope (C2/C3), exactly as
//!   for direct term allocation.
//! - **S2** — A [`Record`] handle crosses the serde boundary only through a
//!   same-thread, same-call thread-local handoff keyed by a private newtype
//!   token; the value placed in the serde data model is a guard that always
//!   errors. A forged token call finds the handoff empty and fails without
//!   reaching FFI with a fabricated handle; foreign formats and serde's
//!   internally buffered positions (untagged/internally-tagged enums,
//!   flatten) fail the same clean way. Scope guards remove unconsumed
//!   handoffs on errors and panics, so a raw handle can never leak into a
//!   later serde call.
//!
//! Leaking values ([`std::mem::forget`]) never causes undefined behavior:
//! a leaked guard leaves an engine attached (and eventually leaked), its
//! generation current — so the thread refuses further plain attaches and
//! dropping an outer guard panics rather than restoring through the leak
//! (E5) — and a leaked engine merely leaves a C engine outstanding for the
//! life of the process. A leaked frame or query leaves its C-side scope open
//! and its depth registered, so closing any outer scope afterwards panics
//! (C2) rather than corrupting the stacks. A leaked [`Record`] merely leaves a
//! copy in the recorded database for the life of the process.

mod args;
mod call;
mod engine;
mod exception;
mod handles;
mod query;
mod record;
mod runtime;
mod scope;
#[cfg(feature = "serde")]
mod serde;
mod term;

pub use args::{Args, ArgsSpec, ArgumentError, CallError};
pub use call::ScopedCallError;
pub use engine::{
    AttachError, AttachedEngine, CurrentEngine, Engine, EngineAttributes, EngineCreateError,
};
pub use exception::PrologException;
pub use handles::{Atom, Functor, HandleError, Module, Predicate};
pub use query::{CallSolutions, Query, QueryError, QueryOptions, Solutions, TrySolutions};
pub use record::{Record, RecordError};
pub use runtime::{InitError, Runtime};
#[cfg(feature = "serde")]
pub use serde::{
    from_term, from_terms, input, input_as, output, to_term, to_terms, Error as SerdeError, Input,
    InputAs, Output, TermArg,
};
pub use term::{
    DictKey, FliContext, Frame, FrameError, ListShape, Term, TermError, TermKind, TermList,
};
