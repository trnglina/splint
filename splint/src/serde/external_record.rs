//! `Serialize`/`Deserialize` for [`ExternalRecord`](crate::ExternalRecord).
//!
//! Unlike the removed `Record` handoff, this needs no coordination with
//! splint's own serializer/deserializer at all: by the time these impls run,
//! the only FFI-touching step (`PL_record_external`/`PL_recorded_external`)
//! has already happened, at construction or recall time. What's left is
//! ordinary owned bytes, so this is just "the bytes are data" — it composes
//! with untagged/internally-tagged enums, `#[serde(flatten)]`, and foreign
//! formats like any other serde type.
//!
//! `Deserialize` is built on
//! [`ExternalRecord::from_bytes`](crate::ExternalRecord::from_bytes), so it
//! inherits the same trust boundary that constructor documents: decoding
//! bytes from an untrusted or adversarial source (e.g. attacker-controlled
//! JSON) is not guaranteed to fail cleanly on `recall`/`recall_into` — see
//! invariant XR2 in the crate docs.

use std::fmt;

use ::serde::de::{SeqAccess, Visitor};
use ::serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ExternalRecord;

impl Serialize for ExternalRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.as_bytes())
    }
}

struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
    type Value = ExternalRecord;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bytes of a recorded Prolog term")
    }

    fn visit_bytes<E: ::serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        Ok(ExternalRecord::from_bytes(v.to_vec()))
    }

    fn visit_byte_buf<E: ::serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        Ok(ExternalRecord::from_bytes(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        Ok(ExternalRecord::from_bytes(bytes))
    }
}

impl<'de> Deserialize<'de> for ExternalRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_bytes(BytesVisitor)
    }
}
