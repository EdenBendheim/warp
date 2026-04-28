//! Client-side pieces of the local-to-cloud Oz conversation handoff:
//!
//! - `touched_repos`: walks the conversation's action history to collect every
//!   filesystem path the local agent has touched, groups those paths into git
//!   roots and orphan files, and exposes the env-overlap pick used by the
//!   handoff pane bootstrap.
//! - `orchestrator`: drives the prep + upload phases of the handoff off the main
//!   thread. The actual cloud-agent spawn happens inside the handoff pane's
//!   `AmbientAgentViewModel::submit_handoff` so the regular streaming spawn flow
//!   (loading screen, shared-session join) is reused unchanged.

pub(crate) mod orchestrator;
pub(crate) mod touched_repos;
