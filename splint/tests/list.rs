use std::sync::LazyLock;

use splint::{Engine, EngineAttributes, FliContext, ListShape, Runtime, TermError};

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
fn list_shape_classifies_proper_and_empty() {
    with_frame(|frame| {
        let proper = frame.term().unwrap();
        proper.put_term_from_text("[a, b, c]").unwrap();
        assert_eq!(proper.list_shape(), ListShape::Proper { len: 3 });

        let empty = frame.term().unwrap();
        empty.put_nil().unwrap();
        assert_eq!(empty.list_shape(), ListShape::Proper { len: 0 });
    });
}

#[test]
fn list_shape_classifies_partial_improper_cyclic_and_non_list() {
    with_frame(|frame| {
        // Partial: [a | _], tail left unbound.
        let a = frame.term().unwrap();
        a.put_atom_text("a").unwrap();
        let partial = frame.term().unwrap();
        let open_tail = frame.term().unwrap();
        partial.cons_list(a, open_tail).unwrap();
        assert_eq!(partial.list_shape(), ListShape::Partial);

        // Improper: [a | b], tail a bound atom.
        let b = frame.term().unwrap();
        b.put_atom_text("b").unwrap();
        let improper = frame.term().unwrap();
        improper.cons_list(a, b).unwrap();
        assert_eq!(improper.list_shape(), ListShape::Improper);

        // Cyclic: X = [a | X].
        let cyclic = frame.term().unwrap();
        let self_tail = frame.term().unwrap();
        cyclic.cons_list(a, self_tail).unwrap();
        assert!(self_tail.unify(cyclic).unwrap());
        assert_eq!(cyclic.list_shape(), ListShape::Cyclic);

        // Not a list at all.
        let atom = frame.term().unwrap();
        atom.put_atom_text("foo").unwrap();
        assert_eq!(atom.list_shape(), ListShape::NotAList);
    });
}

#[test]
fn collect_list_gathers_proper_elements() {
    with_frame(|frame| {
        let list = frame.term().unwrap();
        list.put_term_from_text("[1, 2, 3]").unwrap();

        let elements = list.collect_list(frame).unwrap();
        let values: Vec<i64> = elements
            .iter()
            .map(|term| term.get_i64().unwrap())
            .collect();
        assert_eq!(values, [1, 2, 3]);

        let empty = frame.term().unwrap();
        empty.put_nil().unwrap();
        assert!(empty.collect_list(frame).unwrap().is_empty());
    });
}

#[test]
fn collect_list_rejects_non_proper_lists_without_looping() {
    with_frame(|frame| {
        let a = frame.term().unwrap();
        a.put_atom_text("a").unwrap();

        // Cyclic: collect_list must terminate (not spin) and report the shape.
        let cyclic = frame.term().unwrap();
        let self_tail = frame.term().unwrap();
        cyclic.cons_list(a, self_tail).unwrap();
        assert!(self_tail.unify(cyclic).unwrap());
        assert!(matches!(
            cyclic.collect_list(frame),
            Err(TermError::NotAProperList(ListShape::Cyclic))
        ));

        // Improper: [a | b].
        let b = frame.term().unwrap();
        b.put_atom_text("b").unwrap();
        let improper = frame.term().unwrap();
        improper.cons_list(a, b).unwrap();
        assert!(matches!(
            improper.collect_list(frame),
            Err(TermError::NotAProperList(ListShape::Improper))
        ));
    });
}
