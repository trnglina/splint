//! Safe, low-level bindings to the SWI-Prolog C API.
//!
//! This crate wraps the raw [`swipl_sys`] bindings in types whose ownership
//! and borrowing rules encode SWI-Prolog's threading model, so that the safe
//! surface cannot cause undefined behavior. Currently covered: the
//! process-global [`Runtime`] and thread-movable [`Engine`]s.
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
//!
//! Leaking values ([`std::mem::forget`]) never causes undefined behavior:
//! a leaked guard leaves an engine attached (and eventually leaked), a
//! leaked engine leaves a C engine outstanding (making a later cleanup
//! report failure), and a leaked runtime merely prevents cleanup.

mod engine;
mod error;
mod runtime;

pub use engine::{AttachedEngine, CurrentEngine, Engine, EngineAttributes};
pub use error::{AttachError, CleanupError, CleanupErrorKind, EngineCreateError, InitError};
pub use runtime::{CleanupOptions, Runtime};
