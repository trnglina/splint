use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::LazyLock;

use splint::{
    AttachedEngine, Engine, EngineAttributes, FliContext, Frame, Functor, Module, Predicate, Query,
    QueryError, QueryOptions, Runtime, TermError, TermKind,
};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

/// Runs `body` with a fresh engine attached to the calling thread. Tests run
/// on harness threads that have no engine of their own, so each test brings
/// its own.
fn with_engine<R>(body: impl FnOnce(&AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    let ctx = engine.attach().expect("attach failed");
    body(&ctx)
}

#[test]
fn scalar_roundtrips() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();

        let t = frame.term().unwrap();
        assert!(t.is_variable());
        assert_eq!(t.kind(), TermKind::Variable);

        t.put_i64(-42).unwrap();
        assert_eq!(t.get_i64().unwrap(), -42);
        assert_eq!(t.kind(), TermKind::Integer);

        t.put_u64(u64::MAX).unwrap();
        assert_eq!(t.get_u64().unwrap(), u64::MAX);

        t.put_f64(2.5).unwrap();
        assert_eq!(t.get_f64().unwrap(), 2.5);
        assert_eq!(t.kind(), TermKind::Float);

        t.put_bool(true).unwrap();
        assert!(t.get_bool().unwrap());

        t.put_atom_text("hello world").unwrap();
        assert_eq!(t.get_text().unwrap(), "hello world");
        assert_eq!(t.kind(), TermKind::Atom);

        t.put_string("a prolog string").unwrap();
        assert_eq!(t.get_text().unwrap(), "a prolog string");
        assert_eq!(t.kind(), TermKind::String);

        t.put_nil().unwrap();
        assert!(t.is_nil());
        assert_eq!(t.kind(), TermKind::Nil);

        frame.close();
    });
}

#[test]
fn atoms_and_put_term_copy() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let atom = splint::Atom::new(&frame, "flurble");
        assert_eq!(atom.text(), "flurble");
        let cloned = atom.clone();

        let t = frame.term().unwrap();
        t.put_atom(&cloned).unwrap();
        assert_eq!(t.get_atom().unwrap().text(), "flurble");

        let copy = frame.term().unwrap();
        copy.put_term(t).unwrap();
        assert_eq!(copy.get_text().unwrap(), "flurble");
        // No explicit close: the atoms borrow the frame and drop after it in
        // reverse declaration order, which a consuming `close(self)` cannot
        // wait for (A2).
    });
}

#[test]
fn compound_construction_and_decomposition() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let functor = Functor::from_name(&frame, "foo", 3).unwrap();
        let args = frame.terms(3).unwrap();
        for (index, term) in args.iter().enumerate() {
            term.put_i64(index as i64 + 1).unwrap();
        }

        let t = frame.term().unwrap();
        t.cons_functor(&functor, &args).unwrap();
        assert_eq!(t.kind(), TermKind::Compound);

        let (name, arity) = t.name_arity().unwrap();
        assert_eq!(name.text(), "foo");
        assert_eq!(arity, 3);
        assert_eq!(t.get_arg(&frame, 1).unwrap().get_i64().unwrap(), 2);
        assert_eq!(t.write_to_string().unwrap(), "foo(1,2,3)");

        let wrong_args = frame.terms(2).unwrap();
        assert!(matches!(
            t.cons_functor(&functor, &wrong_args),
            Err(TermError::ArityMismatch {
                expected: 3,
                actual: 2
            })
        ));
    });
}

#[test]
fn list_construction_and_traversal() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();

        // Build [1, 2, 3] back to front.
        let mut tail = frame.term().unwrap();
        tail.put_nil().unwrap();
        for value in [3, 2, 1] {
            let head = frame.term().unwrap();
            head.put_i64(value).unwrap();
            let cell = frame.term().unwrap();
            cell.cons_list(head, tail).unwrap();
            tail = cell;
        }
        assert_eq!(tail.kind(), TermKind::ListPair);

        let mut seen = Vec::new();
        let mut cursor = tail;
        while !cursor.is_nil() {
            let (head, rest) = cursor.get_list(&frame).unwrap();
            seen.push(head.get_i64().unwrap());
            cursor = rest;
        }
        assert_eq!(seen, [1, 2, 3]);
        frame.close();
    });
}

#[test]
fn parse_from_text() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let t = frame.term().unwrap();
        t.put_term_from_text("foo(bar, 1+2)").unwrap();
        let (name, arity) = t.name_arity().unwrap();
        assert_eq!(name.text(), "foo");
        assert_eq!(arity, 2);

        let malformed = frame.term().unwrap();
        let error = malformed.put_term_from_text("foo(").unwrap_err();
        assert!(
            matches!(error, TermError::Exception(_)),
            "syntax error should surface as an exception, got: {error:?}"
        );

        // The exception was cleared: the engine remains usable.
        let ok = frame.term().unwrap();
        ok.put_i64(1).unwrap();
        assert_eq!(ok.get_i64().unwrap(), 1);
    });
}

