//! E2E coverage for `aoe conductor` (issue #553). Verifies the gate is
//! enforced, dry-run status prints the expected preamble on an empty
//! profile, and the tick loop's `--once` mode round-trips a subprocess
//! reasoner and prints valid JSON.

use serial_test::serial;

use crate::harness::TuiTestHarness;

const EXPERIMENTAL_ENV: &str = "AOE_EXPERIMENTAL_AO_MODE";

#[test]
#[serial]
fn conductor_status_bails_without_env_var() {
    let h = TuiTestHarness::new("conductor_gate_off");
    let output = h.run_cli(&["conductor", "status"]);
    assert!(
        !output.status.success(),
        "conductor status should fail when gate is off; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(EXPERIMENTAL_ENV),
        "stderr should name the env var so the user knows how to opt in; got: {}",
        stderr
    );
}

#[test]
#[serial]
fn conductor_status_prints_empty_profile_hint_when_enabled() {
    let mut h = TuiTestHarness::new("conductor_gate_on_empty");
    h.set_env(EXPERIMENTAL_ENV, "1");
    let output = h.run_cli(&["conductor", "status"]);
    assert!(
        output.status.success(),
        "conductor status should succeed with the gate open; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No sessions"),
        "empty profile should print an explanatory line; stdout was: {}",
        stdout
    );
}

#[test]
#[serial]
fn conductor_ao_alias_matches_conductor() {
    // `aoe ao` was requested during the design thread as a shorter form
    // of `aoe conductor`. It must be a real clap alias, not a separate
    // subcommand.
    let mut h = TuiTestHarness::new("conductor_alias");
    h.set_env(EXPERIMENTAL_ENV, "1");
    let long = h.run_cli(&["conductor", "status"]);
    let short = h.run_cli(&["ao", "status"]);
    assert_eq!(
        long.status.code(),
        short.status.code(),
        "`aoe conductor status` and `aoe ao status` should have the same exit"
    );
    assert_eq!(
        String::from_utf8_lossy(&long.stdout),
        String::from_utf8_lossy(&short.stdout),
        "the alias should produce identical stdout to the primary form"
    );
}

#[test]
#[serial]
fn conductor_watch_once_returns_json_via_reasoner_shim() {
    // Point the watcher at a bash shim that emits a valid canned
    // recommendation envelope, exercising the full pipeline:
    // Storage -> observation -> reasoner subprocess -> parser -> JSON.
    let mut h = TuiTestHarness::new("conductor_watch_once");
    h.set_env(EXPERIMENTAL_ENV, "1");

    let shim_path = h.home_path().join("fake-claude.sh");
    let shim_body = "#!/bin/sh\n\
        printf '%s' '{\"recommendations\":[{\"session_id\":\"demo\",\"action\":{\"kind\":\"no_op\"},\"rationale\":\"empty fleet\"}]}'\n";
    std::fs::write(&shim_path, shim_body).expect("write shim");
    let mut perms = std::fs::metadata(&shim_path)
        .expect("metadata")
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    std::fs::set_permissions(&shim_path, perms).expect("chmod shim");

    let output = h.run_cli(&[
        "conductor",
        "watch",
        "--once",
        "--reasoner-binary",
        shim_path.to_str().expect("utf8 shim path"),
    ]);
    assert!(
        output.status.success(),
        "watch --once should succeed with a valid shim; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}; got: {stdout}"));
    let arr = value.as_array().expect("top-level should be an array");
    assert_eq!(arr.len(), 1, "shim emits one recommendation");
    assert_eq!(arr[0]["session_id"], "demo");
    assert_eq!(arr[0]["action"]["kind"], "no_op");
}
