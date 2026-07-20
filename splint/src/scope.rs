//! Per-thread activation bookkeeping for the term/frame/query layer.
//!
//! Borrow-based scoping statically rejects parent/child misuse, but three
//! hazards are invisible to the borrow checker (invariants C1–C3 in the
//! crate docs): sibling frames/queries closed out of LIFO order,
//! term allocation through a non-innermost context, and using stack handles
//! after the thread's current engine changed. This module closes all three
//! with one thread-local record that attach guards save/restore and every
//! stack-handle operation asserts against. Violations panic — a misuse
//! diagnostic, never undefined behavior.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// Mints process-unique engine generations, so a generation observed in one
/// activation can never be confused with a different engine's later
/// activation on the same thread (invariant C1).
static GEN_MINT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The calling thread's current activation. Generation 0 with depth 0 is
    /// the initial "no splint-managed engine" state; the main engine's
    /// witness ([`crate::CurrentEngine`]) snapshots whatever is current when
    /// it is created, including this initial state.
    static ACTIVATION: Cell<Activation> = const {
        Cell::new(Activation { gen: 0, depth: 0 })
    };
}

/// A point in the thread's engine/scope history: which engine activation is
/// current (`gen`) and how many frame/query scopes are open within it
/// (`depth`).
///
/// Public only because the sealed context trait's supertrait method returns
/// it; it is opaque and unobtainable outside the crate.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activation {
    pub(crate) gen: u64,
    pub(crate) depth: usize,
}

/// The calling thread's current activation.
pub(crate) fn current() -> Activation {
    ACTIVATION.with(Cell::get)
}

/// Begins a new engine activation (called when an attach guard is created):
/// mints a fresh generation, resets the scope depth, and returns the previous
/// activation for the guard to restore on drop (invariant C1).
pub(crate) fn enter_engine() -> (u64, Activation) {
    let gen = GEN_MINT.fetch_add(1, Ordering::Relaxed);
    let saved = ACTIVATION.with(|cell| cell.replace(Activation { gen, depth: 0 }));
    (gen, saved)
}

/// Restores the activation saved by [`enter_engine`] (called from the attach
/// guard's drop). Mirroring `PL_set_engine`'s previous-engine restore keeps
/// the record consistent even when guards for different engines are dropped
/// out of order (invariant C1).
pub(crate) fn restore(saved: Activation) {
    ACTIVATION.with(|cell| cell.set(saved));
}

/// Registers a newly opened frame/query scope belonging to the context whose
/// activation is `ctx`. Panics unless that context is the innermost open
/// scope of the current activation (invariants C2/C3). Returns the depth the
/// new scope must be closed at.
pub(crate) fn open_scope(ctx: Activation, kind: &str) -> usize {
    ACTIVATION.with(|cell| {
        let now = cell.get();
        assert_eq!(
            now.gen, ctx.gen,
            "splint: cannot open a {kind}: the engine this context belongs to \
             is not the thread's current engine",
        );
        assert_eq!(
            now.depth,
            ctx.depth,
            "splint: cannot open a {kind} through a context that is not the \
             innermost open scope ({} scope(s) opened after it are still open)",
            now.depth.saturating_sub(ctx.depth),
        );
        cell.set(Activation {
            gen: now.gen,
            depth: now.depth + 1,
        });
        now.depth
    })
}

/// Closes the frame/query scope opened at `depth` within generation `gen`.
/// Panics unless it is the innermost open scope of the current activation —
/// out-of-LIFO-order closes would corrupt SWI-Prolog's stacks (invariant C2).
pub(crate) fn close_scope(gen: u64, depth: usize, kind: &str) {
    ACTIVATION.with(|cell| {
        let now = cell.get();
        assert_eq!(
            now.gen, gen,
            "splint: cannot close this {kind}: the engine it belongs to is \
             not the thread's current engine",
        );
        assert_eq!(
            now.depth,
            depth + 1,
            "splint: {kind} closed out of order: frames and queries must \
             close in exactly the reverse order they were opened",
        );
        cell.set(Activation { gen, depth });
    });
}

/// Attempts to close the scope opened at `depth` within generation `gen`;
/// returns `false` (leaving the record untouched) if it is not the innermost
/// open scope of the current activation. Used by `Drop` implementations,
/// which must not panic again while the thread is already unwinding.
pub(crate) fn try_close_scope(gen: u64, depth: usize) -> bool {
    ACTIVATION.with(|cell| {
        let now = cell.get();
        if now.gen == gen && now.depth == depth + 1 {
            cell.set(Activation { gen, depth });
            true
        } else {
            false
        }
    })
}

/// Asserts that generation `gen` is still the thread's current engine
/// activation; called by every FFI-touching operation on a stack handle
/// (invariant C3).
pub(crate) fn assert_gen(gen: u64, what: &str) {
    let now = ACTIVATION.with(Cell::get);
    assert_eq!(
        now.gen, gen,
        "splint: this {what} belongs to an engine that is not the thread's \
         current engine (another engine was attached after it was created)",
    );
}

/// Asserts that the context whose activation is `ctx` is the innermost open
/// scope of the current activation; term references may only be allocated
/// through the innermost scope, because `PL_new_term_ref` always allocates
/// in the innermost open foreign frame regardless of the handle used
/// (invariant C2).
pub(crate) fn assert_innermost(ctx: Activation, kind: &str) {
    let now = ACTIVATION.with(Cell::get);
    assert_eq!(
        now.gen, ctx.gen,
        "splint: cannot allocate a {kind}: the engine this context belongs \
         to is not the thread's current engine",
    );
    assert_eq!(
        now.depth,
        ctx.depth,
        "splint: cannot allocate a {kind} through a context that is not the \
         innermost open scope: PL_new_term_ref allocates in the innermost \
         open frame, so the reference would not live as long as its type \
         claims ({} scope(s) opened after this context are still open)",
        now.depth.saturating_sub(ctx.depth),
    );
}
