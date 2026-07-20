use std::marker::PhantomData;

use ::serde::de::DeserializeOwned;
use ::serde::Serialize;

use super::{from_term, to_term};
use crate::args::{sealed, ArgumentError};
use crate::term::{FliContext, Term};

/// A serializable input argument whose final binding is decoded as `T`.
pub struct Input<T> {
    value: T,
}

/// A serializable input argument decoded after the call as a different type.
pub struct InputAs<I, O> {
    value: I,
    _output: PhantomData<fn() -> O>,
}

/// A fresh unbound argument decoded after the call as `T`.
pub struct Output<T> {
    _output: PhantomData<fn() -> T>,
}

/// Marks `value` as an input and decodes its final binding as the same type.
pub fn input<T>(value: T) -> Input<T> {
    Input { value }
}

/// Marks `value` as an input and decodes its final binding as `O`.
///
/// This is useful when the convenient serialized representation is borrowed
/// but the result must be owned, for example
/// `input_as::<String, _>("hello")`.
pub fn input_as<O, I>(value: I) -> InputAs<I, O> {
    InputAs {
        value,
        _output: PhantomData,
    }
}

/// Creates a fresh unbound argument decoded after the call as `T`.
pub fn output<T>() -> Output<T> {
    Output {
        _output: PhantomData,
    }
}

/// Adapts an existing [`Term`] for a prepared predicate call.
///
/// The term is passed without modification and its final binding is decoded
/// as `T`. Values of this type are created by [`Term::as_arg`].
pub struct TermArg<'f, T> {
    term: Term<'f>,
    _value: PhantomData<fn() -> T>,
}

impl<T> Clone for TermArg<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for TermArg<'_, T> {}

impl<'f, T> TermArg<'f, T> {
    pub(crate) fn new(term: Term<'f>) -> Self {
        Self {
            term,
            _value: PhantomData,
        }
    }
}

impl<T> sealed::Argument for Input<T>
where
    T: Serialize + DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(
        self,
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<(), ArgumentError> {
        to_term(ctx, term, &self.value)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<Self::Value, ArgumentError> {
        Ok(from_term(ctx, term)?)
    }
}

impl<I, O> sealed::Argument for InputAs<I, O>
where
    I: Serialize,
    O: DeserializeOwned,
{
    type Value = O;

    fn seed<Ctx: FliContext + ?Sized>(
        self,
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<(), ArgumentError> {
        to_term(ctx, term, &self.value)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<Self::Value, ArgumentError> {
        Ok(from_term(ctx, term)?)
    }
}

impl<T> sealed::Argument for Output<T>
where
    T: DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(
        self,
        _ctx: &Ctx,
        _term: Term<'_>,
    ) -> Result<(), ArgumentError> {
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<Self::Value, ArgumentError> {
        Ok(from_term(ctx, term)?)
    }
}

impl<T> sealed::Argument for TermArg<'_, T>
where
    T: DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(
        self,
        _ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<(), ArgumentError> {
        term.put_term(self.term)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<Self::Value, ArgumentError> {
        Ok(from_term(ctx, term)?)
    }
}
