//! A serde data format that maps the serde data model onto Prolog terms.
//!
//! Structs, struct variants, and maps become dicts (tagged with the struct or
//! variant name, `#` for maps); tuple structs and tuple variants become
//! compounds; unit variants become atoms; sequences and tuples become proper
//! lists. [`from_term`] reads the same shapes back, and is self-describing
//! (`deserialize_any`), so serde's untagged and internally-tagged enum
//! representations work.
//!
//! `Option` (and unit) values are only supported in dict-entry position — a
//! struct field or map value — where `None` omits the entry; anywhere else
//! they are an [`Error::OptionOutsideDictEntry`].
//!
//! Everything here allocates scratch term references from the caller-supplied
//! [`FliContext`](crate::FliContext) and opens no frames or queries of its own
//! (S1), so the caller's usual scoping obligations — the context must be the
//! thread's innermost open scope — carry over unchanged.
//!
//! [`FliContext::args`](crate::FliContext::args) builds typed predicate
//! argument blocks from [`input`], [`input_as`], [`output`], and existing
//! [`Term`](crate::Term) values. The typed [`Query`](crate::Query) helpers
//! decode requested final bindings into owned tuples and can keep terms
//! connected across sequential or nested calls.
//!
//! [`Record`](crate::Record) has no `Serialize`/`Deserialize` impl: its raw
//! handle must never travel through the serde data model, and there is no
//! way to make that safe while also supporting positions serde routes
//! through its internal value buffering (`#[serde(untagged)]` and
//! internally-tagged enums, `#[serde(flatten)]`). Use
//! [`ExternalRecord`](crate::ExternalRecord) instead when a record needs to
//! cross serde: it serializes as plain bytes (`PL_record_external`), so it
//! works in every position, including buffered and foreign-format ones, at
//! the cost of not being zero-copy. Its `Deserialize` impl is safe to call
//! but trusts the incoming bytes (see
//! [`ExternalRecord::from_bytes`](crate::ExternalRecord::from_bytes) and
//! invariant XR2) — decoding from an untrusted source is a deliberate,
//! accepted trust boundary, not a validated format.
//!
//! References to the external `serde` crate use the absolute path `::serde`
//! to avoid shadowing by this module (`crate::serde`).

pub(crate) mod args;
mod de;
mod external_record;
mod ser;

pub use args::{input, input_as, output, Input, InputAs, Output, TermArg};
pub use de::{from_term, from_terms};
pub use ser::{to_term, to_terms};

use crate::handles::HandleError;
use crate::term::{TermError, TermKind};

/// The error type for serialization to and deserialization from terms.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A message produced by a `Serialize`/`Deserialize` implementation
    /// (`::serde::ser::Error::custom` / `::serde::de::Error::custom`).
    #[error("{0}")]
    Message(String),

    /// A term operation failed.
    #[error(transparent)]
    Term(#[from] TermError),

    /// A handle construction (e.g. a functor) failed.
    #[error(transparent)]
    Handle(#[from] HandleError),

    /// The term does not have the shape the target type requires.
    #[error("expected a term convertible to {expected}")]
    Type { expected: &'static str },

    /// A compound's name/arity did not match the expected tuple struct or
    /// variant.
    #[error("expected {expected_name}/{expected_arity}, found {actual_name}/{actual_arity}")]
    Functor {
        expected_name: String,
        expected_arity: usize,
        actual_name: String,
        actual_arity: usize,
    },

    /// A dict's tag did not match the expected struct or variant name.
    #[error("expected a dict tagged {expected:?}, found {actual:?}")]
    DictTag {
        expected: String,
        actual: Option<String>,
    },

    /// The term kind has no serde representation.
    #[error("cannot deserialize a {kind:?} term")]
    UnsupportedTerm { kind: TermKind },

    /// A list element read as a byte was outside the `u8` range.
    #[error("byte value {value} is outside the u8 range")]
    ByteRange { value: u64 },

    /// A `None` or unit value appeared outside a dict-entry position.
    #[error("Option and unit values are only valid as struct fields or map values")]
    OptionOutsideDictEntry,

    /// A `SerializeMap`/`MapAccess` user broke the key-then-value protocol.
    #[error("map value {0} before its key")]
    MapValueOrder(&'static str),

    /// A `SerializeMap`/`MapAccess` user supplied or requested another key
    /// before completing the preceding key/value pair.
    #[error("map key {0} before the previous key's value")]
    MapKeyOrder(&'static str),

    /// A `SerializeMap` user ended the map after a key without supplying its
    /// value.
    #[error("map ended before the last key's value was serialized")]
    MapKeyWithoutValue,

    /// A tuple-shaped `Serialize` implementation supplied a different number
    /// of fields than it declared, or than the destination expects.
    #[error("{name} declared arity {expected} but serialized {actual} field(s)")]
    ArityMismatch {
        name: String,
        expected: usize,
        actual: usize,
    },
}

impl ::serde::ser::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

impl ::serde::de::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}
