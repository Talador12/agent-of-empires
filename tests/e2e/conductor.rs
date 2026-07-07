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

#[test]
#[serial]
fn conductor_task_crud_roundtrip() {
    // Exercises the whole task manager surface: add -> list --json ->
    // progress -> complete -> remove. Each step observes the effect of
    // the last through the on-disk task store.
    let mut h = TuiTestHarness::new("conductor_task_crud");
    h.set_env(EXPERIMENTAL_ENV, "1");

    let add = h.run_cli(&[
        "conductor",
        "task",
        "add",
        "--id",
        "ship-pr",
        "--title",
        "Ship the conductor PR",
        "--goal",
        "Land #553",
        "--keyword",
        "conductor",
        "--keyword",
        "553",
    ]);
    assert!(
        add.status.success(),
        "task add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let list = h.run_cli(&["conductor", "task", "list", "--json"]);
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    let tasks: serde_json::Value = serde_json::from_str(&stdout).expect("list --json parseable");
    let arr = tasks.as_array().expect("list is an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "ship-pr");
    assert_eq!(arr[0]["status"], "pending");
    assert_eq!(arr[0]["keywords"], serde_json::json!(["conductor", "553"]));

    // Progress bumps status to in_progress.
    let progress = h.run_cli(&[
        "conductor",
        "task",
        "progress",
        "--id",
        "ship-pr",
        "--note",
        "wrote the e2e tests",
    ]);
    assert!(progress.status.success());
    let list = h.run_cli(&["conductor", "task", "list", "--json"]);
    let tasks: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(tasks[0]["status"], "in_progress");
    assert_eq!(tasks[0]["progress_notes"][0]["note"], "wrote the e2e tests");

    // Complete flips status + sets completed_at.
    let complete = h.run_cli(&["conductor", "task", "complete", "--id", "ship-pr"]);
    assert!(complete.status.success());
    let list = h.run_cli(&["conductor", "task", "list", "--json"]);
    let tasks: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(tasks[0]["status"], "completed");
    assert!(tasks[0]["completed_at"].is_string());

    // Remove empties the store.
    let remove = h.run_cli(&["conductor", "task", "remove", "--id", "ship-pr"]);
    assert!(remove.status.success());
    let list = h.run_cli(&["conductor", "task", "list", "--json"]);
    let tasks: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert!(tasks.as_array().unwrap().is_empty());
}

#[test]
#[serial]
fn conductor_task_verbs_fail_on_unknown_id() {
    // Every mutating task verb should exit non-zero on a missing id so
    // scripts see the failure rather than silently continuing.
    let mut h = TuiTestHarness::new("conductor_task_unknown");
    h.set_env(EXPERIMENTAL_ENV, "1");

    for verb in ["remove", "complete"] {
        let out = h.run_cli(&["conductor", "task", verb, "--id", "ghost"]);
        assert!(
            !out.status.success(),
            "expected `task {verb} --id ghost` to fail; got success"
        );
    }

    let out = h.run_cli(&[
        "conductor",
        "task",
        "progress",
        "--id",
        "ghost",
        "--note",
        "hi",
    ]);
    assert!(
        !out.status.success(),
        "expected `task progress --id ghost` to fail; got success"
    );
}

#[test]
#[serial]
fn conductor_task_add_rejects_duplicate_id() {
    let mut h = TuiTestHarness::new("conductor_task_dup");
    h.set_env(EXPERIMENTAL_ENV, "1");

    let first = h.run_cli(&[
        "conductor",
        "task",
        "add",
        "--id",
        "dup",
        "--title",
        "First",
        "--goal",
        "test",
    ]);
    assert!(first.status.success());

    let second = h.run_cli(&[
        "conductor",
        "task",
        "add",
        "--id",
        "dup",
        "--title",
        "Second",
        "--goal",
        "test",
    ]);
    assert!(
        !second.status.success(),
        "duplicate id should fail; got success"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already exists"),
        "stderr should explain why; got: {stderr}"
    );
}

#[test]
#[serial]
fn conductor_status_json_has_expected_fields() {
    // A downstream consumer scripting against `aoe conductor status
    // --json` needs the field names to stay stable. An empty profile
    // still returns a valid empty JSON array.
    let mut h = TuiTestHarness::new("conductor_status_json");
    h.set_env(EXPERIMENTAL_ENV, "1");
    let output = h.run_cli(&["conductor", "status", "--json"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert!(v.is_array(), "status --json is an array; got {v}");
}

#[test]
#[serial]
fn conductor_watch_once_live_dispatches_no_op() {
    // Live-mode dry-run: shim emits NoOp, executor dispatches, JSON
    // response is the outcomes list rather than the recommendation list.
    let mut h = TuiTestHarness::new("conductor_watch_live");
    h.set_env(EXPERIMENTAL_ENV, "1");

    let shim_path = h.home_path().join("fake-claude.sh");
    std::fs::write(
        &shim_path,
        "#!/bin/sh\nprintf '%s' '{\"recommendations\":[{\"session_id\":\"demo\",\"action\":{\"kind\":\"no_op\"},\"rationale\":\"\"}]}'\n",
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&shim_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim_path, perms).unwrap();
    }

    let output = h.run_cli(&[
        "conductor",
        "watch",
        "--once",
        "--live",
        "--reasoner-binary",
        shim_path.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "watch --once --live should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    let arr = v.as_array().expect("outcomes is an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["outcome"], "no_op");
}
