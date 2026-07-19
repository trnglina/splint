use std::collections::HashMap;
use std::sync::LazyLock;

use splint::{Atom, Engine, EngineAttributes, FliContext, Runtime};

static RT: LazyLock<Runtime> = LazyLock::new(|| {
    Runtime::initialize(["splint-test", "-q"]).expect("shared runtime initialize failed")
});

fn with_engine<R>(body: impl FnOnce(&splint::AttachedEngine<'_>) -> R) -> R {
    let mut engine = Engine::new(&RT, EngineAttributes::default()).expect("engine create failed");
    let ctx = engine.attach().expect("attach failed");
    body(&ctx)
}

#[test]
fn atoms_compare_by_value() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let a = Atom::new(&frame, "point");
        let same = Atom::new(&frame, "point");
        let different = Atom::new(&frame, "line");

        assert_eq!(a, same);
        assert_ne!(a, different);

        // Equality holds across an atom read back out of a term, too.
        let term = frame.term().unwrap();
        term.put_atom(&a).unwrap();
        assert_eq!(term.get_atom().unwrap(), a);
    });
}

#[test]
fn atoms_work_as_hash_map_keys() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let mut counts: HashMap<Atom, u32> = HashMap::new();
        for text in ["x", "y", "x", "x", "y"] {
            *counts.entry(Atom::new(&frame, text)).or_insert(0) += 1;
        }
        assert_eq!(counts.get(&Atom::new(&frame, "x")), Some(&3));
        assert_eq!(counts.get(&Atom::new(&frame, "y")), Some(&2));
        assert_eq!(counts.get(&Atom::new(&frame, "z")), None);
    });
}

#[test]
fn atom_debug_shows_text() {
    with_engine(|ctx| {
        let frame = ctx.frame().unwrap();
        let atom = Atom::new(&frame, "flurble");
        assert_eq!(format!("{atom:?}"), "Atom(\"flurble\")");
    });
}
