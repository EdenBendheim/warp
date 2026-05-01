//! Async executor for `AIAgentActionType::RunAgents`.
//!
//! Mirrors [`super::start_agent::StartAgentExecutor`]: a real action
//! executor with its own `pending` table, lifecycle events, and
//! integration into [`super::BlocklistAIActionExecutor`]'s
//! `try_to_execute_action` / `should_autoexecute` central dispatch.
//!
//! Reuses [`super::start_agent::StartAgentExecutor::dispatch`] as the
//! per-child fan-out primitive, so each agent in a `RunAgents` request
//! lands on the same StartAgent code path the LLM-emitted single-agent
//! `StartAgent` action uses. The novel piece here is the aggregation
//! step (zip per-child outcomes back into a `RunAgentsResult::Launched`
//! / `Failure` / `Cancelled`) and the user-edit-aware sibling entry
//! point [`RunAgentsExecutor::dispatch_run_agents`], which the
//! confirmation card view drives on Accept with the user-edited
//! request rather than the streamed action.
use std::collections::HashMap;

use ai::agent::action::{RunAgentsAgentRunConfig, RunAgentsExecutionMode, RunAgentsRequest};
use ai::agent::action_result::{
    RunAgentsAgentOutcome, RunAgentsAgentOutcomeKind, RunAgentsLaunchedExecutionMode,
    RunAgentsResult,
};
use ai::skills::SkillReference;
use futures::{future::BoxFuture, FutureExt};
use warpui::{Entity, ModelContext, ModelHandle};

use super::start_agent::{StartAgentExecutor, StartAgentOutcome};
use super::{ActionExecution, AnyActionExecution, ExecuteActionInput, PreprocessActionInput};
use crate::ai::agent::conversation::AIConversationId;
use crate::ai::agent::{
    AIAgentAction, AIAgentActionId, AIAgentActionResultType, AIAgentActionType,
    StartAgentExecutionMode,
};
use crate::ai::blocklist::BlocklistAIHistoryModel;
use warpui::SingletonEntity;

/// In-flight UI snapshot used by the confirmation-card view to render
/// "Spawning N agents…" while the dispatch batch resolves.
///
/// Carried through [`RunAgentsExecutorEvent::SpawningStarted`] so the
/// view does not need a separate "agent count at dispatch time"
/// channel.
#[derive(Debug, Clone, Copy)]
pub struct RunAgentsSpawningSnapshot {
    pub agent_count: usize,
}

/// In-flight tracking per `RunAgents` action. The presence of an entry
/// is the executor-side idempotency guard for repeat dispatch attempts
/// against the same `AIAgentActionId`.
struct PendingRunAgents;

pub struct RunAgentsExecutor {
    pending: HashMap<AIAgentActionId, PendingRunAgents>,
    /// Used to fan out per-child dispatches. Each child is just a
    /// regular `StartAgent` request from the StartAgent executor's
    /// perspective; the orchestrate-specific aggregation lives here.
    start_agent_executor: ModelHandle<StartAgentExecutor>,
}

/// Lifecycle events emitted as the executor drives the in-flight
/// dispatch. The confirmation-card view subscribes to these to render
/// the "Spawning N agents…" card while a dispatch is active.
pub enum RunAgentsExecutorEvent {
    /// A dispatch has begun for `action_id`. The `snapshot` carries
    /// the data the view needs to render the in-flight card.
    SpawningStarted {
        action_id: AIAgentActionId,
        snapshot: RunAgentsSpawningSnapshot,
    },
    /// A dispatch has settled (success / partial / failure /
    /// cancelled). Views clear their in-flight state on receipt.
    SpawningFinished { action_id: AIAgentActionId },
}

impl Entity for RunAgentsExecutor {
    type Event = RunAgentsExecutorEvent;
}

impl RunAgentsExecutor {
    pub fn new(
        start_agent_executor: ModelHandle<StartAgentExecutor>,
        _ctx: &mut ModelContext<Self>,
    ) -> Self {
        Self {
            pending: HashMap::new(),
            start_agent_executor,
        }
    }

