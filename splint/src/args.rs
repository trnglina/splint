use std::marker::PhantomData;

use crate::handles::HandleError;
use crate::query::QueryError;
use crate::term::{FliContext, Term, TermError, TermList};
use crate::ScopedCallError;

/// An error from preparing or reading a predicate argument.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgumentError {
    /// A term allocation or copy failed.
    #[error(transparent)]
    Term(#[from] TermError),
    /// Converting between a Rust value and a Prolog term failed.
    #[error(transparent)]
    Codec(#[from] crate::TermCodecError),
}

/// An error from preparing arguments or executing a prepared predicate call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// Resolving a predicate or another global handle failed.
    #[error(transparent)]
    Handle(#[from] HandleError),
    /// Allocating or populating the argument block failed.
    #[error("preparing predicate arguments failed: {0}")]
    Arguments(#[source] ArgumentError),
    /// Opening, advancing, or ending the query failed.
    #[error(transparent)]
    Query(#[from] QueryError),
    /// Reading the final argument bindings failed.
    #[error("reading predicate results failed: {0}")]
    ResultDecoding(#[source] ArgumentError),
    /// A prepared call made while a query solution was current failed, and
    /// ending the outer query then failed independently.
    #[error("predicate call failed ({call}); cleanup also failed ({cleanup})")]
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
/// the prepared [`Query`](crate::Query) helpers. The type parameter records
/// the argument specification, which determines the successful result tuple.
pub struct Args<'f, Spec> {
    terms: TermList<'f>,
    _spec: PhantomData<fn() -> Spec>,
}

impl<'f, Spec> Args<'f, Spec> {
    pub(crate) fn terms(&self) -> TermList<'f> {
        self.terms
    }
}

pub(crate) mod sealed {
    use super::*;

    pub trait Argument {
        type Value;

        fn seed<Ctx: FliContext + ?Sized>(
            self,
            ctx: &Ctx,
            term: Term<'_>,
        ) -> Result<(), ArgumentError>;

        fn decode<Ctx: FliContext + ?Sized>(
            ctx: &Ctx,
            term: Term<'_>,
        ) -> Result<Self::Value, ArgumentError>;
    }

    pub trait Tuple {}
}

impl sealed::Argument for Term<'_> {
    type Value = ();

    fn seed<Ctx: FliContext + ?Sized>(
        self,
        _ctx: &Ctx,
        term: Term<'_>,
    ) -> Result<(), ArgumentError> {
        term.put_term(self)?;
        Ok(())
    }

    fn decode<Ctx: FliContext + ?Sized>(
        _ctx: &Ctx,
        _term: Term<'_>,
    ) -> Result<Self::Value, ArgumentError> {
        Ok(())
    }
}

/// A sealed tuple of predicate argument specifications.
#[doc(hidden)]
pub trait ArgsSpec: sealed::Tuple {
    type Values;

    #[doc(hidden)]
    const LEN: usize;

    #[doc(hidden)]
    fn seed<Ctx: FliContext + ?Sized>(
        self,
        ctx: &Ctx,
        terms: &TermList<'_>,
    ) -> Result<(), ArgumentError>;

    #[doc(hidden)]
    fn decode<Ctx: FliContext + ?Sized>(
        ctx: &Ctx,
        terms: &TermList<'_>,
    ) -> Result<Self::Values, ArgumentError>;
}

impl sealed::Tuple for () {}

impl ArgsSpec for () {
    type Values = ();

    const LEN: usize = 0;

    fn seed<C: FliContext + ?Sized>(
        self,
        _ctx: &C,
        _terms: &TermList<'_>,
    ) -> Result<(), ArgumentError> {
        Ok(())
    }

    fn decode<C: FliContext + ?Sized>(
        _ctx: &C,
        _terms: &TermList<'_>,
    ) -> Result<Self::Values, ArgumentError> {
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
            ) -> Result<(), ArgumentError> {
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
            ) -> Result<Self::Values, ArgumentError> {
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
        .map_err(ArgumentError::from)
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
    fn scoped_cleanup_failures_preserve_the_call_error() {
        let error = CallError::from_scoped(ScopedCallError::BodyAndCleanup {
            body: CallError::Arguments(ArgumentError::Term(TermError::TypeMismatch {
                expected: "test value",
            })),
            cleanup: QueryError::OpenFailed,
        });

        assert!(matches!(
            error,
            CallError::CallAndCleanup { call, cleanup: QueryError::OpenFailed }
                if matches!(
                    *call,
                    CallError::Arguments(ArgumentError::Term(TermError::TypeMismatch {
                        expected: "test value"
                    }))
                )
        ));
    }
}
