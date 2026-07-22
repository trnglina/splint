//! Direct, type-directed conversion between Rust values and Prolog terms.
//!
//! The `derive` feature (enabled by default) provides derives with a separate
//! `#[splint(...)]` configuration namespace:
//!
//! ```
//! use splint::{FromTerm, ToTerm};
//!
//! #[derive(ToTerm, FromTerm)]
//! struct Payload<T> {
//!     #[splint(rename = "request_id")]
//!     id: u64,
//!     value: T,
//! }
//! ```
//!
//! Named structs map to tagged dicts, tuple structs and externally tagged
//! tuple variants to compounds, sequences to proper lists, and maps to dicts.
//! `#[splint(untagged)]`, `#[splint(tag = "...")]`, adjacent
//! `content = "..."`, `flatten`, `rename`, `rename_all`, `default`, and
//! directional skip attributes are supported. Unlike Serde's generic data
//! model, decoding always retains the original live subterm.
//!
//! Derived implementations support finite recursive values through indirection
//! such as `Box`, `Vec`, and acyclic `Arc` values. Decoding creates fresh Rust
//! allocations and does not preserve graph identity. Keep cyclic Prolog terms
//! in an [`ExternalRecord`] rather than decoding them into a recursive Rust
//! type.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;

use crate::{
    Atom, ExternalRecord, FliContext, Functor, HandleError, Record, RecordError, Term, TermError,
};

/// An error converting a Rust value to or from a Prolog term.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TermCodecError {
    #[error(transparent)]
    Term(#[from] TermError),
    #[error(transparent)]
    Handle(#[from] HandleError),
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error("expected {expected}")]
    Type { expected: &'static str },
    #[error("expected {expected_name}/{expected_arity}, found {actual_name}/{actual_arity}")]
    Functor {
        expected_name: String,
        expected_arity: usize,
        actual_name: String,
        actual_arity: usize,
    },
    #[error("expected a dict tagged {expected:?}, found {actual:?}")]
    DictTag {
        expected: String,
        actual: Option<String>,
    },
    #[error("missing field {field:?}")]
    MissingField { field: String },
    #[error("duplicate field {field:?}")]
    DuplicateField { field: String },
    #[error("unknown variant {variant:?}")]
    UnknownVariant { variant: String },
    #[error("Option and unit values are only valid as struct fields or map values")]
    OptionOutsideField,
    #[error("expected {expected} item(s), found {actual}")]
    ArityMismatch { expected: usize, actual: usize },
    #[error("{0}")]
    Message(String),
}

/// Writes a Rust value into a Prolog term.
pub trait ToTerm {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError>;

    #[doc(hidden)]
    fn __to_field<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<bool, TermCodecError> {
        self.to_term(ctx, term)?;
        Ok(true)
    }
}

/// Reads an owned Rust value from a Prolog term.
pub trait FromTerm: Sized {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError>;

    #[doc(hidden)]
    fn __from_field<C: FliContext + ?Sized>(
        ctx: &C,
        term: Option<Term<'_>>,
        name: &str,
    ) -> Result<Self, TermCodecError> {
        match term {
            Some(term) => Self::from_term(ctx, term),
            None => Err(TermCodecError::MissingField {
                field: name.to_owned(),
            }),
        }
    }
}

/// Writes a tuple into an existing contiguous argument block.
pub trait ToTerms {
    const LEN: usize;
    fn to_terms<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        terms: &crate::TermList<'_>,
    ) -> Result<(), TermCodecError>;
}

/// Reads a tuple from an existing contiguous argument block.
pub trait FromTerms: Sized {
    const LEN: usize;
    fn from_terms<C: FliContext + ?Sized>(
        ctx: &C,
        terms: &crate::TermList<'_>,
    ) -> Result<Self, TermCodecError>;
}

