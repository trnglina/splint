use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::LazyLock;

use splint::{AttachError, Engine, EngineAttributes, InitError, Runtime};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

/// Once the runtime exists, a second initialization must be refused.
#[test]
fn second_initialize_is_refused() {
    LazyLock::force(&RT);
    assert!(matches!(
        Runtime::initialize(["splint-test"]),
        Err(InitError::AlreadyInitialized)
    ));
}

/// Attaching binds the engine to the calling thread; dropping the guard
/// restores whatever was attached before (here, this thread's baseline).
#[test]
fn attach_guard_restores_previous_engine() {
    let runtime: &Runtime = &RT;
    let baseline = runtime.current_engine().map(|e| e.as_raw() as usize);

    let mut engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");
    let engine_ptr = engine.as_raw() as usize;
    let guard = engine.attach().expect("attach failed");
    assert_eq!(
        runtime.current_engine().unwrap().as_raw() as usize,
        engine_ptr,
        "attached engine should be current"
    );
    drop(guard);
    assert_eq!(
        runtime.current_engine().map(|e| e.as_raw() as usize),
        baseline,
        "baseline engine should be restored after detach"
    );
}

#[test]
fn with_attached_restores_after_return_error_and_panic() {
    let runtime: &Runtime = &RT;
    let baseline = runtime.current_engine().map(|e| e.as_raw() as usize);
    let mut engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");

    let value = engine
        .with_attached(|_| runtime.current_engine().unwrap().as_raw() as usize)
        .expect("attach failed");
    assert_eq!(value, engine.as_raw() as usize);
    assert_eq!(
        runtime.current_engine().map(|e| e.as_raw() as usize),
        baseline
    );

    let error = engine
        .try_with_attached(|_| Err::<(), _>("body failed"))
        .unwrap_err();
    assert!(matches!(
        error,
        splint::ScopedCallError::Body("body failed")
    ));
    assert_eq!(
        runtime.current_engine().map(|e| e.as_raw() as usize),
        baseline
    );

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = engine.with_attached(|_| panic!("body panic"));
    }));
    assert!(result.is_err());
    assert_eq!(
        runtime.current_engine().map(|e| e.as_raw() as usize),
        baseline
    );
}

/// An engine is `Send`: create it here, then attach/use/detach and destroy it
/// on another thread. A fresh worker thread has no engine attached, so the
/// detach leaves it with none.
#[test]
fn engine_is_send_across_threads() {
    let runtime: &Runtime = &RT;
    let engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut engine = engine;
                let engine_ptr = engine.as_raw() as usize;
                let guard = engine.attach().expect("cross-thread attach failed");
                assert_eq!(
                    runtime.current_engine().unwrap().as_raw() as usize,
                    engine_ptr
                );
                drop(guard);
                assert!(
                    runtime.current_engine().is_none(),
                    "worker thread should have no engine after detach"
                );
                // `engine` dropped here: destroyed on a thread other than the
                // one that created it, while unattached.
            })
            .join()
            .unwrap();
    });
}

/// Error-code mapping for an in-use engine. The safe API makes this state
/// unreachable (attach takes `&mut self`), so the second attach uses raw FFI
/// on purpose, only to verify the translation of `PL_ENGINE_INUSE`.
#[test]
fn attaching_an_in_use_engine_reports_inuse() {
    let runtime: &Runtime = &RT;
    let mut engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");
    let engine_ptr = engine.as_raw() as usize;
    let _guard = engine.attach().expect("attach failed");
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut previous: swipl_sys::PL_engine_t = std::ptr::null_mut();
                let rc = unsafe {
                    swipl_sys::PL_set_engine(engine_ptr as swipl_sys::PL_engine_t, &mut previous)
                };
                assert_eq!(
                    rc,
                    swipl_sys::PL_ENGINE_INUSE as c_int,
                    "attaching an attached engine from another thread should be INUSE"
                );
            })
            .join()
            .unwrap();
    });
}

/// Nested attaches of two engines on one thread; LIFO drop restores the
/// chain back down to this thread's baseline: base -> a -> b -> a -> base.
#[test]
fn nested_attaches_restore_in_lifo_order() {
    let runtime: &Runtime = &RT;
    let baseline = runtime.current_engine().map(|e| e.as_raw() as usize);

    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let a_ptr = a.as_raw() as usize;
    let b_ptr = b.as_raw() as usize;

    let guard_a = a.attach().expect("attach a failed");
    assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, a_ptr);
    let guard_b = b.attach_within(&guard_a).expect("attach b failed");
    assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, b_ptr);
    drop(guard_b);
    assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, a_ptr);
    drop(guard_a);
    assert_eq!(
        runtime.current_engine().map(|e| e.as_raw() as usize),
        baseline
    );
}

#[test]
fn with_attached_within_restores_the_outer_engine() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let a_ptr = a.as_raw() as usize;
    let b_ptr = b.as_raw() as usize;

    a.with_attached(|outer| {
        assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, a_ptr);
        let current = b
            .with_attached_within(outer, |_| {
                runtime.current_engine().unwrap().as_raw() as usize
            })
            .expect("nested attach failed");
        assert_eq!(current, b_ptr);
        assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, a_ptr);
    })
    .expect("outer attach failed");
}

/// A second plain attach on a thread that already has a crate-managed engine
/// attached is refused: the guard's drop re-attaches the previous engine,
/// which a plain attach cannot keep alive (E5).
#[test]
fn plain_attach_while_attached_is_refused() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let _guard = a.attach().expect("attach a failed");
    assert!(matches!(b.attach(), Err(AttachError::AlreadyAttached)));
}

/// `attach_within` demands the guard it nests inside be the thread's
/// innermost attachment; nesting inside an already-covered guard would break
/// the LIFO restore chain (E5).
#[test]
fn attach_within_requires_the_innermost_guard() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let mut c = Engine::new(runtime, EngineAttributes::default()).expect("create c failed");
    let guard_a = a.attach().expect("attach a failed");
    let _guard_b = b.attach_within(&guard_a).expect("attach b failed");
    assert!(matches!(
        c.attach_within(&guard_a),
        Err(AttachError::NotInnermost)
    ));
}

/// Dropping an outer guard past a *leaked* inner attachment panics instead
/// of detaching the leaked engine and desynchronizing the activation record
/// from the engine actually attached (E5).
#[test]
fn dropping_past_a_leaked_inner_attachment_panics() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let b_ptr = b.as_raw() as usize;

    let guard_a = a.attach().expect("attach a failed");
    let guard_b = b.attach_within(&guard_a).expect("attach b failed");
    std::mem::forget(guard_b);

    let result = catch_unwind(AssertUnwindSafe(move || drop(guard_a)));
    let message = *result.unwrap_err().downcast::<&str>().unwrap();
    assert!(
        message.contains("leaked inner attachment"),
        "expected the leaked-attachment panic, got: {message}"
    );
    // The leaked attachment is untouched: b is still this thread's engine.
    assert_eq!(runtime.current_engine().unwrap().as_raw() as usize, b_ptr);
    // `b` is destroyed on drop below while attached to this thread, which
    // SWI-Prolog permits from the owning thread.
}