    /// Returns true when a dispatch is in flight for `action_id`.
    /// Read by the confirmation card view as an additional idempotency
    /// guard on top of its own local "is_spawning" flag.
    pub fn is_pending(&self, action_id: &AIAgentActionId) -> bool {
        self.pending.contains_key(action_id)
    }

    /// Dispatch entry point used by both the standard executor path
    /// (via [`Self::execute`]) and the confirmation card's Accept
    /// handler (via [`super::BlocklistAIActionExecutor::execute_run_agents`]).
    /// Returns a receiver the caller awaits to learn the terminal
    /// `RunAgentsResult`.
    ///
    /// On success this:
    ///   * Inserts a pending entry keyed by `action_id` (idempotency guard).
    ///   * Emits [`RunAgentsExecutorEvent::SpawningStarted`] so views
    ///     can render the in-flight card.
    ///   * Fans out per-child via [`StartAgentExecutor::dispatch`].
    ///   * Spawns an aggregator that builds [`RunAgentsResult::Launched`]
    ///     from the child outcomes and emits
    ///     [`RunAgentsExecutorEvent::SpawningFinished`] on completion.
    ///
    /// Run-level validation failures (empty configs, OpenCode+Cloud)
    /// short-circuit with a pre-loaded `RunAgentsResult::Failure` and
    /// no `SpawningStarted`/`Finished` events.
    pub fn dispatch_run_agents(
        &mut self,
        action_id: AIAgentActionId,
        request: RunAgentsRequest,
        parent_conversation_id: AIConversationId,
        ctx: &mut ModelContext<Self>,
    ) -> async_channel::Receiver<RunAgentsResult> {
        let (sender, receiver) = async_channel::bounded(1);

        if self.pending.contains_key(&action_id) {
            log::warn!("RunAgentsExecutor: dispatch reentered for {action_id:?}; rejecting");
            let _ = sender.try_send(RunAgentsResult::Cancelled);
            return receiver;
        }

        if let Err(error) = validate_request(&request) {
            log::warn!("RunAgentsExecutor: validation failure: {error}");
            let _ = sender.try_send(RunAgentsResult::Failure { error });
            return receiver;
        }

        let snapshot = RunAgentsSpawningSnapshot {
            agent_count: request.agent_run_configs.len(),
        };
        self.pending.insert(action_id.clone(), PendingRunAgents);
        ctx.emit(RunAgentsExecutorEvent::SpawningStarted {
            action_id: action_id.clone(),
            snapshot,
        });

        let parent_run_id = BlocklistAIHistoryModel::as_ref(ctx)
            .conversation(&parent_conversation_id)
            .and_then(|c| c.run_id());

        let RunAgentsRequest {
            execution_mode: run_execution_mode,
            harness_type,
            model_id,
            skills,
            agent_run_configs,
            base_prompt,
            ..
        } = request;

        // Per-child fan-out. Each slot is either an immediately-resolved
        // failure (translation errors like OpenCode+Remote, missing
        // parent_run_id for Cloud) or a pending receiver that will yield
        // a `StartAgentOutcome` when the child reaches a terminal state.
        let mut slots: Vec<ChildSlot> = Vec::with_capacity(agent_run_configs.len());
        for cfg in &agent_run_configs {
            let prompt = compose_run_agents_child_prompt(&base_prompt, &cfg.prompt);
            let mode = match run_agents_to_start_agent_mode(
                &run_execution_mode,
                &harness_type,
                &model_id,
                &skills,
                cfg,
            ) {
                Ok(mode) => mode,
                Err(err) => {
                    slots.push(ChildSlot::Failed(err));
                    continue;
                }
            };
            if matches!(run_execution_mode, RunAgentsExecutionMode::Remote { .. })
                && parent_run_id.is_none()
            {
                slots.push(ChildSlot::Failed(
                    "Remote child agents require the parent run_id to be available.".to_string(),
                ));
                continue;
            }
            let recv = self.start_agent_executor.update(ctx, |executor, exec_ctx| {
                executor.dispatch(
                    cfg.name.clone(),
                    prompt,
                    mode,
                    None, /* lifecycle_subscription */
                    parent_conversation_id,
                    parent_run_id.clone(),
                    exec_ctx,
                )
            });
            slots.push(ChildSlot::Pending(recv));
        }

        let agent_run_configs_for_result = agent_run_configs.clone();
        let action_id_for_aggr = action_id.clone();
        let run_model_id = model_id.clone();
        let run_harness_type = harness_type.clone();
        let run_execution_mode_for_aggr = run_execution_mode.clone();

        ctx.spawn(
            async move {
                let mut outcomes: Vec<RunAgentsAgentOutcomeKind> = Vec::with_capacity(slots.len());
                for slot in slots {
                    let kind = match slot {
                        ChildSlot::Failed(error) => RunAgentsAgentOutcomeKind::Failed { error },
                        ChildSlot::Pending(recv) => match recv.recv().await {
                            Ok(StartAgentOutcome::Started { agent_id }) => {
                                RunAgentsAgentOutcomeKind::Launched { agent_id }
                            }
                            Ok(StartAgentOutcome::Error(error)) => {
                                RunAgentsAgentOutcomeKind::Failed { error }
                            }
                            Err(_) => RunAgentsAgentOutcomeKind::Failed {
                                error: "Cancelled before launch".to_string(),
                            },
                        },
                    };
                    outcomes.push(kind);
                }
                outcomes
            },
            move |me, outcomes, ctx| {
                let agents: Vec<RunAgentsAgentOutcome> = agent_run_configs_for_result
                    .iter()
                    .zip(outcomes)
                    .map(|(cfg, kind)| RunAgentsAgentOutcome {
                        name: cfg.name.clone(),
                        kind,
                    })
                    .collect();
                let launched_mode = match &run_execution_mode_for_aggr {
                    RunAgentsExecutionMode::Local => RunAgentsLaunchedExecutionMode::Local,
                    RunAgentsExecutionMode::Remote {
                        environment_id,
                        worker_host,
                        computer_use_enabled,
                    } => RunAgentsLaunchedExecutionMode::Remote {
                        environment_id: environment_id.clone(),
                        worker_host: worker_host.clone(),
                        computer_use_enabled: *computer_use_enabled,
                    },
                };
                let result = RunAgentsResult::Launched {
                    model_id: run_model_id,
                    harness_type: run_harness_type,
                    execution_mode: launched_mode,
                    agents,
                };
                me.pending.remove(&action_id_for_aggr);
                ctx.emit(RunAgentsExecutorEvent::SpawningFinished {
                    action_id: action_id_for_aggr,
                });
                let _ = sender.try_send(result);
            },
        );

        receiver
    }

