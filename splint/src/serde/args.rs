use std::marker::PhantomData;

use ::serde::de::DeserializeOwned;
use ::serde::Serialize;

use super::{from_term, to_term, Error};
use crate::handles::HandleError;
use crate::query::QueryError;
use crate::term::{FliContext, Term, TermList};
use crate::ScopedCallError;

/// An error from preparing arguments or executing a typed predicate call.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// Resolving a predicate or another global handle failed.
    #[error(transparent)]
    Handle(#[from] HandleError),
    /// Allocating or serializing the argument block failed.
    #[error("preparing predicate arguments failed: {0}")]
    Arguments(#[source] Error),
    /// Opening, advancing, or ending the query failed.
    #[error(transparent)]
    Query(#[from] QueryError),
    /// Reading the final argument bindings failed.
    #[error("decoding predicate results failed: {0}")]
    ResultDecoding(#[source] Error),
    /// A typed call made while a query solution was current failed, and
    /// ending the outer query then failed independently.
    #[error("typed call failed ({call}); cleanup also failed ({cleanup})")]
    CallAndCleanup {
        call: Box<CallError>,
        cleanup: QueryError,
    },
}

impl CallError {
    pub(crate) fn from_scoped(error: ScopedCallError<QueryError, CallError>) -> CallError {
        match error {
            ScopedCallError::Operation(error) => CallError::Query(error),
            ScopedCallError::Body(error) => error,
            ScopedCallError::BodyAndCleanup { body, cleanup } => CallError::CallAndCleanup {
                call: Box::new(body),
                cleanup,
            },
        }
    }
}

/// A prepared, contiguous predicate argument block.
///
/// Values of this type are created by [`FliContext::args`] and consumed by
/// the typed [`Query`](crate::Query) helpers. The type parameter records the
/// argument specification, which determines the successful result tuple.
pub struct Args<'f, Spec> {
    terms: TermList<'f>,
    _spec: PhantomData<fn() -> Spec>,
}

impl<'f, Spec> Args<'f, Spec> {
    pub(crate) fn terms(&self) -> TermList<'f> {
        self.terms
    }
}

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

/// Adapts an existing [`Term`] for a typed predicate call.
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

mod sealed {
    use super::*;

    pub trait Argument {
        type Value;

        fn seed<Ctx: FliContext + ?Sized>(self, ctx: &Ctx, term: Term<'_>) -> Result<(), Error>;

        fn decode<Ctx: FliContext + ?Sized>(
            ctx: &Ctx,
            term: Term<'_>,
        ) -> Result<Self::Value, Error>;
    }

    pub trait Tuple {}
}

impl<T> sealed::Argument for Input<T>
where
    T: Serialize + DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(self, ctx: &Ctx, term: Term<'_>) -> Result<(), Error> {
        to_term(ctx, term, &self.value)
    }

    fn decode<Ctx: FliContext + ?Sized>(ctx: &Ctx, term: Term<'_>) -> Result<Self::Value, Error> {
        from_term(ctx, term)
    }
}

impl<I, O> sealed::Argument for InputAs<I, O>
where
    I: Serialize,
    O: DeserializeOwned,
{
    type Value = O;

    fn seed<Ctx: FliContext + ?Sized>(self, ctx: &Ctx, term: Term<'_>) -> Result<(), Error> {
        to_term(ctx, term, &self.value)
    }

    fn decode<Ctx: FliContext + ?Sized>(ctx: &Ctx, term: Term<'_>) -> Result<Self::Value, Error> {
        from_term(ctx, term)
    }
}

impl<T> sealed::Argument for Output<T>
where
    T: DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(self, _ctx: &Ctx, _term: Term<'_>) -> Result<(), Error> {
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(ctx: &Ctx, term: Term<'_>) -> Result<Self::Value, Error> {
        from_term(ctx, term)
    }
}

impl<T> sealed::Argument for TermArg<'_, T>
where
    T: DeserializeOwned,
{
    type Value = T;

    fn seed<Ctx: FliContext + ?Sized>(self, _ctx: &Ctx, term: Term<'_>) -> Result<(), Error> {
        term.put_term(self.term)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(ctx: &Ctx, term: Term<'_>) -> Result<Self::Value, Error> {
        from_term(ctx, term)
    }
}

impl sealed::Argument for Term<'_> {
    type Value = ();

    fn seed<Ctx: FliContext + ?Sized>(self, _ctx: &Ctx, term: Term<'_>) -> Result<(), Error> {
        term.put_term(self)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(_ctx: &Ctx, _term: Term<'_>) -> Result<Self::Value, Error> {
        Ok(())
    }
}

/// A sealed tuple of typed predicate argument specifications.
#[doc(hidden)]
pub trait ArgsSpec: sealed::Tuple {
    type Values;

    #[doc(hidden)]
    const LEN: usize;

    #[doc(hidden)]
    fn seed<Ctx: FliContext + ?Sized>(self, ctx: &Ctx, terms: &TermList<'_>) -> Result<(), Error>;

    #[doc(hidden)]
    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        terms: &TermList<'_>,
    ) -> Result<Self::Values, Error>;
}

impl sealed::Tuple for () {}

impl ArgsSpec for () {
    type Values = ();

    const LEN: usize = 0;

    fn seed<C: FliContext + ?Sized>(self, _ctx: &C, _terms: &TermList<'_>) -> Result<(), Error> {
        Ok(())
    }

    fn decode<C: FliContext + ?Sized>(
        _ctx: &C,
        _terms: &TermList<'_>,
    ) -> Result<Self::Values, Error> {
        Ok(())
    }
}

macro_rules! impl_args_spec {
    ($len:expr; $(($name:ident, $index:expr)),+ $(,)?) => {
        impl<$($name),+> sealed::Tuple for ($($name,)+)
        where
            $($name: sealed::Argument,)+
        {}

        impl<$($name),+> ArgsSpec for ($($name,)+)
        where
            $($name: sealed::Argument,)+
        {
            type Values = ($($name::Value,)+);

            const LEN: usize = $len;

            #[allow(non_snake_case)]
            fn seed<Ctx: FliContext + ?Sized>(
                self,
                ctx: &Ctx,
                terms: &TermList<'_>,
            ) -> Result<(), Error> {
                let ($($name,)+) = self;
                $(
                    $name.seed(
                        ctx,
                        terms
                            .get($index)
                            .expect("splint: an argument tuple index must be in bounds"),
                    )?;
                )+
                Ok(())
            }

            fn decode<Ctx: FliContext + ?Sized>(
                ctx: &Ctx,
                terms: &TermList<'_>,
            ) -> Result<Self::Values, Error> {
                Ok((
                    $(
                        <$name as sealed::Argument>::decode(
                            ctx,
                            terms
                                .get($index)
                                .expect("splint: an argument tuple index must be in bounds"),
                        )?,
                    )+
                ))
            }
        }
    };
}

impl_args_spec!(1; (A, 0));
impl_args_spec!(2; (A, 0), (B, 1));
impl_args_spec!(3; (A, 0), (B, 1), (C, 2));
impl_args_spec!(4; (A, 0), (B, 1), (C, 2), (D, 3));
impl_args_spec!(5; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4));
impl_args_spec!(6; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5));
impl_args_spec!(7; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6));
impl_args_spec!(8; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7));
impl_args_spec!(9; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8));
impl_args_spec!(10; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9));
impl_args_spec!(11; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10));
impl_args_spec!(12; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10), (L, 11));
impl_args_spec!(13; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10), (L, 11), (M, 12));
impl_args_spec!(14; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10), (L, 11), (M, 12), (N, 13));
impl_args_spec!(15; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10), (L, 11), (M, 12), (N, 13), (O, 14));
impl_args_spec!(16; (A, 0), (B, 1), (C, 2), (D, 3), (E, 4), (F, 5), (G, 6), (H, 7), (I, 8), (J, 9), (K, 10), (L, 11), (M, 12), (N, 13), (O, 14), (P, 15));