pub fn to_term<C, T>(ctx: &C, term: Term<'_>, value: &T) -> Result<(), TermCodecError>
where
    C: FliContext + ?Sized,
    T: ToTerm + ?Sized,
{
    value.to_term(ctx, term)
}

pub fn from_term<C, T>(ctx: &C, term: Term<'_>) -> Result<T, TermCodecError>
where
    C: FliContext + ?Sized,
    T: FromTerm,
{
    T::from_term(ctx, term)
}

pub fn to_terms<C, T>(ctx: &C, terms: &crate::TermList<'_>, value: &T) -> Result<(), TermCodecError>
where
    C: FliContext + ?Sized,
    T: ToTerms + ?Sized,
{
    if terms.len() != T::LEN {
        return Err(TermCodecError::ArityMismatch {
            expected: terms.len(),
            actual: T::LEN,
        });
    }
    value.to_terms(ctx, terms)
}

pub fn from_terms<C, T>(ctx: &C, terms: &crate::TermList<'_>) -> Result<T, TermCodecError>
where
    C: FliContext + ?Sized,
    T: FromTerms,
{
    if terms.len() != T::LEN {
        return Err(TermCodecError::ArityMismatch {
            expected: T::LEN,
            actual: terms.len(),
        });
    }
    T::from_terms(ctx, terms)
}

#[doc(hidden)]
pub trait ToTermFields {
    fn __to_fields<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<(String, Term<'a>)>, TermCodecError>;
}

#[doc(hidden)]
pub trait FromTermFields: Sized {
    fn __from_fields<'a, C: FliContext + ?Sized>(
        ctx: &'a C,
        fields: &mut BTreeMap<String, Term<'a>>,
    ) -> Result<Self, TermCodecError>;
}

#[doc(hidden)]
pub fn put_dict<C: FliContext + ?Sized>(
    ctx: &C,
    dest: Term<'_>,
    tag: &str,
    fields: Vec<(String, Term<'_>)>,
) -> Result<(), TermCodecError> {
    let mut seen = std::collections::HashSet::with_capacity(fields.len());
    for (key, _) in &fields {
        if !seen.insert(key.clone()) {
            return Err(TermCodecError::DuplicateField { field: key.clone() });
        }
    }
    let values = ctx.terms(fields.len())?;
    let keys: Vec<Atom> = fields.iter().map(|(key, _)| Atom::new(ctx, key)).collect();
    for (index, (_, value)) in fields.iter().enumerate() {
        values
            .get(index)
            .expect("field count matches")
            .put_term(*value)?;
    }
    let key_refs: Vec<&Atom> = keys.iter().collect();
    dest.put_dict(&Atom::new(ctx, tag), &key_refs, &values)?;
    Ok(())
}

#[doc(hidden)]
pub fn dict_fields<'a, C: FliContext + ?Sized>(
    ctx: &'a C,
    term: Term<'_>,
) -> Result<BTreeMap<String, Term<'a>>, TermCodecError> {
    let mut result = BTreeMap::new();
    for (key, value) in term.dict_entries(ctx)? {
        let key = match key {
            crate::DictKey::Atom(atom) => atom.text(),
            crate::DictKey::Int(value) => value.to_string(),
        };
        result.insert(key, value);
    }
    Ok(result)
}

#[doc(hidden)]
pub fn require_dict_tag<C: FliContext + ?Sized>(
    ctx: &C,
    term: Term<'_>,
    expected: &str,
) -> Result<(), TermCodecError> {
    let actual = term.dict_tag(ctx)?.map(|atom| atom.text());
    if actual.as_deref() == Some(expected) {
        Ok(())
    } else {
        Err(TermCodecError::DictTag {
            expected: expected.to_owned(),
            actual,
        })
    }
}

#[doc(hidden)]
pub fn put_compound<C: FliContext + ?Sized>(
    ctx: &C,
    dest: Term<'_>,
    name: &str,
    values: &[Term<'_>],
) -> Result<(), TermCodecError> {
    if values.is_empty() {
        dest.put_atom_text(name)?;
        return Ok(());
    }
    let args = ctx.terms(values.len())?;
    for (index, value) in values.iter().enumerate() {
        args.get(index)
            .expect("argument count matches")
            .put_term(*value)?;
    }
    let functor = Functor::from_name(ctx, name, values.len())?;
    dest.cons_functor(&functor, &args)?;
    Ok(())
}

#[doc(hidden)]
pub fn put_list_terms<C: FliContext + ?Sized>(
    ctx: &C,
    dest: Term<'_>,
    values: &[Term<'_>],
) -> Result<(), TermCodecError> {
    let mut tail = ctx.term()?;
    tail.put_nil()?;
    for value in values.iter().rev() {
        let cell = ctx.term()?;
        cell.cons_list(*value, tail)?;
        tail = cell;
    }
    dest.put_term(tail)?;
    Ok(())
}

#[doc(hidden)]
pub fn compound_args<'a, C: FliContext + ?Sized>(
    ctx: &'a C,
    term: Term<'_>,
    expected_name: &str,
    expected_arity: usize,
) -> Result<Vec<Term<'a>>, TermCodecError> {
    if expected_arity == 0 && term.kind() == crate::TermKind::Atom {
        let actual_name = term.get_atom()?.text();
        if actual_name == expected_name {
            return Ok(Vec::new());
        }
        return Err(TermCodecError::Functor {
            expected_name: expected_name.to_owned(),
            expected_arity,
            actual_name,
            actual_arity: 0,
        });
    }
    let (name, arity) = term.name_arity()?;
    let actual_name = name.text();
    if actual_name != expected_name || arity != expected_arity {
        return Err(TermCodecError::Functor {
            expected_name: expected_name.to_owned(),
            expected_arity,
            actual_name,
            actual_arity: arity,
        });
    }
    (0..arity)
        .map(|index| term.get_arg(ctx, index).map_err(Into::into))
        .collect()
}

fn write_list<C: FliContext + ?Sized, T: ToTerm>(
    ctx: &C,
    dest: Term<'_>,
    values: impl IntoIterator<Item = T>,
) -> Result<(), TermCodecError> {
    let mut elements = Vec::new();
    for value in values {
        let term = ctx.term()?;
        value.to_term(ctx, term)?;
        elements.push(term);
    }
    let mut tail = ctx.term()?;
    tail.put_nil()?;
    for element in elements.into_iter().rev() {
        let cell = ctx.term()?;
        cell.cons_list(element, tail)?;
        tail = cell;
    }
    dest.put_term(tail)?;
    Ok(())
}

impl<T: ToTerm + ?Sized> ToTerm for &T {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        (*self).to_term(ctx, term)
    }
    fn __to_field<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<bool, TermCodecError> {
        (*self).__to_field(ctx, term)
    }
}

impl<T: ToTerm + ?Sized> ToTerm for Box<T> {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        (**self).to_term(ctx, term)
    }
}
impl<T: FromTerm> FromTerm for Box<T> {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(Box::new(T::from_term(ctx, term)?))
    }
}
impl<T: ToTerm + ?Sized> ToTerm for Arc<T> {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        (**self).to_term(ctx, term)
    }
}
impl<T: FromTerm> FromTerm for Arc<T> {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(Arc::new(T::from_term(ctx, term)?))
    }
}

