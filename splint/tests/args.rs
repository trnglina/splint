use std::sync::LazyLock;

use splint::{Engine, EngineAttributes, FliContext, Predicate, Query, QueryOptions, Runtime};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

fn with_frame<R>(body: impl for<'a> FnOnce(&'a splint::Frame<'a>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    engine
        .with_attached(|ctx| ctx.with_frame(body).unwrap())
        .expect("attach failed")
}

fn predicate(ctx: &impl FliContext, name: &str, arity: usize) -> Predicate {
    Predicate::from_name(ctx, name, arity, None).unwrap()
}

#[test]
fn bare_terms_form_prepared_argument_blocks_without_decoding() {
    with_frame(|frame| {
        let nonvar = predicate(frame, "nonvar", 1);
        let value = frame.term().unwrap();
        value.put_term_from_text("hello(world)").unwrap();

        let args = frame.args((value,)).unwrap();
        assert_eq!(
            Query::once_with(frame, &nonvar, args, QueryOptions::default()).unwrap(),
            Some(((),))
        );

        let functor = value.get_functor().unwrap();
        assert_eq!(functor.name().text(), "hello");
        assert_eq!(functor.arity(), 1);
    });
}

#[test]
fn bare_terms_keep_bindings_across_prepared_solutions() {
    with_frame(|frame| {
        let between = predicate(frame, "between", 3);
        let low = frame.term().unwrap();
        low.put_i64(1).unwrap();
        let high = frame.term().unwrap();
        high.put_i64(3).unwrap();
        let value = frame.term().unwrap();

        let args = frame.args((low, high, value)).unwrap();
        let mut solutions =
            Query::solutions_with(frame, &between, args, QueryOptions::default()).unwrap();

        assert_eq!(solutions.next().unwrap().unwrap(), ((), (), ()));
        assert_eq!(value.get_i64().unwrap(), 1);
        assert_eq!(solutions.next().unwrap().unwrap(), ((), (), ()));
        assert_eq!(value.get_i64().unwrap(), 2);
        solutions.cut().unwrap();
        assert_eq!(value.get_i64().unwrap(), 2);
    });
}
