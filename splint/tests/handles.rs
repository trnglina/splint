use std::sync::LazyLock;

use splint::{Atom, Engine, EngineAttributes, FliContext, Functor, Module, Predicate, Runtime};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    let ctx = engine.attach().expect("attach failed");
    body(&ctx)
}

#[test]
fn functor_exposes_its_name_and_arity() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let functor = Functor::from_name(&frame, "foo", 2).unwrap();
        assert_eq!(functor.name(&frame), Atom::new(&frame, "foo"));
        assert_eq!(functor.arity(), 2);
    });
}

#[test]
fn module_exposes_its_name() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let module = Module::by_name(&frame, "user");
        assert_eq!(module.name(&frame), Atom::new(&frame, "user"));
    });
}

#[test]
fn predicate_exposes_name_arity_and_module() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let module = Module::by_name(&frame, "user");
        let functor = Functor::from_name(&frame, "succ", 2).unwrap();
        let predicate = Predicate::new(&frame, &functor, &module);

        assert_eq!(predicate.name(&frame), Atom::new(&frame, "succ"));
        assert_eq!(predicate.arity(), 2);
        assert_eq!(predicate.module(&frame).name(&frame), Atom::new(&frame, "user"));
    });
}
