use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeTupleStruct};
use serde::{Deserialize, Serialize, Serializer};
use splint::{
    from_term, from_terms, to_term, to_terms, Engine, EngineAttributes, FliContext, Predicate,
    Query, QueryOptions, Record, Runtime, SerdeError, TermError, TermKind,
};

/// The private newtype-struct name `Record` uses to cross the serde
/// boundary (`splint/src/serde/record_token.rs`). Duplicated here — it isn't
/// exported — to drive the token branch directly from a hand-written
/// `Serialize`/`Deserialize` impl.
const RECORD_TOKEN: &str = "$splint::private::Record";

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

/// Runs `body` with a fresh engine attached to the calling thread. Tests run on
/// harness threads that have no engine of their own, so each brings its own.
fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    engine.with_attached(body).expect("attach failed")
}

fn with_frame<R>(body: impl for<'a> FnOnce(&'a splint::Frame<'a>) -> R) -> R {
    with_engine(|ctx| ctx.with_frame(body).unwrap())
}

/// Serializes `value` into a fresh term and deserializes it back.
fn round_trip<C, T>(ctx: &C, value: &T) -> T
where
    C: FliContext + ?Sized,
    T: Serialize + DeserializeOwned,
{
    let term = ctx.term().unwrap();
    to_term(ctx, term, value).unwrap();
    from_term(ctx, term).unwrap()
}

#[test]
fn scalars_round_trip() {
    with_frame(|frame| {
        assert!(round_trip(frame, &true));
        assert!(!round_trip(frame, &false));
        assert_eq!(round_trip(frame, &42i8), 42);
        assert_eq!(round_trip(frame, &i64::MIN), i64::MIN);
        assert_eq!(round_trip(frame, &i64::MAX), i64::MAX);
        assert_eq!(round_trip(frame, &u64::MAX), u64::MAX);
        assert_eq!(round_trip(frame, &1.5f64), 1.5);
        assert_eq!(round_trip(frame, &'q'), 'q');
        assert_eq!(round_trip(frame, &"hello".to_owned()), "hello");
    });
}

#[test]
fn strings_serialize_as_prolog_strings_and_read_atoms_too() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        to_term(frame, term, "hello").unwrap();
        assert_eq!(term.kind(), TermKind::String);

        let atom = frame.term().unwrap();
        atom.put_atom_text("world").unwrap();
        assert_eq!(from_term::<_, String>(frame, atom).unwrap(), "world");
    });
}

#[test]
fn booleans_read_back_from_atoms() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_atom_text("true").unwrap();
        assert!(from_term::<_, bool>(frame, term).unwrap());
    });
}

#[test]
fn sequences_and_tuples_round_trip_as_lists() {
    with_frame(|frame| {
        assert_eq!(round_trip(frame, &Vec::<i64>::new()), Vec::<i64>::new());
        assert_eq!(round_trip(frame, &vec![1i64, 2, 3]), [1, 2, 3]);
        assert_eq!(round_trip(frame, &[7i64, 8, 9]), [7, 8, 9]);
        assert_eq!(
            round_trip(frame, &(1i64, "two".to_owned(), true)),
            (1, "two".to_owned(), true)
        );
        assert_eq!(
            round_trip(frame, &vec![vec![1i64], vec![2, 3]]),
            [vec![1], vec![2, 3]]
        );

        let empty = frame.term().unwrap();
        to_term(frame, empty, &Vec::<i64>::new()).unwrap();
        assert_eq!(empty.kind(), TermKind::Nil);
    });
}

/// A wrapper that hits `serialize_bytes`/`deserialize_byte_buf` directly
/// (`Vec<u8>`'s derived impls use generic sequence serialization instead).
#[derive(Debug, PartialEq)]
struct Bytes(Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;

        impl<'de> serde::de::Visitor<'de> for BytesVisitor {
            type Value = Bytes;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a byte list")
            }

            fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(bytes))
            }
        }

        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}

