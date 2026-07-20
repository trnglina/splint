//! The private handoff that lets a [`Record`] cross the serde boundary.
//!
//! A record handle is a raw pointer into SWI-Prolog's recorded database, so
//! it must never travel through the serde data model as portable data: a
//! forged or replayed value would reach `PL_recorded`/`PL_erase` as a wild
//! pointer. Instead, [`Record`]'s `Serialize`/`Deserialize` impls exchange
//! the handle with splint's own serializer/deserializer through same-thread,
//! same-call thread-local stacks, keyed by a private newtype-struct token
//! (invariant S2). Whatever appears *in* the data model is a guard value
//! that always errors, so foreign formats (e.g. `serde_json`) fail cleanly
//! rather than emitting a meaningless pointer, and a forged token call finds
//! the stack empty and fails without touching FFI.

use std::cell::RefCell;
use std::fmt;

use ::serde::de::{Error as _, IntoDeserializer, Visitor};
use ::serde::ser::Error as _;
use ::serde::{Deserialize, Deserializer, Serialize, Serializer};
use swipl_sys::record_t;

use crate::record::Record;

/// Private newtype-struct name used to pass a [`Record`] through serde.
/// Collision-free with derived struct names, which come from `stringify!` and
/// so can never contain `::` or `$`.
pub(super) const RECORD_TOKEN: &str = "$splint::private::Record";

thread_local! {
    /// Record handles in flight from [`Record::serialize`] to the splint
    /// serializer's token branch.
    static OUTGOING: RefCell<Vec<record_t>> = const { RefCell::new(Vec::new()) };
    /// Record handles in flight from the splint deserializer's token branch
    /// to [`RecordVisitor`].
    static INCOMING: RefCell<Vec<record_t>> = const { RefCell::new(Vec::new()) };
}

/// Takes the record handle most recently offered by [`Record::serialize`],
/// if the current token call was reached through it (and not forged).
pub(super) fn pop_outgoing() -> Option<record_t> {
    OUTGOING.with(|stack| stack.borrow_mut().pop())
}

fn push_outgoing(raw: record_t) -> OutgoingGuard {
    OUTGOING.with(|stack| stack.borrow_mut().push(raw));
    OutgoingGuard { raw }
}

/// Removes `raw` if the serializer never consumed it (a foreign format, or
/// an error or panic before the token branch ran).
fn remove_outgoing(raw: record_t) {
    OUTGOING.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(index) = stack.iter().position(|candidate| *candidate == raw) {
            stack.swap_remove(index);
        }
    });
}

/// Keeps an outgoing handoff confined to the `Record::serialize` call that
/// created it, including when the serializer unwinds.
struct OutgoingGuard {
    raw: record_t,
}

impl Drop for OutgoingGuard {
    fn drop(&mut self) {
        remove_outgoing(self.raw);
    }
}

/// Offers a freshly recorded handle to [`RecordVisitor`].
pub(super) fn push_incoming(raw: record_t) -> IncomingGuard {
    INCOMING.with(|stack| stack.borrow_mut().push(raw));
    IncomingGuard { raw }
}

fn pop_incoming() -> Option<record_t> {
    INCOMING.with(|stack| stack.borrow_mut().pop())
}

/// Removes `raw` if the visitor never claimed it.
fn take_incoming(raw: record_t) -> bool {
    INCOMING.with(|stack| {
        let mut stack = stack.borrow_mut();
        match stack.iter().position(|candidate| *candidate == raw) {
            Some(index) => {
                stack.swap_remove(index);
                true
            }
            None => false,
        }
    })
}

/// Owns an incoming record until `RecordVisitor` claims it. If the visitor
/// errors or unwinds first, dropping this guard erases the record.
pub(super) struct IncomingGuard {
    raw: record_t,
}

impl Drop for IncomingGuard {
    fn drop(&mut self) {
        if take_incoming(self.raw) {
            drop(Record::from_raw(self.raw));
        }
    }
}

/// The `value` argument passed under [`RECORD_TOKEN`]. Splint's own
/// serializer never looks at it — the thread-local handoff is authoritative —
/// so the only way to reach this impl is a foreign serializer that treats the
/// token as an ordinary newtype struct, which fails cleanly here instead of
/// emitting a meaningless raw pointer as portable data.
struct ForeignRecordGuard;

impl Serialize for ForeignRecordGuard {
    fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom(
            "splint records can only be serialized into Prolog terms",
        ))
    }
}

impl Serialize for Record {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let _handoff = push_outgoing(self.as_raw());
        serializer.serialize_newtype_struct(RECORD_TOKEN, &ForeignRecordGuard)
    }
}

struct RecordVisitor;

impl<'de> Visitor<'de> for RecordVisitor {
    type Value = Record;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a splint record")
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, _: D) -> Result<Record, D::Error> {
        // An empty stack means the token call was not driven by the splint
        // deserializer: a foreign format, or a Content-buffered position
        // (untagged/internally-tagged enums, `#[serde(flatten)]`).
        let raw = pop_incoming().ok_or_else(|| {
            D::Error::custom("splint records can only be deserialized from Prolog terms")
        })?;
        Ok(Record::from_raw(raw))
    }
}

impl<'de> Deserialize<'de> for Record {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(RECORD_TOKEN, RecordVisitor)
    }
}

/// Pins the inert deserializer handed to [`Visitor::visit_newtype_struct`] in
/// the token branch to the module's error type.
pub(super) fn unit_deserializer() -> ::serde::de::value::UnitDeserializer<super::Error> {
    ().into_deserializer()
}

#[cfg(test)]
mod tests {
    use super::{pop_outgoing, push_outgoing};

    #[test]
    fn outgoing_handoff_is_removed_during_unwind() {
        let raw = std::ptr::dangling_mut();
        let result = std::panic::catch_unwind(|| {
            let _handoff = push_outgoing(raw);
            panic!("intentional panic");
        });

        assert!(result.is_err());
        assert!(pop_outgoing().is_none());
    }
}
