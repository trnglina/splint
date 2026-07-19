//! Capturing SWI-Prolog exceptions as the crate's shared error currency.
//!
//! A pending Prolog exception is engine state, not term state: it can be
//! raised by any FLI operation — building a handle, opening a frame, running a
//! query, recording a term — and every layer surfaces it the same way, as a
//! rendered [`PrologException`] captured and cleared before the raising scope
//! is torn down. Keeping that machinery here, depending on nothing but
//! `swipl_sys`, lets every module share it without reaching into `term.rs`.

use std::os::raw::{c_char, c_uint, c_void};
use std::ptr;

use swipl_sys::term_t;

/// A Prolog exception captured as text.
///
/// The exception term is rendered (as by `writeq/1`) at the moment it is
/// observed, before the raising query or frame is torn down, and the engine's
/// pending-exception state is cleared. The representation is a plain
/// [`String`]: the raw exception term dies with the scope that raised it, and
/// carrying a `PL_record` instead would tie every error type to the runtime's
/// lifetime. Structured exception inspection is future work.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct PrologException(pub String);

/// Reads the text of `raw` via `PL_get_nchars`. `flags` must include a
/// `CVT_*` selection; `BUF_MALLOC | REP_UTF8` are always added, so the
/// returned buffer is copied into an owned `String` and freed immediately,
/// decoupling its lifetime from term scoping.
///
/// # Safety
///
/// `raw` must be a live term reference on the thread's current engine.
pub(crate) unsafe fn text_from_term(raw: term_t, cvt: c_uint) -> Option<String> {
    let mut chars: *mut c_char = ptr::null_mut();
    let mut len: usize = 0;
    let flags = cvt | swipl_sys::BUF_MALLOC | swipl_sys::REP_UTF8;
    // SAFETY: `raw` is live per this function's contract; `BUF_MALLOC`
    // makes SWI-Prolog return a PL_malloc'ed buffer we own and free below.
    let ok = unsafe { swipl_sys::PL_get_nchars(raw, &mut len, &mut chars, flags) };
    if !ok {
        return None;
    }
    // SAFETY: on success `chars` points to `len` bytes (plus a NUL) that we
    // own; they are copied out before being freed.
    let bytes = unsafe { std::slice::from_raw_parts(chars.cast::<u8>(), len) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: `chars` was allocated by PL_malloc per BUF_MALLOC.
    unsafe { swipl_sys::PL_free(chars.cast::<c_void>()) };
    Some(text)
}

/// Captures and clears the exception pending on the given query (or, with a
/// null `qid`, on the engine itself). Returns `None` if no exception is
/// pending. Must run while the raising query/frame is still open, so the
/// exception term is still live for rendering.
pub(crate) fn take_exception(qid: swipl_sys::qid_t) -> Option<PrologException> {
    // SAFETY: caller context guarantees an engine is current on this thread
    // (all call sites hold an FliContext witness); a null qid means "the
    // engine's pending exception", the sentinel PL_clear_exception pairs
    // with.
    let raw = unsafe { swipl_sys::PL_exception(qid) };
    if raw == 0 {
        return None;
    }
    // SAFETY: `raw` was just returned as a live exception term reference.
    let text = unsafe { text_from_term(raw, swipl_sys::CVT_WRITEQ) }
        .unwrap_or_else(|| "<unrenderable exception>".to_owned());
    // SAFETY: an exception is pending (checked above); clearing it restores
    // a clean engine state, which converting to a Rust error requires.
    unsafe { swipl_sys::PL_clear_exception() };
    Some(PrologException(text))
}

/// Captures and clears the engine's pending exception, if any.
pub(crate) fn take_pending_exception() -> Option<PrologException> {
    take_exception(ptr::null_mut())
}
