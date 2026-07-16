use ai_daily_scanner_contract::{TransportErrorResponse, Validate, VersionResponse};
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Output;

#[test]
fn version_is_requestless_and_emits_one_strict_response() {
    let output = cargo_bin_cmd!("ai-daily-scanner")
        .arg("version")
        .write_stdin("ignored requestless stdin")
        .output()
        .expect("scanner version command should start");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let response: VersionResponse =
        serde_json::from_slice(&output.stdout).expect("stdout should be strict JSON");
    response
        .validate()
        .expect("version response should validate");
}

#[test]
fn missing_command_uses_the_transport_error_contract() {
    let output = cargo_bin_cmd!("ai-daily-scanner")
        .output()
        .expect("scanner should start");

    assert_transport_error(output);
}

#[test]
fn multiple_commands_use_the_transport_error_contract() {
    let output = cargo_bin_cmd!("ai-daily-scanner")
        .args(["version", "doctor"])
        .output()
        .expect("scanner should start");

    assert_transport_error(output);
}

#[test]
fn unknown_command_uses_the_transport_error_contract() {
    let output = cargo_bin_cmd!("ai-daily-scanner")
        .arg("unknown")
        .write_stdin("ignored")
        .output()
        .expect("scanner should start");

    assert_transport_error(output);
}

fn assert_transport_error(output: Output) {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let response: TransportErrorResponse =
        serde_json::from_slice(&output.stdout).expect("stdout should be strict JSON");
    response
        .validate()
        .expect("transport response should validate");
}
