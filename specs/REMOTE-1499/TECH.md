# REMOTE-1499 Tech Spec: Client-side prompt-less local-to-cloud handoff
Linear: [REMOTE-1499](https://linear.app/warpdotdev/issue/REMOTE-1499)
Server tech spec: `../warp-server/specs/REMOTE-1499/TECH.md`
Builds on: REMOTE-1486 (`specs/REMOTE-1486/TECH.md`)
## Problem
The local-to-cloud handoff pane (REMOTE-1486) requires the user to type a follow-up prompt before the send button activates. There are flows where the source conversation history plus the workspace snapshot is the entire input the cloud agent needs — the user wants to "just send this to the cloud as-is". Today the cloud-mode submit path short-circuits when the prompt buffer is empty, so the user can't dispatch the handoff without typing something.
## Relevant code
- `app/src/terminal/input.rs:11860-11959` — the cloud-mode submit dispatch (`handle_input_submit` branch). Contains the `if prompt.is_empty() { return; }` short-circuit at line 11870 that blocks empty-prompt submission for both regular cloud-mode runs and handoff panes.
- `app/src/terminal/view/ambient_agent/model.rs:1142-1187` — `AmbientAgentViewModel::submit_handoff`. Already accepts an empty `prompt: String` and threads it into `SpawnAgentRequest` unchanged; only the upstream input gate needs relaxing.
- `app/src/terminal/view/ambient_agent/model.rs:332-334` — `is_local_to_cloud_handoff()`, the predicate used to branch handoff-vs-fresh-cloud-mode behavior across the input + view layers.
- `app/src/terminal/view/ambient_agent/view_impl.rs:141-179` — `DispatchedAgent` handler. Already gates the queued-user-query block insertion on `!prompt.is_empty()`, so an empty-prompt handoff renders the standard "WaitingForSession" UI without a stray empty user-query block. No change needed here, but the behavior is load-bearing for this feature.
- `app/src/server/server_api/ai.rs:181-218` — `SpawnAgentRequest`. The `prompt` field is `String` (not `Option<String>`); empty strings serialize fine and the server accepts them once the new gate lands.
- `specs/REMOTE-1486/PRODUCT.md:33` — the line that says *"send button follows the regular cloud-mode rules (prompt non-empty)"*. Needs amending.
## Current state
- The submit path at `input.rs:11860-11959` is shared between fresh cloud-mode spawns and handoff submits. The `prompt.is_empty()` early return runs before either branch — there's no handoff-aware branch on emptiness today.
- `submit_handoff` already builds a `SpawnAgentRequest { prompt: "", … }` correctly when called with an empty string — the gate is upstream of this method, so the function itself needs no change.
- The optimistic queued-user-query block insertion in `view_impl.rs` already no-ops on empty prompts.
- There is no separate render-time "send disabled when buffer empty" check for cloud-mode panes; the only gate is the input.rs short-circuit.
- The CLI surface (`agent run-cloud`) is not in scope for this feature — REMOTE-1486 explicitly excluded a CLI handoff entry point from V0.
## Proposed changes
### 1. Allow empty-prompt submission for handoff panes
Edit the cloud-mode submit dispatch in `app/src/terminal/input.rs` (around line 11860-11959). Today the relevant fragment is:
```rust path=null start=null
if self
    .ambient_agent_view_model()
    .is_some_and(|m| m.as_ref(ctx).is_configuring_ambient_agent())
{
    let prompt = command.trim().to_owned();
    if prompt.is_empty() {
        return;
    }
    // … attachment collection + dispatch to submit_handoff / spawn_agent
}
```
Replace the unconditional `prompt.is_empty()` short-circuit with one that exempts handoff panes:
```rust path=null start=null
let prompt = command.trim().to_owned();
let is_handoff = self
    .ambient_agent_view_model()
    .is_some_and(|m| m.as_ref(ctx).is_local_to_cloud_handoff());
if prompt.is_empty() && !is_handoff {
    return;
}
```
The downstream branch at lines 11949-11957 already routes to `submit_handoff` vs `spawn_agent` based on `is_local_to_cloud_handoff()`, so no further dispatch logic changes. `submit_handoff` accepts `prompt: ""` unchanged and lets the spawn flow through to the server.
Fresh cloud-mode runs are unaffected: `is_handoff = false` for those, so the empty-prompt short-circuit still fires.
### 2. No additional send-button gating change
The send button itself isn't gated by emptiness — the only enable check is the input.rs path above. The handoff-specific guards added by REMOTE-1486 (workspace derivation complete, prep token cached) live inside `submit_handoff` and remain in force:
```rust path=null start=null
if handoff.touched_workspace.is_none() {
    log::warn!("submit_handoff called before touched-workspace derivation completed");
    return;
}
let Some(prep_token) = handoff.snapshot_prep_token.clone() else {
    log::warn!("submit_handoff called before snapshot upload completed");
    return;
};
```
These continue to gate submission whether or not the prompt is empty.
### 3. Amend the product spec
Update `specs/REMOTE-1486/PRODUCT.md:33` to drop the "prompt non-empty" requirement for handoff panes:
- Old: *"The send button follows the regular cloud-mode rules (prompt non-empty) plus a guard until touched-repo derivation completes."*
- New: *"The send button is enabled once touched-repo derivation completes and the snapshot prep token has been minted; the prompt may be empty (in which case the cloud agent receives only the forked conversation history and the rehydration message)."*
Also update §11 ("Submitting") to note that the prompt is optional, and §16 ("Pre-SessionStarted visualization") to clarify that the queued-user-query indicator is suppressed when the prompt is empty (already the implementation behavior).
### 4. CLI surface — out of scope
The `agent run-cloud` clap arg group at `crates/warp_cli/src/agent.rs:339-344` requires one of `prompt`, `saved_prompt`, or `skill`. CLI-driven handoff isn't shipping in V0 (per REMOTE-1486 PRODUCT.md non-goals), so the clap group stays as-is. When CLI handoff lands, that group will need to add `task_id` / a new `--fork-from-conversation` flag and the runtime check at `app/src/ai/agent_sdk/ambient.rs:259-268` will need a parallel relaxation. Tracking issue at follow-ups.
## End-to-end flow
```mermaid
sequenceDiagram
    participant U as User
    participant Pane as Handoff pane (Input)
    participant VM as AmbientAgentViewModel
    participant API as warp-server
    U->>Pane: Click "Hand off to cloud" chip
    Pane->>VM: set_pending_handoff(...)
    Pane->>VM: derive_touched_workspace + upload_snapshot_for_handoff (async)
    VM-->>Pane: PendingHandoffChanged (workspace + prep_token cached)
    U->>Pane: Press Cmd-Enter with empty buffer
    Pane->>Pane: prompt = "" ; is_handoff = true ; do NOT short-circuit
    Pane->>VM: submit_handoff(prompt="", attachments=[])
    VM->>VM: Build SpawnAgentRequest { prompt: "", fork_from_conversation_id, handoff_prep_token, ... }
    VM->>API: POST /agent/runs
    API-->>VM: {task_id, run_id}
    VM->>VM: Status::WaitingForSession
    Note over Pane: view_impl.rs DispatchedAgent: queued-prompt block skipped (prompt is empty)
    Note over Pane: Standard "Setting up environment" + "Running setup commands" indicators show
    API-->>VM: SessionStarted (cloud agent first turn = rehydration summary)
```
## Risks and mitigations
### Accidental empty submission on fresh cloud-mode panes
The new `!is_handoff` guard means we still block empty-prompt submits on fresh cloud-mode panes. Risk: a future refactor accidentally drops the `is_handoff` predicate and lets fresh cloud-mode runs submit with empty prompts, which the server would now also accept (per the relaxed `prompt or skill_spec or fork_from_conversation_id` gate would still reject because no fork is set, but the symmetry argument is worth preserving).
*Mitigation:* keep the predicate inline at the call site — don't extract it into a helper that future changes could grow conditions onto. The server-side gate's `fork_from_conversation_id` requirement is the load-bearing safety net regardless.
### User confusion: "did my submit go through?"
Submitting with an empty buffer produces no visible user message in the conversation (the queued-prompt block already suppresses on empty). The pane still shows the standard "Setting up environment" + spinner UI, but the submit feels less acknowledged.
*Mitigation:* the existing handoff submission state (`HandoffSubmissionState::Starting`) already drives the input button's "Starting…" state. The visual feedback is the same as the with-prompt case minus the queued-user-query block. Consider a follow-up to insert a small "Handed off without prompt" pill if user feedback indicates the no-prompt case feels under-acknowledged.
### Send key bindings other than Cmd-Enter
The submit dispatch handles a single submit path; any alternate trigger (e.g. clicking the send icon) routes through the same `handle_input_submit` flow. Visual inspection of the input footer + agent input footer confirms there's no second submit path that bypasses this gate.
## Testing and validation
### Automated coverage
- New unit test on the input submit path — empty buffer + handoff pane dispatches `submit_handoff` with `prompt: ""`; empty buffer + fresh cloud-mode pane no-ops as today.
- Existing handoff tests in `app/src/ai/blocklist/handoff/` should still pass; add a parameterized variant covering the empty-prompt path.
- `app/src/terminal/view/ambient_agent/view_impl.rs` queued-user-query test (if not already) — confirms an empty `request.prompt` skips block insertion.
### Manual / integration validation
- Open a local conversation with at least one touched repo. Click the chip. Wait for derivation + prep token to settle. Click send with an empty prompt. Confirm cloud sandbox starts, applies patches, and posts a summary turn.
- Repeat with a typed prompt to confirm no regression.
- Open a fresh cloud-mode pane (not via handoff). Confirm empty-prompt submit still no-ops.
- Toggle `LocalToCloudHandoff` off; confirm the chip is hidden (no UI change in this PR).
### Feature-flag rollout
This change is gated by the existing `LocalToCloudHandoff` client flag — handoff panes only exist when the flag is on, so the new branch is unreachable otherwise. No new flag.
## Follow-ups
- **CLI handoff entry point.** When CLI handoff lands, relax the `agent run-cloud` clap arg group and the runtime check at `ambient.rs:259-268` to accept a `--task-id` / `--fork-from-conversation` invocation as a sufficient prompt source.
- **"Handed off without prompt" pill.** If user feedback on the no-prompt case suggests the submit feels under-acknowledged, add a small visual marker in the queued region indicating the handoff was sent without a follow-up.
- **Send button label nuance.** Today the send button shows the standard send icon. In the no-prompt handoff case we could swap to a "Hand off" or arrow icon to make the action feel less like a regular message send. Defer until UX feedback warrants it.
