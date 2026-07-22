use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use splint::{
    from_term, from_terms, to_term, to_terms, Engine, EngineAttributes, ExternalRecord, FliContext,
    FromTerm, Predicate, Query, QueryOptions, Runtime, TermCodecError, TermKind, ToTerm,
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

fn round_trip<C, T>(ctx: &C, value: &T) -> T
where
    C: FliContext + ?Sized,
    T: ToTerm + FromTerm,
{
    let term = ctx.term().unwrap();
    to_term(ctx, term, value).unwrap();
    from_term(ctx, term).unwrap()
}

#[test]
fn built_in_values_round_trip() {
    with_frame(|frame| {
        assert_eq!(round_trip(frame, &42_i64), 42);
        assert_eq!(round_trip(frame, &u64::MAX), u64::MAX);
        assert_eq!(round_trip(frame, &1.5_f64), 1.5);
        assert_eq!(round_trip(frame, &"hello".to_owned()), "hello");
        assert_eq!(round_trip(frame, &vec![1_i64, 2, 3]), [1, 2, 3]);
        assert_eq!(
            round_trip(frame, &(1_i64, "two".to_owned())),
            (1, "two".to_owned())
        );

        let map: BTreeMap<String, i64> = [("a".to_owned(), 1), ("b".to_owned(), 2)]
            .into_iter()
            .collect();
        assert_eq!(round_trip(frame, &map), map);
    });
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
#[splint(rename_all = "snake_case")]
struct Details {
    display_name: String,
    count: Option<i64>,
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
struct Envelope {
    id: u64,
    #[splint(flatten)]
    details: Details,
    payload: ExternalRecord,
    #[splint(default = "default_priority")]
    priority: i64,
}

fn default_priority() -> i64 {
    11
}

#[test]
fn structs_flatten_and_capture_an_ordinary_term_losslessly() {
    with_frame(|frame| {
        let source = frame.term().unwrap();
        source
            .put_term_from_text("'Envelope'{id: 7, display_name: \"hello\", payload: foo(X, X)}")
            .unwrap();

        let decoded: Envelope = from_term(frame, source).unwrap();
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.details.display_name, "hello");
        assert_eq!(decoded.details.count, None);
        assert_eq!(decoded.priority, 11);

        let dest = frame.term().unwrap();
        to_term(frame, dest, &decoded).unwrap();
        assert_eq!(dest.kind(), TermKind::Dict);
        let fields = dest.dict_entries(frame).unwrap();
        let payload = fields
            .into_iter()
            .find_map(|(key, value)| match key {
                splint::DictKey::Atom(atom) if atom.text() == "payload" => Some(value),
                _ => None,
            })
            .unwrap();
        assert!(payload.write_to_string().unwrap().starts_with("foo("));
        let first = payload.get_arg(frame, 0).unwrap();
        let second = payload.get_arg(frame, 1).unwrap();
        let nine = frame.term().unwrap();
        nine.put_i64(9).unwrap();
        assert!(first.unify(nine).unwrap());
        assert_eq!(second.get_i64().unwrap(), 9, "variable sharing was lost");
    });
}

#[derive(Default)]
struct RuntimeOnly;

#[derive(ToTerm, FromTerm)]
#[splint(rename_all = "camelCase")]
struct Config<T> {
    generic_value: T,
    #[splint(skip)]
    runtime_only: RuntimeOnly,
}

#[derive(ToTerm, FromTerm)]
struct SkippedGeneric<T: Default> {
    value: i64,
    #[splint(skip)]
    runtime_only: T,
}

#[derive(FromTerm)]
#[splint(default)]
struct Defaults {
    value: i64,
}

#[test]
fn derives_add_generic_bounds_and_skip_runtime_only_fields() {
    with_frame(|frame| {
        let value = Config {
            generic_value: 5_i64,
            runtime_only: RuntimeOnly,
        };
        let term = frame.term().unwrap();
        to_term(frame, term, &value).unwrap();
        let text = term.write_to_string().unwrap();
        assert!(text.contains("genericValue"));
        assert!(!text.contains("runtime_only"));
        let decoded: Config<i64> = from_term(frame, term).unwrap();
        assert_eq!(decoded.generic_value, 5);
        let _ = decoded.runtime_only;

        let skipped = SkippedGeneric::<RuntimeOnly> {
            value: 6,
            runtime_only: RuntimeOnly,
        };
        let term = frame.term().unwrap();
        to_term(frame, term, &skipped).unwrap();
        let decoded: SkippedGeneric<RuntimeOnly> = from_term(frame, term).unwrap();
        assert_eq!(decoded.value, 6);
        let _ = decoded.runtime_only;

        let empty = frame.term().unwrap();
        empty.put_term_from_text("'Defaults'{}").unwrap();
        assert_eq!(from_term::<_, Defaults>(frame, empty).unwrap().value, 0);
    });
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
struct RecursiveNode {
    value: i64,
    next: Option<Box<RecursiveNode>>,
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
struct Tree<T> {
    value: T,
    children: Vec<Tree<T>>,
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
struct BoxedTree<T> {
    value: T,
    children: Vec<Box<BoxedTree<T>>>,
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
struct SharedTree {
    value: i64,
    children: Vec<Arc<SharedTree>>,
}

#[test]
fn recursive_structs_round_trip_through_option_vec_box_and_arc() {
    with_frame(|frame| {
        let node = RecursiveNode {
            value: 1,
            next: Some(Box::new(RecursiveNode {
                value: 2,
                next: None,
            })),
        };
        assert_eq!(round_trip(frame, &node), node);

        let tree = Tree {
            value: "root".to_owned(),
            children: vec![
                Tree {
                    value: "first".to_owned(),
                    children: Vec::new(),
                },
                Tree {
                    value: "second".to_owned(),
                    children: vec![Tree {
                        value: "grandchild".to_owned(),
                        children: Vec::new(),
                    }],
                },
            ],
        };
        assert_eq!(round_trip(frame, &tree), tree);

        let boxed = BoxedTree {
            value: 1_i64,
            children: vec![Box::new(BoxedTree {
                value: 2,
                children: vec![Box::new(BoxedTree {
                    value: 3,
                    children: Vec::new(),
                })],
            })],
        };
        assert_eq!(round_trip(frame, &boxed), boxed);

        let shared = SharedTree {
            value: 1,
            children: vec![Arc::new(SharedTree {
                value: 2,
                children: Vec::new(),
            })],
        };
        assert_eq!(round_trip(frame, &shared), shared);
    });
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
enum Left {
    End,
    Rights(Vec<Right>),
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
enum Right {
    End,
    Lefts(Vec<Left>),
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
#[splint(tag = "kind")]
enum TaggedTree {
    Leaf { value: i64 },
    Branch { children: Vec<TaggedTree> },
}

#[test]
fn recursive_enums_and_mutually_recursive_vecs_round_trip() {
    with_frame(|frame| {
        let mutual = Left::Rights(vec![
            Right::End,
            Right::Lefts(vec![Left::End, Left::Rights(Vec::new())]),
        ]);
        assert_eq!(round_trip(frame, &mutual), mutual);

        let tagged = TaggedTree::Branch {
            children: vec![
                TaggedTree::Leaf { value: 1 },
                TaggedTree::Branch {
                    children: vec![TaggedTree::Leaf { value: 2 }],
                },
            ],
        };
        assert_eq!(round_trip(frame, &tagged), tagged);
    });
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
enum Shape {
    Empty,
    Circle(f64),
    Rect(i64, i64),
    Label { text: String },
}

#[test]
fn externally_tagged_enums_round_trip() {
    with_frame(|frame| {
        for value in [
            Shape::Empty,
            Shape::Circle(2.5),
            Shape::Rect(3, 4),
            Shape::Label { text: "x".into() },
        ] {
            assert_eq!(round_trip(frame, &value), value);
        }
    });
}

#[derive(ToTerm, FromTerm)]
#[splint(untagged)]
enum KnownOrOpaque {
    Number(i64),
    Pair(i64, String),
    Opaque(ExternalRecord),
}

#[test]
fn untagged_external_record_is_a_lossless_catch_all() {
    with_frame(|frame| {
        let number = frame.term().unwrap();
        number.put_i64(4).unwrap();
        assert!(matches!(
            from_term(frame, number).unwrap(),
            KnownOrOpaque::Number(4)
        ));

        let pair = frame.term().unwrap();
        to_term(frame, pair, &KnownOrOpaque::Pair(3, "three".to_owned())).unwrap();
        assert_eq!(pair.kind(), TermKind::ListPair);
        assert!(matches!(
            from_term(frame, pair).unwrap(),
            KnownOrOpaque::Pair(3, text) if text == "three"
        ));

        let compound = frame.term().unwrap();
        compound.put_term_from_text("unknown(a, 1r3)").unwrap();
        let KnownOrOpaque::Opaque(record) = from_term(frame, compound).unwrap() else {
            panic!("opaque variant did not match");
        };
        let recalled = record.recall(frame).unwrap();
        assert_eq!(recalled.write_to_string().unwrap(), "unknown(a,1r3)");
    });
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
#[splint(tag = "kind")]
enum Node {
    Leaf { value: i64 },
    Branch { left: i64, right: i64 },
}

#[derive(ToTerm, FromTerm, Debug, PartialEq)]
#[splint(tag = "kind", content = "data")]
enum Event {
    Stop,
    Value(ExternalRecord),
    Named { value: i64 },
}

#[test]
fn tagged_enums_round_trip_without_buffering_payloads() {
    with_frame(|frame| {
        assert_eq!(
            round_trip(frame, &Node::Leaf { value: 3 }),
            Node::Leaf { value: 3 }
        );
        let payload = frame.term().unwrap();
        payload.put_term_from_text("kept(X, X)").unwrap();
        let event = Event::Value(payload.record_external().unwrap());
        let decoded = round_trip(frame, &event);
        let Event::Value(record) = decoded else {
            panic!("wrong variant")
        };
        assert!(record
            .recall(frame)
            .unwrap()
            .write_to_string()
            .unwrap()
            .starts_with("kept("));
        assert_eq!(
            round_trip(frame, &Event::Named { value: 8 }),
            Event::Named { value: 8 }
        );
    });
}

#[derive(ToTerm, FromTerm, Serialize, Deserialize)]
struct Persisted {
    payload: ExternalRecord,
}

#[test]
fn serde_persists_bytes_while_splint_maps_the_ordinary_term() {
    with_frame(|frame| {
        let source = frame.term().unwrap();
        source
            .put_term_from_text("'Persisted'{payload: arbitrary(foo)}")
            .unwrap();
        let value: Persisted = from_term(frame, source).unwrap();
        let json = serde_json::to_string(&value).unwrap();
        let restored: Persisted = serde_json::from_str(&json).unwrap();
        let dest = frame.term().unwrap();
        to_term(frame, dest, &restored).unwrap();
        assert!(dest.write_to_string().unwrap().contains("arbitrary(foo)"));
    });
}

#[test]
fn term_lists_use_the_new_tuple_traits() {
    with_frame(|frame| {
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        to_terms(frame, &args, &(2_i64, 3_i64)).unwrap();
        Query::once(frame, &succ, &args, QueryOptions::default(), |_| ()).unwrap();
        assert_eq!(from_terms::<_, (i64, i64)>(frame, &args).unwrap(), (2, 3));

        let short = frame.terms(1).unwrap();
        assert!(matches!(
            to_terms(frame, &short, &(1_i64, 2_i64)).unwrap_err(),
            TermCodecError::ArityMismatch { .. }
        ));
    });
}