#[test]
fn type_mismatch_is_not_an_exception() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let t = frame.term().unwrap();
        t.put_atom_text("not_a_number").unwrap();
        assert!(matches!(
            t.get_i64(),
            Err(TermError::TypeMismatch { .. })
        ));
        // get_text on a compound is also a type mismatch...
        t.put_term_from_text("f(x)").unwrap();
        assert!(matches!(t.get_text(), Err(TermError::TypeMismatch { .. })));
        // ...but write_to_string renders it.
        assert_eq!(t.write_to_string().unwrap(), "f(x)");
        frame.close();
    });
}

#[test]
fn unification_and_frame_data_lifetimes() {
    with_engine(|ctx| {
        let outer = ctx.frame().unwrap();

        // unify: variable-atom succeeds, distinct integers fail cleanly.
        let one = outer.term().unwrap();
        one.put_i64(1).unwrap();
        let two = outer.term().unwrap();
        two.put_i64(2).unwrap();
        assert!(!one.unify(two).unwrap());

        // A binding made inside a closed frame survives.
        let kept = outer.term().unwrap();
        {
            let inner = outer.frame().unwrap();
            let value = inner.term().unwrap();
            value.put_atom_text("kept").unwrap();
            assert!(kept.unify(value).unwrap());
            inner.close();
        }
        assert_eq!(kept.get_text().unwrap(), "kept");

        // A binding made inside a discarded frame is undone.
        let lost = outer.term().unwrap();
        {
            let inner = outer.frame().unwrap();
            let value = inner.term().unwrap();
            value.put_atom_text("lost").unwrap();
            assert!(lost.unify(value).unwrap());
            inner.discard();
        }
        assert!(lost.is_variable());

        // Dropping a frame discards it, like `discard`.
        let dropped = outer.term().unwrap();
        {
            let inner = outer.frame().unwrap();
            let value = inner.term().unwrap();
            value.put_i64(9).unwrap();
            assert!(dropped.unify(value).unwrap());
        }
        assert!(dropped.is_variable());

        outer.close();
    });
}

#[test]
fn rewind_frees_references_and_bindings() {
    with_engine(|ctx| {
        let mut frame = ctx.frame().unwrap();
        let t = frame.term().unwrap();
        t.put_i64(7).unwrap();
        assert_eq!(t.get_i64().unwrap(), 7);
        frame.rewind();
        let fresh = frame.term().unwrap();
        assert!(fresh.is_variable());
        frame.close();
    });
}

#[test]
fn deterministic_query_binds_output() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let succ = Predicate::resolve(&frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(0).put_i64(1).unwrap();

        let mut query = Query::open(&frame, &succ, &args, QueryOptions::default()).unwrap();
        assert!(query.next_solution().unwrap());
        assert_eq!(args.get(1).get_i64().unwrap(), 2);
        assert!(!query.next_solution().unwrap());
        query.close().unwrap();
        frame.close();
    });
}

#[test]
fn nondeterministic_query_enumerates_solutions() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let member = Predicate::resolve(&frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1).put_term_from_text("[a, b, c]").unwrap();

        let mut query = Query::open(&frame, &member, &args, QueryOptions::default()).unwrap();
        let mut seen = Vec::new();
        while query.next_solution().unwrap() {
            seen.push(args.get(0).get_text().unwrap());
        }
        assert_eq!(seen, ["a", "b", "c"]);
        query.close().unwrap();
        frame.close();
    });
}

#[test]
fn query_is_a_scope_for_solution_inspection() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let member = Predicate::resolve(&frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1).put_term_from_text("[f(1), f(2)]").unwrap();

        let mut query = Query::open(&frame, &member, &args, QueryOptions::default()).unwrap();
        let mut seen = Vec::new();
        while query.next_solution().unwrap() {
            // Scratch references for decomposing the solution come from the
            // query itself — the innermost scope while it is open.
            let arg = args.get(0).get_arg(&query, 0).unwrap();
            seen.push(arg.get_i64().unwrap());
        }
        assert_eq!(seen, [1, 2]);
        query.close().unwrap();
        frame.close();
    });
}

#[test]
fn cut_keeps_bindings_close_discards_them() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let succ = Predicate::resolve(&frame, "succ", 2, None).unwrap();

        let cut_args = frame.terms(2).unwrap();
        cut_args.get(0).put_i64(4).unwrap();
        let mut query = Query::open(&frame, &succ, &cut_args, QueryOptions::default()).unwrap();
        assert!(query.next_solution().unwrap());
        query.cut().unwrap();
        assert_eq!(cut_args.get(1).get_i64().unwrap(), 5);

        let close_args = frame.terms(2).unwrap();
        close_args.get(0).put_i64(4).unwrap();
        let mut query = Query::open(&frame, &succ, &close_args, QueryOptions::default()).unwrap();
        assert!(query.next_solution().unwrap());
        query.close().unwrap();
        assert!(close_args.get(1).is_variable());

        frame.close();
    });
}

