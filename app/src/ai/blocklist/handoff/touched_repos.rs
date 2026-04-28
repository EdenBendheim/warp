//! Touched-workspace derivation for local-to-cloud handoff (REMOTE-1486).
//!
//! Given an [`AIConversation`] (or the flat list of paths extracted from one) and
//! the user's currently-known cloud agent environments, this module produces:
//!
//! 1. The flat set of filesystem paths an agent run has touched, walked off the
//!    conversation's action history and the per-exchange `working_directory`
//!    (see [`extract_paths_from_conversation`]).
//! 2. A [`TouchedWorkspace`] enumerating the distinct git repos and orphan files the
//!    local agent has touched. Each repo carries a parsed `repo_id` (`<owner>/<repo>`)
//!    derived from its `origin` remote URL, fetched via an async `git` invocation so
//!    derivation never blocks the UI thread.
//! 3. A repo-aware default environment selection that layers on top of the existing
//!    cloud-agent setup recency-sort.
//!
//! Path extraction is sync and pure (no I/O), and the workspace derivation is async
//! (one `git remote get-url origin` per unique repo). Callers run them in sequence
//! off the main thread; see `app/src/workspace/view.rs::start_local_to_cloud_handoff`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use command::r#async::Command;
use command::Stdio;
use futures::future::join_all;
use warpui::r#async::FutureExt as _;

use crate::ai::agent::conversation::AIConversation;
use crate::ai::agent::{AIAgentAction, AIAgentActionType, AIAgentOutputMessageType};
use crate::ai::blocklist::agent_view::agent_input_footer::sort_environments_by_recency;
use crate::ai::cloud_environments::{CloudAmbientAgentEnvironment, GithubRepo};
use crate::server::ids::SyncId;

/// Cap on how many of the conversation's action results we scan for paths,
/// counted from most-recent backwards. Conversations with more than this many
/// tool calls only contribute paths from their most recent
/// [`MAX_TOOL_CALLS_TO_SCAN`].
pub(crate) const MAX_TOOL_CALLS_TO_SCAN: usize = 500;

/// Soft cap on each git invocation we dispatch. Mirrors the cap used by the cloud-side
/// snapshot pipeline so individual filesystem hiccups don't stall the modal indefinitely.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The collection of git repos and orphan files the local agent has touched in the
/// active conversation. Drives both the snapshot upload plan and the modal's env-
/// overlap status row.
#[derive(Clone, Debug, Default)]
pub(crate) struct TouchedWorkspace {
    pub repos: Vec<TouchedRepo>,
    /// Files touched outside any `.git` directory.
    /// They're captured as raw file contents in the snapshot manifest.
    pub orphan_files: Vec<PathBuf>,
}

/// A single git repo touched by the local agent.
#[derive(Clone, Debug)]
pub(crate) struct TouchedRepo {
    /// Absolute path to the working tree root (the directory containing `.git`).
    pub git_root: PathBuf,
    /// `<owner>/<repo>` parsed from the `origin` remote URL, when discoverable.
    /// Drives env-overlap matching against `CloudAmbientAgentEnvironment.github_repos`
    /// and the modal's per-repo status row label.
    pub repo_id: Option<GithubRepo>,
}

/// Derive the `TouchedWorkspace` from a flat list of absolute paths.
///
/// Walks each path up to the nearest `.git` directory; paths whose walk-up doesn't
/// find one go into `orphan_files`. For each unique git root, runs
/// `git remote get-url origin` to parse out the `<owner>/<repo>` for env-overlap
/// matching. Errors on the git call are non-fatal — `repo_id` stays `None`.
///
/// `paths` must already be absolute. Callers are responsible for collecting them
/// from the conversation's action results (`RequestFileEdits` / `ReadFile` / `Diff` /
/// `Grep` / `Glob` `path` fields and `RunShellCommand`'s resolved `cwd`).
pub(crate) async fn derive_touched_workspace(paths: Vec<PathBuf>) -> TouchedWorkspace {
    if paths.is_empty() {
        return TouchedWorkspace::default();
    }

    let mut git_roots: Vec<PathBuf> = Vec::new();
    let mut orphan_files: Vec<PathBuf> = Vec::new();
    let mut seen_roots: HashSet<PathBuf> = HashSet::new();

    for path in paths {
        match find_git_root(&path) {
            Some(root) => {
                if seen_roots.insert(root.clone()) {
                    git_roots.push(root);
                }
            }
            None => {
                if path.is_file() {
                    orphan_files.push(path);
                }
            }
        }
    }

    let metadata_futures = git_roots
        .into_iter()
        .map(|root| async move { gather_repo_metadata(root).await });
    let repos: Vec<TouchedRepo> = join_all(metadata_futures).await;

    TouchedWorkspace {
        repos,
        orphan_files,
    }
}

