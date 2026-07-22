use std::{collections::BTreeMap, env, path::PathBuf, process::Command};

const EXPECTED_PLVERSION: &str = "100002";

fn main() {
    println!("cargo:rerun-if-env-changed=SWIPL");

    let swipl = env::var_os("SWIPL").unwrap_or_else(|| "swipl".into());
    let output = Command::new(&swipl)
        .arg("--dump-runtime-variables=sh")
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", swipl.to_string_lossy()));

    if !output.status.success() {
        panic!(
            "{} --dump-runtime-variables=sh failed: {}",
            swipl.to_string_lossy(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let runtime_variables = String::from_utf8_lossy(&output.stdout);
    let variables = parse_runtime_variables(&runtime_variables);
    let plversion = variables
        .get("PLVERSION")
        .expect("SWI-Prolog did not report PLVERSION");
    assert_eq!(
        *plversion, EXPECTED_PLVERSION,
        "SWI-Prolog PLVERSION mismatch: expected {EXPECTED_PLVERSION}, found {plversion}"
    );
    let plbase = variables
        .get("PLBASE")
        .expect("SWI-Prolog did not report PLBASE");
    let library_dir = variables
        .get("PLLIBSWIPL")
        .and_then(|library| PathBuf::from(library).parent().map(PathBuf::from))
        .or_else(|| variables.get("PLLIBDIR").map(PathBuf::from))
        .expect("SWI-Prolog did not report PLLIBSWIPL or PLLIBDIR");
    let include_dir = PathBuf::from(plbase).join("include");
    let header = include_dir.join("SWI-Prolog.h");

    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    println!("cargo:rustc-link-lib=dylib=swipl");
}

fn parse_runtime_variables(output: &str) -> BTreeMap<&str, &str> {
    output
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            Some((name, value.strip_prefix('"')?.strip_suffix("\";")?))
        })
        .collect()
}
