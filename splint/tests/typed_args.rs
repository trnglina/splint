use std::sync::LazyLock;

use splint::{
    input, input_as, output, ArgumentError, CallError, Engine, EngineAttributes, FliContext,
    Predicate, Query, QueryError, QueryOptions, Record, Runtime,
};

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
fn typed_once_encodes_inputs_and_decodes_all_final_arguments() {
    with_frame(|frame| {
        let succ = predicate(frame, "succ", 2);
        let args = frame.args((input(41_i64), output::<i64>())).unwrap();

        let result = Query::once_with(frame, &succ, args, QueryOptions::default()).unwrap();
        assert_eq!(result, (41, 42));
    });
}

#[test]
fn input_as_accepts_borrowed_input_and_decodes_an_owned_value() {
    with_frame(|frame| {
        let string_length = predicate(frame, "string_length", 2);
        let args = frame
            .args((input_as::<String, _>("world"), output::<i64>()))
            .unwrap();

        let result =
            Query::once_with(frame, &string_length, args, QueryOptions::default()).unwrap();
        assert_eq!(result, ("world".to_owned(), 5));
    });
}

#[test]
fn typed_outputs_can_be_records() {
    with_frame(|frame| {
        let member = predicate(frame, "member", 2);
        let args = frame
            .args((output::<Record>(), input(vec![7_i64, 8, 9])))
            .unwrap();

        let (record, values) =
            Query::once_with(frame, &member, args, QueryOptions::default()).unwrap();
        assert_eq!(values, [7, 8, 9]);
        assert_eq!(record.recall(frame).unwrap().get_i64().unwrap(), 7);
    });
}

#[test]
fn prepared_calls_require_a_solution_and_have_an_explicit_optional_form() {
    with_frame(|frame| {
        let true_predicate = predicate(frame, "true", 0);
        let args = frame.args(()).unwrap();
        assert_eq!(
            Query::once_with(frame, &true_predicate, args, QueryOptions::default()).unwrap(),
            ()
        );

        let succ = predicate(frame, "succ", 2);
        let args = frame.args((input(1_i64), input(3_i64))).unwrap();
        assert!(matches!(
            Query::once_with(frame, &succ, args, QueryOptions::default()),
            Err(CallError::Query(QueryError::NoSolution))
        ));

        let args = frame.args((input(1_i64), input(3_i64))).unwrap();
        assert_eq!(
            Query::once_optional_with(frame, &succ, args, QueryOptions::default()).unwrap(),
            None
        );
    });
}

#[test]
fn typed_calls_report_argument_query_and_result_errors() {
    with_frame(|frame| {
        let argument_error = match frame.args((input(None::<i64>),)) {
            Ok(_) => panic!("None outside a dict should not encode"),
            Err(error) => error,
        };
        assert!(matches!(
            argument_error,
            CallError::Arguments(ArgumentError::Serde(_))
        ));

        let succ = predicate(frame, "succ", 2);
        let args = frame.args((input(1_i64),)).unwrap();
        assert!(matches!(
            Query::once_with(frame, &succ, args, QueryOptions::default()),
            Err(CallError::Query(QueryError::ArityMismatch {
                expected: 2,
                actual: 1
            }))
        ));

        let member = predicate(frame, "member", 2);
        let args = frame
            .args((
                output::<i64>(),
                input_as::<Vec<String>, _>(vec!["not an integer"]),
            ))
            .unwrap();
        assert!(matches!(
            Query::once_with(frame, &member, args, QueryOptions::default()),
            Err(CallError::ResultDecoding(ArgumentError::Serde(_)))
        ));
    });
}

#[test]
fn terms_preserve_bindings_across_committed_calls() {
    with_frame(|frame| {
        let succ = predicate(frame, "succ", 2);
        let middle = frame.term().unwrap();

        let first_args = frame.args((input(1_i64), middle.as_arg::<i64>())).unwrap();
        assert_eq!(
            Query::once_with(frame, &succ, first_args, QueryOptions::default()).unwrap(),
            (1, 2)
        );
        assert_eq!(middle.get_i64().unwrap(), 2);

        let second_args = frame
            .args((middle.as_arg::<i64>(), output::<i64>()))
            .unwrap();
        assert_eq!(
            Query::once_with(frame, &succ, second_args, QueryOptions::default()).unwrap(),
            (2, 3)
        );
    });
}