pub(crate) fn prepare_args<C, S>(ctx: &C, spec: S) -> Result<Args<'_, S>, CallError>
where
    C: FliContext + ?Sized,
    S: ArgsSpec,
{
    let terms = ctx
        .terms(S::LEN)
        .map_err(Error::from)
        .map_err(CallError::Arguments)?;
    spec.seed(ctx, &terms).map_err(CallError::Arguments)?;
    Ok(Args {
        terms,
        _spec: PhantomData,
    })
}

pub(crate) fn decode_args<C, S>(ctx: &C, terms: &TermList<'_>) -> Result<S::Values, CallError>
where
    C: FliContext + ?Sized,
    S: ArgsSpec,
{
    S::decode(ctx, terms).map_err(CallError::ResultDecoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_cleanup_failures_preserve_the_typed_call_error() {
        let error = CallError::from_scoped(ScopedCallError::BodyAndCleanup {
            body: CallError::ResultDecoding(Error::Message("bad result".to_owned())),
            cleanup: QueryError::OpenFailed,
        });

        assert!(matches!(
            error,
            CallError::CallAndCleanup { call, cleanup: QueryError::OpenFailed }
                if matches!(*call, CallError::ResultDecoding(Error::Message(ref message))
                    if message == "bad result")
        ));
    }
}
