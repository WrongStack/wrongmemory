//! WM-511: the generated SAGE surface matrix
//! (`contracts/certification/sage-surface.json`, from
//! `scripts/certification/build-sage-surface.ts`) must stay true: every
//! method it names is a WrongMemory method, and every Rust evidence test it
//! cites still exists. Adapter evidence lives in the WrongStack worktree and
//! is checked by the generator (`--adapter`).

use std::path::{Path, PathBuf};

use serde_json::Value;
use wrongmemory_contracts::method;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut String) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n != "target") {
                rust_sources(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push_str(&std::fs::read_to_string(&p).unwrap());
            out.push('\n');
        }
    }
}

#[test]
fn certification_matrix_names_real_methods_and_tests() {
    let root = repo();
    let matrix: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("contracts/certification/sage-surface.json")).unwrap(),
    )
    .unwrap();
    let ops = matrix["operations"].as_array().unwrap();
    assert_eq!(
        ops.len() as u64,
        matrix["baseline"]["operations"].as_u64().unwrap()
    );
    let mut sources = String::new();
    rust_sources(&root.join("crates"), &mut sources);
    let mut problems = Vec::new();
    for op in ops {
        let name = op["op"].as_str().unwrap();
        for m in op["methods"].as_array().unwrap() {
            let m = m.as_str().unwrap();
            if !method::ALL.contains(&m) {
                problems.push(format!("{name}: unknown method {m}"));
            }
        }
        for t in op["rustEvidence"].as_array().unwrap() {
            let t = t.as_str().unwrap();
            if !sources.contains(&format!("fn {t}(")) {
                problems.push(format!("{name}: no Rust test {t}"));
            }
        }
        let evidence = op["rustEvidence"].as_array().unwrap().len()
            + op["adapterEvidence"].as_array().unwrap().len();
        if op["status"] == "COVERED" && evidence == 0 {
            problems.push(format!("{name}: COVERED without evidence"));
        }
    }
    assert!(
        problems.is_empty(),
        "stale certification matrix:\n{}",
        problems.join("\n")
    );
}
