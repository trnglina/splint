use std::ffi::NulError;
use std::os::raw::c_int;

use thiserror::Error;

use crate::Runtime;

/// Errors from [`Runtime::initialize`] and [`Runtime::initialize_from_state`].
#[derive(Debug, Error)]
pub enum InitError {
    /// A [`Runtime`] already exists in this process.
    #[error("the SWI-Prolog runtime is already initialized in this process")]
    AlreadyInitialized,
    /// `PL_initialise` reported failure (invalid options, missing boot state,
    /// or a resource error).
    #[error("PL_initialise reported failure")]
    InitialiseFailed,
    /// `PL_set_resource_db_mem` rejected the saved-state buffer (not a valid
    /// zip archive produced by `qsave_program/2`).
    #[error("the saved state is not a valid SWI-Prolog resource archive")]
    InvalidSavedState,
    /// An argument contained an interior NUL byte and cannot be passed as a
    /// C string.
    #[error("argument at index {index} contains an interior NUL byte")]
    InvalidArgument {
        index: usize,
        #[source]
        source: NulError,
    },
    /// More arguments were supplied than fit in a C `int`.
    #[error("too many arguments to fit in a C int")]
    TooManyArguments,
}

/// Why a [`Runtime::cleanup`] call did not tear the runtime down.
#[derive(Debug, Error)]
pub enum CleanupErrorKind {
    /// An `at_halt/1` hook vetoed the cleanup (`PL_CLEANUP_CANCELED`). The
    /// runtime remains fully initialized.
    #[error("cleanup was canceled by an at-halt hook")]
    Canceled,
    /// `PL_cleanup` failed (`PL_CLEANUP_FAILED`), e.g. because engines or
    /// threads are still outstanding. The runtime remains initialized.
    #[error("PL_cleanup failed (e.g. outstanding engines or threads)")]
    Failed,
    /// Another cleanup was already in progress (`PL_CLEANUP_RECURSIVE`).
    #[error("a cleanup was already in progress")]
    Recursive,
    /// `PL_cleanup` returned a status code this crate does not know.
    #[error("PL_cleanup returned an unrecognized status code {0}")]
    Unknown(c_int),
}

/// A failed [`Runtime::cleanup`]. Carries the still-valid [`Runtime`] back to
/// the caller: a non-successful cleanup leaves Prolog fully initialized, so
/// the runtime can be kept in use or the cleanup retried.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct CleanupError {
    pub runtime: Runtime,
    pub kind: CleanupErrorKind,
}

/// Errors from [`crate::Engine::new`].
#[derive(Debug, Error)]
pub enum EngineCreateError {
    /// `PL_create_engine` returned NULL (creation failed, e.g. resource
    /// limits).
    #[error("PL_create_engine failed")]
    Failed,
}

/// Errors from [`crate::Engine::attach`].
#[derive(Debug, Error)]
pub enum AttachError {
    /// The engine handle was rejected by SWI-Prolog (`PL_ENGINE_INVAL`).
    #[error("the engine no longer exists")]
    Invalid,
    /// The engine is currently attached to another thread
    /// (`PL_ENGINE_INUSE`).
    #[error("the engine is already attached to another thread")]
    InUse,
    /// `PL_set_engine` returned a status code this crate does not know.
    #[error("PL_set_engine returned an unrecognized status code {0}")]
    Unknown(c_int),
}