impl ToTerm for bool {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term.put_bool(*self)?)
    }
}
impl FromTerm for bool {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(term.get_bool()?)
    }
}

macro_rules! signed {
    ($($ty:ty),* $(,)?) => {$(
        impl ToTerm for $ty { fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> { Ok(term.put_i64((*self).into())?) } }
        impl FromTerm for $ty { fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> { <$ty>::try_from(term.get_i64()?).map_err(|_| TermCodecError::Type { expected: stringify!($ty) }) } }
    )*};
}
signed!(i8, i16, i32, i64);
impl ToTerm for isize {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term
            .put_i64(i64::try_from(*self).map_err(|_| TermCodecError::Type { expected: "i64" })?)?)
    }
}
impl FromTerm for isize {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        isize::try_from(term.get_i64()?).map_err(|_| TermCodecError::Type { expected: "isize" })
    }
}

macro_rules! unsigned {
    ($($ty:ty),* $(,)?) => {$(
        impl ToTerm for $ty { fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> { Ok(term.put_u64((*self).into())?) } }
        impl FromTerm for $ty { fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> { <$ty>::try_from(term.get_u64()?).map_err(|_| TermCodecError::Type { expected: stringify!($ty) }) } }
    )*};
}
unsigned!(u8, u16, u32, u64);
impl ToTerm for usize {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term
            .put_u64(u64::try_from(*self).map_err(|_| TermCodecError::Type { expected: "u64" })?)?)
    }
}
impl FromTerm for usize {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        usize::try_from(term.get_u64()?).map_err(|_| TermCodecError::Type { expected: "usize" })
    }
}

