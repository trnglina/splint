use ::serde::ser::{
    Impossible, Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};

use super::Error;
use crate::handles::{Atom, Functor};
use crate::term::{FliContext, Term, TermList};

/// Serializes `value` into `term`, allocating scratch references from `ctx`.
pub fn to_term<C, T>(ctx: &C, term: Term<'_>, value: &T) -> Result<(), Error>
where
    C: FliContext + ?Sized,
    T: Serialize + ?Sized,
{
    value.serialize(TermSerializer {
        ctx,
        term,
        option_allowed: false,
    })?;
    Ok(())
}

/// Serializes `values` — a Rust tuple whose arity matches `args.len()` — into
/// `args`, one slot per tuple element. Typically used to seed a predicate's
/// argument vector before [`Query::open`](crate::Query::open).
pub fn to_terms<C, T>(ctx: &C, args: &TermList<'_>, values: &T) -> Result<(), Error>
where
    C: FliContext + ?Sized,
    T: Serialize + ?Sized,
{
    values.serialize(ArgsSerializer { ctx, args })
}

/// Serializes `value` into a freshly allocated term at a non-dict position,
/// where an absent value (`None`/unit) is an error.
fn serialize_child<'x, C, T>(ctx: &'x C, value: &T) -> Result<Term<'x>, Error>
where
    C: FliContext + ?Sized,
    T: Serialize + ?Sized,
{
    let child = ctx.term()?;
    value.serialize(TermSerializer {
        ctx,
        term: child,
        option_allowed: false,
    })?;
    Ok(child)
}

/// Builds the proper list of `elements` in `dest`. The spine is assembled
/// back to front in scratch references (`cons_list` needs the tail first),
/// then aliased into `dest` with one `put_term`.
fn write_list<'x, C>(ctx: &'x C, dest: Term<'_>, elements: &[Term<'x>]) -> Result<(), Error>
where
    C: FliContext + ?Sized,
{
    let mut tail = ctx.term()?;
    tail.put_nil()?;
    for element in elements.iter().rev() {
        let cell = ctx.term()?;
        cell.cons_list(*element, tail)?;
        tail = cell;
    }
    dest.put_term(tail)?;
    Ok(())
}

/// A serde serializer that writes into a Prolog term.
///
/// `Ok` is a `bool` reporting whether a term was written: concrete values
/// return `true`, while an absent value (`None`/unit in a dict-entry position)
/// returns `false` so the enclosing dict can drop the entry.
struct TermSerializer<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    option_allowed: bool,
}

impl<'x, 'f, C: FliContext + ?Sized> TermSerializer<'x, 'f, C> {
    /// Handles `None`/unit: permitted only in a dict-entry position, where it
    /// reports absence so the entry is dropped.
    fn absent(self) -> Result<bool, Error> {
        if self.option_allowed {
            Ok(false)
        } else {
            Err(Error::OptionOutsideDictEntry)
        }
    }
}

