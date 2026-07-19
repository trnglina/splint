use std::os::raw::c_int;
use std::sync::LazyLock;

use splint::{Engine, EngineAttributes, InitError, Runtime};

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
    let baseline = runtime.current_engine().map(|e| e.as_ptr() as usize);

    let mut engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");
    let engine_ptr = engine.as_ptr() as usize;
    let guard = engine.attach().expect("attach failed");
    assert_eq!(
        runtime.current_engine().unwrap().as_ptr() as usize,
        engine_ptr,
        "attached engine should be current"
    );
    drop(guard);
    assert_eq!(
        runtime.current_engine().map(|e| e.as_ptr() as usize),
        baseline,
        "baseline engine should be restored after detach"
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
                let engine_ptr = engine.as_ptr() as usize;
                let guard = engine.attach().expect("cross-thread attach failed");
                assert_eq!(
                    runtime.current_engine().unwrap().as_ptr() as usize,
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
    let engine_ptr = engine.as_ptr() as usize;
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
    let baseline = runtime.current_engine().map(|e| e.as_ptr() as usize);

    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let a_ptr = a.as_ptr() as usize;
    let b_ptr = b.as_ptr() as usize;

    let guard_a = a.attach().expect("attach a failed");
    assert_eq!(runtime.current_engine().unwrap().as_ptr() as usize, a_ptr);
    let guard_b = b.attach().expect("attach b failed");
    assert_eq!(runtime.current_engine().unwrap().as_ptr() as usize, b_ptr);
    drop(guard_b);
    assert_eq!(runtime.current_engine().unwrap().as_ptr() as usize, a_ptr);
    drop(guard_a);
    assert_eq!(
        runtime.current_engine().map(|e| e.as_ptr() as usize),
        baseline
    );
}
