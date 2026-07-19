use std::sync::LazyLock;

use splint::{Engine, EngineAttributes, FliContext, Record, Runtime};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

/// Runs `body` with a fresh engine attached to the calling thread, returning
/// whatever it produces. Used to let a [`Record`] escape the engine that made
/// it: the record borrows the [`Runtime`], not the engine, so returning it is
/// sound.
fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    let ctx = engine.attach().expect("attach failed");
    body(&ctx)
}

#[test]
fn record_survives_its_frame() {
    with_engine(|ctx| {
        let record = {
            let frame = ctx.frame().unwrap();
            let term = frame.term().unwrap();
            term.put_term_from_text("foo(bar, 42)").unwrap();
            let record = term.record(&RT).unwrap();
            frame.close();
            record
        };

        // The originating frame is gone; the recorded value is intact.
        let frame = ctx.frame().unwrap();
        let recalled = record.recall(&frame).unwrap();
        assert_eq!(recalled.write_to_string().unwrap(), "foo(bar,42)");
        frame.close();
    });
}

#[test]
fn record_recalls_into_an_existing_term() {
    with_engine(|ctx| {
        let record = {
            let frame = ctx.frame().unwrap();
            let term = frame.term().unwrap();
            term.put_i64(123).unwrap();
            let record = term.record(&RT).unwrap();
            frame.close();
            record
        };

        let frame = ctx.frame().unwrap();
        // A pre-existing slot: recall overwrites it in place, then it unifies.
        let slot = frame.term().unwrap();
        assert!(slot.is_variable());
        record.recall_into(slot).unwrap();
        assert_eq!(slot.get_i64().unwrap(), 123);
        frame.close();
    });
}

#[test]
fn record_is_engine_independent() {
    // Record on one engine...
    let record: Record<'static> = with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let term = frame.term().unwrap();
        term.put_i64(99).unwrap();
        let record = term.record(&RT).unwrap();
        frame.close();
        record
    });

    // ...and recall it on a different engine created later.
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let recalled = record.recall(&frame).unwrap();
        assert_eq!(recalled.get_i64().unwrap(), 99);
        frame.close();
    });
}

#[test]
fn record_drops_without_an_engine_attached() {
    // Force RT to exist so a record can be made, then drop the record on this
    // harness thread — which has no crate-managed engine attached once
    // `with_engine` returns. Exercises PL_erase's engine-independence.
    let record: Record<'static> = with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let term = frame.term().unwrap();
        term.put_i64(7).unwrap();
        let record = term.record(&RT).unwrap();
        frame.close();
        record
    });
    drop(record);
}

#[test]
fn record_moves_across_threads_and_recalls() {
    let record: Record<'static> = with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let term = frame.term().unwrap();
        term.put_term_from_text("shared(data)").unwrap();
        let record = term.record(&RT).unwrap();
        frame.close();
        record
    });

    std::thread::scope(|scope| {
        scope.spawn(move || {
            // A fresh thread with its own engine recalls the moved record and
            // then drops it here.
            let mut engine =
                Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
            let ctx = engine.attach().expect("attach failed");
            let frame = ctx.frame().unwrap();
            let recalled = record.recall(&frame).unwrap();
            assert_eq!(recalled.write_to_string().unwrap(), "shared(data)");
            frame.close();
        });
    });
}
