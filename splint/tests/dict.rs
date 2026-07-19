use std::sync::LazyLock;

use splint::{Atom, DictKey, Engine, EngineAttributes, FliContext, Runtime, TermError, TermKind};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

/// Runs `body` with a fresh engine attached to the calling thread. Tests run on
/// harness threads that have no engine of their own, so each brings its own.
fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    let ctx = engine.attach().expect("attach failed");
    body(&ctx)
}

#[test]
fn dict_construction_and_key_access() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let tag = Atom::new(&frame, "point");
        let x = Atom::new(&frame, "x");
        let y = Atom::new(&frame, "y");

        let values = frame.terms(2).unwrap();
        values.get(0).put_i64(1).unwrap();
        values.get(1).put_i64(2).unwrap();

        let dict = frame.term().unwrap();
        dict.put_dict(&tag, &[&x, &y], &values).unwrap();
        assert_eq!(dict.kind(), TermKind::Dict);

        assert_eq!(dict.get_dict(&frame, &x).unwrap().get_i64().unwrap(), 1);
        assert_eq!(dict.get_dict(&frame, &y).unwrap().get_i64().unwrap(), 2);

        let rendered = dict.write_to_string().unwrap();
        assert!(
            rendered.contains("point") && rendered.contains("x:1") && rendered.contains("y:2"),
            "unexpected dict rendering: {rendered}"
        );
        // No frame.close(): the key atoms borrow the frame (A2).
    });
}

#[test]
fn dict_entries_are_enumerated_sorted() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let tag = Atom::new(&frame, "point");
        let x = Atom::new(&frame, "x");
        let y = Atom::new(&frame, "y");
        let values = frame.terms(2).unwrap();
        values.get(0).put_i64(10).unwrap();
        values.get(1).put_i64(20).unwrap();
        let dict = frame.term().unwrap();
        // Pass keys out of order to prove the enumeration is sorted, not
        // insertion-ordered.
        dict.put_dict(&tag, &[&y, &x], &values).unwrap();

        let entries = dict.dict_entries(&frame).unwrap();
        let seen: Vec<(String, i64)> = entries
            .iter()
            .map(|(key, value)| {
                let name = match key {
                    DictKey::Atom(atom) => atom.text(),
                    DictKey::Int(_) => panic!("expected atom keys"),
                };
                (name, value.get_i64().unwrap())
            })
            .collect();
        assert_eq!(seen, [("x".to_owned(), 20), ("y".to_owned(), 10)]);

        // Integer keys round-trip as DictKey::Int.
        let int_dict = frame.term().unwrap();
        int_dict.put_term_from_text("_{1: a, 2: b}").unwrap();
        let int_entries = int_dict.dict_entries(&frame).unwrap();
        let int_keys: Vec<i64> = int_entries
            .iter()
            .map(|(key, _)| match key {
                DictKey::Int(value) => *value,
                DictKey::Atom(_) => panic!("expected integer keys"),
            })
            .collect();
        assert_eq!(int_keys, [1, 2]);
        assert_eq!(int_entries[0].1.get_text().unwrap(), "a");
    });
}

#[test]
fn dict_tag_reads_atom_and_variable_tags() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let tag = Atom::new(&frame, "point");
        let x = Atom::new(&frame, "x");
        let values = frame.terms(1).unwrap();
        values.get(0).put_i64(1).unwrap();
        let dict = frame.term().unwrap();
        dict.put_dict(&tag, &[&x], &values).unwrap();
        assert_eq!(
            dict.dict_tag(&frame).unwrap().map(|atom| atom.text()),
            Some("point".to_owned())
        );

        // A parsed dict with an anonymous tag has a variable tag.
        let var_tagged = frame.term().unwrap();
        var_tagged.put_term_from_text("_{a: 1}").unwrap();
        assert!(var_tagged.dict_tag(&frame).unwrap().is_none());
    });
}

#[test]
fn dict_error_paths() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let tag = Atom::new(&frame, "point");
        let x = Atom::new(&frame, "x");

        // More keys than values.
        let values = frame.terms(1).unwrap();
        assert!(matches!(
            frame.term().unwrap().put_dict(&tag, &[&x, &x], &values),
            Err(TermError::DictLengthMismatch { keys: 2, values: 1 })
        ));

        // Dict operations on a non-dict are a type mismatch.
        let not_a_dict = frame.term().unwrap();
        not_a_dict.put_i64(3).unwrap();
        assert!(matches!(
            not_a_dict.get_dict(&frame, &x),
            Err(TermError::TypeMismatch { .. })
        ));
        assert!(matches!(
            not_a_dict.dict_entries(&frame),
            Err(TermError::TypeMismatch { .. })
        ));
        assert!(matches!(
            not_a_dict.dict_tag(&frame),
            Err(TermError::TypeMismatch { .. })
        ));
    });
}