impl<'x, 'f, C: FliContext + ?Sized> Serializer for TermSerializer<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;
    type SerializeSeq = SerializeList<'x, 'f, C>;
    type SerializeTuple = SerializeList<'x, 'f, C>;
    type SerializeTupleStruct = SerializeCompound<'x, 'f, C>;
    type SerializeTupleVariant = SerializeCompound<'x, 'f, C>;
    type SerializeMap = SerializeDict<'x, 'f, C>;
    type SerializeStruct = SerializeDict<'x, 'f, C>;
    type SerializeStructVariant = SerializeDict<'x, 'f, C>;

    fn serialize_bool(self, value: bool) -> Result<bool, Error> {
        self.term.put_bool(value)?;
        Ok(true)
    }

    fn serialize_i8(self, value: i8) -> Result<bool, Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i16(self, value: i16) -> Result<bool, Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i32(self, value: i32) -> Result<bool, Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i64(self, value: i64) -> Result<bool, Error> {
        self.term.put_i64(value)?;
        Ok(true)
    }

    fn serialize_u8(self, value: u8) -> Result<bool, Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u16(self, value: u16) -> Result<bool, Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u32(self, value: u32) -> Result<bool, Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u64(self, value: u64) -> Result<bool, Error> {
        self.term.put_u64(value)?;
        Ok(true)
    }

    fn serialize_f32(self, value: f32) -> Result<bool, Error> {
        self.serialize_f64(value.into())
    }

    fn serialize_f64(self, value: f64) -> Result<bool, Error> {
        self.term.put_f64(value)?;
        Ok(true)
    }

    fn serialize_char(self, value: char) -> Result<bool, Error> {
        let mut buffer = [0u8; 4];
        self.term.put_string(value.encode_utf8(&mut buffer))?;
        Ok(true)
    }

    fn serialize_str(self, value: &str) -> Result<bool, Error> {
        self.term.put_string(value)?;
        Ok(true)
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<bool, Error> {
        let mut elements = Vec::with_capacity(value.len());
        for byte in value {
            let element = self.ctx.term()?;
            element.put_u64((*byte).into())?;
            elements.push(element);
        }
        write_list(self.ctx, self.term, &elements)?;
        Ok(true)
    }

    fn serialize_none(self) -> Result<bool, Error> {
        self.absent()
    }

    fn serialize_some<T>(self, value: &T) -> Result<bool, Error>
    where
        T: Serialize + ?Sized,
    {
        if !self.option_allowed {
            return Err(Error::OptionOutsideDictEntry);
        }
        value.serialize(TermSerializer {
            ctx: self.ctx,
            term: self.term,
            option_allowed: false,
        })
    }

    fn serialize_unit(self) -> Result<bool, Error> {
        self.absent()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<bool, Error> {
        self.absent()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<bool, Error> {
        self.term.put_atom(&Atom::new(self.ctx, variant))?;
        Ok(true)
    }

    fn serialize_newtype_struct<T>(self, _name: &'static str, value: &T) -> Result<bool, Error>
    where
        T: Serialize + ?Sized,
    {
        value.serialize(TermSerializer {
            ctx: self.ctx,
            term: self.term,
            option_allowed: false,
        })
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<bool, Error>
    where
        T: Serialize + ?Sized,
    {
        let args = self.ctx.terms(1)?;
        value.serialize(TermSerializer {
            ctx: self.ctx,
            term: args.get(0),
            option_allowed: false,
        })?;
        let functor = Functor::from_name(self.ctx, variant, 1)?;
        self.term.cons_functor(&functor, &args)?;
        Ok(true)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Ok(SerializeList {
            ctx: self.ctx,
            term: self.term,
            elements: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Error> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        SerializeCompound::open(self.ctx, self.term, name, len)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        SerializeCompound::open(self.ctx, self.term, variant, len)
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Ok(SerializeDict::open(self.ctx, self.term, "#", len))
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Ok(SerializeDict::open(self.ctx, self.term, name, Some(len)))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Ok(SerializeDict::open(self.ctx, self.term, variant, Some(len)))
    }
}

/// Accumulates elements into a Prolog list (`seq` and `tuple`).
struct SerializeList<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    elements: Vec<Term<'x>>,
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeList<'x, 'f, C> {
    fn push<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.elements.push(serialize_child(self.ctx, value)?);
        Ok(())
    }

    fn finish(self) -> Result<bool, Error> {
        write_list(self.ctx, self.term, &self.elements)?;
        Ok(true)
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeSeq for SerializeList<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.push(value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeTuple for SerializeList<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.push(value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

/// Fills a compound's argument block (`tuple_struct`, `tuple_variant`).
///
/// The declared `len` is exact for these shapes, so the block is allocated up
/// front and each field is serialized straight into its slot.
struct SerializeCompound<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    name: &'static str,
    args: TermList<'x>,
    index: usize,
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeCompound<'x, 'f, C> {
    fn open(ctx: &'x C, term: Term<'f>, name: &'static str, len: usize) -> Result<Self, Error> {
        Ok(SerializeCompound {
            ctx,
            term,
            name,
            args: ctx.terms(len)?,
            index: 0,
        })
    }

    fn push<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        // Over-supplying fields relative to the declared arity lands on
        // `TermList::get`'s bounds panic, like any other index misuse.
        let slot = self.args.get(self.index);
        value.serialize(TermSerializer {
            ctx: self.ctx,
            term: slot,
            option_allowed: false,
        })?;
        self.index += 1;
        Ok(())
    }

    fn finish(self) -> Result<bool, Error> {
        if self.index != self.args.len() {
            return Err(Error::ArityMismatch {
                name: self.name.to_owned(),
                expected: self.args.len(),
                actual: self.index,
            });
        }
        if self.args.is_empty() {
            // A zero-arity functor does not make a compound; represent
            // zero-field tuple structs/variants as atoms, like unit variants.
            self.term.put_atom(&Atom::new(self.ctx, self.name))?;
        } else {
            let functor = Functor::from_name(self.ctx, self.name, self.args.len())?;
            self.term.cons_functor(&functor, &self.args)?;
        }
        Ok(true)
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeTupleStruct for SerializeCompound<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.push(value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeTupleVariant for SerializeCompound<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.push(value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

/// Accumulates key/value entries into a Prolog dict (`map`, `struct`,
/// `struct_variant`).
struct SerializeDict<'x, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    term: Term<'f>,
    tag: &'static str,
    entries: Vec<(String, Term<'x>)>,
    pending_key: Option<String>,
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeDict<'x, 'f, C> {
    fn open(ctx: &'x C, term: Term<'f>, tag: &'static str, len: Option<usize>) -> Self {
        SerializeDict {
            ctx,
            term,
            tag,
            entries: Vec::with_capacity(len.unwrap_or(0)),
            pending_key: None,
        }
    }

    fn insert<T>(&mut self, key: String, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        let child = self.ctx.term()?;
        let present = value.serialize(TermSerializer {
            ctx: self.ctx,
            term: child,
            option_allowed: true,
        })?;
        if present {
            self.entries.push((key, child));
        }
        Ok(())
    }

    fn finish(self) -> Result<bool, Error> {
        // `put_dict` needs the values as a contiguous block, so alias each
        // accumulated entry into a throwaway one; `PL_put_dict`'s in-place
        // sorting only reorders that block.
        let values = self.ctx.terms(self.entries.len())?;
        let mut keys = Vec::with_capacity(self.entries.len());
        for (index, (key, value)) in self.entries.iter().enumerate() {
            values.get(index).put_term(*value)?;
            keys.push(Atom::new(self.ctx, key));
        }
        let key_refs: Vec<&Atom<'_>> = keys.iter().collect();
        self.term
            .put_dict(&Atom::new(self.ctx, self.tag), &key_refs, &values)?;
        Ok(true)
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeMap for SerializeDict<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.pending_key = Some(key.serialize(MapKeySerializer)?);
        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        let key = self
            .pending_key
            .take()
            .ok_or(Error::MapValueOrder("serialized"))?;
        self.insert(key, value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeStruct for SerializeDict<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.insert(key.to_owned(), value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

impl<'x, 'f, C: FliContext + ?Sized> SerializeStructVariant for SerializeDict<'x, 'f, C> {
    type Ok = bool;
    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        self.insert(key.to_owned(), value)
    }

    fn end(self) -> Result<bool, Error> {
        self.finish()
    }
}

/// Serializes a map key to the atom text used for the dict key. Only scalar
/// keys are supported.
struct MapKeySerializer;

impl MapKeySerializer {
    fn unsupported() -> Error {
        Error::Message("map keys must be strings, integers, booleans, or chars".to_owned())
    }
}

impl Serializer for MapKeySerializer {
    type Ok = String;
    type Error = Error;
    type SerializeSeq = Impossible<String, Error>;
    type SerializeTuple = Impossible<String, Error>;
    type SerializeTupleStruct = Impossible<String, Error>;
    type SerializeTupleVariant = Impossible<String, Error>;
    type SerializeMap = Impossible<String, Error>;
    type SerializeStruct = Impossible<String, Error>;
    type SerializeStructVariant = Impossible<String, Error>;

    fn serialize_bool(self, value: bool) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_i8(self, value: i8) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_i16(self, value: i16) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_i32(self, value: i32) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_i64(self, value: i64) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_u8(self, value: u8) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_u16(self, value: u16) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_u32(self, value: u32) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_u64(self, value: u64) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_f32(self, _value: f32) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_f64(self, _value: f64) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_char(self, value: char) -> Result<String, Error> {
        Ok(value.to_string())
    }

    fn serialize_str(self, value: &str) -> Result<String, Error> {
        Ok(value.to_owned())
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_none(self) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_some<T>(self, _value: &T) -> Result<String, Error>
    where
        T: Serialize + ?Sized,
    {
        Err(Self::unsupported())
    }

    fn serialize_unit(self) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, Error> {
        Err(Self::unsupported())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<String, Error> {
        Ok(variant.to_owned())
    }

    fn serialize_newtype_struct<T>(self, _name: &'static str, value: &T) -> Result<String, Error>
    where
        T: Serialize + ?Sized,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<String, Error>
    where
        T: Serialize + ?Sized,
    {
        Err(Self::unsupported())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Self::unsupported())
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Self::unsupported())
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Self::unsupported())
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Self::unsupported())
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Self::unsupported())
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Err(Self::unsupported())
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Self::unsupported())
    }
}

/// Serializes a Rust tuple into consecutive slots of an argument block.
struct ArgsSerializer<'x, 'a, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    args: &'a TermList<'f>,
}

impl<'x, 'a, 'f, C: FliContext + ?Sized> ArgsSerializer<'x, 'a, 'f, C> {
    fn expects_tuple() -> Error {
        Error::Message("to_terms expects a tuple whose arity matches the argument list".to_owned())
    }
}

impl<'x, 'a, 'f, C: FliContext + ?Sized> Serializer for ArgsSerializer<'x, 'a, 'f, C> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Impossible<(), Error>;
    type SerializeTuple = SerializeArgs<'x, 'a, 'f, C>;
    type SerializeTupleStruct = Impossible<(), Error>;
    type SerializeTupleVariant = Impossible<(), Error>;
    type SerializeMap = Impossible<(), Error>;
    type SerializeStruct = Impossible<(), Error>;
    type SerializeStructVariant = Impossible<(), Error>;

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Error> {
        if len != self.args.len() {
            return Err(Error::ArityMismatch {
                name: "the argument list".to_owned(),
                expected: self.args.len(),
                actual: len,
            });
        }
        Ok(SerializeArgs {
            ctx: self.ctx,
            args: self.args,
            index: 0,
        })
    }

    fn serialize_unit(self) -> Result<(), Error> {
        // `()` seeds an empty argument list.
        if self.args.is_empty() {
            Ok(())
        } else {
            Err(Error::ArityMismatch {
                name: "the argument list".to_owned(),
                expected: self.args.len(),
                actual: 0,
            })
        }
    }

    fn serialize_newtype_struct<T>(self, _name: &'static str, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        value.serialize(self)
    }

    fn serialize_bool(self, _value: bool) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_i8(self, _value: i8) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_i16(self, _value: i16) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_i32(self, _value: i32) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_i64(self, _value: i64) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_u8(self, _value: u8) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_u16(self, _value: u16) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_u32(self, _value: u32) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_u64(self, _value: u64) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_f32(self, _value: f32) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_f64(self, _value: f64) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_char(self, _value: char) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_str(self, _value: &str) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_none(self) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_some<T>(self, _value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        Err(Self::expects_tuple())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<(), Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        Err(Self::expects_tuple())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Err(Self::expects_tuple())
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Self::expects_tuple())
    }
}

/// Writes each tuple element into the next argument slot.
struct SerializeArgs<'x, 'a, 'f, C: FliContext + ?Sized> {
    ctx: &'x C,
    args: &'a TermList<'f>,
    index: usize,
}

impl<'x, 'a, 'f, C: FliContext + ?Sized> SerializeTuple for SerializeArgs<'x, 'a, 'f, C> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Error>
    where
        T: Serialize + ?Sized,
    {
        let slot = self.args.get(self.index);
        value.serialize(TermSerializer {
            ctx: self.ctx,
            term: slot,
            option_allowed: false,
        })?;
        self.index += 1;
        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        if self.index != self.args.len() {
            return Err(Error::ArityMismatch {
                name: "the argument list".to_owned(),
                expected: self.args.len(),
                actual: self.index,
            });
        }
        Ok(())
    }
}
