use std::marker::PhantomData;

use crate::args::{sealed, ArgumentError};
use crate::{FliContext, FromTerm, Term, ToTerm};

/// A [`ToTerm`](crate::ToTerm) input decoded after the call as the same type.
pub struct Input<T> {
    value: T,
}
/// A [`ToTerm`](crate::ToTerm) input decoded after the call as another type.
pub struct InputAs<I, O> {
    value: I,
    _output: PhantomData<fn() -> O>,
}
/// A fresh unbound argument decoded as `T` after a successful call.
pub struct Output<T> {
    _output: PhantomData<fn() -> T>,
}

/// Marks `value` as an input and decodes its final binding as the same type.
pub fn input<T>(value: T) -> Input<T> {
    Input { value }
}
/// Marks `value` as an input and decodes its final binding as `O`.
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

/// An existing [`Term`] passed through and decoded as `T` after the call.
///
/// Values of this type are created by [`Term::as_arg`](crate::Term::as_arg).
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

impl<T: ToTerm + FromTerm> sealed::Argument for Input<T> {
    type Value = T;
    fn seed<C: FliContext + ?Sized>(self, ctx: &C, term: Term<'_>) -> Result<(), ArgumentError> {
        self.value.to_term(ctx, term)?;
        Ok(())
    }
    fn decode<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<T, ArgumentError> {
        Ok(T::from_term(ctx, term)?)
    }
}
impl<I: ToTerm, O: FromTerm> sealed::Argument for InputAs<I, O> {
    type Value = O;
    fn seed<C: FliContext + ?Sized>(self, ctx: &C, term: Term<'_>) -> Result<(), ArgumentError> {
        self.value.to_term(ctx, term)?;
        Ok(())
    }
    fn decode<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<O, ArgumentError> {
        Ok(O::from_term(ctx, term)?)
    }
}
impl<T: FromTerm> sealed::Argument for Output<T> {
    type Value = T;
    fn seed<C: FliContext + ?Sized>(self, _: &C, _: Term<'_>) -> Result<(), ArgumentError> {
        Ok(())
    }
    fn decode<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<T, ArgumentError> {
        Ok(T::from_term(ctx, term)?)
    }
}
impl<T: FromTerm> sealed::Argument for TermArg<'_, T> {
    type Value = T;
    fn seed<C: FliContext + ?Sized>(self, _: &C, term: Term<'_>) -> Result<(), ArgumentError> {
        term.put_term(self.term)?;
        Ok(())
    }
    fn decode<C: FliContext + ?Sized>(ctx: &C, term: Term<'_>) -> Result<T, ArgumentError> {
        Ok(T::from_term(ctx, term)?)
    }
}
