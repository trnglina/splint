use std::process::Command;

use splint::{FliContext, InitError, Runtime};

/// Generates a saved state with `qsave_program/2` by shelling out to the
/// `swipl` on PATH (provided by the nix devshell), then leaks it to
/// `'static` as `Runtime::initialize_from_state` requires.
fn generate_saved_state() -> &'static [u8] {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("lifecycle.state");
    let goal = format!("qsave_program('{}', [])", path.display());
    let output = Command::new("swipl")
        .args(["-q", "-g", &goal, "-t", "halt"])
        .output()
        .expect("failed to run swipl to generate a saved state");
    assert!(
        output.status.success(),
        "qsave_program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Box::leak(
        std::fs::read(&path)
            .expect("saved state not readable")
            .into_boxed_slice(),
    )
}

/// The runtime is initialized at most once per process and is never torn down,
/// so this lives in its own test binary with a single test function: it is the
/// one test that cares about the uninitialized state.
#[test]
fn lifecycle() {
    let saved_state = generate_saved_state();

    // 1. A garbage saved-state buffer is rejected before initialization, so
    //    the runtime is still uninitialized afterwards.
    static NOT_A_STATE: &[u8] = b"definitely not a zip archive";
    assert!(matches!(
        Runtime::initialize_from_state(["splint-test", "-q"], NOT_A_STATE),
        Err(InitError::InvalidSavedState)
    ));

    // 2. Boot from the real saved state generated earlier, exercising
    //    PL_set_resource_db_mem.
    let runtime = Runtime::initialize_from_state(["splint-test", "-q"], saved_state)
        .expect("initialize from saved state failed");

    // 3. A second initialization must be refused; the runtime is initialized
    //    at most once per process (R1).
    assert!(matches!(
        Runtime::initialize(["splint-test"]),
        Err(InitError::AlreadyInitialized)
    ));

    // 4. The initializing thread has the main engine attached.
    assert!(
        runtime.current_engine().is_some(),
        "main engine missing on init thread"
    );

    // 5. The required accessor reuses the main engine.
    let current = runtime.engine().expect("reuse main engine failed");
    let term = current.term().expect("term allocation failed");
    term.put_i64(42).expect("term write failed");
    assert_eq!(term.get_i64().expect("term read failed"), 42);
}
