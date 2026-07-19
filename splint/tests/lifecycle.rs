use std::process::Command;

use splint::{CleanupOptions, InitError, Runtime};

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

#[test]
fn lifecycle() {
    let saved_state = generate_saved_state();

    // 1. Initialize; a second initialization must be refused.
    let runtime = Runtime::initialize(["splint-test", "-q"]).expect("initial initialize failed");
    assert!(matches!(
        Runtime::initialize(["splint-test"]),
        Err(InitError::AlreadyInitialized)
    ));

    // 2. The initializing thread has the main engine attached.
    assert!(
        runtime.current_engine().is_some(),
        "main engine missing on init thread"
    );

    // 3. With no engines outstanding, cleanup succeeds.
    runtime
        .cleanup(CleanupOptions::default())
        .expect("cleanup failed");

    // 4a. A garbage saved-state buffer is rejected before initialization.
    static NOT_A_STATE: &[u8] = b"definitely not a zip archive";
    assert!(matches!(
        Runtime::initialize_from_state(["splint-test", "-q"], NOT_A_STATE),
        Err(InitError::InvalidSavedState)
    ));

    // 4b. Re-initialize from the real saved state generated earlier: proves
    // both re-initialization after cleanup and booting via
    // PL_set_resource_db_mem.
    let runtime = Runtime::initialize_from_state(["splint-test", "-q"], saved_state)
        .expect("re-initialize from saved state failed");
    assert!(runtime.current_engine().is_some());

    // 5. Tear the re-initialized runtime back down. A leaked engine is *not*
    // exercised here: `PL_cleanup` waits in `exitPrologThreads` for
    // outstanding engines to be destroyed, and whether that reports `Failed`
    // or blocks indefinitely is platform dependent — an outstanding engine
    // deadlocks cleanup on at least some builds, so the test avoids it.
    runtime
        .cleanup(CleanupOptions::default())
        .expect("final cleanup failed");
}
