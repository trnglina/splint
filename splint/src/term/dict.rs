//! Dict construction and inspection.
//!
//! Dicts are ordinary [`Term`]s (`TermKind::Dict`, invariant D1), so these are
//! `&self` methods on `Term` split out here only to keep `term.rs` from
//! carrying the callback plumbing (`PL_for_dict`) and the `is_dict/2` query
//! detour the tag accessor needs. Every operation is a generation-checked term
//! operation like any other; `dict_entries` additionally asserts its context
//! is the innermost open scope, because each value is copied into the
//! innermost open frame during the callback.

use std::marker::PhantomData;
use std::os::raw::c_int;

use swipl_sys::term_t;

use crate::exception::take_pending_exception;
use crate::handles::Atom;
use crate::scope;

use super::{check_get, check_put, FliContext, Term, TermError, TermList};

impl<'f> Term<'f> {
    /// Builds a dict `tag{key: value, ...}` in this reference
    /// (`PL_put_dict`). `keys` and `values` must have equal length; `values`
    /// is a contiguous block (as from [`FliContext::terms`]).
    ///
    /// `PL_put_dict` sorts the key/value pairs in place — reordering the
    /// `values` block — and rejects duplicate keys (surfaced as a
    /// [`TermError::Exception`]), so `values` should not be relied on to keep
    /// its original order after this call.
    pub fn put_dict(
        &self,
        tag: &Atom<'_>,
        keys: &[&Atom<'_>],
        values: &TermList<'_>,
    ) -> Result<(), TermError> {
        scope::assert_gen(self.gen, "term");
        scope::assert_gen(values.gen(), "term");
        if keys.len() != values.len() {
            return Err(TermError::DictLengthMismatch {
                keys: keys.len(),
                values: values.len(),
            });
        }
        let raw_keys: Vec<swipl_sys::atom_t> = keys.iter().map(|atom| atom.as_raw()).collect();
        // SAFETY: C3 asserts above cover `self` and the `values` block; `tag`
        // and the key atoms are live handles (A1); `raw_keys` and `values` are
        // both `keys.len()` long (checked above), matching the `len` argument.
        check_put(
            unsafe {
                swipl_sys::PL_put_dict(
                    self.raw,
                    tag.as_raw(),
                    raw_keys.len(),
                    raw_keys.as_ptr(),
                    values.as_raw(),
                )
            },
            "PL_put_dict",
        )
    }

    /// Reads the value stored under `key` in this dict into a fresh reference
    /// allocated from `ctx` (`PL_get_dict_key`). A missing key or a non-dict
    /// term is a [`TermError::TypeMismatch`].
    pub fn get_dict<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
        key: &Atom<'_>,
    ) -> Result<Term<'a>, TermError> {
        scope::assert_gen(self.gen, "term");
        let dest = ctx.term()?;
        // SAFETY: C3 assert above; `key` is a live atom handle (A1); `dest` is
        // a fresh live reference the value is read into.
        check_get(
            unsafe { swipl_sys::PL_get_dict_key(key.as_raw(), self.raw, dest.raw) },
            "a dict containing the key",
        )?;
        Ok(dest)
    }

    /// Enumerates this dict's key/value pairs (`PL_for_dict`), in the dict's
    /// sorted key order. Each value is copied into a fresh reference allocated
    /// from `ctx`. A non-dict term is a [`TermError::TypeMismatch`].
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3): the value copies are allocated in the innermost open
    /// frame, which the check proves is `ctx`'s scope.
    pub fn dict_entries<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<(DictKey<'a>, Term<'a>)>, TermError> {
        scope::assert_gen(self.gen, "term");
        let activation = ctx.activation();
        scope::assert_innermost(activation, "dict value reference");
        // SAFETY: C3 assert above; classifying is always valid.
        if !unsafe { swipl_sys::PL_is_dict(self.raw) } {
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        let mut collector = DictCollector {
            entries: Vec::new(),
            error: None,
            resource_failed: false,
        };
        // SAFETY: `self.raw` is a live dict on the current engine (C3 assert +
        // `PL_is_dict` above); the callback allocates value copies in the
        // innermost open frame, which the innermost assert above proves is
        // `ctx`'s scope, so they live exactly as long as `'a`.
        unsafe {
            swipl_sys::PL_for_dict(
                self.raw,
                Some(collect_dict_entry),
                (&mut collector as *mut DictCollector).cast(),
                swipl_sys::PL_FOR_DICT_SORTED as c_int,
            );
        }
        if collector.resource_failed {
            // Copying a value failed mid-iteration under resource exhaustion;
            // capture and clear the pending exception now (matching
            // `FliContext::term`'s handling of the same failure) so it cannot
            // leak onto a later operation.
            return Err(match take_pending_exception() {
                Some(exception) => TermError::Exception(exception),
                None => panic!("splint: a dict value copy failed with no pending exception"),
            });
        }
        if let Some(error) = collector.error {
            return Err(error);
        }
        Ok(collector
            .entries
            .into_iter()
            .map(|(key, value)| {
                let value = Term {
                    raw: value,
                    gen: activation.gen,
                    _scope: PhantomData,
                    _not_send_sync: PhantomData,
                };
                let key = match key {
                    RawDictKey::Atom(raw) => {
                        // SAFETY: `raw` is a live atom handle read from the
                        // dict during iteration; `from_raw` takes its own
                        // registration (A1).
                        DictKey::Atom(unsafe { Atom::from_raw(raw) })
                    }
                    RawDictKey::Int(value) => DictKey::Int(value),
                };
                (key, value)
            })
            .collect())
    }

    /// Reads this dict's tag: `Some(atom)` for an atom tag, `None` for an
    /// unbound (variable) tag. A non-dict term is a
    /// [`TermError::TypeMismatch`].
    ///
    /// The C API exposes no native tag accessor, so the tag is read through
    /// the `is_dict/2` builtin — the sanctioned path, as reading the internal
    /// dict layout would be fragile.
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3): a query is opened through it.
    pub fn dict_tag<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Option<Atom<'a>>, TermError> {
        scope::assert_gen(self.gen, "term");
        // SAFETY: C3 assert above.
        if !unsafe { swipl_sys::PL_is_dict(self.raw) } {
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        let is_dict = crate::Predicate::resolve(ctx, "is_dict", 2, None)
            .map_err(|_| TermError::TypeMismatch { expected: "a dict" })?;
        let args = ctx.terms(2)?;
        args.get(0).put_term(*self)?;
        let tag = args.get(1);
        let mut query = crate::Query::open(ctx, &is_dict, &args, crate::QueryOptions::default())
            .map_err(dict_query_error)?;
        let matched = query.next_solution().map_err(dict_query_error)?;
        if !matched {
            // No solution: discard and report the (already checked, but be
            // defensive) non-dict shape.
            query.close().map_err(dict_query_error)?;
            return Err(TermError::TypeMismatch { expected: "a dict" });
        }
        // Keep the solution's bindings so the tag stays bound in `args`;
        // `close` would undo them.
        query.cut().map_err(dict_query_error)?;
        if tag.is_variable() {
            Ok(None)
        } else {
            Ok(Some(tag.get_atom()?))
        }
    }
}

