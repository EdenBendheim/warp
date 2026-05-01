//! Pure-data tests for `OrchestrateEditState` transition logic.
//!
//! Full UI integration tests (cards rendered into a live `AIBlock` view)
//! belong under `crates/integration`. Stage 1 dispatch ordering / mid-batch /
//! M=0 / pre-dispatch coverage lives in
//! `crate::ai::blocklist::orchestrate_dispatch_tests`.
use ai::agent::action::{OrchestrateAgentRunConfig, OrchestrateExecutionMode, OrchestrateRequest};

use crate::ai::blocklist::block::OrchestrateEditState;

fn make_request(harness: &str, mode: OrchestrateExecutionMode) -> OrchestrateRequest {
    OrchestrateRequest {
        summary: "summary".to_string(),
        base_prompt: "base".to_string(),
        skills: Vec::new(),
        model_id: "auto".to_string(),
        harness_type: harness.to_string(),
        execution_mode: mode,
        agent_run_configs: vec![OrchestrateAgentRunConfig {
            name: "child".to_string(),
            prompt: "do work".to_string(),
            title: "Child agent".to_string(),
        }],
        auto_launch: false,
    }
}

#[test]
fn local_to_cloud_initializes_remote_with_empty_environment() {
    let mut state =
        OrchestrateEditState::from_request(&make_request("oz", OrchestrateExecutionMode::Local));
    assert!(matches!(
        state.execution_mode,
        OrchestrateExecutionMode::Local
    ));

    state.toggle_execution_mode_to_remote(true);
    let OrchestrateExecutionMode::Remote {
        environment_id,
        worker_host,
        computer_use_enabled,
    } = state.execution_mode
    else {
        panic!("expected Remote after toggle");
    };
    assert_eq!(environment_id, "");
    assert_eq!(worker_host, "warp");
    assert!(!computer_use_enabled);
}

#[test]
fn cloud_to_local_drops_environment() {
    let mut state = OrchestrateEditState::from_request(&make_request(
        "oz",
        OrchestrateExecutionMode::Remote {
            environment_id: "env-1".to_string(),
            worker_host: "warp".to_string(),
            computer_use_enabled: false,
        },
    ));
    state.toggle_execution_mode_to_remote(false);
    assert!(matches!(
        state.execution_mode,
        OrchestrateExecutionMode::Local
    ));
}

#[test]
fn local_to_cloud_resets_opencode_to_oz() {
    let mut state = OrchestrateEditState::from_request(&make_request(
        "opencode",
        OrchestrateExecutionMode::Local,
    ));
    state.toggle_execution_mode_to_remote(true);
    assert_eq!(state.harness_type, "oz");
}

#[test]
fn cloud_without_env_no_longer_disables_accept() {
    // Round 6 follow-up B2: empty environment_id is now a soft
    // recommendation surfaced inline in the Cloud editor (rendered as
    // `ui_warning_color` text), not a hard validation error. The Accept
    // button stays enabled so users can launch with no environment.
    let state = OrchestrateEditState::from_request(&make_request(
        "oz",
        OrchestrateExecutionMode::Remote {
            environment_id: String::new(),
            worker_host: "warp".to_string(),
            computer_use_enabled: false,
        },
    ));
    assert!(
        state.accept_disabled_reason().is_none(),
        "Cloud without env should NOT disable Accept (soft recommendation only)"
    );
}

#[test]
fn cloud_with_opencode_disables_accept() {
    // Bypassing the toggle helper that resets OpenCode to Oz so we can
    // exercise the validation gate's defensive coverage of the LLM-supplied
    // (Cloud, OpenCode) pairing.
    let state = OrchestrateEditState::from_request(&make_request(
        "opencode",
        OrchestrateExecutionMode::Remote {
            environment_id: "env-1".to_string(),
            worker_host: "warp".to_string(),
            computer_use_enabled: false,
        },
    ));
    let reason = state.accept_disabled_reason();
    assert!(reason.is_some(), "Cloud + OpenCode should disable Accept");
    assert!(reason.unwrap().contains("OpenCode"));
}

#[test]
fn local_with_any_harness_does_not_disable_accept() {
    for harness in ["oz", "claude", "gemini", "opencode"] {
        let state = OrchestrateEditState::from_request(&make_request(
            harness,
            OrchestrateExecutionMode::Local,
        ));
        assert!(
            state.accept_disabled_reason().is_none(),
            "Local + {harness} should allow Accept"
        );
    }
}

#[test]
fn cloud_with_env_and_non_opencode_harness_allows_accept() {
    for harness in ["oz", "claude", "gemini"] {
        let state = OrchestrateEditState::from_request(&make_request(
            harness,
            OrchestrateExecutionMode::Remote {
                environment_id: "env-1".to_string(),
                worker_host: "warp".to_string(),
                computer_use_enabled: false,
            },
        ));
        assert!(
            state.accept_disabled_reason().is_none(),
            "Cloud + env + {harness} should allow Accept"
        );
    }
}

#[test]
fn set_environment_id_no_op_in_local_mode() {
    let mut state =
        OrchestrateEditState::from_request(&make_request("oz", OrchestrateExecutionMode::Local));
    state.set_environment_id("env-1".to_string());
    assert!(matches!(
        state.execution_mode,
        OrchestrateExecutionMode::Local
    ));
}

#[test]
fn set_environment_id_updates_remote() {
    let mut state = OrchestrateEditState::from_request(&make_request(
        "oz",
        OrchestrateExecutionMode::Remote {
            environment_id: "old".to_string(),
            worker_host: "warp".to_string(),
            computer_use_enabled: false,
        },
    ));
    state.set_environment_id("new-env".to_string());
    let OrchestrateExecutionMode::Remote { environment_id, .. } = state.execution_mode else {
        panic!("expected Remote");
    };
    assert_eq!(environment_id, "new-env");
}

#[test]
fn to_request_round_trips_request_fields() {
    let req = make_request(
        "claude",
        OrchestrateExecutionMode::Remote {
            environment_id: "env-2".to_string(),
            worker_host: "warp".to_string(),
            computer_use_enabled: true,
        },
    );
    let state = OrchestrateEditState::from_request(&req);
    let round_tripped = state.to_request();
    assert_eq!(round_tripped.summary, req.summary);
    assert_eq!(round_tripped.base_prompt, req.base_prompt);
    assert_eq!(round_tripped.model_id, req.model_id);
    assert_eq!(round_tripped.harness_type, req.harness_type);
    assert_eq!(round_tripped.execution_mode, req.execution_mode);
    assert_eq!(round_tripped.agent_run_configs, req.agent_run_configs);
    assert!(
        !round_tripped.auto_launch,
        "Stage 1 always passes auto_launch=false"
    );
}

#[test]
fn local_to_cloud_idempotent_when_already_remote() {
    let mut state = OrchestrateEditState::from_request(&make_request(
        "oz",
        OrchestrateExecutionMode::Remote {
            environment_id: "env-1".to_string(),
            worker_host: "warp".to_string(),
            computer_use_enabled: true,
        },
    ));
    state.toggle_execution_mode_to_remote(true);
    let OrchestrateExecutionMode::Remote {
        environment_id,
        computer_use_enabled,
        ..
    } = state.execution_mode
    else {
        panic!("expected Remote");
    };
    assert_eq!(
        environment_id, "env-1",
        "toggle to Remote when already Remote should not clobber env"
    );
    assert!(
        computer_use_enabled,
        "toggle to Remote when already Remote should not clobber computer_use"
    );
}