impl ToTerm for f32 {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term.put_f64((*self).into())?)
    }
}
impl FromTerm for f32 {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(term.get_f64()? as f32)
    }
}
impl ToTerm for f64 {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term.put_f64(*self)?)
    }
}
impl FromTerm for f64 {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(term.get_f64()?)
    }
}

impl ToTerm for str {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(term.put_string(self)?)
    }
}
impl ToTerm for String {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        self.as_str().to_term(ctx, term)
    }
}
impl FromTerm for String {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(term.get_text()?)
    }
}
impl ToTerm for char {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        let mut b = [0; 4];
        Ok(term.put_string(self.encode_utf8(&mut b))?)
    }
}
impl FromTerm for char {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        let s = term.get_text()?;
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(c),
            _ => Err(TermCodecError::Type {
                expected: "a character",
            }),
        }
    }
}

impl ToTerm for () {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, _: Term<'_>) -> Result<(), TermCodecError> {
        Err(TermCodecError::OptionOutsideField)
    }
    fn __to_field<C: FliContext + ?Sized>(
        &self,
        _: &C,
        _: Term<'_>,
    ) -> Result<bool, TermCodecError> {
        Ok(false)
    }
}
impl FromTerm for () {
    fn from_term<C: FliContext + ?Sized>(_: &C, _: Term<'_>) -> Result<Self, TermCodecError> {
        Err(TermCodecError::OptionOutsideField)
    }
    fn __from_field<C: FliContext + ?Sized>(
        _: &C,
        _: Option<Term<'_>>,
        _: &str,
    ) -> Result<Self, TermCodecError> {
        Ok(())
    }
}

impl<T: ToTerm> ToTerm for Option<T> {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, _: Term<'_>) -> Result<(), TermCodecError> {
        Err(TermCodecError::OptionOutsideField)
    }
    fn __to_field<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<bool, TermCodecError> {
        match self {
            Some(value) => {
                value.to_term(ctx, term)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}
impl<T: FromTerm> FromTerm for Option<T> {
    fn from_term<C: FliContext + ?Sized>(_: &C, _: Term<'_>) -> Result<Self, TermCodecError> {
        Err(TermCodecError::OptionOutsideField)
    }
    fn __from_field<C: FliContext + ?Sized>(
        ctx: &C,
        term: Option<Term<'_>>,
        _: &str,
    ) -> Result<Self, TermCodecError> {
        term.map(|t| T::from_term(ctx, t)).transpose()
    }
}

impl<T: ToTerm> ToTerm for Vec<T> {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        write_list(ctx, term, self.iter())
    }
}
impl<T: FromTerm> FromTerm for Vec<T> {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        term.collect_list(ctx)?
            .into_iter()
            .map(|t| T::from_term(ctx, t))
            .collect()
    }
}
impl<T: ToTerm> ToTerm for [T] {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        write_list(ctx, term, self.iter())
    }
}
impl<T: ToTerm, const N: usize> ToTerm for [T; N] {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        self.as_slice().to_term(ctx, term)
    }
}
impl<T: FromTerm, const N: usize> FromTerm for [T; N] {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        let values = Vec::<T>::from_term(ctx, term)?;
        let actual = values.len();
        values
            .try_into()
            .map_err(|_| TermCodecError::ArityMismatch {
                expected: N,
                actual,
            })
    }
}

