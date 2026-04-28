# Local-to-Cloud Handoff — Tech Spec
Product spec: `specs/REMOTE-1486/PRODUCT.md`
Linear: [REMOTE-1486](https://linear.app/warpdotdev/issue/REMOTE-1486)
## Context
The product spec describes a chip + `/oz-cloud-handoff` slash command that opens a split cloud-mode pane next to the local agent to hand off the active local Oz conversation to the cloud. The user types the follow-up prompt and submits inside the pane's existing cloud-mode input bar; the cloud agent runs in a fresh sandbox, gets a forked copy of the conversation history, and rehydrates from a workspace snapshot taken on the local machine.
The pieces this builds on already exist:
- **Cloud→cloud handoff and rehydration** (REMOTE-1290): `snapshots/{run_id}/{execution_id}/` GCS layout, the `<system-message>`-wrapped `UserQuery` rehydration prompt injected by `logic/ai/multi_agent/runtime/interceptors/input.go:433` via `ResolveHandoffRehydrationPrompt` in `../warp-server/logic/ai/ambient_agents/handoff_rehydration.go`. Server discovers snapshot files by GCS path convention (`ListSnapshotFiles` in `../warp-server/logic/ai/ambient_agents/attachment_storage.go:281`), no DB column needed.
- **End-of-run snapshot pipeline** (REMOTE-1332): `app/src/ai/agent_sdk/driver/snapshot.rs` reads JSONL declarations and uploads patches + a `snapshot_state.json` manifest. The pipeline is generic over JSONL — it doesn't care who wrote the declarations or where the artifacts go.
- **`task.AgentConversationID` is the load-bearing field**: `RunAgentRequest` already accepts `ConversationID *string` at `../warp-server/router/handlers/public_api/agent_webhooks.go:205`, persisted onto the new task as `AgentConversationID`. The cloud-side resume happens via the `--task-id` chain: the worker passes only `--task-id` (not `--conversation`); the embedded CLI's `--task-id` path fetches the task metadata, reads `conversation_id` off it, and resumes via `get_ai_conversation`. See section 8 for the full trace.
- **Local fork**: `BlocklistAIHistoryModel::fork_conversation` at `app/src/ai/blocklist/history_model.rs:1016` already produces a forked AIConversation by copying tasks. We need a server-side analogue that operates on a `server_conversation_token`.
- **EnvironmentSelector**: existing component at `app/src/ai/blocklist/agent_view/agent_input_footer/environment_selector.rs` reads `CloudAmbientAgentEnvironment::get_all` from `app/src/ai/cloud_environments/mod.rs:114`. Each env carries `github_repos: Vec<GithubRepo>` so overlap with our touched-repo set is computable client-side.
- **Agent input footer chips**: rendered by `app/src/ai/blocklist/agent_view/agent_input_footer/chips.rs`. The chip system is data-driven via `ChipResult` and slot positions (left/right). We add a new chip kind here.
- **Slash commands**: registered in `app/src/search/slash_command_menu/static_commands/commands.rs`. Commands flow through dispatch in `app/src/terminal/input/slash_commands/mod.rs`.
## Diagram
```mermaid
sequenceDiagram
    participant U as User
    participant C as Local Warp Client
    participant LC as Local Conversation
    participant API as warp-server (public API)
    participant DB as Postgres
    participant GCS
    participant Disp as Dispatcher
    participant Wk as Worker
    participant Sand as New Cloud Sandbox
    U->>C: Click "Hand off to cloud" chip
    C->>C: Split a fresh cloud-mode pane next to the local pane
    C->>U: Show handoff pane (standard cloud-mode input)
    C->>LC: Walk action history → touched repos + orphan files (async)
    U->>C: Pick env (or accept default), type prompt, submit
    C->>C: Build declarations programmatically (no script)
    C->>API: POST /agent/handoff/prepare-snapshot {files: [{filename, mime_type}]}
    API-->>C: {prep_token, expires_at, uploads: [{filename, upload_url}]}
    par Snapshot uploads
        C->>GCS: PUT each file under handoff_prep/{prep_token}/...
    end
    C->>API: POST /agent/runs {fork_from_conversation_id, handoff_prep_token, prompt, config}
    API->>DB: Create forked AI conversation (copy tasks from source)
    API->>DB: Create task with AgentConversationID = forked_conversation_id
    API->>GCS: Move handoff_prep/{prep_token}/* → snapshots/{task_id}/0/
    API->>DB: Insert synthetic ai_run_executions row (id=0, state=ENDED) for task
    API-->>C: {task_id, run_id}
    C->>C: Pane transitions into live cloud-mode session (queued-prompt + setup affordances)
    C->>U: User stays in the same pane; cloud agent's first turn streams in
    Note over LC: Local conversation continues, user can keep typing
    Disp->>DB: Pop queued task; create new execution (id=1) in PENDING
    Disp->>Wk: Assign task with task.AgentConversationID = <forked_id>
    Wk->>Sand: oz agent run --task-id <new_task_id> --sandboxed (no --conversation flag)
    Sand->>API: GET /agent/runs/<new_task_id> (fetch task metadata)
    API-->>Sand: AmbientAgentTask { conversation_id: <forked_id>, ... }
    Sand->>API: get_ai_conversation(<forked_id>) (resume via the --task-id→conversation_id chain)
    API-->>Sand: ConversationData (forked source's tasks/messages)
    Sand->>Sand: driver_options.resume = ResumeOptions::Oz(Historical{...})
    Sand->>API: GET /agent/runs/<new_task_id>/handoff/attachments
    API-->>Sand: presigned download URLs (latest ENDED execution = id=0)
    Sand->>GCS: Download handoff snapshot files
    Sand->>API: StartFromAmbientRunPrompt (resolves rehydration message)
    API-->>Sand: <system-message>-wrapped rehydration UserQuery + user prompt
    Sand->>Sand: Apply patches via git apply, then handle user prompt
```
## Proposed changes
### 1. Touched-repo derivation (client)
The client-side handoff lives under `app/src/ai/blocklist/handoff/`:
- `path_extraction.rs`: walks an `AIConversation` and collects every filesystem path the local agent touched, capped to the most recent `MAX_TOOL_CALLS_TO_SCAN = 500` action results.
- `touched_repos.rs`: groups those paths into git repos and orphan files, gathers per-repo metadata, and exposes the env-overlap pick.
```rust path=null start=null
pub(crate) struct TouchedWorkspace {
    pub repos: Vec<TouchedRepo>,
    pub orphan_files: Vec<PathBuf>,
}
pub(crate) struct TouchedRepo {
    pub git_root: PathBuf,
    pub repo_id: Option<GithubRepo>, // parsed from `git remote get-url origin`, best-effort
}
pub(crate) async fn derive_touched_workspace(paths: Vec<PathBuf>) -> TouchedWorkspace;
pub(crate) fn extract_paths_from_conversation(conversation: &AIConversation) -> Vec<PathBuf>;
```
`extract_paths_from_conversation` covers these action sources: `RequestFileEdits`, `ReadFiles`, `Grep`, `FileGlob` / `FileGlobV2`, `SearchCodebase` (only for absolute `partial_paths`), `InsertCodeReviewComments`, `UploadArtifact`, plus the per-exchange `working_directory` for shell commands. Both the conversation walk and the filesystem walks run inside a `ctx.spawn`ed future so the pane renders immediately on chip click.
`derive_touched_workspace` walks each input path up to the nearest `.git` directory and dispatches `git remote get-url origin` per repo via `command::r#async::Command` (no per-call OS thread). Per-repo `branch` / `head_sha` metadata is **not** gathered here — the existing `repo_metadata` helper in `app/src/ai/agent_sdk/driver/snapshot.rs` does that during snapshot upload, keeping the rehydration prompt's plumbing in `../warp-server/logic/ai/ambient_agents/handoff_rehydration.go` unchanged.
```rust path=null start=null
/// Picks the env with the most overlap with the touched repos, ties broken by recency.
/// Returns `None` when no env contains any of the touched repos; callers leave the
/// existing env-selector default in place (`last_selected_environment_id` →
/// most-recently-used → none) in that case.
pub(crate) fn pick_handoff_overlap_env(
    workspace: &TouchedWorkspace,
    envs: Vec<CloudAmbientAgentEnvironment>,
) -> Option<SyncId>;
```
The function sorts `envs` internally via `sort_environments_by_recency` (the same helper the env selector uses) so ties on overlap count resolve to the most-recently-used env.
### 2. Handoff pane: split-pane bootstrap
There is no dedicated modal view. On chip click or `/oz-cloud-handoff` activation, `Workspace::start_local_to_cloud_handoff` (in `app/src/workspace/view.rs`) drives the open path:
1. Resolve the source conversation from the active session view's `BlocklistAIHistoryModel::active_conversation` (must be non-empty and have a `server_conversation_token`).
2. Call `pane_group.add_ambient_agent_pane(ctx)` to split a new cloud-mode pane next to the active pane (mirrors `Workspace::open_network_log_pane`'s pattern but pre-mounts the cloud-mode chrome).
3. Pre-fill the new pane's prompt editor when the slash command supplied an argument (slash command args do not flow through `PendingHandoff` itself).
4. If the source conversation didn't resolve, return early — the new pane stays as an ordinary fresh cloud-mode pane with no handoff context. Non-eligible clicks are not surfaced as errors.
5. Otherwise, seed `PendingHandoff` onto the new pane's `AmbientAgentViewModel` (see below) and `ctx.spawn` an async block that calls `extract_paths_from_conversation` and then `derive_touched_workspace(...)`. When derivation completes, apply `pick_handoff_overlap_env(...)` to the model's `environment_id` (the env selector's `ensure_default_selection` already runs first; the handoff-aware pick overrides on a real overlap match and is skipped on no-overlap).
#### Handoff context on `AmbientAgentViewModel`
Add a `pending_handoff: Option<PendingHandoff>` field on `AmbientAgentViewModel` (`app/src/terminal/view/ambient_agent/model.rs`):
```rust path=null start=null
pub(crate) struct PendingHandoff {
    pub(crate) source_conversation_id: ServerConversationToken,
    /// `None` until `derive_touched_workspace` completes.
    pub(crate) touched_workspace: Option<TouchedWorkspace>,
    /// Gates `submit_handoff` against double-submits and surfaces inline errors.
    pub(crate) submission_state: HandoffSubmissionState, // Idle | Starting | Failed(String)
}
```
`is_local_to_cloud_handoff()` returns `pending_handoff.is_some()` and is the single source of truth for "this pane is in handoff mode". The new pane needs that predicate true from the moment it opens so the V2-input suppression and the submit-interception logic both fire before the spawn.
#### Suppress `CloudModeInputV2` for handoff panes
Update `Input::is_cloud_mode_input_v2_composing` (`app/src/terminal/input/agent.rs:65`) to also require `!ambient_agent_view_model.is_local_to_cloud_handoff()`. V2 is for fresh cloud-mode runs only; handoff stays on the existing input UI regardless of the flag's state.
#### No banner UI in V0
V0 ships with no dedicated handoff banner. `PendingHandoffChanged` triggers a `ctx.notify()` for future banner work; today the only user-visible effects of derivation completing are (a) the env selector's default updating to the overlap winner and (b) `submit_handoff` being unblocked. Submission errors surface inline via `HandoffSubmissionState::Failed` for future banner work to consume.
### 3. Chip and slash command (client)
- Add a new `AgentToolbarItemKind::HandoffToCloud` variant in `app/src/ai/blocklist/agent_view/agent_input_footer/toolbar_item.rs`. The chip is rendered with the `bundled/svg/upload-cloud-01.svg` icon. Visibility is gated only on `FeatureFlag::OzHandoff && FeatureFlag::LocalToCloudHandoff`; conversation eligibility (synced token, non-empty, harness) is enforced via fall-through inside `Workspace::start_local_to_cloud_handoff` rather than at the visibility level. The chip is also hidden from session viewers (`available_to_session_viewer()` returns `!status.is_viewer()`).
- Add the chip to `default_right()` (and `all_available()`) in the same file, gated on the same flags so the user-facing toolbar configurator picks it up.
- The chip's on-click action emits `AgentInputFooterEvent::OpenHandoffPane { initial_prompt: None }`. The terminal `Input` subscriber forwards it to `WorkspaceAction::OpenLocalToCloudHandoffPane`.
- Add `OZ_CLOUD_HANDOFF` to `app/src/search/slash_command_menu/static_commands/commands.rs`:
  ```rust path=null start=null
  pub static OZ_CLOUD_HANDOFF: LazyLock<StaticCommand> = LazyLock::new(|| StaticCommand {
      name: "/oz-cloud-handoff",
      description: "Hand off this conversation to a cloud agent",
      icon_path: "bundled/svg/upload-cloud-01.svg",
      availability: Availability::AGENT_VIEW
          | Availability::ACTIVE_CONVERSATION
          | Availability::AI_ENABLED,
      auto_enter_ai_mode: false,
      argument: Some(Argument::optional()
          .with_hint_text("<optional follow-up prompt>")
          .with_execute_on_selection()),
  });
  ```
  Gate registration on the same flags inside `all_commands()` in that file.
- Wire the slash command's execute path in `app/src/terminal/input/slash_commands/mod.rs` to dispatch `WorkspaceAction::OpenLocalToCloudHandoffPane { initial_prompt: argument.cloned().filter(|s| !s.is_empty()) }`. Like the chip, conversation eligibility is enforced in the workspace handler, so the slash command itself only checks the feature flags.
### 4. Snapshot pipeline: local-mode entry point
Add a sibling entry point in `app/src/ai/agent_sdk/driver/snapshot.rs` that reuses the existing gathering and upload internals (`gather_snapshot_entries`, `apply_per_run_cap`, `upload_prepared_snapshot_files`) but skips `run_declarations_script`:
```rust path=null start=null
pub(crate) async fn upload_snapshot_for_handoff(
    repo_paths: Vec<PathBuf>,
    orphan_file_paths: Vec<PathBuf>,
    client: Arc<dyn AIClient>,
    http: &http_client::Client,
    source_conversation_id: &ServerConversationToken,
) -> Result<Option<String>>;
```
- Translates the input paths into the same internal `Vec<DeclarationEntry>` that `parse_declarations` produces today (repos → `EntryKind::Repo`, orphan files → `EntryKind::File`).
- Uploads through `AIClient::prepare_handoff_snapshot` rather than the existing `HarnessSupportClient::get_snapshot_upload_targets`, since at this point in the flow there's no task yet — only a `prep_token` and a GCS prefix.
- `source_conversation_id` is used only for log-correlation; the on-the-wire request identifies the upload prefix solely by the minted token.
- Returns `Ok(Some(prep_token))` when a token was minted (regardless of how many blobs landed; cloud-side rehydration matches the cloud→cloud best-effort posture). Returns `Ok(None)` when the workspace was empty so callers spawn the cloud agent without a `handoff_prep_token`. `Err(_)` only for hard failures of `prepare_handoff_snapshot` itself (auth, etc.). If the prep token was minted but every blob upload failed, the call is also routed through `report_error!` so on-call alerting catches it.
- New server endpoint `POST /agent/handoff/prepare-snapshot` returns `{prep_token, expires_at, uploads: [{filename, upload_url}]}` scoped to `handoff_prep/{prep_token}/`. The handler mints a UUID-v4 `prep_token`, authorizes against the user, and generates URLs via the existing `GeneratePresignedUploadURLs`-style helper but with a different prefix. No DB writes.
### 5. Server-side conversation fork
The existing fork mechanism is client-driven: `BlocklistAIHistoryModel::fork_conversation` (`app/src/ai/blocklist/history_model.rs:1016`) copies tasks locally, then the next request sends `forked_from_conversation_id` + `tasks` together; the server (`router/middleware/set_conversation_info.go:45`) mints a new UUID and records `forked_from_conversation_id` for telemetry only (no DB column persists it). That doesn't fit local→cloud: the cloud sandbox has no source-task in memory, and we need `task.AgentConversationID` to point at a materialized conversation at task-creation time so the local pane can fetch the fork immediately.
We add a server-side helper that materializes the fork synchronously:
```go path=null start=null
// ForkConversationForHandoff copies an existing conversation's GCS data and metadata into a
// new conversation owned by `principal`. Returns the new conversation_id.
func ForkConversationForHandoff(
    ctx context.Context,
    db database.SqlQuerier,
    datastores types.Stores,
    sourceConversationID string,
    principal types.Principal,
) (string, error)
```
Location: alongside `UpsertAIConversationMetadata` / `CreateThirdPartyAIConversation` in `../warp-server/logic/ai_conversation_object.go`. Steps:
1. **Authorize.** `GetAIConversationObjectInfo(sourceConversationID)` + require `ViewAction` for `principal` (mirrors `CheckAndRecordConversationAccess` at `ai_conversation_object.go:603`); reject with `NotAuthorizedError` otherwise.
2. **Read source data.** `gcs.ReadConversationDataFromGCS(ctx, sourceConversationID)` (`logic/ai/multi_agent/gcs/conversation_data.go:120`). The conversation's task/message tree is stored as `{conversation_id}.pb` in the `warp-server-ai-conversations` bucket and is the single source of truth for `final_task_list`.
3. **Read source metadata.** `AIConversationMetadataStore.GetUsageByConversationIDs([sourceConversationID])` so the fork inherits `title`, `working_directory`, `harness`, `latest_git_branch`.
4. **Mint and write.** `newID := uuid.NewString()`, then `gcs.WriteConversationDataToGCS(ctx, newID, data, nil)` with the bytes from step 2.
5. **Insert metadata + WD object.** `UpsertAIConversationMetadata(..., shouldCreateConversationObject: true)` (`logic/ai_conversation_object.go:43`) inserts the metadata row and creates the `object_metadata` row owned by `principal` (the user owns their fork; the source's permissions are not inherited).
6. **Set `has_gcs_data = TRUE`** via `AIConversationMetadataStore.SetHasGCSData(newID, true)` so the GraphQL `list_ai_conversations` filter (`logic/ai_conversation_object.go:484`) picks it up.
7. **Return `newID`** and emit a structured log line linking source→fork→principal.
For pathologically large conversations (10s of MB), `client.CopierFrom(...)` on `cloud_storage.Client` keeps bytes in GCS; the simple read+write path is fine for V0. No lineage column is persisted today; if one is added later, the helper can populate it.
### 6. Server-side `RunAgentRequest` extensions
Extend `RunAgentRequest` in `../warp-server/router/handlers/public_api/agent_webhooks.go:199` with two new fields:
```go path=null start=null
type RunAgentRequest struct {
    // existing fields...
    ForkFromConversationID *string `json:"fork_from_conversation_id,omitempty"`
    HandoffPrepToken       *string `json:"handoff_prep_token,omitempty"`
}
```
`enqueueAgentRun` is updated:
- If `ForkFromConversationID` is set, call `ForkConversationForHandoff(...)` to mint `<forked_id>`, then set `req.ConversationID = &<forked_id>` (overriding any caller value). Existing logic at `agent_webhooks.go:381` continues to set `task.AgentConversationID` from `req.ConversationID`.
- If `HandoffPrepToken` is set, after the task is created: (1) server-side copy + delete every object under `handoff_prep/{prep_token}/` to `snapshots/{task.ID}/0/` (new helper in `attachment_storage.go`); (2) insert a synthetic `ai_run_executions` row with `run_id = task.ID, id = 0, state = ENDED`. The synthetic execution makes `GetLatestEndedExecutionForRun(task.ID)` find it, so the existing rehydration path in `handoff_rehydration.go:105` Just Works without modification.
- Both new fields are gated behind `local_to_cloud_handoff_enabled` (server-side flag, mirroring the client `LocalToCloudHandoff`).
- Authorization: user must have view access on `ForkFromConversationID` (existing AI conversation auth checks). The `HandoffPrepToken` only authorizes uploads back to the prefix that minted it.
- Error handling: fork failure aborts task creation. Snapshot move failure after task creation is logged as a WARN; the task is still created without rehydration content (best-effort, matching cloud→cloud).
### 7. Client API surface and handoff orchestrator
Add to `app/src/server/server_api/ai.rs`:
- `AIClient::prepare_handoff_snapshot(PrepareHandoffSnapshotRequest) → PrepareHandoffSnapshotResponse` against `POST /agent/handoff/prepare-snapshot`. The request carries only `files: Vec<HandoffSnapshotFileInfo>` (filename + mime type per file); the response is `{prep_token, expires_at, uploads: [{filename, upload_url}]}`.
- Extend `SpawnAgentRequest` (already JSON-serializes) with the two new fields:
  ```rust path=null start=null
  pub fork_from_conversation_id: Option<String>,
  pub handoff_prep_token: Option<String>,
  ```
The client-side handoff orchestrator (`app/src/ai/blocklist/handoff/orchestrator.rs::run_handoff`) is a single `ctx.spawn`ed future that owns only the prep + upload phases; the actual cloud-agent spawn happens inside the pane's `AmbientAgentViewModel::submit_handoff` (§7a) so the existing streaming flow is reused unchanged. `run_handoff` calls `upload_snapshot_for_handoff` (§4) to mint the prep token, gather repo patches and orphan-file contents, and upload everything to `handoff_prep/{prep_token}/` (`source_conversation_id` is passed only for log-correlation). It returns `HandoffPrepared { prep_token: Option<String>, fork_from_conversation_id: String, prompt: String }`.
The agent config (env, model, worker_host, computer_use_enabled, harness) is intentionally not threaded through `HandoffPrepared`. By the time `submit_handoff` consumes it, the user has already picked an env via the pane's existing env selector chip; `build_default_spawn_config` reads everything else from the model + global preferences.
Failures of the prep / upload phase set `pending_handoff.submission_state = Failed(msg)`. V0 has no banner; the user retries by re-submitting from the same pane. Failures of the spawn itself surface via the model's existing cloud-mode error rendering.
### 7a. Submit interception in the handoff pane (client)
The handoff pane is a regular cloud-mode pane, so the user's submission flows through the existing input dispatch path. We intercept it when `AmbientAgentViewModel::is_local_to_cloud_handoff()` is true (i.e. `pending_handoff.is_some()`) so the orchestrator runs *before* the spawn.
#### `AmbientAgentViewModel::submit_handoff`
```rust path=null start=null
pub(crate) fn submit_handoff(
    &mut self,
    prompt: String,
    attachments: Vec<AttachmentInput>,
    ctx: &mut ModelContext<Self>,
);
```
Flow:
1. No-op if `pending_handoff` is absent, derivation hasn't completed (`touched_workspace.is_none()`), or `submission_state` is already `Starting`.
2. Set `submission_state = Starting` and emit `PendingHandoffChanged`.
3. `ctx.spawn` the orchestrator with the model's `source_conversation_id` and `touched_workspace`.
4. On success, build a `SpawnAgentRequest` with `fork_from_conversation_id` + `handoff_prep_token` set and `config = Some(self.build_default_spawn_config(ctx))`, then call `self.spawn_agent_with_request(request, ctx)` — the same helper the regular `spawn_agent` path uses. This flips the model to `WaitingForSession` and emits `DispatchedAgent`.
5. On failure, set `submission_state = Failed(msg)` so the user can retry.
#### Wiring submit interception
The submit dispatch in `Input::handle_input_action` (`app/src/terminal/input.rs`) routes through `submit_handoff` instead of `spawn_agent` when `model.is_local_to_cloud_handoff()` is true. `pending_handoff` is seeded by the chip / slash command's open path (§2) and is not cleared after the spawn — it stays so post-spawn flows that query `is_local_to_cloud_handoff()` (queued-prompt rendering, V2-input suppression) keep behaving consistently.
#### DispatchedAgent + queued-prompt rendering
`DispatchedAgent` (`app/src/terminal/view/ambient_agent/view_impl.rs`) renders the user's prompt via `insert_cloud_mode_queued_user_query_block` (REMOTE-1454's helper) when `is_local_to_cloud_handoff()` is true. The block is removed on the same transitions the non-oz harness path already handles in `handle_ambient_agent_event`: `Failed`, `Cancelled`, `NeedsGithubAuth`, `HarnessCommandStarted`. For the Oz handoff specifically, the first `AppendedExchange` also clears the block (the analogous "harness CLI started" transition for Oz). Each path calls `remove_pending_user_query_block(ctx)` (idempotent). The cloud agent's exchanges flow into the pane via the shared-session replication path that regular cloud-mode runs already use.
### 8. How the conversation reaches the new sandbox (no worker or sandbox CLI changes)
The only invariant the new task needs to satisfy is `task.AgentConversationID = <forked_id>`. From there, the existing `--task-id` chain plumbs the conversation into the cloud agent without any new client-side or worker-side changes:
1. **Worker.** `oz-agent-worker/internal/common/task_utils.go::AugmentArgsForTask` passes only `--task-id <T>` (never `--conversation`). Pinned by `task_utils_test.go:152` ("does not forward --conversation even when AgentConversationID is set").
2. **Embedded CLI.** Inside the sandbox, `setup_and_run_driver` (`app/src/ai/agent_sdk/mod.rs:545`) sees `args.task_id = Some(T)` and `args.conversation = None`. `build_driver_options_and_task` fetches the task via `get_ambient_agent_task(T)` (`mod.rs:1031-1051`); the returned `AmbientAgentTask.conversation_id` (= `task.AgentConversationID`) is merged into `resume_conversation_id`.
3. **Resume.** `load_conversation_information(<forked_id>, HarnessKind::Oz)` (`mod.rs:1105`) calls `get_ai_conversation(<forked_id>)` and produces `ResumeOptions::Oz(ConversationRestorationInNewPaneType::Historical { conversation, ... })`. The terminal driver restores the conversation and the agent starts with the forked history visible.
Our change wires the front of this chain: the client sends `POST /agent/runs` with `fork_from_conversation_id = <local_token>` (and deliberately does *not* set the existing `conversation_id` field, which has resume semantics rather than fork semantics). `enqueueAgentRun` calls `ForkConversationForHandoff(<local_token>)` to mint `<forked_id>`, sets `req.ConversationID = &<forked_id>`, and the existing line `agent_webhooks.go:381` plumbs it onto `task.AgentConversationID`. Callers should set exactly one of `conversation_id` (resume) and `fork_from_conversation_id` (fork); both live on `SpawnAgentRequest` / `RunAgentRequest` and pick different branches inside `enqueueAgentRun`.
### 9. Sandbox-side: rehydration prompt (no client-side changes)
With the conversation-resume side covered above, the only remaining sandbox-side work is the rehydration prompt that tells the agent to apply the snapshot patches:
- `fetch_and_download_handoff_snapshot_attachments` (`app/src/ai/agent_sdk/driver/attachments.rs:68`) calls `GET /agent/runs/:runId/handoff/attachments`. The server resolves this against `snapshots/{run_id}/{latest_ended_execution_id}/` — our synthetic id=0 ENDED execution is what `GetLatestEndedExecutionForRun` finds, so this Just Works.
- The runtime's rehydration message construction (`logic/ai/multi_agent/runtime/interceptors/input.go:433` → `resolveHandoffRehydrationMessage`) is the same code path cloud→cloud handoff uses. It lists the snapshot files at the same prefix and prepends the `<system-message>`-wrapped UserQuery to the runtime's first input. No changes to either codepath.
### 10. Feature flags
Add `FeatureFlag::LocalToCloudHandoff` in `crates/warp_features/src/lib.rs`. The chip, slash command, client API methods, and server endpoint behavior are all gated on `OzHandoff && LocalToCloudHandoff`. Both flags must be enabled for the feature to function.
On the server, mirror with a `local_to_cloud_handoff_enabled` flag in `config/features/features.go`. The server feature-flag check happens at the request handler level (returns 404 / `feature not available` when off). This mirrors `CloudToCloudHandoffEnabled` which already exists.
## Risks and mitigations
- **Prep token expires before task creation.** The `prep_token` is short-lived (15 min, matching presigned URL lifetime); a stalled handoff past expiry would fail with a "can't find files" error. *Mitigation:* the prepare-snapshot endpoint returns the expiry timestamp so the pane can re-prepare before the deadline; as a backstop, the task-creation handler returns a structured "prep token expired" error so the client can transparently retry.
- **Fork on a very large conversation.** `ForkConversationForHandoff` reads the source's full GCS object into memory and writes a copy; 10s of MB on the warp-server process for the duration of the call. *Mitigation:* the simple read+write path is fine for V0; switch to `storage.CopierFrom` (server-side GCS copy) if measurements show it matters.
- **Source conversation isn't fully synced to GCS.** A `server_conversation_token` only proves the metadata row exists; the GCS data (`{conversation_id}.pb`) may still be in flight or never written. *Mitigation:* the fork helper checks `BatchDoesConversationDataExist` (same check `list_ai_conversations` uses at `logic/ai_conversation_object.go:484`) before copying and returns a structured `SourceConversationNotPersisted` error; the pane surfaces it via `HandoffSubmissionState::Failed`.
- **Unauthorized cross-user fork.** A caller could try to fork another user's conversation. *Mitigation:* `ForkConversationForHandoff` step 1 requires `ViewAction` on the source via the existing `auth_types.For(ctx)` engine (same posture as `CheckAndRecordConversationAccess`); the new fork is owned by the requesting principal, not the source's owner.
- **Local-only changes that aren't reproducible in cloud.** Private forks, submodules, large LFS files. `git diff --binary HEAD` and `git ls-files --others --exclude-standard` cover the common cases; submodules are not recursed (same as cloud→cloud). *Mitigation:* acceptable for V0; the rehydration prompt instructs the agent to report apply failures.
- **Worker/server flag drift.** Client flag on, server flag off → endpoint 404. *Mitigation:* standard rollout sequencing (server first); the client surfaces the 404 as `HandoffSubmissionState::Failed`.
- **Snapshot upload tail latency.** Pathological binary diffs hit the existing pipeline's cap (3 retries, exponential backoff, 2-min ceiling). *Mitigation:* same caps as cloud→cloud; the user sees the "Starting…" state for the duration and closing the pane aborts in-flight uploads.
## Testing and validation
### Unit tests
- `touched_repos_tests.rs`: covers `find_git_root` against a temporary directory layout (file inside a repo, directory inside a repo, path outside any repo). The pure helpers (`parse_github_repo`, `pick_handoff_overlap_env`) are exercised end-to-end through the orchestrator.
- `snapshot_tests.rs`: `upload_snapshot_for_handoff` produces the same manifest shape as the cloud→cloud path for a `TouchedWorkspace` fixture (mockito for the prepare-snapshot endpoint and presigned URL targets), and the manifest reports the per-blob upload outcomes so downstream rehydration sees the same artifact layout.
### Server tests (`../warp-server`)
- `agent_webhooks_test.go::TestHandoff_ForkAndPrepToken`: end-to-end inside the test harness. Pre-creates a source conversation, calls the prepare-snapshot endpoint, uploads test files, calls `POST /agent/runs` with both new fields, asserts that the new task has `AgentConversationID` pointing at a fresh forked conversation, that `snapshots/{task.ID}/0/` contains the uploaded files, and that an `ai_run_executions` row with `id=0, state=ENDED` exists.
- `agent_webhooks_test.go::TestHandoff_FlagOff`: with `local_to_cloud_handoff_enabled=false`, the request fails with the expected error and no task / no fork side effects.
- `agent_webhooks_test.go::TestHandoff_PrepTokenExpired`: missing handoff-prep files returns the structured "prep token expired" error.
### Integration / manual
- Starting a handoff with a touched repo containing uncommitted changes, opening the resulting cloud run, and confirming the agent's first turn applies the patches before answering. Verified via the cloud agent's tool calls (`git apply`, `git status`), not by the LLM's chat output.
- After a successful handoff the local conversation accepts new user input and the local agent continues responding. The user can fork it locally too, run other commands, etc.
- The cloud agent's conversation has a different `server_conversation_token` than the local one and that token appears in the cloud agent management view.
- Toggling settings to verify chip availability under various states (no synced server token, `CloudConversations` disabled, etc.).
- Manually break a touched repo's `.git` and confirm the manifest captures it as `gather_failed` and the rest of the snapshot proceeds.
### Feature-flag rollout
- Server flag (`local_to_cloud_handoff_enabled`) goes Dogfood first, end-to-end tested with a Warp engineer's local→cloud handoff against a staging worker.
- Client flag (`LocalToCloudHandoff`) follows once server flag is stable.
- Promote together to Preview and Stable per the standard `promote-feature` skill.
## Follow-ups
- `/claude-cloud-handoff` slash command for Claude harness conversations. Most of the plumbing (touched-repo derivation, snapshot pipeline, prepare-snapshot endpoint, server-side fork) is reusable; the differences are (a) the chip/command gating reads `harness_kind() == HarnessKind::ThirdParty(Claude)`, (b) the server handler must also upload the local Claude transcript envelope to the right GCS slot so the cloud Claude run resumes via REMOTE-1373's existing transcript rehydration path.
- A CLI surface for handoff (e.g. `oz agent handoff --conversation <local-id> --env <id> --prompt "..."`). Opens up automation. Out of scope for V0; the public API surface is already CLI-friendly when we get there.
- A "this conversation was handed off to <link>" indicator on the local conversation, persisted on the local conversation metadata. V0 only surfaces the link by auto-opening the new cloud-mode pane; the local pane has no persistent breadcrumb back to its handoff destination.
- Multi-conversation handoff (batch operation) and "handoff with this exact context but a different prompt" (re-launch with the same snapshot prep). These would benefit from making the prep token re-usable across multiple `POST /agent/runs` calls before expiry.
- Snapshot file size cap on the prepare-snapshot endpoint. Today the size cap is implicit (presigned URL upload limits + the 100-file cap inherited from `MAX_SNAPSHOT_FILES_PER_RUN`). Worth surfacing more explicitly so the handoff pane can warn the user before they submit.
- Banner UI surfacing touched-repo overlap status, derivation progress, and inline submission errors. V0 ships with no banner; the data is all available on `pending_handoff` but not visualized.