    pub(super) fn execute(
        &mut self,
        input: ExecuteActionInput,
        ctx: &mut ModelContext<Self>,
    ) -> impl Into<AnyActionExecution> {
        let AIAgentAction { action, id, .. } = input.action;
        let AIAgentActionType::RunAgents(request) = action else {
            return ActionExecution::InvalidAction;
        };
        let request = request.clone();
        let action_id = id.clone();
        let parent_conversation_id = input.conversation_id;
        let receiver = self.dispatch_run_agents(action_id, request, parent_conversation_id, ctx);

        ActionExecution::new_async(
            async move { receiver.recv().await },
            |result, _| match result {
                Ok(r) => AIAgentActionResultType::RunAgents(r),
                Err(_) => AIAgentActionResultType::RunAgents(RunAgentsResult::Cancelled),
            },
        )
    }

    pub(super) fn should_autoexecute(
        &self,
        _input: ExecuteActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> bool {
        // Confirmation card always required.
        false
    }

    pub(super) fn preprocess_action(
        &mut self,
        _action: PreprocessActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> BoxFuture<'static, ()> {
        futures::future::ready(()).boxed()
    }
}

enum ChildSlot {
    Failed(String),
    Pending(async_channel::Receiver<StartAgentOutcome>),
}

/// Run-level validation that should hold regardless of how the request
/// reached the executor. The card view's `accept_disabled_reason`
/// gates the Accept button for these same conditions in the editor;
/// this is the defence-in-depth check for paths that bypass the
/// button (Enter outside the card, the standard executor path).
fn validate_request(request: &RunAgentsRequest) -> Result<(), String> {
    if request.agent_run_configs.is_empty() {
        return Err("orchestrate: empty agent_run_configs".to_string());
    }
    if matches!(
        request.execution_mode,
        RunAgentsExecutionMode::Remote { .. }
    ) && request.harness_type.eq_ignore_ascii_case("opencode")
    {
        return Err("Remote child agents do not support the opencode harness yet.".to_string());
    }
    Ok(())
}

/// Compose the per-child prompt per spec invariant:
/// `base_prompt + "\n\n" + agent_run_configs[i].prompt` when both are
/// non-empty, just `base_prompt` when the per-agent `prompt` is empty,
/// and just the per-agent `prompt` when `base_prompt` is empty
/// (defensive).
pub fn compose_run_agents_child_prompt(base_prompt: &str, per_agent_prompt: &str) -> String {
    let base_trimmed = base_prompt.trim();
    let per_agent_trimmed = per_agent_prompt.trim();
    match (base_trimmed.is_empty(), per_agent_trimmed.is_empty()) {
        (false, false) => format!("{base_prompt}\n\n{per_agent_prompt}"),
        (false, true) => base_prompt.to_string(),
        (true, false) => per_agent_prompt.to_string(),
        (true, true) => String::new(),
    }
}

/// Translate a single `(orchestrate_execution_mode, harness_type,
/// model_id, per-agent config)` tuple into the
/// [`StartAgentExecutionMode`] the StartAgent executor expects.
///
/// Returns `Err(reason)` if the combination is rejected pre-flight
/// (e.g. OpenCode+Remote or an unrecognised local harness); the
/// caller surfaces the reason as a per-child `Failed` outcome.
pub fn run_agents_to_start_agent_mode(
    run_execution_mode: &RunAgentsExecutionMode,
    run_harness_type: &str,
    run_model_id: &str,
    run_skills: &[SkillReference],
    cfg: &RunAgentsAgentRunConfig,
) -> Result<StartAgentExecutionMode, String> {
    match run_execution_mode {
        RunAgentsExecutionMode::Local => {
            // Empty/oz harness uses the legacy local Oz path
            // (`harness_type: None`). Other harnesses route through the
            // Local-with-harness arm.
            let trimmed = run_harness_type.trim();
            // Honor the user's run-wide model selection on local launches.
            // `propagate_parent_agent_settings` would otherwise inherit the
            // parent's preferred LLM and silently discard this choice.
            let trimmed_model_id = run_model_id.trim();
            let model_id = (!trimmed_model_id.is_empty()).then(|| trimmed_model_id.to_string());
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("oz") {
                Ok(StartAgentExecutionMode::Local {
                    harness_type: None,
                    model_id,
                })
            } else {
                Ok(StartAgentExecutionMode::Local {
                    harness_type: Some(trimmed.to_string()),
                    model_id,
                })
            }
        }
        RunAgentsExecutionMode::Remote {
            environment_id,
            worker_host,
            computer_use_enabled,
        } => {
            // OpenCode is unsupported on Remote per `start_agent::execute`;
            // surface as a child-level failure so other children can still
            // launch.
            if run_harness_type.eq_ignore_ascii_case("opencode") {
                return Err(
                    "Remote child agents do not support the opencode harness yet.".to_string(),
                );
            }
            Ok(StartAgentExecutionMode::Remote {
                environment_id: environment_id.clone(),
                skill_references: run_skills.to_vec(),
                model_id: run_model_id.to_string(),
                computer_use_enabled: *computer_use_enabled,
                worker_host: worker_host.clone(),
                harness_type: run_harness_type.to_string(),
                title: cfg.title.clone(),
            })
        }
    }
}
