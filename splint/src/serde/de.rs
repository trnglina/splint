use ::serde::de::{
    DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use ::serde::{forward_to_deserialize_any, Deserializer};

use super::{record_token, Error};
use crate::term::{DictKey, FliContext, Term, TermError, TermKind, TermList};

/// Builds a string deserializer with the error type pinned to [`Error`], used
/// to feed dict keys and variant names into serde seeds.
fn string_deserializer(value: String) -> ::serde::de::value::StringDeserializer<Error> {
    value.into_deserializer()
}

/// Deserializes a `T` out of `term`, allocating scratch references from `ctx`.
pub fn from_term<C, T>(ctx: &C, term: Term<'_>) -> Result<T, Error>
where
    C: FliContext + ?Sized,
    T: DeserializeOwned,
{
    T::deserialize(TermDeserializer {
        ctx,
        term,
        option_allowed: false,
    })
}

/// Deserializes each term of `args` into a `T` — typically a tuple whose
/// arity matches `args.len()`. Composes with the mappers of
/// [`Query::once`](crate::Query::once) and
/// [`Query::solutions`](crate::Query::solutions), which receive `&Query` (an
/// [`FliContext`]) and must return owned data.
pub fn from_terms<C, T>(ctx: &C, args: &TermList<'_>) -> Result<T, Error>
where
    C: FliContext + ?Sized,
    T: DeserializeOwned,
{
    T::deserialize(ArgsDeserializer {
        ctx,
        items: args.iter().collect(),
    })
}

/// Reads a dict's entries with the keys rendered as text, matching the
/// serializer's key representation (integer keys stringified).
fn dict_string_entries<'x, C>(ctx: &'x C, term: Term<'_>) -> Result<Vec<(String, Term<'x>)>, Error>
where
    C: FliContext + ?Sized,
{
    Ok(term
        .dict_entries(ctx)?
        .into_iter()
        .map(|(key, value)| {
            let key = match key {
                DictKey::Atom(atom) => atom.text(),
                DictKey::Int(value) => value.to_string(),
            };
            (key, value)
        })
        .collect())
}

/// A serde deserializer that reads a Prolog term. Self-describing via
/// [`Deserializer::deserialize_any`], which is what lets serde's untagged and
/// internally-tagged enums work.
struct TermDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    option_allowed: bool,
}

impl<'x, 'f, C: FliContext + ?Sized> TermDeserializer<'x, 'f, C> {
    /// Reads the term as text, accepting an atom, string, or number.
    fn text(&self) -> Result<String, Error> {
        Ok(self.term.get_text()?)
    }
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> Deserializer<'de> for TermDeserializer<'x, 'f, C> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        match self.term.kind() {
            TermKind::Variable => visitor.visit_unit(),
            TermKind::Atom => {
                let text = self.term.get_atom()?.text();
                match text.as_str() {
                    "true" => visitor.visit_bool(true),
                    "false" => visitor.visit_bool(false),
                    _ => visitor.visit_string(text),
                }
            }
            TermKind::Integer => match self.term.get_i64() {
                Ok(value) => visitor.visit_i64(value),
                Err(_) => visitor.visit_u64(self.term.get_u64()?),
            },
            TermKind::Float => visitor.visit_f64(self.term.get_f64()?),
            TermKind::String => visitor.visit_string(self.term.get_text()?),
            TermKind::Nil | TermKind::ListPair => visitor.visit_seq(SeqDeserializer::new(
                self.ctx,
                self.term.collect_list(self.ctx)?,
            )),
            TermKind::Dict => visitor.visit_map(MapDeserializer::new(
                self.ctx,
                dict_string_entries(self.ctx, self.term)?,
            )),
            TermKind::Compound => {
                let (name, arity) = self.term.name_arity()?;
                visitor.visit_map(CompoundDeserializer {
                    ctx: self.ctx,
                    term: self.term,
                    name: Some(name.text()),
                    arity,
                })
            }
            kind @ (TermKind::Rational | TermKind::Blob) => Err(Error::UnsupportedTerm { kind }),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_bool(self.term.get_bool()?)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.term.get_i64()?)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.term.get_i64()?)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.term.get_i64()?)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.term.get_i64()?)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.term.get_u64()?)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.term.get_u64()?)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.term.get_u64()?)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.term.get_u64()?)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f64(self.term.get_f64()?)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f64(self.term.get_f64()?)
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str(&self.text()?)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(self.text()?)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(self.text()?)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        let mut bytes = Vec::new();
        for item in self.term.collect_list(self.ctx)? {
            let value = item.get_u64()?;
            let byte = u8::try_from(value).map_err(|_| Error::ByteRange { value })?;
            bytes.push(byte);
        }
        visitor.visit_byte_buf(bytes)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        if !self.option_allowed {
            return Err(Error::OptionOutsideDictEntry);
        }
        visitor.visit_some(TermDeserializer {
            ctx: self.ctx,
            term: self.term,
            option_allowed: false,
        })
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        if name == record_token::RECORD_TOKEN {
            let raw = crate::record::record_raw(self.term)?;
            let _handoff = record_token::push_incoming(raw);
            return visitor.visit_newtype_struct(record_token::unit_deserializer());
        }
        visitor.visit_newtype_struct(TermDeserializer {
            ctx: self.ctx,
            term: self.term,
            option_allowed: false,
        })
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(SeqDeserializer::new(
            self.ctx,
            self.term.collect_list(self.ctx)?,
        ))
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        // A zero-field tuple struct is represented as an atom (a zero-arity
        // functor does not make a compound), so match the atom by name.
        if self.term.kind() == TermKind::Atom {
            let actual_name = self.term.get_atom()?.text();
            if actual_name != name || len != 0 {
                return Err(Error::Functor {
                    expected_name: name.to_owned(),
                    expected_arity: len,
                    actual_name,
                    actual_arity: 0,
                });
            }
            return visitor.visit_seq(SeqDeserializer::new(self.ctx, Vec::new()));
        }
        let (name_atom, arity) = self.term.name_arity()?;
        let actual_name = name_atom.text();
        if actual_name != name || arity != len {
            return Err(Error::Functor {
                expected_name: name.to_owned(),
                expected_arity: len,
                actual_name,
                actual_arity: arity,
            });
        }
        let arguments = collect_arguments(self.ctx, self.term, len)?;
        visitor.visit_seq(SeqDeserializer::new(self.ctx, arguments))
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(MapDeserializer::new(
            self.ctx,
            dict_string_entries(self.ctx, self.term)?,
        ))
    }

    fn deserialize_struct<V>(
        self,
        name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        let tag = self.term.dict_tag(self.ctx)?;
        match &tag {
            Some(atom) if atom.text() == name => {}
            _ => {
                return Err(Error::DictTag {
                    expected: name.to_owned(),
                    actual: tag.map(|atom| atom.text()),
                });
            }
        }
        visitor.visit_map(MapDeserializer::new(
            self.ctx,
            dict_string_entries(self.ctx, self.term)?,
        ))
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        let shape = match self.term.kind() {
            TermKind::Atom => VariantShape::Unit(self.term.get_atom()?.text()),
            TermKind::Compound => {
                let (name, arity) = self.term.name_arity()?;
                VariantShape::Compound {
                    variant: name.text(),
                    arity,
                }
            }
            TermKind::Dict => {
                let tag = self.term.dict_tag(self.ctx)?.ok_or(Error::DictTag {
                    expected: "an enum variant".to_owned(),
                    actual: None,
                })?;
                VariantShape::Dict(tag.text())
            }
            _ => return Err(Error::Type { expected: "enum" }),
        };
        visitor.visit_enum(TermEnumAccess {
            ctx: self.ctx,
            term: self.term,
            shape,
        })
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

/// Reads a compound's first `arity` arguments into fresh references.
fn collect_arguments<'x, C>(
    ctx: &'x C,
    term: Term<'_>,
    arity: usize,
) -> Result<Vec<Term<'x>>, Error>
where
    C: FliContext + ?Sized,
{
    (0..arity)
        .map(|index| term.get_arg(ctx, index))
        .collect::<Result<Vec<_>, TermError>>()
        .map_err(Error::from)
}