#[test]
fn thrown_exception_is_captured_and_cleared() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let throw = Predicate::resolve(&frame, "throw", 1, None).unwrap();
        let args = frame.terms(1).unwrap();
        args.get(0).put_term_from_text("my_error(42)").unwrap();

        let mut query = Query::open(&frame, &throw, &args, QueryOptions::default()).unwrap();
        let error = query.next_solution().unwrap_err();
        match &error {
            QueryError::Exception(exception) => {
                assert!(
                    exception.0.contains("my_error(42)"),
                    "exception text should contain the thrown term, got: {}",
                    exception.0,
                );
            }
            other => panic!("expected an exception, got: {other:?}"),
        }
        query.close().unwrap();

        // The exception was cleared: the engine remains usable.
        let t = frame.term().unwrap();
        t.put_i64(1).unwrap();
        assert_eq!(t.get_i64().unwrap(), 1);
        frame.close();
    });
}

#[test]
fn predicate_via_functor_and_module() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let module = Module::by_name(&frame, "user");
        let functor = Functor::from_name(&frame, "succ", 2).unwrap();
        let succ = Predicate::new(&frame, &functor, &module);

        let args = frame.terms(2).unwrap();
        args.get(0).put_i64(7).unwrap();
        let mut query = Query::open(&frame, &succ, &args, QueryOptions::default()).unwrap();
        assert!(query.next_solution().unwrap());
        assert_eq!(args.get(1).get_i64().unwrap(), 8);
        query.close().unwrap();
        frame.close();
    });
}

#[test]
fn query_arity_mismatch_is_rejected() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let succ = Predicate::resolve(&frame, "succ", 2, None).unwrap();
        let args = frame.terms(1).unwrap();
        assert!(matches!(
            Query::open(&frame, &succ, &args, QueryOptions::default()),
            Err(QueryError::ArityMismatch {
                expected: 2,
                actual: 1
            })
        ));
        frame.close();
    });
}

#[test]
fn allocating_through_a_non_innermost_context_panics() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.term();
        }));
        let message = *result.unwrap_err().downcast::<String>().unwrap();
        assert!(
            message.contains("innermost"),
            "expected the innermost-allocation panic, got: {message}"
        );
        // The frame is untouched and still usable.
        let t = frame.term().unwrap();
        t.put_i64(3).unwrap();
        frame.close();
    });
}

#[test]
fn closing_scopes_out_of_order_panics() {
    with_engine(|ctx| {
        let outer = ctx.frame().unwrap();
        // A CurrentEngine witness aliases the frame's scope position without
        // borrowing it, which is how an out-of-order close becomes
        // expressible at all.
        let witness = RT.current_engine().unwrap();
        let inner = Frame::open(&witness).unwrap();
        std::mem::forget(inner);
        let result = catch_unwind(AssertUnwindSafe(|| outer.close()));
        let message = *result.unwrap_err().downcast::<String>().unwrap();
        assert!(
            message.contains("reverse order"),
            "expected the LIFO panic, got: {message}"
        );
    });
}

#[test]
fn advancing_a_query_under_an_open_sibling_scope_panics() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let member = Predicate::resolve(&frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1).put_term_from_text("[1, 2]").unwrap();

        let mut query = Query::open(&frame, &member, &args, QueryOptions::default()).unwrap();
        // As in `closing_scopes_out_of_order_panics`, a CurrentEngine witness
        // aliases the query's scope position without borrowing it, so the
        // borrow checker cannot see that backtracking would invalidate the
        // frame opened on top of the query.
        let witness = RT.current_engine().unwrap();
        let inner = Frame::open(&witness).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| query.next_solution()));
        let message = *result.unwrap_err().downcast::<String>().unwrap();
        assert!(
            message.contains("innermost"),
            "expected the innermost panic, got: {message}"
        );

        // With the sibling scope gone, the query works again.
        inner.close();
        assert!(query.next_solution().unwrap());
        query.close().unwrap();
        frame.close();
    });
}

#[test]
fn terms_refuse_to_operate_after_an_engine_switch() {
    with_engine(|ctx| {
        let term = ctx.term().unwrap();
        term.put_i64(11).unwrap();

        let mut other = Engine::new(&RT, EngineAttributes::default()).expect("create failed");
        {
            let _other_ctx = other.attach().expect("attach failed");
            let result = catch_unwind(AssertUnwindSafe(|| term.get_i64()));
            let message = *result.unwrap_err().downcast::<String>().unwrap();
            assert!(
                message.contains("current engine"),
                "expected the generation panic, got: {message}"
            );
        }

        // With the original engine current again, the term works.
        assert_eq!(term.get_i64().unwrap(), 11);
    });
}

#[test]
#[should_panic(expected = "out of bounds")]
fn term_list_indexing_is_bounds_checked() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let args = frame.terms(2).unwrap();
        let _ = args.get(2);
    });
}
