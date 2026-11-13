use std::process::{Command, Output};

use serde_json::Value;

fn demo(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_drivetest")).arg("--demo").args(args).output().unwrap()
}

#[test]
fn unsupported_test_is_nonzero_and_keeps_a_json_report() {
    let output = demo(&["self-test", "3", "--json", "--no-report"]);
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["test"]["unsupported"], true);
    assert!(String::from_utf8_lossy(&output.stderr).contains("UNSUPPORTED"));
}

#[test]
fn empty_scan_range_and_unacknowledged_remap_are_rejected() {
    let output = demo(&["scan", "3", "--mode", "range", "--start-gb", "100", "--end-gb", "200", "--no-report"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("contains no bytes"));
    let remap = demo(&["write-test", "1", "--kind", "remap", "--read-write", "--no-report"]);
    assert_eq!(remap.status.code(), Some(3));
}

#[test]
fn health_formats_have_the_same_exit_status_and_json_includes_the_custom_score() {
    assert_eq!(demo(&["health", "1"]).status.code(), Some(1));
    let json = demo(&["health", "1", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let snapshot: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(snapshot["health_score"]["status"], "caution");
    let flash = demo(&["health", "2", "--json"]);
    let snapshot: Value = serde_json::from_slice(&flash.stdout).unwrap();
    assert!(snapshot.get("health_score").unwrap().is_null());
}

#[test]
fn saved_scan_recheck_rereads_problem_addresses_and_refuses_a_different_disk() {
    let directory = std::env::temp_dir().join(format!(
        "drivetest-cli-recheck-{}-{}",
        std::process::id(),
        drivetest_core::signature::fresh_seed()
    ));
    std::fs::create_dir(&directory).unwrap();
    let source = demo(&["scan", "1", "--no-pinpoint", "--report-dir", directory.to_str().unwrap()]);
    assert_eq!(source.status.code(), Some(1));
    let path = std::fs::read_dir(&directory).unwrap().next().unwrap().unwrap().path();
    let wrong = demo(&["recheck", "3", "--from-report", path.to_str().unwrap(), "--no-report"]);
    assert_eq!(wrong.status.code(), Some(2));
    let right = demo(&["recheck", "1", "--from-report", path.to_str().unwrap(), "--json", "--no-report"]);
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(right.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&right.stdout).unwrap();
    assert_eq!(report["test"]["tier"], "read_only");
    assert!(report["recheck"]["reads"].as_array().unwrap().iter().any(|read| read["state"] == "unreadable"));
}