#[test]
fn typed_solution_iterators_decode_and_can_keep_a_binding() {
    with_frame(|frame| {
        let member = predicate(frame, "member", 2);
        let value = frame.term().unwrap();
        let args = frame
            .args((value.as_arg::<i64>(), input(vec![1_i64, 2, 3])))
            .unwrap();
        let mut solutions =
            Query::solutions_with(frame, &member, args, QueryOptions::default()).unwrap();

        assert_eq!(solutions.next().unwrap().unwrap(), (1, vec![1, 2, 3]));
        solutions.cut().unwrap();
        assert_eq!(value.get_i64().unwrap(), 1);
    });
}

#[test]
fn bare_terms_are_passed_without_decoding() {
    with_frame(|frame| {
        let nonvar = predicate(frame, "nonvar", 1);
        let value = frame.term().unwrap();
        value.put_term_from_text("hello(world)").unwrap();

        let recorded_args = frame.args((value.as_arg::<Record>(),)).unwrap();
        let (record,) =
            Query::once_with(frame, &nonvar, recorded_args, QueryOptions::default()).unwrap();

        let functor = value.get_functor().unwrap();
        assert_eq!(functor.name().text(), "hello");
        assert_eq!(functor.arity(), 1);
        assert_eq!(
            record.recall(frame).unwrap().write_to_string().unwrap(),
            "hello(world)"
        );

        let pass_through_args = frame.args((value,)).unwrap();
        assert_eq!(
            Query::once_with(frame, &nonvar, pass_through_args, QueryOptions::default()).unwrap(),
            ((),)
        );
    });
}

#[test]
fn typed_once_callbacks_can_nest_a_call() {
    with_frame(|frame| {
        let succ = predicate(frame, "succ", 2);
        let middle = frame.term().unwrap();
        let outer_args = frame.args((input(1_i64), middle.as_arg::<i64>())).unwrap();

        let nested = Query::try_once_with(
            frame,
            &succ,
            outer_args,
            QueryOptions::default(),
            |query, (_, current)| {
                assert_eq!(current, 2);
                let inner_args = query.args((middle.as_arg::<i64>(), output::<i64>()))?;
                Query::once_with(query, &succ, inner_args, QueryOptions::default())
            },
        )
        .unwrap();

        assert_eq!(nested, (2, 3));
    });
}

#[test]
fn typed_solution_callbacks_can_nest_calls_for_each_solution() {
    with_frame(|frame| {
        let between = predicate(frame, "between", 3);
        let succ = predicate(frame, "succ", 2);
        let value = frame.term().unwrap();
        let outer_args = frame
            .args((input(1_i64), input(3_i64), value.as_arg::<i64>()))
            .unwrap();

        let results = Query::try_solutions_with(
            frame,
            &between,
            outer_args,
            QueryOptions::default(),
            move |query, (_, _, current)| {
                assert_eq!(value.get_i64().unwrap(), current);
                let inner_args = query.args((value.as_arg::<i64>(), output::<i64>()))?;
                Query::once_with(query, &succ, inner_args, QueryOptions::default())
            },
        )
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

        assert_eq!(results, [(1, 2), (2, 3), (3, 4)]);
    });
}

#[test]
fn typed_argument_tuples_support_sixteen_positions() {
    with_frame(|frame| {
        let true_predicate = predicate(frame, "true", 0);
        let args = frame
            .args((
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
                output::<i64>(),
            ))
            .unwrap();

        assert!(matches!(
            Query::once_with(frame, &true_predicate, args, QueryOptions::default()),
            Err(CallError::Query(QueryError::ArityMismatch {
                expected: 0,
                actual: 16
            }))
        ));
    });
}
