use std::collections::HashMap;

use futures::{future::BoxFuture, FutureExt};
use warpui::{Entity, ModelContext, SingletonEntity};

use crate::ai::agent::conversation::{AIConversationId, ConversationStatus};
use crate::ai::agent::{
    AIAgentAction, AIAgentActionResultType, AIAgentActionType, LifecycleEventType,
    StartAgentExecutionMode, StartAgentResult,
};
use crate::ai::blocklist::orchestration_event_streamer::OrchestrationEventStreamer;
use crate::ai::blocklist::orchestration_events::OrchestrationEventService;
use crate::ai::blocklist::{BlocklistAIHistoryEvent, BlocklistAIHistoryModel};
use warp_cli::agent::Harness;
use warp_core::features::FeatureFlag;

use super::{ActionExecution, AnyActionExecution, ExecuteActionInput, PreprocessActionInput};

/// Outcome the executor surfaces back to a caller awaiting a single
/// StartAgent request. Internal to this module historically; made `pub` so
/// the orchestrate Accept fan-out (which dispatches N requests in
/// parallel and `join_all`s their receivers) can reason about the result
/// for each child without going through the action-model `on_complete`
/// machinery.
///
/// `Cancelled` is reserved for callers; the existing `execute()` path
/// continues to map a dropped receiver to
/// [`StartAgentResult::Cancelled`] via its `on_complete` closure.
#[derive(Debug, Clone)]
pub enum StartAgentOutcome {
    /// The child conversation was created successfully and the server
    /// assigned an orchestration agent id.
    Started { agent_id: String },
    /// An error occurred while starting the agent.
    Error(String),
}

fn invalid_local_child_harness_error(harness_type: &str) -> String {
    let harness_name = harness_type.trim();
    if harness_name.is_empty() {
        "Local child harness type is missing.".to_string()
    } else {
        format!("Unsupported local child harness '{harness_name}'.")
    }
}

/// Opaque, monotonically increasing identifier minted by
/// [`StartAgentExecutor::execute`] for each in-flight StartAgent request.
///
/// Embedded in [`StartAgentRequest`] so the executor can disambiguate
/// per-request side effects when multiple requests are in flight in
/// parallel (e.g. the orchestrate Accept fan-out spawns N concurrent
/// StartAgents that all share the same `parent_conversation_id`). The
/// terminal pane echoes this id back via
/// [`StartAgentExecutor::record_child_conversation`] once the synchronously-
/// created child conversation id is known, replacing the previous
/// `parent_conversation_id`-only matching heuristic.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Default)]
pub struct StartAgentRequestId(u64);

impl StartAgentRequestId {
    /// Convenience constructor for tests that need to supply a fixed id
    /// outside the executor's monotonic counter. `const fn` so callers
    /// can declare module-level `const`s without going through `lazy_static`.
    #[cfg(test)]
    pub const fn from_raw_for_test(value: u64) -> Self {
        Self(value)
    }
}

/// Groups the data for a single StartAgent invocation as it flows from the
/// executor through the terminal view and pane group into the controller.
#[derive(Clone)]
pub struct StartAgentRequest {
    /// Executor-minted request identifier. Plumbed back through
    /// [`StartAgentExecutor::record_child_conversation`] so per-request
    /// pendings are disambiguated when N requests run in parallel.
    pub id: StartAgentRequestId,
    pub name: String,
    pub prompt: String,
    pub execution_mode: StartAgentExecutionMode,
    pub lifecycle_subscription: Option<Vec<LifecycleEventType>>,
    pub parent_conversation_id: AIConversationId,
    pub parent_run_id: Option<String>,
}

/// Tracks a single in-flight StartAgent action.
struct PendingStartAgent {
    parent_conversation_id: AIConversationId,
    /// Set when the terminal pane finishes synchronously creating the
    /// child conversation (via
    /// [`StartAgentExecutor::record_child_conversation`]). Until that
    /// happens, this stays `None` so history events targeting the child
    /// can be ignored for this pending.
    child_conversation_id: Option<AIConversationId>,
    sender: async_channel::Sender<StartAgentOutcome>,
}

pub struct StartAgentExecutor {
    /// In-flight StartAgent requests keyed by their executor-minted id.
    /// Multiple entries can be live concurrently when callers fan out
    /// (orchestrate Accept). Dropped synchronously when the request reaches
    /// a terminal `StartAgentDecision`.
    pending: HashMap<StartAgentRequestId, PendingStartAgent>,
    next_request_id: u64,
}

impl StartAgentExecutor {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let history_model = BlocklistAIHistoryModel::handle(ctx);
        ctx.subscribe_to_model(&history_model, Self::handle_history_event);

