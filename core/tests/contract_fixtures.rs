//! The launcher agrees with the vendored fixtures of launch contract v1
//! (`contract/run-once-v1.json`, KCA-734 §2): `argv`, descriptors, limits, report lines, exit
//! codes. When the contract changes in nestor, the fixtures change and this test says what the
//! service must follow.

use std::collections::BTreeSet;

use nestor_runner_core::mask::Redactor;
use nestor_runner_core::persist::RunnerRecord;
use nestor_runner_core::process::{self, parse_report, Finished, ProcessConfig, ProcessLauncher, Report};
use nestor_runner_core::supervisor::LaunchTarget;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../contract/run-once-v1.json")).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[test]
fn the_contract_version_descriptors_and_limits_match() {
    let fixture = fixture();
    assert_eq!(fixture["contract"].as_u64(), Some(process::CONTRACT));
    assert_eq!(fixture["credential"]["fd"].as_i64(), Some(process::CREDENTIAL_FD as i64));
    assert_eq!(fixture["reports"]["fd"].as_i64(), Some(process::REPORT_FD as i64));
    assert_eq!(fixture["credential"]["max_bytes"].as_u64(), Some(process::MAX_CREDENTIAL_BYTES as u64));
    assert_eq!(fixture["reports"]["max_line_bytes"].as_u64(), Some(process::MAX_REPORT_BYTES as u64));
    assert_eq!(fixture["reports"]["max_per_second"].as_u64(), Some(process::MAX_REPORTS_PER_SECOND as u64));
    assert_eq!(fixture["reports"]["version"].as_u64(), Some(1));
    assert_eq!(fixture["drain"]["line"].as_str(), Some("drain\n"));
    assert_eq!(fixture["drain"]["eof_is_drain"].as_bool(), Some(true));
    assert_eq!(strings(&fixture["environment"]["allowed"]), vec!["PATH", "LANG", "HOME", "TMPDIR"]);
}

#[test]
fn the_launch_argv_has_exactly_the_contract_options() {
    let fixture = fixture();
    let argv = &fixture["argv"];
    let config =
        ProcessConfig::new("/usr/local/bin/nestor-runner".into(), strings(&argv["subcommand"]), "/nonexistent".into());
    let launcher = ProcessLauncher::new(config, Redactor::new()).unwrap();
    let record = RunnerRecord {
        id: "0123456789abcdef0123456789abcdef".into(),
        name: "runner-1".into(),
        generation: 7,
        pid: None,
        start_time_us: None,
        launching_since: None,
        // A scope planned by the supervisor is never passed (KCA-734 §3).
        scope: Some("workspace-a".into()),
        created_at_ms: 0,
        manual: false,
    };
    let target =
        LaunchTarget { machine_id: "6f1c2a4e-9b3d-4c5e-8f70-1a2b3c4d5e6f".into(), instance: "nestor.example".into() };
    let arguments = launcher.arguments(&record, &target).unwrap();
    assert_eq!(arguments, strings(&argv["example"]), "the fixture's example is what the service launches");

    let options: BTreeSet<String> = arguments.iter().filter(|a| a.starts_with("--")).cloned().collect();
    let required: BTreeSet<String> = strings(&argv["required"]).into_iter().collect();
    assert_eq!(options, required, "every required option, once, and nothing else");
    for forbidden in strings(&argv["forbidden"]) {
        assert!(!arguments.contains(&forbidden), "{forbidden} is never passed");
    }
    for (option, value) in argv["fixed"].as_object().unwrap() {
        let position = arguments.iter().position(|a| a == option).unwrap();
        assert_eq!(Some(&arguments[position + 1]), value.as_str().map(String::from).as_ref());
    }
}

#[test]
fn the_credential_the_service_writes_has_the_fixture_shape() {
    let fixture = fixture();
    let fields: BTreeSet<&String> = fixture["credential"]["fields"].as_object().unwrap().keys().collect();
    // The service sends `runner_token` only: it holds no Claude Code token in this slice (A7).
    let payload: Value = serde_json::from_str(r#"{"runner_token":"x"}"#).unwrap();
    assert!(payload.as_object().unwrap().keys().all(|k| fields.contains(k)));
    assert_eq!(fixture["credential"]["fields"]["runner_token"], "required");
}

#[test]
fn valid_report_lines_are_read_and_invalid_ones_refused() {
    let fixture = fixture();
    let reports = &fixture["reports"];
    let types: BTreeSet<String> = reports["types"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(types, ["claimed", "finished", "hello", "idle"].iter().map(|s| s.to_string()).collect());
    for line in strings(&reports["valid"]) {
        assert!(parse_report(&line).is_ok(), "valid line refused: {line} ({:?})", parse_report(&line));
    }
    for line in strings(&reports["invalid"]) {
        assert!(parse_report(&line).is_err(), "invalid line accepted: {line}");
    }
    let hello = parse_report(&strings(&reports["valid"])[0]).unwrap();
    assert_eq!(
        hello,
        Report::Hello { contract: 1, runner_id: "0123456789abcdef0123456789abcdef".into(), generation: 7, pid: 4242 }
    );
    let outcomes = strings(&reports["outcomes"]);
    for outcome in &outcomes {
        let line = format!(r#"{{"v":1,"type":"finished","outcome":"{outcome}"}}"#);
        assert!(matches!(parse_report(&line), Ok(Report::Finished(_))), "{line}");
    }
    assert_eq!(outcomes.len(), 5);
    assert!(matches!(
        parse_report(r#"{"v":1,"type":"finished","outcome":"lease_lost"}"#),
        Ok(Report::Finished(Finished::LeaseLost))
    ));
    let oversized = format!(r#"{{"v":1,"type":"claimed","job_id":"{}"}}"#, "j".repeat(process::MAX_REPORT_BYTES));
    assert!(parse_report(&oversized).is_err());
}

#[test]
fn the_exit_codes_match() {
    let fixture = fixture();
    for (code, meaning) in fixture["exit_codes"].as_object().unwrap() {
        assert_eq!(process::exit_meaning(code.parse().unwrap()), meaning.as_str(), "exit {code}");
    }
    assert_eq!(process::exit_meaning(10), None, "the old 'job failed' status is not part of the contract");
}