#[test]
fn bytes_round_trip_as_integer_lists() {
    with_frame(|frame| {
        let bytes = Bytes(vec![0, 127, 255]);
        assert_eq!(round_trip(frame, &bytes), bytes);

        let term = frame.term().unwrap();
        term.put_term_from_text("[1, 300]").unwrap();
        let error = from_term::<_, Bytes>(frame, term).unwrap_err();
        assert!(matches!(error, SerdeError::ByteRange { value: 300 }));
    });
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Point {
    x: i64,
    y: i64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Wrapper {
    origin: Point,
    labels: Vec<String>,
}

#[test]
fn structs_round_trip_as_tagged_dicts() {
    with_frame(|frame| {
        let point = Point { x: 1, y: -2 };
        assert_eq!(round_trip(frame, &point), point);

        let term = frame.term().unwrap();
        to_term(frame, term, &point).unwrap();
        assert_eq!(term.kind(), TermKind::Dict);
        let rendered = term.write_to_string().unwrap();
        assert!(rendered.contains("Point"), "missing tag in: {rendered}");

        let wrapper = Wrapper {
            origin: Point { x: 3, y: 4 },
            labels: vec!["a".to_owned(), "b".to_owned()],
        };
        assert_eq!(round_trip(frame, &wrapper), wrapper);
    });
}

#[test]
fn struct_deserialization_requires_the_matching_tag() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_term_from_text("wrong{x: 1, y: 2}").unwrap();
        let error = from_term::<_, Point>(frame, term).unwrap_err();
        assert!(matches!(
            &error,
            SerdeError::DictTag { expected, actual: Some(actual) }
                if expected == "Point" && actual == "wrong"
        ));
    });
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct MaybePoint {
    x: Option<i64>,
    y: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Marker;

#[derive(Serialize)]
struct HasMarker {
    marker: Marker,
    n: i64,
}

#[test]
fn optional_fields_are_omitted_when_absent() {
    with_frame(|frame| {
        let value = MaybePoint {
            x: Some(1),
            y: None,
        };
        let term = frame.term().unwrap();
        to_term(frame, term, &value).unwrap();
        assert_eq!(term.dict_entries(frame).unwrap().len(), 1);
        assert_eq!(from_term::<_, MaybePoint>(frame, term).unwrap(), value);

        // A unit-struct field is likewise always omitted.
        let with_marker = frame.term().unwrap();
        to_term(
            frame,
            with_marker,
            &HasMarker {
                marker: Marker,
                n: 7,
            },
        )
        .unwrap();
        assert_eq!(with_marker.dict_entries(frame).unwrap().len(), 1);
    });
}

#[test]
fn options_and_units_are_rejected_outside_dict_entries() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        assert!(matches!(
            to_term(frame, term, &Option::<i64>::None).unwrap_err(),
            SerdeError::OptionOutsideDictEntry
        ));
        assert!(matches!(
            to_term(frame, term, &Some(1i64)).unwrap_err(),
            SerdeError::OptionOutsideDictEntry
        ));
        assert!(matches!(
            to_term(frame, term, &()).unwrap_err(),
            SerdeError::OptionOutsideDictEntry
        ));

        term.put_i64(1).unwrap();
        assert!(matches!(
            from_term::<_, Option<i64>>(frame, term).unwrap_err(),
            SerdeError::OptionOutsideDictEntry
        ));
    });
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum Shape {
    Empty,
    Zero(),
    Circle(f64),
    Rect(i64, i64),
    Label { text: String, size: i64 },
}