        Self {
            pending: HashMap::new(),
            next_request_id: 0,
        }
    }

    /// Returns the next monotonic request id, advancing the internal
    /// counter. Wraps on overflow (practically unreachable; reuse is
    /// harmless because the counter overflows long after every prior
    /// pending has resolved).
    fn next_request_id(&mut self) -> StartAgentRequestId {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        StartAgentRequestId(id)
    }

    /// Records the synchronously-created child conversation id for a
    /// pending request. Called when
    /// [`BlocklistAIHistoryEvent::NewConversationRequestComplete`] arrives;
    /// subsequent `ConversationServerTokenAssigned` /
    /// `UpdatedConversationStatus` history events use this id to find the
    /// matching pending.
    fn record_child_conversation(
        &mut self,
        request_id: StartAgentRequestId,
        child_conversation_id: AIConversationId,
    ) {
        if let Some(pending) = self.pending.get_mut(&request_id) {
            pending.child_conversation_id = Some(child_conversation_id);
        }
    }

    /// Finds the pending request id whose recorded
    /// `child_conversation_id` matches the supplied id, if any. Returns
    /// the `StartAgentRequestId` so the caller can `pending.remove(&id)`
    /// and consume the entry by value.
    fn find_pending_by_child(
        &self,
        child_conversation_id: &AIConversationId,
    ) -> Option<StartAgentRequestId> {
        self.pending.iter().find_map(|(id, pending)| {
            (pending.child_conversation_id.as_ref() == Some(child_conversation_id)).then_some(*id)
        })
    }

    fn handle_history_event(
        &mut self,
        event: &BlocklistAIHistoryEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        match event {
            BlocklistAIHistoryEvent::ConversationServerTokenAssigned {
                conversation_id, ..
            } => {
                let Some(request_id) = self.find_pending_by_child(conversation_id) else {
                    return;
                };
                // Safe to unwrap: `find_pending_by_child` returned the id.
                let pending = self.pending.remove(&request_id).unwrap();
                let agent_id = BlocklistAIHistoryModel::as_ref(ctx)
                    .conversation(conversation_id)
                    .and_then(|c| c.orchestration_agent_id());
                match agent_id {
                    Some(id) => {
                        let _ = pending.sender.try_send(StartAgentOutcome::Started {
                            agent_id: id.clone(),
                        });
                        if FeatureFlag::OrchestrationV2.is_enabled() {
                            OrchestrationEventStreamer::handle(ctx).update(ctx, |streamer, ctx| {
                                streamer.register_watched_run_id(
                                    pending.parent_conversation_id,
                                    id,
                                    ctx,
                                );
                            });
                        } else {
                            OrchestrationEventService::handle(ctx).update(ctx, |svc, ctx| {
                                svc.emit_child_startup_started(*conversation_id, ctx);
                            });
                        }
                    }
                    None => {
                        log::error!(
                            "ConversationServerTokenAssigned fired but no agent identifier for \
                             {conversation_id:?}"
                        );
                        let _ = pending.sender.try_send(StartAgentOutcome::Error(
                            "Server did not assign an agent identifier".to_string(),
                        ));
                        if !FeatureFlag::OrchestrationV2.is_enabled() {
                            OrchestrationEventService::handle(ctx).update(ctx, |svc, ctx| {
                                svc.emit_child_startup_errored(
                                    *conversation_id,
                                    "missing_agent_id".to_string(),
                                    "Server did not assign an agent identifier".to_string(),
                                    ctx,
                                );
                            });
                        }
                    }
                }
            }
            BlocklistAIHistoryEvent::UpdatedConversationStatus {
                conversation_id, ..
            } => {
                let Some(request_id) = self.find_pending_by_child(conversation_id) else {
                    return;
                };
                let history = BlocklistAIHistoryModel::as_ref(ctx);
                let Some(conversation) = history.conversation(conversation_id) else {
                    return;
                };
                let error_msg = start_agent_error_message_for_status(
                    conversation.status(),
                    conversation.status_error_message(),
                );
                if let Some(error_msg) = error_msg {
                    let pending = self.pending.remove(&request_id).unwrap();
                    let _ = pending
                        .sender
                        .try_send(StartAgentOutcome::Error(error_msg.clone()));
                    if !FeatureFlag::OrchestrationV2.is_enabled() {
                        OrchestrationEventService::handle(ctx).update(ctx, |svc, ctx| {
                            svc.emit_child_startup_errored(
                                *conversation_id,
                                "conversation_status".to_string(),
                                error_msg,
                                ctx,
                            );
                        });
                    }
                }
            }
            BlocklistAIHistoryEvent::NewConversationRequestComplete {
                request_id,
                conversation_id,
            } => {
                self.record_child_conversation(*request_id, *conversation_id);
            }
            BlocklistAIHistoryEvent::StartedNewConversation { .. }
            | BlocklistAIHistoryEvent::CreatedSubtask { .. }
            | BlocklistAIHistoryEvent::UpgradedTask { .. }
            | BlocklistAIHistoryEvent::AppendedExchange { .. }
            | BlocklistAIHistoryEvent::ReassignedExchange { .. }
            | BlocklistAIHistoryEvent::UpdatedStreamingExchange { .. }
            | BlocklistAIHistoryEvent::SetActiveConversation { .. }
            | BlocklistAIHistoryEvent::ClearedActiveConversation { .. }
            | BlocklistAIHistoryEvent::ClearedConversationsInTerminalView { .. }
            | BlocklistAIHistoryEvent::UpdatedTodoList { .. }
            | BlocklistAIHistoryEvent::UpdatedAutoexecuteOverride { .. }
            | BlocklistAIHistoryEvent::SplitConversation { .. }
            | BlocklistAIHistoryEvent::RemoveConversation { .. }
            | BlocklistAIHistoryEvent::DeletedConversation { .. }
            | BlocklistAIHistoryEvent::RestoredConversations { .. }
            | BlocklistAIHistoryEvent::UpdatedConversationMetadata { .. }
            | BlocklistAIHistoryEvent::UpdatedConversationArtifacts { .. } => {}
        }
    }

    pub(super) fn should_autoexecute(
        &self,
        _input: ExecuteActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> bool {
        // TODO(QUALITY-342): this should be a setting
        true
    }

    pub(super) fn execute(
        &mut self,
        input: ExecuteActionInput,
        ctx: &mut ModelContext<Self>,
    ) -> impl Into<AnyActionExecution> {
        let AIAgentAction {
            action:
                AIAgentActionType::StartAgent {
                    version,
                    name,
                    prompt,
                    execution_mode,
                    lifecycle_subscription,
                },
            ..
        } = input.action
        else {
            return ActionExecution::InvalidAction;
        };

        let prompt = prompt.clone();
        let version = *version;
        let parent_conversation_id = input.conversation_id;
        let (execution_mode, parent_run_id) = match execution_mode.clone() {
            StartAgentExecutionMode::Local {
                harness_type: None,
                model_id,
            } => {
                // Legacy local Oz child agents do not use
                // StartAgentRequest.parent_run_id. Instead, the child
                // conversation is linked back to its parent on the first
                // request via Request.metadata.parent_agent_id, sourced
                // from the conversation's versioned orchestration_agent_id()
                // (run_id in v2, server conversation token in v1). Remote
                // child agents and local third-party harness children need
                // parent_run_id here because their run is spawned before that
                // first child request exists.
                (
                    StartAgentExecutionMode::Local {
                        harness_type: None,
                        model_id,
                    },
                    None,
                )
            }
            StartAgentExecutionMode::Local {
                harness_type: Some(harness_type),
                model_id,
            } => {
                let Some(harness) = Harness::parse_local_child_harness(&harness_type) else {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error: invalid_local_child_harness_error(&harness_type),
                            version,
                        },
                    ));
                };

                if !FeatureFlag::OrchestrationV2.is_enabled() {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error: "Local harness child agents require orchestration v2."
                                .to_string(),
                            version,
                        },
                    ));
                }

                let parent_run_id = BlocklistAIHistoryModel::as_ref(ctx)
                    .conversation(&parent_conversation_id)
                    .and_then(|conversation| conversation.run_id());
                let Some(parent_run_id) = parent_run_id else {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error:
                                "Local harness child agents require the parent run_id to be available."
                                    .to_string(),
                            version,
                        },
                    ));
                };

                (
                    StartAgentExecutionMode::Local {
                        harness_type: Some(harness.to_string()),
                        model_id,
                    },
                    Some(parent_run_id),
                )
            }
            StartAgentExecutionMode::Remote {
                environment_id,
                skill_references,
                model_id,
                computer_use_enabled,
                worker_host,
                harness_type,
                title,
            } => {
                if !FeatureFlag::OrchestrationV2.is_enabled() {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error: "Remote child agents require orchestration v2.".to_string(),
                            version,
                        },
                    ));
                }

                let harness_type = Harness::parse_orchestration_harness(&harness_type)
                    .map(|harness| harness.to_string())
                    .unwrap_or(harness_type);
                if Harness::parse_orchestration_harness(&harness_type) == Some(Harness::OpenCode) {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error: "Remote child agents do not support the opencode harness yet."
                                .to_string(),
                            version,
                        },
                    ));
                }

                // An empty environment_id is allowed and means the child will be spawned with an
                // empty environment (no preconfigured repositories, secrets, or integrations).
                // Callers are discouraged from relying on this, but we intentionally do not reject
                // it here so that agent authors can opt into running without an environment.
                if environment_id.trim().is_empty() {
                    log::warn!(
                        "Starting remote child agent with empty environment_id; the child will run \
                         with an empty environment."
                    );
                }

                let parent_run_id = BlocklistAIHistoryModel::as_ref(ctx)
                    .conversation(&parent_conversation_id)
                    .and_then(|conversation| conversation.run_id());
                let Some(parent_run_id) = parent_run_id else {
                    return ActionExecution::Sync(AIAgentActionResultType::StartAgent(
                        StartAgentResult::Error {
                            error: "Remote child agents require the parent run_id to be available."
                                .to_string(),
                            version,
                        },
                    ));
                };

                (
                    StartAgentExecutionMode::Remote {
                        environment_id,
                        skill_references,
                        model_id,
                        computer_use_enabled,
                        worker_host,
                        harness_type,
                        title,
                    },
                    Some(parent_run_id),
                )
            }
        };

        let (sender, receiver) = async_channel::bounded(1);
        let request_id = self.next_request_id();
        self.pending.insert(
            request_id,
            PendingStartAgent {
                parent_conversation_id,
                child_conversation_id: None,
                sender,
            },
        );

        ctx.emit(StartAgentExecutorEvent::CreateAgent(StartAgentRequest {
            id: request_id,
            name: name.clone(),
            prompt,
            execution_mode,
            lifecycle_subscription: lifecycle_subscription.clone(),
            parent_conversation_id,
            parent_run_id,
        }));

        ActionExecution::new_async(async move { receiver.recv().await }, move |result, _ctx| {
            match result {
                Ok(StartAgentOutcome::Started { agent_id }) => {
                    AIAgentActionResultType::StartAgent(StartAgentResult::Success {
                        agent_id,
                        version,
                    })
                }
                Ok(StartAgentOutcome::Error(error)) => {
                    AIAgentActionResultType::StartAgent(StartAgentResult::Error { error, version })
                }
                Err(_) => {
                    AIAgentActionResultType::StartAgent(StartAgentResult::Cancelled { version })
                }
            }
        })
    }

    /// Public dispatch entrypoint for callers that already have a fully-built
    /// `StartAgentExecutionMode` and have done their own validation (e.g.
    /// the orchestrate Accept fan-out, which validates the user-edited
    /// configuration in the confirmation card before reaching the
    /// executor).
    ///
    /// Mints a fresh [`StartAgentRequestId`], inserts a pending entry
    /// keyed by it, emits [`StartAgentExecutorEvent::CreateAgent`] so the
    /// pane group can stand up the child pane, and returns the receiver
    /// the caller awaits to learn the per-request [`StartAgentOutcome`].
    /// The history-event handler completes the receiver (Started / Error)
    /// via the same reservation flow used by [`Self::execute`].
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        &mut self,
        name: String,
        prompt: String,
        execution_mode: StartAgentExecutionMode,
        lifecycle_subscription: Option<Vec<LifecycleEventType>>,
        parent_conversation_id: AIConversationId,
        parent_run_id: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) -> async_channel::Receiver<StartAgentOutcome> {
        let (sender, receiver) = async_channel::bounded(1);
        let request_id = self.next_request_id();
        self.pending.insert(
            request_id,
            PendingStartAgent {
                parent_conversation_id,
                child_conversation_id: None,
                sender,
            },
        );
        ctx.emit(StartAgentExecutorEvent::CreateAgent(StartAgentRequest {
            id: request_id,
            name,
            prompt,
            execution_mode,
            lifecycle_subscription,
            parent_conversation_id,
            parent_run_id,
        }));
        receiver
    }

    pub(super) fn preprocess_action(
        &mut self,
        _action: PreprocessActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> BoxFuture<'static, ()> {
        futures::future::ready(()).boxed()
    }
}

fn start_agent_error_message_for_status(
    status: &ConversationStatus,
    error_message: Option<&str>,
) -> Option<String> {
    match status {
        ConversationStatus::Error => Some(
            error_message
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("Child agent failed to initialize")
                .to_string(),
        ),
        ConversationStatus::Cancelled => {
            Some("Child agent was cancelled before initialization".to_string())
        }
        ConversationStatus::Blocked { blocked_action } => {
            let blocked_action = blocked_action.trim();
            Some(if blocked_action.is_empty() {
                "Child agent startup was blocked before initialization".to_string()
            } else {
                blocked_action.to_string()
            })
        }
        ConversationStatus::InProgress | ConversationStatus::Success => None,
    }
}

impl Entity for StartAgentExecutor {
    type Event = StartAgentExecutorEvent;
}

pub enum StartAgentExecutorEvent {
    CreateAgent(StartAgentRequest),
}

#[cfg(test)]
#[path = "start_agent_tests.rs"]
mod tests;
