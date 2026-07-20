use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::LazyLock;

use splint::{
    AttachedEngine, Engine, EngineAttributes, FliContext, Functor, Module, Predicate, Query,
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
    engine.with_attached(body).expect("attach failed")
}

fn with_frame<R>(body: impl for<'a> FnOnce(&'a splint::Frame<'a>) -> R) -> R {
    with_engine(|ctx| ctx.with_frame(body).unwrap())
}

#[test]
fn scalar_roundtrips() {
    with_frame(|frame| {
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
    });
}

#[test]
fn atoms_and_put_term_copy() {
    with_frame(|frame| {
        let atom = splint::Atom::new(frame, "flurble");
        assert_eq!(atom.text(), "flurble");
        let cloned = atom.clone();

        let t = frame.term().unwrap();
        t.put_atom(&cloned).unwrap();
        assert_eq!(t.get_atom().unwrap().text(), "flurble");

        let copy = frame.term().unwrap();
        copy.put_term(t).unwrap();
        assert_eq!(copy.get_text().unwrap(), "flurble");
        // Atom handles are process-global and own their registrations, so
        // they do not borrow the frame that was used to create them (A2).
    });
}

#[test]
fn compound_construction_and_decomposition() {
    with_frame(|frame| {
        let functor = Functor::from_name(frame, "foo", 3).unwrap();
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
        assert_eq!(t.get_arg(frame, 1).unwrap().get_i64().unwrap(), 2);
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
fn compound_arguments_reject_an_unrepresentable_index() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_term_from_text("f(x)").unwrap();

        assert!(matches!(
            term.get_arg(frame, usize::MAX),
            Err(TermError::ArgumentIndexOutOfRange { index: usize::MAX })
        ));
    });
}

#[test]
fn get_functor_yields_a_reusable_handle() {
    with_frame(|frame| {
        let source = frame.term().unwrap();
        source.put_term_from_text("foo(1, 2, 3)").unwrap();

        let functor = source.get_functor().unwrap();
        assert_eq!(functor.arity(), 3);

        // The handle rebuilds an equivalent compound.
        let args = frame.terms(3).unwrap();
        for (index, term) in args.iter().enumerate() {
            term.put_i64(index as i64 + 4).unwrap();
        }
        let rebuilt = frame.term().unwrap();
        rebuilt.cons_functor(&functor, &args).unwrap();
        assert_eq!(rebuilt.write_to_string().unwrap(), "foo(4,5,6)");

        // An atom is its own arity-0 functor.
        let atom = frame.term().unwrap();
        atom.put_atom_text("bar").unwrap();
        assert_eq!(atom.get_functor().unwrap().arity(), 0);

        // A non-callable term (an integer) has no functor.
        let number = frame.term().unwrap();
        number.put_i64(7).unwrap();
        assert!(matches!(
            number.get_functor(),
            Err(TermError::TypeMismatch { .. })
        ));
    });
}

#[test]
fn list_construction_and_traversal() {
    with_frame(|frame| {
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
            let (head, rest) = cursor.get_list(frame).unwrap();
            seen.push(head.get_i64().unwrap());
            cursor = rest;
        }
        assert_eq!(seen, [1, 2, 3]);
    });
}

#[test]
fn parse_from_text() {
    with_frame(|frame| {
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
    with_frame(|frame| {
        let t = frame.term().unwrap();
        t.put_atom_text("not_a_number").unwrap();
        assert!(matches!(t.get_i64(), Err(TermError::TypeMismatch { .. })));
        // get_text on a compound is also a type mismatch...
        t.put_term_from_text("f(x)").unwrap();
        assert!(matches!(t.get_text(), Err(TermError::TypeMismatch { .. })));
        // ...but write_to_string renders it.
        assert_eq!(t.write_to_string().unwrap(), "f(x)");
    });
}

#[test]
fn unification_and_frame_data_lifetimes() {
    with_frame(|outer| {
        // unify: variable-atom succeeds, distinct integers fail cleanly.
        let one = outer.term().unwrap();
        one.put_i64(1).unwrap();
        let two = outer.term().unwrap();
        two.put_i64(2).unwrap();
        assert!(!one.unify(two).unwrap());

        // A binding made inside a closed frame survives.
        let kept = outer.term().unwrap();
        outer
            .with_frame(|inner| {
                let value = inner.term().unwrap();
                value.put_atom_text("kept").unwrap();
                assert!(kept.unify(value).unwrap());
            })
            .unwrap();
        assert_eq!(kept.get_text().unwrap(), "kept");

        // A binding made inside a discarded frame is undone.
        let lost = outer.term().unwrap();
        let error = outer
            .try_with_frame(|inner| {
                let value = inner.term().unwrap();
                value.put_atom_text("lost").unwrap();
                assert!(lost.unify(value).unwrap());
                Err::<(), _>("discard")
            })
            .unwrap_err();
        assert!(matches!(error, splint::ScopedCallError::Body("discard")));
        assert!(lost.is_variable());
    });
}

#[test]
fn frame_closures_commit_success_and_rollback_failure_or_panic() {
    with_engine(|ctx| {
        let kept = ctx.term().unwrap();
        ctx.with_frame(|frame| {
            let value = frame.term().unwrap();
            value.put_atom_text("kept").unwrap();
            assert!(kept.unify(value).unwrap());
        })
        .unwrap();
        assert_eq!(kept.get_text().unwrap(), "kept");

        let failed = ctx.term().unwrap();
        let error = ctx
            .try_with_frame(|frame| {
                let value = frame.term().unwrap();
                value.put_atom_text("failed").unwrap();
                assert!(failed.unify(value).unwrap());
                Err::<(), _>("body failed")
            })
            .unwrap_err();
        assert!(matches!(
            error,
            splint::ScopedCallError::Body("body failed")
        ));
        assert!(failed.is_variable());

        let panicked = ctx.term().unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.with_frame(|frame| {
                let value = frame.term().unwrap();
                value.put_atom_text("panicked").unwrap();
                assert!(panicked.unify(value).unwrap());
                panic!("body panic");
            });
        }));
        assert!(result.is_err());
        assert!(panicked.is_variable());
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
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(0).unwrap().put_i64(1).unwrap();

        let found = Query::once(frame, &succ, &args, QueryOptions::default(), |_| ())
            .unwrap()
            .is_some();
        assert!(found);
        assert_eq!(args.get(1).unwrap().get_i64().unwrap(), 2);
    });
}