/// Walk `path` up to find the nearest enclosing `.git` directory and return its parent
/// (the working-tree root). Returns `None` if no `.git` is found.
fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut cursor: Option<&Path> = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(dir) = cursor {
        let candidate = dir.join(".git");
        if candidate.exists() {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

/// Gather the git metadata we actually consume. Errors are absorbed; `repo_id`
/// just stays `None` if `git remote get-url origin` fails or returns a non-GitHub URL.
async fn gather_repo_metadata(git_root: PathBuf) -> TouchedRepo {
    let remote_url = git_string(&git_root, &["remote", "get-url", "origin"]).await;
    let repo_id = remote_url.as_deref().and_then(parse_github_repo);
    TouchedRepo { git_root, repo_id }
}

/// Run `git <args>` in `repo_dir`, returning the trimmed stdout if the exit status is 0
/// and the output decodes as UTF-8. Caps each invocation at [`GIT_COMMAND_TIMEOUT`] so a
/// stalled git process can't pin the modal's loading state forever.
async fn git_string(repo_dir: &Path, args: &[&str]) -> Option<String> {
    match git_string_inner(repo_dir, args).await {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(_) => None,
    }
}

async fn git_string_inner(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = match command.output().with_timeout(GIT_COMMAND_TIMEOUT).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => anyhow::bail!("git invocation failed: {e}"),
        Err(_) => anyhow::bail!("git timed out"),
    };

    if !output.status.success() {
        anyhow::bail!("git exited with non-zero status");
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Parse a GitHub remote URL of either the SSH (`git@github.com:owner/repo.git`) or
/// HTTPS (`https://github.com/owner/repo[.git]`) flavor into a [`GithubRepo`].
/// Returns `None` for non-GitHub remotes (we only support env-overlap for GitHub today,
/// matching the env-creation flow).
fn parse_github_repo(remote_url: &str) -> Option<GithubRepo> {
    let trimmed = remote_url.trim();
    let path_part = if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        rest
    } else {
        return None;
    };

    let path_part = path_part.strip_suffix(".git").unwrap_or(path_part);
    let mut segments = path_part.splitn(2, '/');
    let owner = segments.next()?.to_string();
    let repo = segments.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GithubRepo::new(owner, repo))
}

/// Pick the env that has the most overlap with the touched repos, breaking ties by
/// recency. Returns `None` when no env contains any of the touched repos (or when
/// `envs` is empty / the workspace touched no GitHub-mapped repos).
///
/// This is the "strict" overlap-aware pick used by the handoff pane bootstrap,
/// which calls it unconditionally and applies the result on top of whatever the
/// `EnvironmentSelector`'s `ensure_default_selection` had already picked. When
/// this returns `None`, callers leave the existing selection alone.
pub(crate) fn pick_handoff_overlap_env(
    workspace: &TouchedWorkspace,
    envs: Vec<CloudAmbientAgentEnvironment>,
) -> Option<SyncId> {
    if envs.is_empty() {
        return None;
    }

    let touched_repo_ids: Vec<&GithubRepo> = workspace
        .repos
        .iter()
        .filter_map(|r| r.repo_id.as_ref())
        .collect();
    if touched_repo_ids.is_empty() {
        return None;
    }

    // Sort most-recent-first so that ties on overlap count resolve to the most-
    // recently-used env. We then iterate and keep the first-best score.
    let mut envs = envs;
    sort_environments_by_recency(&mut envs);
    let mut best: Option<(&CloudAmbientAgentEnvironment, usize)> = None;
    for env in &envs {
        let env_repos = &env.model().string_model.github_repos;
        let score = touched_repo_ids
            .iter()
            .filter(|id| env_repos.iter().any(|r| &r == *id))
            .count();
        if score == 0 {
            continue;
        }
        match best {
            None => best = Some((env, score)),
            Some((_, current)) if score > current => best = Some((env, score)),
            _ => {}
        }
    }
    best.map(|(env, _)| env.id)
}

// --- Path extraction from `AIConversation` ---
//
// Walks an [`AIConversation`] and collects every filesystem path the local agent
// touched. The output feeds [`derive_touched_workspace`], which groups paths by
// enclosing `.git` repo and produces the [`TouchedWorkspace`] the orchestrator
// uploads from.
//
// Path sources, per action: `RequestFileEdits` (each edit's file), `ReadFiles`
// (each location), `Grep` (path), `FileGlob` / `FileGlobV2` (optional path /
// search_dir), `SearchCodebase` (codebase_path plus absolute partial_paths),
// `InsertCodeReviewComments` (repo_path), `UploadArtifact` (file_path), plus
// the per-exchange `working_directory` for shell commands.
//
// Paths are kept absolute when they look absolute, and resolved against the
// exchange's `working_directory` when they don't. Empty / non-existent entries
// are filtered later in `derive_touched_workspace` — this module produces the
// maximally-permissive raw set.
//
// Cost is bounded by walking only the [`MAX_TOOL_CALLS_TO_SCAN`] most recent
// action results across all exchanges. Older actions are skipped under the
// assumption that the workspace state the user wants to hand off is dominated
// by recent work; this keeps very long conversations from paying an unbounded
// per-handoff scan cost.

/// Collect every filesystem path that appears in any of the conversation's
/// action requests (and the cwd of every exchange that ran shell commands),
/// capped to the most recent [`MAX_TOOL_CALLS_TO_SCAN`] action results.
///
/// The returned vec is deduplicated and may contain both absolute and
/// resolved-against-`working_directory` paths. Per-path filesystem checks
/// (does the path exist? does it have a `.git` ancestor?) happen later in
/// [`derive_touched_workspace`].
pub(crate) fn extract_paths_from_conversation(conversation: &AIConversation) -> Vec<PathBuf> {
    // Walk exchanges newest-first so we can stop once we've consumed the cap.
    // Within each exchange we count every `Action` message against the budget
    // and bail early if we hit it mid-exchange.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut tool_calls_remaining = MAX_TOOL_CALLS_TO_SCAN;

    for exchange in conversation.all_exchanges().into_iter().rev() {
        if tool_calls_remaining == 0 {
            break;
        }
        let cwd = exchange.working_directory.as_deref();

        // Track the per-exchange cwd unconditionally (it doesn't count as a tool
        // call). Covers `RunShellCommand` cwds without walking action results.
        if let Some(cwd) = cwd {
            let cwd_path = PathBuf::from(cwd);
            if cwd_path.is_absolute() && seen.insert(cwd_path.clone()) {
                paths.push(cwd_path);
            }
        }

        let Some(output) = exchange.output_status.output() else {
            continue;
        };
        let output = output.get();
        for message in &output.messages {
            let AIAgentOutputMessageType::Action(action) = &message.message else {
                continue;
            };
            if tool_calls_remaining == 0 {
                break;
            }
            tool_calls_remaining -= 1;
            extract_action_paths(action, cwd, &mut paths, &mut seen);
        }
    }

    paths
}

fn extract_action_paths(
    action: &AIAgentAction,
    cwd: Option<&str>,
    paths: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) {
    match &action.action {
        AIAgentActionType::RequestFileEdits { file_edits, .. } => {
            for edit in file_edits {
                push_resolved(edit.file(), cwd, paths, seen);
            }
        }
        AIAgentActionType::ReadFiles(req) => {
            for loc in &req.locations {
                push_resolved(Some(loc.name.as_str()), cwd, paths, seen);
            }
        }
        AIAgentActionType::Grep { path, .. } => {
            push_resolved(Some(path.as_str()), cwd, paths, seen);
        }
        AIAgentActionType::FileGlob { path, .. } => {
            push_resolved(path.as_deref(), cwd, paths, seen);
        }
        AIAgentActionType::FileGlobV2 { search_dir, .. } => {
            push_resolved(search_dir.as_deref(), cwd, paths, seen);
        }
        AIAgentActionType::SearchCodebase(req) => {
            push_resolved(req.codebase_path.as_deref(), cwd, paths, seen);
            if let Some(partial_paths) = &req.partial_paths {
                for partial in partial_paths {
                    let candidate = Path::new(partial);
                    if candidate.is_absolute() {
                        push_resolved(Some(partial.as_str()), cwd, paths, seen);
                    }
                }
            }
        }
        AIAgentActionType::InsertCodeReviewComments { repo_path, .. } => {
            if seen.insert(repo_path.clone()) {
                paths.push(repo_path.clone());
            }
        }
        AIAgentActionType::UploadArtifact(req) => {
            push_resolved(Some(req.file_path.as_str()), cwd, paths, seen);
        }
        // Actions below don't reference a workspace path the agent touched.
        AIAgentActionType::RequestCommandOutput { .. }
        | AIAgentActionType::WriteToLongRunningShellCommand { .. }
        | AIAgentActionType::ReadShellCommandOutput { .. }
        | AIAgentActionType::ReadMCPResource { .. }
        | AIAgentActionType::CallMCPTool { .. }
        | AIAgentActionType::SuggestNewConversation { .. }
        | AIAgentActionType::SuggestPrompt(_)
        | AIAgentActionType::InitProject
        | AIAgentActionType::OpenCodeReview
        | AIAgentActionType::ReadDocuments(_)
        | AIAgentActionType::EditDocuments(_)
        | AIAgentActionType::CreateDocuments(_)
        | AIAgentActionType::UseComputer(_)
        | AIAgentActionType::RequestComputerUse(_)
        | AIAgentActionType::ReadSkill(_)
        | AIAgentActionType::FetchConversation { .. }
        | AIAgentActionType::StartAgent { .. }
        | AIAgentActionType::SendMessageToAgent { .. }
        | AIAgentActionType::TransferShellCommandControlToUser { .. }
        | AIAgentActionType::AskUserQuestion { .. } => {}
    }
}

/// Push `raw` into `paths` after resolving it against `cwd` if necessary.
/// Empty / `None` entries are ignored.
fn push_resolved(
    raw: Option<&str>,
    cwd: Option<&str>,
    paths: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) {
    let Some(raw) = raw else { return };
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let candidate = Path::new(raw);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if let Some(cwd) = cwd {
        Path::new(cwd).join(candidate)
    } else {
        // No cwd context, no absolute path — we have nothing actionable.
        return;
    };
    if seen.insert(resolved.clone()) {
        paths.push(resolved);
    }
}

#[cfg(test)]
#[path = "touched_repos_tests.rs"]
mod tests;
