use std::panic::{catch_unwind, AssertUnwindSafe};

use splint::{from_term, CleanupOptions, Engine, EngineAttributes, FliContext, Record, Runtime};

/// Exercises the session-stamp hardening (RC2) that backstops `Record` once
/// its lifetime can be chosen freely by a deserializing caller: a stale
/// record — one made in a runtime session that has since been cleaned up and
/// replaced — must panic on `recall`/`Clone` and silently no-op on `Drop`,
/// while a same-session record keeps working exactly as before.
///
/// `Record::of`/`Term::record` tie `'rt` to the `&Runtime` borrow passed in,
/// so the borrow checker statically refuses to let a record made that way
/// outlive a `cleanup()` + re-`initialize()` cycle — RC1's guarantee working
/// as intended. The only way to mint a record whose lifetime isn't pinned to
/// a live borrow is [`Deserialize`], which is what actually needs RC2's
/// dynamic check; hence this test requires the `serde` feature to construct
/// the stale scenario at all.
///
/// `Runtime::initialize`/`cleanup` are process-wide singletons, so this lives
/// in its own test binary with a single test function, like `lifecycle.rs`.
#[test]
fn record_session_hardening() {
    // 1. First session: mint a `Record<'static>` via deserialization — the
    // one path that can outlive the runtime borrow that made it.
    let runtime = Runtime::initialize(["splint-test", "-q"]).expect("initial initialize failed");
    let stale: Record<'static> = {
        let mut engine =
            Engine::new(&runtime, EngineAttributes::default()).expect("engine create failed");
        let ctx = engine.attach().expect("attach failed");
        let frame = ctx.frame().unwrap();
        let term = frame.term().unwrap();
        term.put_i64(1).unwrap();
        let record = from_term(&frame, term).unwrap();
        frame.close();
        record
    };

    // 2. Same-session Clone/recall/Drop still work post-hardening.
    {
        let mut engine =
            Engine::new(&runtime, EngineAttributes::default()).expect("engine create failed");
        let ctx = engine.attach().expect("attach failed");
        let frame = ctx.frame().unwrap();
        let term = frame.term().unwrap();
        term.put_i64(2).unwrap();
        let record: Record<'static> = from_term(&frame, term).unwrap();
        let clone = record.clone();
        let recalled = clone.recall(&frame).unwrap();
        assert_eq!(recalled.get_i64().unwrap(), 2);
        drop(record);
        drop(clone);
        frame.close();
    }

    // 3. Cleanup ends the first session.
    runtime
        .cleanup(CleanupOptions::default())
        .expect("first cleanup failed");

    // 4. Re-initialize: a new session begins.
    let runtime = Runtime::initialize(["splint-test", "-q"]).expect("re-initialize failed");

    // 5. `stale` belongs to the dead session: recall and clone must panic.
    {
        let mut engine =
            Engine::new(&runtime, EngineAttributes::default()).expect("engine create failed");
        let ctx = engine.attach().expect("attach failed");
        let frame = ctx.frame().unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| stale.recall(&frame)));
        assert!(result.is_err(), "recall of a stale record did not panic");
        frame.close();
    }
    {
        let result = catch_unwind(AssertUnwindSafe(|| stale.clone()));
        assert!(result.is_err(), "clone of a stale record did not panic");
    }

    // 6. Dropping the stale record is a silent no-op, not a crash.
    drop(stale);

    // 7. Final cleanup of the second session.
    runtime
        .cleanup(CleanupOptions::default())
        .expect("final cleanup failed");
}
