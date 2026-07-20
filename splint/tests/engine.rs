use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::LazyLock;

use splint::{AttachError, Engine, EngineAttributes, FliContext, InitError, Runtime};

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

#[test]
fn with_attached_restores_after_return_error_and_panic() {
    let runtime: &Runtime = &RT;
    let had_engine = runtime.current_engine().is_some();
    let mut engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");

    engine
        .with_attached(|_| assert!(runtime.current_engine().is_some()))
        .expect("attach failed");
    assert_eq!(runtime.current_engine().is_some(), had_engine);

    let error = engine
        .try_with_attached(|_| Err::<(), _>("body failed"))
        .unwrap_err();
    assert!(matches!(
        error,
        splint::ScopedCallError::Body("body failed")
    ));
    assert_eq!(runtime.current_engine().is_some(), had_engine);

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = engine.with_attached(|_| panic!("body panic"));
    }));
    assert!(result.is_err());
    assert_eq!(runtime.current_engine().is_some(), had_engine);
}

/// An engine is `Send`: create it here, then attach, use, detach, and destroy
/// it on another thread.
#[test]
fn engine_is_send_across_threads() {
    let runtime: &Runtime = &RT;
    let engine = Engine::new(runtime, EngineAttributes::default()).expect("create failed");
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let mut engine = engine;
                assert!(runtime.current_engine().is_none());
                engine
                    .with_attached(|_| assert!(runtime.current_engine().is_some()))
                    .expect("cross-thread attach failed");
                assert!(
                    runtime.current_engine().is_none(),
                    "worker thread should have no engine after detach"
                );
            })
            .join()
            .unwrap();
    });
}

#[test]
fn runtime_engine_attaches_persists_and_restores_on_a_worker_thread() {
    let runtime: &Runtime = &RT;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                assert!(runtime.current_engine().is_none());

                {
                    let current = runtime.engine().expect("thread engine attach failed");
                    let term = current.term().expect("term allocation failed");
                    term.put_i64(41).expect("term write failed");

                    let mut temporary =
                        Engine::new(runtime, EngineAttributes::default()).expect("create failed");
                    temporary
                        .with_attached(|ctx| {
                            let temporary_term =
                                ctx.term().expect("temporary term allocation failed");
                            temporary_term
                                .put_atom_text("temporary")
                                .expect("temporary term write failed");
                        })
                        .expect("temporary attach failed");

                    assert_eq!(
                        term.get_i64().expect("persistent engine was not restored"),
                        41
                    );
                }
                assert!(
                    runtime.current_engine().is_some(),
                    "dropping the witness detached the persistent engine"
                );

                let reused = runtime.engine().expect("thread engine reuse failed");
                let reused_term = reused.term().expect("reused term allocation failed");
                reused_term.put_i64(42).expect("reused term write failed");
                assert_eq!(reused_term.get_i64().expect("reused term read failed"), 42);
            })
            .join()
            .unwrap();
    });
}

#[test]
fn nested_attachment_restores_the_outer_engine() {
    let runtime: &Runtime = &RT;
    let had_engine = runtime.current_engine().is_some();
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");

    a.with_attached(|outer| {
        assert!(runtime.current_engine().is_some());
        b.with_attached_within(outer, |_| {
            assert!(runtime.current_engine().is_some());
        })
        .expect("nested attach failed");
        assert!(runtime.current_engine().is_some());
    })
    .expect("outer attach failed");
    assert_eq!(runtime.current_engine().is_some(), had_engine);
}

#[test]
fn plain_nested_attachment_is_refused() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");

    a.with_attached(|_| {
        assert!(matches!(
            b.with_attached(|_| ()),
            Err(AttachError::AlreadyAttached)
        ));
    })
    .expect("attach a failed");
}

#[test]
fn nested_attachment_requires_the_innermost_witness() {
    let runtime: &Runtime = &RT;
    let mut a = Engine::new(runtime, EngineAttributes::default()).expect("create a failed");
    let mut b = Engine::new(runtime, EngineAttributes::default()).expect("create b failed");
    let mut c = Engine::new(runtime, EngineAttributes::default()).expect("create c failed");

    a.with_attached(|outer| {
        b.with_attached_within(outer, |_| {
            assert!(matches!(
                c.with_attached_within(outer, |_| ()),
                Err(AttachError::NotInnermost)
            ));
        })
        .expect("attach b failed");
    })
    .expect("attach a failed");
}