#[test]
fn enum_variants_round_trip_in_all_four_shapes() {
    with_frame(|frame| {
        for shape in [
            Shape::Empty,
            Shape::Zero(),
            Shape::Circle(2.5),
            Shape::Rect(3, 4),
            Shape::Label {
                text: "hi".to_owned(),
                size: 12,
            },
        ] {
            assert_eq!(round_trip(frame, &shape), shape);
        }

        let unit = frame.term().unwrap();
        to_term(frame, unit, &Shape::Empty).unwrap();
        assert_eq!(unit.kind(), TermKind::Atom);

        let compound = frame.term().unwrap();
        to_term(frame, compound, &Shape::Rect(3, 4)).unwrap();
        assert_eq!(compound.kind(), TermKind::Compound);
        assert_eq!(compound.write_to_string().unwrap(), "'Rect'(3,4)");
    });
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Pair(i64, String);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Nothing();

#[test]
fn tuple_structs_round_trip_as_compounds() {
    with_frame(|frame| {
        let pair = Pair(5, "five".to_owned());
        assert_eq!(round_trip(frame, &pair), pair);
        assert_eq!(round_trip(frame, &Nothing()), Nothing());

        // A zero-field tuple struct is an atom, like a unit variant.
        let empty = frame.term().unwrap();
        to_term(frame, empty, &Nothing()).unwrap();
        assert_eq!(empty.kind(), TermKind::Atom);

        let term = frame.term().unwrap();
        term.put_term_from_text("other(1, \"x\")").unwrap();
        let error = from_term::<_, Pair>(frame, term).unwrap_err();
        assert!(matches!(
            &error,
            SerdeError::Functor { expected_name, actual_name, .. }
                if expected_name == "Pair" && actual_name == "other"
        ));
    });
}

#[test]
fn maps_round_trip_with_scalar_keys() {
    with_frame(|frame| {
        let by_name: BTreeMap<String, i64> = [("a".to_owned(), 1), ("b".to_owned(), 2)]
            .into_iter()
            .collect();
        assert_eq!(round_trip(frame, &by_name), by_name);

        let by_index: BTreeMap<i64, String> = [(1, "one".to_owned()), (2, "two".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(round_trip(frame, &by_index), by_index);

        let by_flag: BTreeMap<bool, i64> = [(true, 1), (false, 0)].into_iter().collect();
        assert_eq!(round_trip(frame, &by_flag), by_flag);

        // Non-scalar keys are unsupported.
        let by_pair: BTreeMap<(i64, i64), i64> = [((1, 2), 3)].into_iter().collect();
        let term = frame.term().unwrap();
        assert!(matches!(
            to_term(frame, term, &by_pair).unwrap_err(),
            SerdeError::Message(_)
        ));
    });
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(untagged)]
enum Loose {
    Int(i64),
    Text(String),
    List(Vec<i64>),
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "kind")]
enum Node {
    Leaf { value: i64 },
    Branch { left: i64, right: i64 },
}

#[test]
fn self_describing_deserialization_supports_untagged_and_internally_tagged_enums() {
    with_frame(|frame| {
        for (text, expected) in [
            ("42", Loose::Int(42)),
            ("\"hello\"", Loose::Text("hello".to_owned())),
            ("[1, 2, 3]", Loose::List(vec![1, 2, 3])),
        ] {
            let term = frame.term().unwrap();
            term.put_term_from_text(text).unwrap();
            assert_eq!(from_term::<_, Loose>(frame, term).unwrap(), expected);
        }

        let leaf = frame.term().unwrap();
        leaf.put_term_from_text("_{kind: 'Leaf', value: 7}")
            .unwrap();
        assert_eq!(
            from_term::<_, Node>(frame, leaf).unwrap(),
            Node::Leaf { value: 7 }
        );

        let branch = frame.term().unwrap();
        branch
            .put_term_from_text("_{kind: 'Branch', left: 1, right: 2}")
            .unwrap();
        assert_eq!(
            from_term::<_, Node>(frame, branch).unwrap(),
            Node::Branch { left: 1, right: 2 }
        );
    });
}

#[test]
fn query_arguments_round_trip_through_to_terms_and_from_terms() {
    with_frame(|frame| {
        // Seed both arguments of succ/2 and confirm the goal holds.
        let succ = Predicate::from_name(frame, "succ", 2, None).unwrap();
        let args = frame.terms(2).unwrap();
        to_terms(frame, &args, &(2i64, 3i64)).unwrap();
        assert!(
            Query::once(frame, &succ, &args, QueryOptions::default(), |_| ())
                .unwrap()
                .is_some()
        );

        // A tuple of the wrong arity is rejected up front.
        assert!(matches!(
            to_terms(frame, &args, &(1i64, 2i64, 3i64)).unwrap_err(),
            SerdeError::ArityMismatch { .. }
        ));

        // Decode each solution of between/3, leaving the output unbound.
        let between = Predicate::from_name(frame, "between", 3, None).unwrap();
        let range = frame.terms(3).unwrap();
        to_term(frame, range.get(0).unwrap(), &1i64).unwrap();
        to_term(frame, range.get(1).unwrap(), &3i64).unwrap();
        let triples: Vec<(i64, i64, i64)> =
            Query::try_solutions(frame, &between, &range, QueryOptions::default(), |query| {
                from_terms(query, &range)
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(triples, [(1, 3, 1), (1, 3, 2), (1, 3, 3)]);
    });
}

/// Serializes the same key twice, driving `PL_put_dict`'s duplicate-key
/// rejection through the map path.
struct DuplicateKeys;

impl Serialize for DuplicateKeys {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("k", &1i64)?;
        map.serialize_entry("k", &2i64)?;
        map.end()
    }
}

/// Declares arity 2 but supplies a single field.
struct UnderFilled;

impl Serialize for UnderFilled {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut compound = serializer.serialize_tuple_struct("under", 2)?;
        compound.serialize_field(&1i64)?;
        compound.end()
    }
}

/// Declares arity 1 but supplies two fields.
struct OverFilled;

impl Serialize for OverFilled {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut compound = serializer.serialize_tuple_struct("over", 1)?;
        compound.serialize_field(&1i64)?;
        compound.serialize_field(&2i64)?;
        compound.end()
    }
}

/// Ends a map after serializing a key but not its value.
struct KeyWithoutValue;

impl Serialize for KeyWithoutValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_key("k")?;
        map.end()
    }
}

