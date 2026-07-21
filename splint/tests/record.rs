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
    engine.with_attached(body).expect("attach failed")
}

fn with_frame<R>(body: impl for<'a> FnOnce(&'a splint::Frame<'a>) -> R) -> R {
    with_engine(|ctx| ctx.with_frame(body).unwrap())
}

#[test]
fn record_survives_its_frame() {
    with_engine(|ctx| {
        let record = ctx
            .with_frame(|frame| {
                let term = frame.term().unwrap();
                term.put_term_from_text("foo(bar, 42)").unwrap();
                term.record().unwrap()
            })
            .unwrap();

        // The originating frame is gone; the recorded value is intact.
        ctx.with_frame(|frame| {
            let recalled = record.recall(frame).unwrap();
            assert_eq!(recalled.write_to_string().unwrap(), "foo(bar,42)");
        })
        .unwrap();
    });
}

#[test]
fn record_recalls_into_an_existing_term() {
    with_engine(|ctx| {
        let record = ctx
            .with_frame(|frame| {
                let term = frame.term().unwrap();
                term.put_i64(123).unwrap();
                term.record().unwrap()
            })
            .unwrap();

        ctx.with_frame(|frame| {
            // A pre-existing slot: recall overwrites it in place, then it
            // unifies.
            let slot = frame.term().unwrap();
            assert!(slot.is_variable());
            record.recall_into(slot).unwrap();
            assert_eq!(slot.get_i64().unwrap(), 123);
        })
        .unwrap();
    });
}

#[test]
fn record_is_engine_independent() {
    // Record on one engine...
    let record: Record = with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(99).unwrap();
        term.record().unwrap()
    });

    // ...and recall it on a different engine created later.
    with_frame(|frame| {
        let recalled = record.recall(frame).unwrap();
        assert_eq!(recalled.get_i64().unwrap(), 99);
    });
}

#[test]
fn record_drops_without_an_engine_attached() {
    // Force RT to exist so a record can be made, then drop the record on this
    // harness thread — which has no crate-managed engine attached once
    // `with_engine` returns. Exercises PL_erase's engine-independence.
    let record: Record = with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(7).unwrap();
        term.record().unwrap()
    });
    drop(record);
}

#[test]
fn record_identity_tracks_clones_not_value() {
    let (a, b) = with_frame(|frame| {
        let make = || {
            let term = frame.term().unwrap();
            term.put_term_from_text("same(term)").unwrap();
            term.record().unwrap()
        };
        // Two independent recordings of an identical term.
        (make(), make())
    });

    // A clone shares identity with its source...
    let a_clone = a.clone();
    assert!(a.ptr_eq(&a_clone));

    // ...but independent recordings of the same term are distinct identities.
    assert!(!a.ptr_eq(&b));
}

#[test]
fn record_identity_backs_an_external_hash_impl() {
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};

    // The pattern an external crate uses: wrap `Record` and delegate the three
    // traits to its identity token.
    struct ByIdentity(Record);
    impl PartialEq for ByIdentity {
        fn eq(&self, other: &Self) -> bool {
            self.0.ptr_eq(&other.0)
        }
    }
    impl Eq for ByIdentity {}
    impl Hash for ByIdentity {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.0.ptr_hash(state);
        }
    }

    let (a, b) = with_frame(|frame| {
        let make = || {
            let term = frame.term().unwrap();
            term.put_i64(1).unwrap();
            term.record().unwrap()
        };
        (make(), make())
    });

    let mut set = HashSet::new();
    set.insert(ByIdentity(a.clone()));
    // A clone of `a` is already present (same identity); `b` is not.
    assert!(!set.insert(ByIdentity(a)));
    assert!(set.insert(ByIdentity(b)));
    assert_eq!(set.len(), 2);
}

#[test]
fn record_is_debug_printable() {
    let record: Record = with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(1).unwrap();
        term.record().unwrap()
    });
    // Debug works without an engine attached (the record is opaque here).
    assert!(format!("{record:?}").contains("Record"));
}

#[test]
fn record_clones_recall_and_drop_concurrently() {
    // Clones share one recorded copy through an atomic refcount, so many
    // threads may recall through their own clone and then drop it at once —
    // the copy is erased exactly once, when the last clone drops. (Before the
    // Arc, clone/drop raced on SWI-Prolog's non-atomic refcount: UB.)
    let record: Record = with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_term_from_text("concurrent(copy)").unwrap();
        term.record().unwrap()
    });

    std::thread::scope(|scope| {
        for _ in 0..16 {
            let clone = record.clone();
            scope.spawn(move || {
                let mut engine =
                    Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
                engine
                    .with_attached(|ctx| {
                        ctx.with_frame(|frame| {
                            let recalled = clone.recall(frame).unwrap();
                            assert_eq!(recalled.write_to_string().unwrap(), "concurrent(copy)");
                        })
                        .unwrap();
                    })
                    .expect("attach failed");
                // `clone` is dropped here, on this thread, concurrently with
                // every other thread's clone and the original below.
            });
        }
        // The original drops when the scope ends, racing the last clones.
    });

    // The shared copy survived every clone's drop but the last: recall the
    // original one final time.
    with_frame(|frame| {
        let recalled = record.recall(frame).unwrap();
        assert_eq!(recalled.write_to_string().unwrap(), "concurrent(copy)");
    });
}

#[test]
fn record_moves_across_threads_and_recalls() {
    let record: Record = with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_term_from_text("shared(data)").unwrap();
        term.record().unwrap()
    });

    std::thread::scope(|scope| {
        scope.spawn(move || {
            // A fresh thread with its own engine recalls the moved record and
            // then drops it here.
            let mut engine =
                Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
            engine
                .with_attached(|ctx| {
                    ctx.with_frame(|frame| {
                        let recalled = record.recall(frame).unwrap();
                        assert_eq!(recalled.write_to_string().unwrap(), "shared(data)");
                    })
                    .unwrap();
                })
                .expect("attach failed");
        });
    });
}