/// `SeqAccess` over an owned vector of terms (list elements or compound
/// arguments).
struct SeqDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    items: std::vec::IntoIter<Term<'f>>,
}

impl<'x, 'f, C: FliContext + ?Sized> SeqDeserializer<'x, 'f, C> {
    fn new(ctx: &'x C, items: Vec<Term<'f>>) -> Self {
        Self {
            ctx,
            items: items.into_iter(),
        }
    }
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> SeqAccess<'de> for SeqDeserializer<'x, 'f, C> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Error>
    where
        T: DeserializeSeed<'de>,
    {
        match self.items.next() {
            Some(term) => seed
                .deserialize(TermDeserializer {
                    ctx: self.ctx,
                    term,
                    option_allowed: false,
                })
                .map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.items.len())
    }
}

/// `MapAccess` over an owned vector of dict entries.
struct MapDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    entries: std::vec::IntoIter<(String, Term<'f>)>,
    value: Option<Term<'f>>,
}

impl<'x, 'f, C: FliContext + ?Sized> MapDeserializer<'x, 'f, C> {
    fn new(ctx: &'x C, entries: Vec<(String, Term<'f>)>) -> Self {
        Self {
            ctx,
            entries: entries.into_iter(),
            value: None,
        }
    }
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> MapAccess<'de> for MapDeserializer<'x, 'f, C> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Error>
    where
        K: DeserializeSeed<'de>,
    {
        match self.entries.next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(DictKeyDeserializer { key }).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Error>
    where
        V: DeserializeSeed<'de>,
    {
        let term = self.value.take().ok_or(Error::MapValueOrder("requested"))?;
        seed.deserialize(TermDeserializer {
            ctx: self.ctx,
            term,
            option_allowed: true,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// Deserializes dict key text as the scalar type requested by serde.
/// SWI-Prolog exposes dict keys as atoms or integers, while the serializer
/// represents boolean keys using their atom names.
struct DictKeyDeserializer {
    key: String,
}

impl DictKeyDeserializer {
    fn invalid_type(&self, expected: &'static str) -> Error {
        Error::Message(format!("dict key {:?} is not a {expected}", self.key))
    }
}

macro_rules! deserialize_key_integer {
    ($name:ident, $visit:ident, $type:ty) => {
        fn $name<V>(self, visitor: V) -> Result<V::Value, Error>
        where
            V: Visitor<'de>,
        {
            let value: $type = self
                .key
                .parse()
                .map_err(|_| self.invalid_type(stringify!($type)))?;
            visitor.$visit(value)
        }
    };
}

impl<'de> Deserializer<'de> for DictKeyDeserializer {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        match self.key.as_str() {
            "true" => visitor.visit_bool(true),
            "false" => visitor.visit_bool(false),
            _ => match self.key.parse::<i64>() {
                Ok(value) => visitor.visit_i64(value),
                Err(_) => match self.key.parse::<u64>() {
                    Ok(value) => visitor.visit_u64(value),
                    Err(_) => visitor.visit_string(self.key),
                },
            },
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        match self.key.as_str() {
            "true" => visitor.visit_bool(true),
            "false" => visitor.visit_bool(false),
            _ => Err(self.invalid_type("boolean")),
        }
    }

    deserialize_key_integer!(deserialize_i8, visit_i8, i8);
    deserialize_key_integer!(deserialize_i16, visit_i16, i16);
    deserialize_key_integer!(deserialize_i32, visit_i32, i32);
    deserialize_key_integer!(deserialize_i64, visit_i64, i64);
    deserialize_key_integer!(deserialize_u8, visit_u8, u8);
    deserialize_key_integer!(deserialize_u16, visit_u16, u16);
    deserialize_key_integer!(deserialize_u32, visit_u32, u32);
    deserialize_key_integer!(deserialize_u64, visit_u64, u64);

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(self.key)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(self.key)
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_string(visitor)
    }

    forward_to_deserialize_any! {
        f32 f64 char bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum ignored_any
    }
}

/// Presents a compound `name(args..)` as the single-entry map `{name: args}`
/// used by self-describing deserialization.
struct CompoundDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    name: Option<String>,
    arity: usize,
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> MapAccess<'de> for CompoundDeserializer<'x, 'f, C> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Error>
    where
        K: DeserializeSeed<'de>,
    {
        match self.name.take() {
            Some(name) => seed.deserialize(string_deserializer(name)).map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Error>
    where
        V: DeserializeSeed<'de>,
    {
        let arguments = collect_arguments(self.ctx, self.term, self.arity)?;
        if let [single] = arguments.as_slice() {
            seed.deserialize(TermDeserializer {
                ctx: self.ctx,
                term: *single,
                option_allowed: false,
            })
        } else {
            seed.deserialize(SeqValueDeserializer {
                ctx: self.ctx,
                items: arguments,
            })
        }
    }
}

/// A deserializer whose value is a sequence of terms (compound arguments).
struct SeqValueDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    items: Vec<Term<'f>>,
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> Deserializer<'de> for SeqValueDeserializer<'x, 'f, C> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(SeqDeserializer::new(self.ctx, self.items))
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// A deserializer over an argument block, presented as a sequence.
struct ArgsDeserializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    items: Vec<Term<'f>>,
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> Deserializer<'de> for ArgsDeserializer<'x, 'f, C> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(SeqDeserializer::new(self.ctx, self.items))
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// Describes how an enum variant is encoded in the term being decoded.
enum VariantShape {
    Unit(String),
    Compound { variant: String, arity: usize },
    Dict(String),
}

impl VariantShape {
    fn variant(&self) -> &str {
        match self {
            VariantShape::Unit(variant)
            | VariantShape::Compound { variant, .. }
            | VariantShape::Dict(variant) => variant,
        }
    }
}

struct TermEnumAccess<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    shape: VariantShape,
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> EnumAccess<'de> for TermEnumAccess<'x, 'f, C> {
    type Error = Error;
    type Variant = TermVariantAccess<'x, 'f, C>;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Error>
    where
        V: DeserializeSeed<'de>,
    {
        let value = seed.deserialize(string_deserializer(self.shape.variant().to_owned()))?;
        Ok((
            value,
            TermVariantAccess {
                ctx: self.ctx,
                term: self.term,
                shape: self.shape,
            },
        ))
    }
}

struct TermVariantAccess<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    shape: VariantShape,
}

impl<'x, 'de, 'f, C: FliContext + ?Sized> VariantAccess<'de> for TermVariantAccess<'x, 'f, C> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        match self.shape {
            VariantShape::Unit(_) => Ok(()),
            _ => Err(Error::Type {
                expected: "unit variant",
            }),
        }
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, Error>
    where
        T: DeserializeSeed<'de>,
    {
        match self.shape {
            VariantShape::Compound { arity: 1, .. } => {
                let argument = self.term.get_arg(self.ctx, 0)?;
                seed.deserialize(TermDeserializer {
                    ctx: self.ctx,
                    term: argument,
                    option_allowed: false,
                })
            }
            _ => Err(Error::Type {
                expected: "newtype variant",
            }),
        }
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        match self.shape {
            VariantShape::Compound { arity, .. } => {
                let arguments = collect_arguments(self.ctx, self.term, arity)?;
                visitor.visit_seq(SeqDeserializer::new(self.ctx, arguments))
            }
            // A zero-field tuple variant is serialized as an atom, like a
            // unit variant.
            VariantShape::Unit(_) if len == 0 => {
                visitor.visit_seq(SeqDeserializer::new(self.ctx, Vec::new()))
            }
            _ => Err(Error::Type {
                expected: "tuple variant",
            }),
        }
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: Visitor<'de>,
    {
        match self.shape {
            VariantShape::Dict(_) => visitor.visit_map(MapDeserializer::new(
                self.ctx,
                dict_string_entries(self.ctx, self.term)?,
            )),
            _ => Err(Error::Type {
                expected: "struct variant",
            }),
        }
    }
}