impl ToTerm for ExternalRecord {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(self.recall_into(term)?)
    }
}
impl FromTerm for ExternalRecord {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(Self::from_term(term)?)
    }
}

impl ToTerm for Record {
    fn to_term<C: FliContext + ?Sized>(&self, _: &C, term: Term<'_>) -> Result<(), TermCodecError> {
        Ok(self.recall_into(term)?)
    }
}
impl FromTerm for Record {
    fn from_term<C: FliContext + ?Sized>(_: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        Ok(term.record()?)
    }
}

#[doc(hidden)]
pub trait TermMapKey: Sized {
    fn encode_key(&self) -> Result<String, TermCodecError>;
    fn decode_key(value: &str) -> Result<Self, TermCodecError>;
}
impl TermMapKey for String {
    fn encode_key(&self) -> Result<String, TermCodecError> {
        Ok(self.clone())
    }
    fn decode_key(v: &str) -> Result<Self, TermCodecError> {
        Ok(v.to_owned())
    }
}
impl TermMapKey for bool {
    fn encode_key(&self) -> Result<String, TermCodecError> {
        Ok(self.to_string())
    }
    fn decode_key(v: &str) -> Result<Self, TermCodecError> {
        v.parse().map_err(|_| TermCodecError::Type {
            expected: "a boolean dict key",
        })
    }
}
macro_rules! map_key_num { ($($ty:ty),*) => {$(
    impl TermMapKey for $ty { fn encode_key(&self)->Result<String,TermCodecError>{Ok(self.to_string())} fn decode_key(v:&str)->Result<Self,TermCodecError>{v.parse().map_err(|_|TermCodecError::Type{expected:"a numeric dict key"})} }
)*}; }
map_key_num!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl<K: TermMapKey + Ord, V: ToTerm> ToTermFields for BTreeMap<K, V> {
    fn __to_fields<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<(String, Term<'a>)>, TermCodecError> {
        self.iter()
            .map(|(k, v)| {
                let term = ctx.term()?;
                v.to_term(ctx, term)?;
                Ok((k.encode_key()?, term))
            })
            .collect()
    }
}
impl<K: TermMapKey + Ord, V: ToTerm> ToTerm for BTreeMap<K, V> {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        put_dict(ctx, term, "#", self.__to_fields(ctx)?)
    }
}
impl<K: TermMapKey + Ord, V: FromTerm> FromTermFields for BTreeMap<K, V> {
    fn __from_fields<'a, C: FliContext + ?Sized>(
        ctx: &'a C,
        fields: &mut BTreeMap<String, Term<'a>>,
    ) -> Result<Self, TermCodecError> {
        std::mem::take(fields)
            .into_iter()
            .map(|(k, t)| Ok((K::decode_key(&k)?, V::from_term(ctx, t)?)))
            .collect()
    }
}
impl<K: TermMapKey + Ord, V: FromTerm> FromTerm for BTreeMap<K, V> {
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        let mut f = dict_fields(ctx, term)?;
        Self::__from_fields(ctx, &mut f)
    }
}

