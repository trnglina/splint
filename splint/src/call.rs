/// An error from a closure-based lifecycle helper.
///
/// `Operation` is the error type of the managed resource itself (for example
/// [`AttachError`](crate::AttachError), [`FrameError`](crate::FrameError), or
/// [`QueryError`](crate::QueryError)); `Body` is the error returned by the
/// user closure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopedCallError<Operation, Body> {
    /// Opening, advancing, or finalizing the managed resource failed.
    #[error("managed operation failed: {0}")]
    Operation(Operation),
    /// The user closure failed and the scope was ended according to the
    /// helper's documented failure semantics.
    #[error("scoped body failed: {0}")]
    Body(Body),
    /// The user closure failed and ending the scope also failed.
    #[error("scoped body failed ({body}); cleanup also failed ({cleanup})")]
    BodyAndCleanup { body: Body, cleanup: Operation },
}
