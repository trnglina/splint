use std::sync::LazyLock;

use splint::{
    Atom, DictKey, Engine, EngineAttributes, FliContext, Functor, HandleError, Module, Predicate,
    Runtime,
};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    engine.with_attached(body).expect("attach failed")
}

fn with_frame<R>(body: impl for<'a> FnOnce(&'a splint::Frame<'a>) -> R) -> R {
    with_engine(|ctx| ctx.with_frame(body).unwrap())
}

#[test]
fn functor_exposes_its_name_and_arity() {
    with_frame(|frame| {
        let functor = Functor::from_name(frame, "foo", 2).unwrap();
        assert_eq!(functor.name(), Atom::new(frame, "foo"));
        assert_eq!(functor.arity(), 2);
    });
}

#[test]
fn module_exposes_its_name() {
    with_frame(|frame| {
        let module = Module::from_name(frame, "user");
        assert_eq!(module.name(), Atom::new(frame, "user"));
    });
}

#[test]
fn predicate_exposes_name_arity_and_module() {
    with_frame(|frame| {
        let module = Module::from_name(frame, "user");
        let functor = Functor::from_name(frame, "splint_handles_test_predicate", 2).unwrap();
        let predicate = Predicate::new(frame, &functor, &module).unwrap();

        assert_eq!(
            predicate.name(),
            Atom::new(frame, "splint_handles_test_predicate")
        );
        assert_eq!(predicate.arity(), 2);
        assert_eq!(predicate.module().name(), Atom::new(frame, "user"));
    });
}

#[test]
fn predicate_rejects_arities_outside_the_c_api_range() {
    with_engine(|ctx| {
        assert!(matches!(
            Predicate::from_name(ctx, "p", usize::MAX, None),
            Err(HandleError::PredicateArityOutOfRange { arity: usize::MAX })
        ));
    });
}

#[test]
fn global_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Atom>();
    assert_send_sync::<Functor>();
    assert_send_sync::<Module>();
    assert_send_sync::<Predicate>();
    assert_send_sync::<DictKey>();
}

#[test]
fn global_handles_can_be_reused_on_another_thread_and_engine() {
    let (atom, functor, module, predicate) = with_frame(|frame| {
        (
            Atom::new(frame, "portable"),
            Functor::from_name(frame, "portable", 1).unwrap(),
            Module::from_name(frame, "user"),
            Predicate::from_name(frame, "succ", 2, None).unwrap(),
        )
    });

    std::thread::spawn(move || {
        with_frame(|frame| {
            assert_eq!(atom.text(), "portable");
            assert_eq!(functor.name(), atom);
            assert_eq!(module.name().text(), "user");
            assert_eq!(predicate.name().text(), "succ");

            let atom_term = frame.term().unwrap();
            atom_term.put_atom(&atom).unwrap();
            assert_eq!(atom_term.get_atom().unwrap(), atom);

            let compound_args = frame.terms(1).unwrap();
            compound_args.get(0).unwrap().put_i64(7).unwrap();
            let compound = frame.term().unwrap();
            compound.cons_functor(&functor, &compound_args).unwrap();
            assert_eq!(compound.write_to_string().unwrap(), "portable(7)");

            let query_args = frame.terms(2).unwrap();
            query_args.get(0).unwrap().put_i64(7).unwrap();
            splint::Query::once(
                frame,
                &predicate,
                &query_args,
                splint::QueryOptions::default(),
                |_| (),
            )
            .unwrap();
            assert_eq!(query_args.get(1).unwrap().get_i64().unwrap(), 8);
        });
    })
    .join()
    .unwrap();
}