/// Serializes a second map key before the first key's value.
struct KeyBeforeValue;

impl Serialize for KeyBeforeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_key("a")?;
        map.serialize_key("b")?;
        map.serialize_value(&1i64)?;
        map.end()
    }
}

/// Requests a second dict key before consuming the first key's value.
#[derive(Debug)]
struct RequestsKeyBeforeValue;

impl<'de> Deserialize<'de> for RequestsKeyBeforeValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RequestsKey;

        impl<'de> Visitor<'de> for RequestsKey {
            type Value = RequestsKeyBeforeValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let _: Option<String> = map.next_key()?;
                let _: Option<String> = map.next_key()?;
                Ok(RequestsKeyBeforeValue)
            }
        }

        deserializer.deserialize_map(RequestsKey)
    }
}

#[test]
fn serializer_contract_violations_surface_as_errors() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        assert!(matches!(
            to_term(frame, term, &DuplicateKeys).unwrap_err(),
            SerdeError::Term(TermError::Exception(_))
        ));
        assert!(matches!(
            to_term(frame, term, &UnderFilled).unwrap_err(),
            SerdeError::ArityMismatch {
                expected: 2,
                actual: 1,
                ..
            }
        ));
        assert!(matches!(
            to_term(frame, term, &OverFilled).unwrap_err(),
            SerdeError::ArityMismatch {
                expected: 1,
                actual: 2,
                ..
            }
        ));
        assert!(matches!(
            to_term(frame, term, &KeyWithoutValue).unwrap_err(),
            SerdeError::MapKeyWithoutValue
        ));
        assert!(matches!(
            to_term(frame, term, &KeyBeforeValue).unwrap_err(),
            SerdeError::MapKeyOrder("serialized")
        ));

        term.put_term_from_text("_{a: 1, b: 2}").unwrap();
        assert!(matches!(
            from_term::<_, RequestsKeyBeforeValue>(frame, term).unwrap_err(),
            SerdeError::MapKeyOrder("requested")
        ));
    });
}

#[derive(Serialize, Deserialize)]
struct WithRecord {
    n: i64,
    rec: Record,
}

