//! Drives the local-to-cloud handoff lifecycle.
//!
//! Runs the prep + upload phases off the main thread by handing a `TouchedWorkspace`
//! to `agent_sdk::driver::upload_snapshot_for_handoff`, which mints a `prep_token`,
//! gathers patches and file contents, and uploads everything (plus a
//! `snapshot_state.json` manifest) to GCS.
//!
//! The actual cloud-agent spawn happens inside the handoff pane's
//! `AmbientAgentViewModel::submit_handoff` so the streaming `TaskSpawned` →
//! `SessionStarted` events drive the loading screen + shared-session join the same
//! way a normal cloud agent does. Doing the spawn here would leave us with only a
//! task id, no streaming hook, and a blank pane.

use std::sync::Arc;

use anyhow::Result;
use http_client::Client as HttpClient;

use crate::ai::agent::api::ServerConversationToken;
use crate::ai::agent_sdk::driver::upload_snapshot_for_handoff;
use crate::ai::blocklist::handoff::touched_repos::TouchedWorkspace;
use crate::server::server_api::ai::AIClient;

/// Outcome of a successful prep + upload. `submit_handoff` builds a
/// `SpawnAgentRequest` from this and dispatches it through the same
/// `spawn_agent_with_request` path that regular cloud-mode runs use.
///
/// The agent config (env, model, worker_host, computer_use_enabled, harness) is
/// intentionally not carried here — by the time `submit_handoff` consumes this, the
/// pane's env selector chip has already updated the model's `environment_id` and
/// `build_default_spawn_config` reads the rest from the model + global preferences.
pub(crate) struct HandoffPrepared {
    /// `handoff_prep_token` returned by `prepare_handoff_snapshot`. `None` when the
    /// touched workspace had no declarations — the cloud-side spawn skips snapshot
    /// rehydration in that case.
    pub prep_token: Option<String>,
    /// `fork_from_conversation_id` to set on the spawn request — always the source
    /// conversation's server token.
    pub fork_from_conversation_id: String,
    /// User prompt typed into the handoff pane.
    pub prompt: String,
}

/// Drive the prep + upload phases of a handoff. Runs entirely off the main thread;
/// callers should `ctx.spawn` this future so the local pane stays interactive
/// throughout. The actual `spawn_agent` call is intentionally NOT performed here
/// — see the module docs for why.
pub(crate) async fn run_handoff(
    source_conversation_id: ServerConversationToken,
    workspace: TouchedWorkspace,
    prompt: String,
    client: Arc<dyn AIClient>,
    http: Arc<HttpClient>,
) -> Result<HandoffPrepared> {
    let repo_paths = workspace.repos.into_iter().map(|r| r.git_root).collect();
    let prep_token = upload_snapshot_for_handoff(
        repo_paths,
        workspace.orphan_files,
        client,
        http.as_ref(),
        &source_conversation_id,
    )
    .await?;

    Ok(HandoffPrepared {
        prep_token,
        fork_from_conversation_id: source_conversation_id.as_str().to_string(),
        prompt,
    })
}
