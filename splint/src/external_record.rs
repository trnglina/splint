use std::fmt;
use std::os::raw::c_char;

use crate::record::{pending_record_error, recall_via, Record, RecordError};
use crate::term::{FliContext, Term};

/// A term copied into SWI-Prolog's portable *external record* byte format
/// (`PL_record_external`).
///
/// Unlike [`Record`], which holds a live handle into the engine-independent
/// recorded database, an `ExternalRecord` is a self-contained, ordinary owned
/// byte buffer: it carries no live FFI obligation, no lifetime, and no engine
/// generation at all (invariant XR1). It is [`Send`] + [`Sync`] + [`Clone`]
/// for the same reason a `Vec<u8>` is, and comparable by value. Producing one
/// from a term, and recalling one back into a term, both still require an
/// [`FliContext`] — both cross FFI.
///
/// Its [`ToTerm`](crate::ToTerm)/[`FromTerm`](crate::FromTerm) mapping recalls
/// or records the ordinary Prolog term, allowing an opaque term to be embedded
/// losslessly in a derived Rust structure.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ExternalRecord {
    bytes: Box<[u8]>,
}

/// Sole owner of one `PL_erase_external` obligation for a raw
/// `PL_record_external` buffer, erased unconditionally on drop — including
/// during an unwind — so a panic while copying the buffer out doesn't leak
/// the C-side allocation. Private to [`ExternalRecord::from_term`].
struct ExternalRecordGuard(*mut c_char);

impl Drop for ExternalRecordGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live `PL_record_external` buffer whose sole
        // erase obligation this guard holds, erased exactly once here.
        unsafe {
            swipl_sys::PL_erase_external(self.0);
        }
    }
}

impl ExternalRecord {
    /// Records `term` as portable bytes (`PL_record_external`).
    pub fn from_term(term: Term<'_>) -> Result<Self, RecordError> {
        crate::scope::assert_gen(term.gen(), "term");
        let mut size: usize = 0;
        // SAFETY: the gen assert above proves `term` is live on the current
        // engine; `PL_record_external` writes the encoded length into `size`
        // and returns an owned buffer of that many bytes carrying one erase
        // obligation, taken on by `guard` immediately below.
        let ptr = unsafe { swipl_sys::PL_record_external(term.as_raw(), &mut size) };
        if ptr.is_null() {
            return Err(pending_record_error());
        }
        // Erases `ptr` unconditionally on drop, including if the `to_vec()`
        // copy below panics, so the FFI buffer is never leaked.
        let guard = ExternalRecordGuard(ptr);
        // SAFETY: `ptr` is valid for exactly `size` bytes, per the call above.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
        drop(guard);
        Ok(ExternalRecord {
            bytes: bytes.into_boxed_slice(),
        })
    }

    /// Recalls the bytes into a fresh reference allocated from `ctx`
    /// (`PL_recorded_external`).
    ///
    /// # Panics
    ///
    /// Panics if `ctx` is not the innermost open scope of the thread's current
    /// engine (C2/C3), as for [`FliContext::term`].
    pub fn recall<'a, C: FliContext + ?Sized>(&self, ctx: &'a C) -> Result<Term<'a>, RecordError> {
        recall_via(ctx, |dest| self.recall_into(dest))
    }

    /// Recalls the bytes into the existing reference `term`
    /// (`PL_recorded_external`), overwriting whatever it held.
    ///
    /// `PL_recorded_external` performs no bounds checking against `bytes`: it
    /// trusts the buffer's own embedded structure and reads accordingly. Some
    /// malformations — e.g. an incompatible version/word-size header — are
    /// caught immediately and return `RecordError::Failed`. Others — a buffer
    /// that looks structurally plausible but is truncated or otherwise
    /// corrupted past the header — are not caught, and cause SWI-Prolog to
    /// read past the end of `bytes`. See [`ExternalRecord::from_bytes`] for
    /// the resulting trust boundary (invariant XR2 in the crate docs).
    ///
    /// # Panics
    ///
    /// Panics if `term` does not belong to the thread's current engine (C3).
    pub fn recall_into(&self, term: Term<'_>) -> Result<(), RecordError> {
        crate::scope::assert_gen(term.gen(), "term");
        // SAFETY: the gen assert above proves `term` is live on the current
        // engine; `PL_recorded_external` only reads `self.bytes`, which stays
        // borrowed for the duration of the call.
        let ok = unsafe {
            swipl_sys::PL_recorded_external(self.bytes.as_ptr() as *const c_char, term.as_raw())
        };
        if ok {
            return Ok(());
        }
        Err(pending_record_error())
    }

    /// The raw portable bytes, e.g. to persist to disk or send elsewhere.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Wraps raw bytes obtained elsewhere (e.g. read back from disk) as an
    /// `ExternalRecord`.
    ///
    /// This does not validate the bytes structurally — there is no safe way
    /// to do so without a live engine. This is a deliberate, accepted trust
    /// boundary (invariant XR2 in the crate docs), not an oversight: some
    /// malformed encodings are caught cleanly by
    /// [`recall`](Self::recall)/[`recall_into`](Self::recall_into), returning
    /// `RecordError::Failed`, but others — bytes that are corrupted in a way
    /// SWI-Prolog's header check doesn't catch — cause an out-of-bounds read
    /// (undefined behavior) rather than a clean error. Only pass bytes that
    /// were themselves produced by [`ExternalRecord::as_bytes`]/
    /// [`from_term`](Self::from_term) (directly, or via a trusted round-trip
    /// such as writing them to and reading them back from disk) — never bytes
    /// from an untrusted or adversarial source.
    pub fn from_bytes(bytes: impl Into<Box<[u8]>>) -> Self {
        ExternalRecord {
            bytes: bytes.into(),
        }
    }

    /// Recalls the bytes into a scratch term and records that (`PL_record`),
    /// producing a live, engine-scoped [`Record`] handle.
    pub fn to_record<C: FliContext + ?Sized>(&self, ctx: &C) -> Result<Record, RecordError> {
        let term = self.recall(ctx)?;
        term.record()
    }
}

impl fmt::Debug for ExternalRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalRecord")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}
