//! The service's command line: the unsigned fallback exists only in a development build with the
//! `dev-unsigned` feature (M1); usage errors exit 64.

use std::process::Command;

fn service(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nestor-runner-service")).args(arguments).output().unwrap()
}

#[cfg(not(feature = "dev-unsigned"))]
#[test]
fn without_the_feature_the_unsigned_fallback_is_an_unknown_option() {
    let output = service(&["--allow-unsigned-clients", "--help"]);
    assert_eq!(output.status.code(), Some(64), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument --allow-unsigned-clients"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("[--allow-unsigned-clients]"), "not in the usage");
}

#[cfg(feature = "dev-unsigned")]
#[test]
fn a_development_build_accepts_it_unless_built_with_a_team_id() {
    let output = service(&["--allow-unsigned-clients", "--help"]);
    if option_env!("NESTOR_TEAM_ID").is_some() {
        assert_eq!(output.status.code(), Some(64), "{output:?}");
    } else {
        assert_eq!(output.status.code(), Some(0), "{output:?}");
    }
}

#[test]
fn usage_errors_exit_64() {
    assert_eq!(service(&["--bogus"]).status.code(), Some(64));
    assert_eq!(service(&["--data-dir"]).status.code(), Some(64));
    assert_eq!(service(&["--help"]).status.code(), Some(0));
}