/// A dict key, which SWI-Prolog represents as either an atom or a small
/// integer.
pub enum DictKey<'c> {
    Atom(Atom<'c>),
    Int(i64),
}

/// The key form collected during `PL_for_dict` iteration, before atoms are
/// given owning registrations. Kept separate from [`DictKey`] so the callback
/// stays free of [`Atom`] construction (which the `?Sized` context lifetimes
/// make awkward inside an `extern "C"` function).
enum RawDictKey {
    Atom(swipl_sys::atom_t),
    Int(i64),
}

/// Accumulates dict entries across [`collect_dict_entry`] invocations.
struct DictCollector {
    entries: Vec<(RawDictKey, term_t)>,
    /// A *final* error with no pending Prolog exception (e.g. a malformed
    /// key), returned to the caller as-is.
    error: Option<TermError>,
    /// Set when copying a value failed under resource exhaustion
    /// (`PL_new_term_ref` returning 0, or `PL_put_term` returning false): a
    /// Prolog exception is pending and must be captured *after* `PL_for_dict`
    /// returns (rendering it inside the callback, while already out of stack,
    /// is best avoided).
    resource_failed: bool,
}

/// Records one dict entry per invocation (`PL_for_dict` callback). Returns
/// non-zero to abort iteration.
///
/// The `key` and `value` term references are only valid for the duration of
/// this call, so the key is read eagerly and the value is copied into a fresh
/// reference on the innermost open frame — which the caller has asserted is the
/// scope the collected values are attributed to.
unsafe extern "C" fn collect_dict_entry(
    key: term_t,
    value: term_t,
    closure: *mut std::os::raw::c_void,
) -> c_int {
    // SAFETY: `closure` is the `DictCollector` pointer passed to `PL_for_dict`,
    // exclusively borrowed here for the duration of this synchronous callback.
    let collector = unsafe { &mut *closure.cast::<DictCollector>() };
    let mut atom: swipl_sys::atom_t = 0;
    // SAFETY: `key`/`value` are live references for this call. Dict keys are
    // atoms or small integers; try each.
    let dict_key = if unsafe { swipl_sys::PL_get_atom(key, &mut atom) } {
        RawDictKey::Atom(atom)
    } else {
        let mut int: i64 = 0;
        if unsafe { swipl_sys::PL_get_int64(key, &mut int) } {
            RawDictKey::Int(int)
        } else {
            collector.error = Some(TermError::TypeMismatch {
                expected: "a dict key (atom or integer)",
            });
            return 1;
        }
    };
    // SAFETY: allocates in the innermost open frame (the caller's asserted
    // `ctx` scope); the copy outlives this call, unlike `value`.
    let copied = unsafe { swipl_sys::PL_new_term_ref() };
    if copied == 0 {
        // Resource exhaustion: a Prolog exception is pending. Flag it so the
        // caller captures and clears it after iteration, rather than leaving
        // it to surface on a later, unrelated operation.
        collector.resource_failed = true;
        return 1;
    }
    // SAFETY: `copied` and `value` are both live references on the current
    // engine. PL_put_term can fail under global-stack exhaustion (e.g.
    // globalizing a variable value), leaving a pending exception; treat that
    // like the allocation failure above rather than recording a bogus value.
    if !unsafe { swipl_sys::PL_put_term(copied, value) } {
        collector.resource_failed = true;
        return 1;
    }
    collector.entries.push((dict_key, copied));
    0
}

/// Maps a [`QueryError`](crate::QueryError) raised while reading a dict tag
/// back onto [`TermError`], preserving a captured Prolog exception.
fn dict_query_error(error: crate::QueryError) -> TermError {
    match error {
        crate::QueryError::Exception(exception) => TermError::Exception(exception),
        _ => TermError::TypeMismatch { expected: "a dict" },
    }
}