#[test]
fn record_field_round_trips_and_survives_its_source_frame() {
    with_engine(|ctx| {
        let value = ctx
            .with_frame(|frame| {
                let term = frame.term().unwrap();
                term.put_term_from_text("foo(bar, 42)").unwrap();
                let rec = term.record().unwrap();
                WithRecord { n: 7, rec }
            })
            .unwrap();

        ctx.with_frame(|frame| {
            let term = frame.term().unwrap();
            to_term(frame, term, &value).unwrap();
            let decoded: WithRecord = from_term(frame, term).unwrap();
            assert_eq!(decoded.n, 7);
            let recalled = decoded.rec.recall(frame).unwrap();
            assert_eq!(recalled.write_to_string().unwrap(), "foo(bar,42)");
        })
        .unwrap();
    });
}

/// Calls the private record token directly with an arbitrary payload,
/// simulating a forged `Serialize` impl rather than `Record`'s own.
struct FakeRecord;

impl Serialize for FakeRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_newtype_struct(RECORD_TOKEN, &42u64)
    }
}

#[test]
fn forged_record_token_serialize_errors() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        assert!(matches!(
            to_term(frame, term, &FakeRecord).unwrap_err(),
            SerdeError::ForeignRecord
        ));
    });
}

#[test]
fn record_to_and_from_serde_json_fails_cleanly() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(1).unwrap();
        let record = term.record().unwrap();

        assert!(serde_json::to_string(&record).is_err());
        assert!(serde_json::from_str::<Record>("null").is_err());
    });
}

/// Drives the record token's deserialize branch (claiming a fresh record
/// handle from the source term) but always fails afterward, so the claimed
/// handle is never turned into a `Record` — exercising the unclaimed-record
/// cleanup path.
struct AlwaysErrorsAfterRecording;

impl<'de> Deserialize<'de> for AlwaysErrorsAfterRecording {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AlwaysErrors;

        impl<'de> serde::de::Visitor<'de> for AlwaysErrors {
            type Value = AlwaysErrorsAfterRecording;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("never succeeds")
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                _deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                Err(serde::de::Error::custom("intentional failure"))
            }
        }

        deserializer.deserialize_newtype_struct(RECORD_TOKEN, AlwaysErrors)
    }
}

#[test]
fn unclaimed_incoming_record_is_discarded_without_crashing() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(1).unwrap();
        assert!(from_term::<_, AlwaysErrorsAfterRecording>(frame, term).is_err());

        // The frame is still usable: no crash, no corrupted engine state.
        let other = frame.term().unwrap();
        other.put_i64(2).unwrap();
        assert_eq!(other.get_i64().unwrap(), 2);
    });
}

/// Panics after the term deserializer has offered it a fresh record handle.
struct PanicsAfterRecording;

impl<'de> Deserialize<'de> for PanicsAfterRecording {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Panics;

        impl<'de> serde::de::Visitor<'de> for Panics {
            type Value = PanicsAfterRecording;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("always panics")
            }

            fn visit_newtype_struct<D: serde::Deserializer<'de>>(
                self,
                _deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                panic!("intentional visitor panic")
            }
        }

        deserializer.deserialize_newtype_struct(RECORD_TOKEN, Panics)
    }
}

#[test]
fn incoming_record_handoff_is_removed_during_unwind() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_i64(1).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = from_term::<_, PanicsAfterRecording>(frame, term);
        }));
        assert!(result.is_err());

        // A stale handoff would let this foreign deserializer claim the
        // record created above.
        assert!(serde_json::from_str::<Record>("null").is_err());
    });
}

#[derive(Deserialize)]
#[serde(untagged)]
#[allow(dead_code)] // only `is_err()` is asserted; the variant payloads are never read
enum MaybeRecord {
    Rec(Record),
    Num(i64),
}

#[test]
fn record_inside_untagged_enum_fails_cleanly() {
    with_frame(|frame| {
        let term = frame.term().unwrap();
        term.put_term_from_text("foo(bar)").unwrap();
        assert!(from_term::<_, MaybeRecord>(frame, term).is_err());
    });
}
