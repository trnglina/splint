/// An error from a closure-based lifecycle helper.
///
/// `Operation` is the error type of the managed resource itself (for example
/// [`AttachError`](crate::AttachError), [`FrameError`](crate::FrameError), or
/// [`QueryError`](crate::QueryError)); `Body` is the error returned by the
/// user closure.
#[derive(Debug, thiserror::Error)]
pub enum ScopedCallError<Operation, Body> {
    /// Opening, advancing, or finalizing the managed resource failed.
    #[error("managed operation failed: {0}")]
    Operation(Operation),
    /// The user closure failed. The managed resource was rolled back.
    #[error("scoped body failed: {0}")]
    Body(Body),
    /// An operation failed and rolling the resource back also failed.
    #[error("managed operation failed ({operation}); cleanup also failed ({cleanup})")]
    OperationAndCleanup {
        operation: Operation,
        cleanup: Operation,
    },
    /// The user closure failed and rolling the resource back also failed.
    #[error("scoped body failed ({body}); cleanup also failed ({cleanup})")]
    BodyAndCleanup { body: Body, cleanup: Operation },
}