impl<K: TermMapKey + Eq + Hash, V: ToTerm, S: BuildHasher> ToTermFields for HashMap<K, V, S> {
    fn __to_fields<'a, C: FliContext + ?Sized>(
        &self,
        ctx: &'a C,
    ) -> Result<Vec<(String, Term<'a>)>, TermCodecError> {
        self.iter()
            .map(|(k, v)| {
                let term = ctx.term()?;
                v.to_term(ctx, term)?;
                Ok((k.encode_key()?, term))
            })
            .collect()
    }
}
impl<K: TermMapKey + Eq + Hash, V: ToTerm, S: BuildHasher> ToTerm for HashMap<K, V, S> {
    fn to_term<C: FliContext + ?Sized>(
        &self,
        ctx: &C,
        term: Term<'_>,
    ) -> Result<(), TermCodecError> {
        put_dict(ctx, term, "#", self.__to_fields(ctx)?)
    }
}
impl<K: TermMapKey + Eq + Hash, V: FromTerm, S: BuildHasher + Default> FromTermFields
    for HashMap<K, V, S>
{
    fn __from_fields<'a, C: FliContext + ?Sized>(
        ctx: &'a C,
        fields: &mut BTreeMap<String, Term<'a>>,
    ) -> Result<Self, TermCodecError> {
        std::mem::take(fields)
            .into_iter()
            .map(|(k, t)| Ok((K::decode_key(&k)?, V::from_term(ctx, t)?)))
            .collect()
    }
}
impl<K: TermMapKey + Eq + Hash, V: FromTerm, S: BuildHasher + Default> FromTerm
    for HashMap<K, V, S>
{
    fn from_term<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<Self, TermCodecError> {
        let mut f = dict_fields(ctx, term)?;
        Self::__from_fields(ctx, &mut f)
    }
}

macro_rules! tuples {
    ($($len:literal => ($($T:ident:$idx:tt),*)),* $(,)?) => {$(
        impl<$($T:ToTerm),*> ToTerm for ($($T,)*) { fn to_term<Ctx:FliContext+?Sized>(&self,ctx:&Ctx,term:Term<'_>)->Result<(),TermCodecError>{ let mut __items=Vec::new(); $({let __term=ctx.term()?; self.$idx.to_term(ctx,__term)?; __items.push(__term);})* let mut __tail=ctx.term()?;__tail.put_nil()?;for __item in __items.into_iter().rev(){let __cell=ctx.term()?;__cell.cons_list(__item,__tail)?;__tail=__cell;}term.put_term(__tail)?;Ok(()) } }
        impl<$($T:FromTerm),*> FromTerm for ($($T,)*) { fn from_term<Ctx:FliContext+?Sized>(ctx:&Ctx,term:Term<'_>)->Result<Self,TermCodecError>{ let mut items=term.collect_list(ctx)?.into_iter(); let value=($(<$T>::from_term(ctx,items.next().ok_or(TermCodecError::ArityMismatch{expected:$len,actual:0})?)?,)*); if items.next().is_some(){return Err(TermCodecError::ArityMismatch{expected:$len,actual:$len+1});} Ok(value) } }
        impl<$($T:ToTerm),*> ToTerms for ($($T,)*) { const LEN:usize=$len; fn to_terms<Ctx:FliContext+?Sized>(&self,ctx:&Ctx,terms:&crate::TermList<'_>)->Result<(),TermCodecError>{$((self.$idx).to_term(ctx,terms.get($idx).expect("tuple index"))?;)* Ok(())} }
        impl<$($T:FromTerm),*> FromTerms for ($($T,)*) { const LEN:usize=$len; fn from_terms<Ctx:FliContext+?Sized>(ctx:&Ctx,terms:&crate::TermList<'_>)->Result<Self,TermCodecError>{Ok(($(<$T>::from_term(ctx,terms.get($idx).expect("tuple index"))?,)*))} }
    )*};
}
impl ToTerms for () {
    const LEN: usize = 0;
    fn to_terms<C: FliContext + ?Sized>(
        &self,
        _: &C,
        _: &crate::TermList<'_>,
    ) -> Result<(), TermCodecError> {
        Ok(())
    }
}
impl FromTerms for () {
    const LEN: usize = 0;
    fn from_terms<C: FliContext + ?Sized>(
        _ctx: &C,
        _: &crate::TermList<'_>,
    ) -> Result<Self, TermCodecError> {
        Ok(())
    }
}
tuples!(1=>(A:0),2=>(A:0,B:1),3=>(A:0,B:1,C:2),4=>(A:0,B:1,C:2,D:3),5=>(A:0,B:1,C:2,D:3,E:4),6=>(A:0,B:1,C:2,D:3,E:4,F:5),7=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6),8=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7),9=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8),10=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9),11=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10),12=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11),13=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12),14=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13),15=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13,O:14),16=>(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13,O:14,P:15));