#[test]
fn query_once_commits_a_solution_and_reports_no_solution() {
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(0).unwrap().put_i64(8).unwrap();

        let value = Query::once(frame, &succ, &args, QueryOptions::default(), |_| {
            args.get(1).unwrap().get_i64().unwrap()
        })
        .unwrap();
        assert_eq!(value, Some(9));
        assert_eq!(args.get(1).unwrap().get_i64().unwrap(), 9);

        let fail = Predicate::from_name(frame, "fail", 0, None).unwrap();
        let no_args = frame.terms(0).unwrap();
        let value = Query::once(frame, &fail, &no_args, QueryOptions::default(), |_| 1).unwrap();
        assert_eq!(value, None);
    });
}

#[test]
fn try_once_rolls_back_a_body_error() {
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(0).unwrap().put_i64(8).unwrap();

        let error = Query::try_once(frame, &succ, &args, QueryOptions::default(), |_| {
            Err::<(), _>("reject solution")
        })
        .unwrap_err();
        assert!(matches!(
            error,
            splint::ScopedCallError::Body("reject solution")
        ));
        assert!(args.get(1).unwrap().is_variable());
    });
}

#[test]
fn nondeterministic_query_enumerates_solutions() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1)
            .unwrap()
            .put_term_from_text("[a, b, c]")
            .unwrap();

        let seen = Query::solutions(frame, &member, &args, QueryOptions::default(), |_| {
            args.get(0).unwrap().get_text().unwrap()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(seen, ["a", "b", "c"]);
    });
}

#[test]
fn solution_iterator_maps_owned_values_and_closes_on_exhaustion() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1)
            .unwrap()
            .put_term_from_text("[f(1), f(2), f(3)]")
            .unwrap();

        let values = Query::solutions(frame, &member, &args, QueryOptions::default(), |solution| {
            args.get(0)
                .unwrap()
                .get_arg(solution, 0)
                .unwrap()
                .get_i64()
                .unwrap()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(values, [1, 2, 3]);
        assert!(args.get(0).unwrap().is_variable());
    });
}

#[test]
fn solution_iterator_rolls_back_on_drop_and_can_cut_explicitly() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();

        let rolled_back = frame.terms(2).unwrap();
        rolled_back
            .get(1)
            .unwrap()
            .put_term_from_text("[a, b]")
            .unwrap();
        let mut solutions = Query::solutions(
            frame,
            &member,
            &rolled_back,
            QueryOptions::default(),
            |_| (),
        )
        .unwrap();
        assert!(solutions.next().unwrap().is_ok());
        drop(solutions);
        assert!(rolled_back.get(0).unwrap().is_variable());

        let kept = frame.terms(2).unwrap();
        kept.get(1).unwrap().put_term_from_text("[a, b]").unwrap();
        let mut solutions =
            Query::solutions(frame, &member, &kept, QueryOptions::default(), |_| ()).unwrap();
        assert!(solutions.next().unwrap().is_ok());
        solutions.cut().unwrap();
        assert_eq!(kept.get(0).unwrap().get_text().unwrap(), "a");
    });
}

#[test]
fn try_solution_iterator_rolls_back_mapper_errors() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1).unwrap().put_term_from_text("[a, b]").unwrap();

        let mut solutions =
            Query::try_solutions(frame, &member, &args, QueryOptions::default(), |_| {
                Err::<(), _>("reject solution")
            })
            .unwrap();
        let error = solutions.next().unwrap().unwrap_err();
        assert!(matches!(
            error,
            splint::ScopedCallError::Body("reject solution")
        ));
        assert!(solutions.next().is_none());
        assert!(args.get(0).unwrap().is_variable());
        drop(solutions);
    });
}

#[test]
fn solution_iterator_rolls_back_a_mapper_panic_even_if_caught() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1).unwrap().put_term_from_text("[a, b]").unwrap();

        let mut solutions =
            Query::solutions(frame, &member, &args, QueryOptions::default(), |_| {
                panic!("mapper panic")
            })
            .unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| solutions.next()));
        assert!(result.is_err());
        assert!(solutions.next().is_none());
        assert!(args.get(0).unwrap().is_variable());
        drop(solutions);
    });
}

#[test]
fn query_is_a_scope_for_solution_inspection() {
    with_frame(|frame| {
        let member = Predicate::from_name(frame, "member", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        args.get(1)
            .unwrap()
            .put_term_from_text("[f(1), f(2)]")
            .unwrap();

        let seen = Query::solutions(frame, &member, &args, QueryOptions::default(), |solution| {
            // Scratch references for decomposing the solution come from the
            // query itself — the innermost scope while it is open.
            let arg = args.get(0).unwrap().get_arg(solution, 0).unwrap();
            arg.get_i64().unwrap()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        assert_eq!(seen, [1, 2]);
    });
}

#[test]
fn cut_keeps_bindings_close_discards_them() {
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();

        let cut_args = frame.terms(2).unwrap();
        cut_args.get(0).unwrap().put_i64(4).unwrap();
        assert!(
            Query::once(frame, &succ, &cut_args, QueryOptions::default(), |_| ())
                .unwrap()
                .is_some()
        );
        assert_eq!(cut_args.get(1).unwrap().get_i64().unwrap(), 5);

        let close_args = frame.terms(2).unwrap();
        close_args.get(0).unwrap().put_i64(4).unwrap();
        let mut solutions =
            Query::solutions(frame, &succ, &close_args, QueryOptions::default(), |_| ()).unwrap();
        assert!(solutions.next().unwrap().is_ok());
        solutions.close().unwrap();
        assert!(close_args.get(1).unwrap().is_variable());
    });
}

#[test]
fn thrown_exception_is_captured_and_cleared() {
    with_frame(|frame| {
        let throw = Predicate::from_name(frame, "throw", 1, None).unwrap();
        let args = frame.terms(1).unwrap();
        args.get(0)
            .unwrap()
            .put_term_from_text("my_error(42)")
            .unwrap();

        let error = Query::once(frame, &throw, &args, QueryOptions::default(), |_| ()).unwrap_err();
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

        // The exception was cleared: the engine remains usable.
        let t = frame.term().unwrap();
        t.put_i64(1).unwrap();
        assert_eq!(t.get_i64().unwrap(), 1);
    });
}

#[test]
fn predicate_via_functor_and_module() {
    with_frame(|frame| {
        let module = Module::from_name(frame, "user");
        let functor = Functor::from_name(frame, "succ", 2).unwrap();
        let succ = Predicate::new(frame, &functor, &module).unwrap();

        let args = frame.terms(2).unwrap();
        args.get(0).unwrap().put_i64(7).unwrap();
        assert!(
            Query::once(frame, &succ, &args, QueryOptions::default(), |_| ())
                .unwrap()
                .is_some()
        );
        assert_eq!(args.get(1).unwrap().get_i64().unwrap(), 8);
    });
}

#[test]
fn query_arity_mismatch_is_rejected() {
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(1).unwrap();
        assert!(matches!(
            Query::once(frame, &succ, &args, QueryOptions::default(), |_| ()),
            Err(QueryError::ArityMismatch {
                expected: 2,
                actual: 1
            })
        ));
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
        let inner = witness.frame().unwrap();
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
fn terms_refuse_to_operate_after_an_engine_switch() {
    with_engine(|ctx| {
        let term = ctx.term().unwrap();
        term.put_i64(11).unwrap();

        let mut other = Engine::new(&RT, EngineAttributes::default()).expect("create failed");
        other
            .with_attached_within(ctx, |_| {
                let result = catch_unwind(AssertUnwindSafe(|| term.get_i64()));
                let message = *result.unwrap_err().downcast::<String>().unwrap();
                assert!(
                    message.contains("current engine"),
                    "expected the generation panic, got: {message}"
                );
            })
            .expect("attach failed");

        // With the original engine current again, the term works.
        assert_eq!(term.get_i64().unwrap(), 11);
    });
}

#[test]
fn term_list_get_returns_none_out_of_bounds() {
    with_frame(|frame| {
        let args = frame.terms(2).unwrap();
        assert!(args.get(0).is_some());
        assert!(args.get(1).is_some());
        assert!(args.get(2).is_none());
    });
}
