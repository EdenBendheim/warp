//! Inline view for the orchestrate (`RunAgents`) tool call.
//!
//! Mirrors the pattern used by `code_diff_view`: a real `View` with its
//! own keymap context and `TypedActionView`, embedded by `AIBlock` via
//! `ChildView` rather than rendered through inline free functions. The
//! view owns all per-card state (edit state, button + picker handles,
//! in-flight dispatch snapshot) and is keyed by `AIAgentActionId` so
//! multiple cards can coexist on one `AIBlock`.
//!
//! ## Architectural notes
//!
//! * **Keybindings live on the view, not the block.** `pub fn init`
//!   registers the orchestrate-card-scoped FixedBindings predicated on
//!   `id!(RunAgentsCardView::ui_name())`. They only resolve while a card
//!   view is in the focus chain, so we don't need a global
//!   "RUN_AGENTS_EDITOR_OPEN" context flag on the parent `AIBlock`.
//!   `keymap_context` inserts a per-view flag (`RUN_AGENTS_EDITOR_OPEN`)
//!   when this card's editor is open so the `escape` binding only fires
//!   on cards that actually have an editor to discard.
//! * **Dispatch on Accept/Reject still lives on `AIBlock`.** The view
//!   emits `AcceptRequested` / `RejectRequested` events; `AIBlock` reads
//!   the resolved `RunAgentsRequest` from the view via
//!   [`RunAgentsCardView::current_request`] and runs the existing
//!   `dispatch_run_agents` flow. Step 2 of the architectural refactor
//!   will move that flow into a `RunAgentsExecutor` paralleling
//!   `StartAgentExecutor`.
//! * **Spawning snapshot** is held inside the view (an
//!   `Option<RunAgentsSpawningSnapshot>`); `AIBlock` flips it via
//!   [`RunAgentsCardView::set_spawning_snapshot`] /
//!   [`RunAgentsCardView::clear_spawning_snapshot`]. The presence of a
//!   snapshot doubles as the idempotency guard for repeat Accept
//!   dispatches and as the source for the in-flight "Spawning N
//!   agents…" card.
//!
//! Spec references: TECH.md §8, §9; PRODUCT.md "Confirmation card",
//! "Post-action card states", "Invariants".
use ai::agent::action::{RunAgentsAgentRunConfig, RunAgentsExecutionMode, RunAgentsRequest};
use ai::agent::action_result::{RunAgentsAgentOutcomeKind, RunAgentsResult};
use ai::skills::SkillReference;
use pathfinder_color::ColorU;
use std::rc::Rc;
use warpui::elements::{
    Border, ChildView, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Empty,
    Expanded, Flex, Hoverable, MainAxisAlignment, MainAxisSize, MouseStateHandle, ParentElement,
    Radius, Text,
};
use warpui::keymap::{FixedBinding, Keystroke};
use warpui::platform::Cursor;
use warpui::ui_components::button::ButtonVariant;
use warpui::ui_components::components::{Coords, UiComponentStyles};
use warpui::{
    AppContext, Element, Entity, ModelHandle, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use warp_cli::agent::Harness;
use warp_core::ui::theme::Fill;

use crate::LLMPreferences;
use crate::ai::agent::icons;
use crate::ai::agent::{AIAgentActionId, AIAgentActionResultType};
use crate::ai::agent_conversations_model::AgentConversationsModel;
use crate::ai::blocklist::action_model::{AIActionStatus, BlocklistAIActionModel};
use crate::ai::blocklist::agent_view::orchestration_pill_bar::render_static_agent_pill;
use crate::ai::blocklist::block::AIBlock;
use crate::ai::blocklist::block::model::AIBlockModel;
use crate::ai::blocklist::block::view_impl::WithContentItemSpacing;
use crate::ai::blocklist::inline_action::inline_action_header::{HeaderConfig, InteractionMode};
use crate::ai::blocklist::inline_action::inline_action_icons;
use crate::ai::blocklist::inline_action::requested_action::{
    CTRL_C_KEYSTROKE, ENTER_KEYSTROKE, render_requested_action_row_for_text,
};
use crate::ai::execution_profiles::model_menu_items::available_model_menu_items;
use crate::ai::harness_display;
use crate::appearance::Appearance;
use crate::menu::{MenuItem, MenuItemFields};
use crate::ui_components::blended_colors;
use crate::ui_components::icons::Icon;
use crate::view_components::action_button::{ButtonSize, KeystrokeSource, NakedTheme};
use crate::view_components::compactible_action_button::{
    CompactibleActionButton, MEDIUM_SIZE_SWITCH_THRESHOLD, RenderCompactibleActionButton,
};
use crate::view_components::compactible_split_action_button::CompactibleSplitActionButton;
use crate::view_components::dropdown::{Dropdown, DropdownAction, DropdownEvent, DropdownStyle};
use crate::view_components::{FilterableDropdown, FilterableDropdownEvent};

/// Round 6 follow-up B3: canonical worker-host value (lowercase) used
/// throughout the orchestrate edit state. The recommendation copy in
/// `render_editor` is gated on this so non-Warp hosts — where the
/// environment concept doesn't apply — don't surface the recommendation.
const RUN_AGENTS_WARP_WORKER_HOST: &str = "warp";

/// Static title rendered in the orchestrate confirmation card header.
/// Per spec §8 this is invariant client copy; the LLM-supplied
/// `summary` field is repurposed as the body description.
const RUN_AGENTS_CARD_TITLE: &str = "Can I add additional agents to this task?";

/// Display label for the synthetic "(no environment)" item at the top
/// of the Cloud-mode environment picker. Selecting this item dispatches
/// `EnvironmentChanged` with an empty `environment_id`, which clears
/// any previously chosen environment.
const RUN_AGENTS_ENV_NONE_LABEL: &str = "(no environment)";

/// Per-view keymap context flag set when this card's inline editor is
/// open. Gates the `escape` binding so it only fires when the editor is
/// actually open and doesn't shadow Esc elsewhere.
const RUN_AGENTS_EDITOR_OPEN: &str = "RunAgentsEditorOpen";

/// Registers orchestrate-card-scoped keybindings. Mirrors the
/// `code_diff_view::init` pattern: bindings are scoped to the view's
/// `ui_name()` so they only fire while the focused view is a
/// `RunAgentsCardView`. Per-view keymap state (e.g. is-editor-open)
/// further gates the `escape` binding.
pub fn init(app: &mut AppContext) {
    use warpui::keymap::macros::*;

    app.register_fixed_bindings([
        FixedBinding::new(
            "enter",
            RunAgentsCardViewAction::Accept,
            id!(RunAgentsCardView::ui_name()),
        ),
        FixedBinding::new(
            "numpadenter",
            RunAgentsCardViewAction::Accept,
            id!(RunAgentsCardView::ui_name()),
        ),
        FixedBinding::new(
            "cmdorctrl-e",
            RunAgentsCardViewAction::ToggleEdit,
            id!(RunAgentsCardView::ui_name()),
        ),
        // Esc only fires when this card's editor is open (gated by the
        // per-view `RUN_AGENTS_EDITOR_OPEN` flag inserted by
        // `keymap_context`). Reject's documented shortcut is `Ctrl-C`,
        // not Esc.
        FixedBinding::new(
            "escape",
            RunAgentsCardViewAction::DiscardEdits,
            id!(RunAgentsCardView::ui_name()) & id!(RUN_AGENTS_EDITOR_OPEN),
        ),
    ]);
}

// ---------------------------------------------------------------------------
// State structs
// ---------------------------------------------------------------------------

/// Per-action edit state for the `orchestrate` tool call's inline
/// confirmation card. Updated in response to picker actions while the
/// editor is open.
///
/// Spec references: TECH.md §8 ("Client: confirmation card"),
/// PRODUCT.md "Confirmation card actions".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentsEditState {
    /// Whether the inline editor is currently visible. Toggled by the
    /// Edit button.
    pub is_editor_open: bool,
    /// Currently-selected model_id for run-wide config. Initialized
    /// from the LLM-supplied `RunAgentsRequest.model_id`.
    pub model_id: String,
    /// Currently-selected harness_type. Initialized from the
    /// LLM-supplied `RunAgentsRequest.harness_type`.
    pub harness_type: String,
    /// Currently-selected execution mode (Local or Remote{env, host, ...}).
    pub execution_mode: RunAgentsExecutionMode,
    /// Per-agent run configs (passed straight through; not
    /// user-editable in Stage 1).
    pub agent_run_configs: Vec<RunAgentsAgentRunConfig>,
    /// Run-wide base prompt (passed through verbatim).
    pub base_prompt: String,
    /// Summary text rendered in the title row.
    pub summary: String,
    /// Run-wide skills (passed through verbatim per PRODUCT.md
    /// "Skills and base prompt are passed through verbatim and not
    /// displayed"). Propagated to each child's
    /// `StartAgentExecutionMode::Remote.skill_references` at dispatch.
    pub skills: Vec<SkillReference>,
}

impl RunAgentsEditState {
    pub fn from_request(req: &RunAgentsRequest) -> Self {
        Self {
            is_editor_open: false,
            model_id: req.model_id.clone(),
            harness_type: req.harness_type.clone(),
            execution_mode: req.execution_mode.clone(),
            agent_run_configs: req.agent_run_configs.clone(),
            base_prompt: req.base_prompt.clone(),
            summary: req.summary.clone(),
            skills: req.skills.clone(),
        }
    }

    /// Toggle Local <-> Cloud. Per spec §8, OpenCode harness is not
    /// supported on Cloud; toggling Local→Cloud while OpenCode is
    /// selected resets the harness to Oz.
    pub fn toggle_execution_mode_to_remote(&mut self, is_remote: bool) {
        if is_remote {
            // Local → Cloud: reset OpenCode to Oz per spec.
            if self.harness_type.eq_ignore_ascii_case("opencode") {
                self.harness_type = "oz".to_string();
            }
            // Initialize Remote with default empty fields and
            // worker_host="warp" (TODO(QUALITY-569 fast-follow): expose
            // worker_host as an editable picker).
            if !self.execution_mode.is_remote() {
                self.execution_mode = RunAgentsExecutionMode::Remote {
                    environment_id: String::new(),
                    worker_host: "warp".to_string(),
                    computer_use_enabled: false,
                };
            }
        } else {
            self.execution_mode = RunAgentsExecutionMode::Local;
        }
    }

    pub fn set_environment_id(&mut self, environment_id: String) {
        if let RunAgentsExecutionMode::Remote {
            environment_id: id, ..
        } = &mut self.execution_mode
        {
            *id = environment_id;
        }
    }

    /// Returns Some(reason) if Accept must be disabled, None if it's
    /// enabled. Round 6 follow-up: Cloud-without-env is no longer a
    /// hard block — it's now a soft recommendation rendered in
    /// `render_editor`. The remaining hard block is OpenCode+Cloud,
    /// which is unsupported per spec §8.
    pub fn accept_disabled_reason(&self) -> Option<&'static str> {
        match &self.execution_mode {
            RunAgentsExecutionMode::Remote { .. }
                if self.harness_type.eq_ignore_ascii_case("opencode") =>
            {
                Some(
                    "OpenCode is not supported on Cloud yet. Switch to Local or pick a different harness.",
                )
            }
            RunAgentsExecutionMode::Local | RunAgentsExecutionMode::Remote { .. } => None,
        }
    }

    pub fn to_request(&self) -> RunAgentsRequest {
        RunAgentsRequest {
            summary: self.summary.clone(),
            base_prompt: self.base_prompt.clone(),
            skills: self.skills.clone(),
            model_id: self.model_id.clone(),
            harness_type: self.harness_type.clone(),
            execution_mode: self.execution_mode.clone(),
            agent_run_configs: self.agent_run_configs.clone(),
        }
    }
}

/// Per-action UI handles for the `orchestrate` confirmation card.
///
/// Holds the `CompactibleActionButton` views for Reject and Edit, the
/// `CompactibleSplitActionButton` for Accept, `MouseStateHandle`s for
/// the Local/Cloud toggle inside the inline editor, and the
/// lazily-created picker `ViewHandle`s for the inline editor
/// (model/harness `Dropdown`s and a filterable env
/// `FilterableDropdown`). Each picker field is `Option<...>` so the
/// pickers stay un-built until the user first opens the editor.
#[derive(Default, Clone)]
struct RunAgentsCardHandles {
    reject_button: Option<CompactibleActionButton>,
    edit_button: Option<CompactibleActionButton>,
    accept_button: Option<CompactibleSplitActionButton>,
    local_toggle: MouseStateHandle,
    cloud_toggle: MouseStateHandle,
    model_picker: Option<ViewHandle<Dropdown<RunAgentsCardViewAction>>>,
    harness_picker: Option<ViewHandle<Dropdown<RunAgentsCardViewAction>>>,
    environment_picker: Option<ViewHandle<FilterableDropdown<RunAgentsCardViewAction>>>,
    /// Visual-only host picker (currently always "Warp"). The picker is
    /// non-functional today — the worker host is fixed at `"warp"` and
    /// editing it is a fast-follow per spec. Constructing it as a real
    /// `Dropdown` keeps the four-column editor layout consistent with
    /// Figma node 4340:117057.
    host_picker: Option<ViewHandle<Dropdown<RunAgentsCardViewAction>>>,
}

/// Snapshot captured at orchestrate-Accept time used for two purposes:
///
/// 1. **Idempotency guard.** The presence of a snapshot indicates a
///    previous Accept invocation has already begun dispatching this
///    card's children. Repeat invocations (Enter auto-repeat,
///    double-click on Accept) hit the guard and short-circuit.
/// 2. **Source for the in-flight "Spawning N agents…" card.** While
///    the async dispatch batch is still running, the view renders an
///    in-flight status card in place of the confirmation card. The
///    parent's outcome callback clears the snapshot before the
///    `BlocklistAIActionExecutor::FinishedAction` event fires, at
///    which point the post-action terminal card takes over.
#[derive(Debug, Clone)]
pub struct RunAgentsSpawningSnapshot {
    /// Number of agents being spawned. Drives pluralization of the
    /// in-flight card label ("Spawning 1 agent…" vs "Spawning N
    /// agents…").
    pub agent_count: usize,
}

// ---------------------------------------------------------------------------
// Action / Event enums
// ---------------------------------------------------------------------------

/// View-level interactions for the orchestrate confirmation card.
///
/// Each card view dispatches these against itself; focus determines
/// which card receives keyboard-bound actions, replacing the older
/// `*CurrentCard` action variants that walked the action-model to
/// resolve a target.
#[derive(Clone, Debug)]
pub enum RunAgentsCardViewAction {
    /// User accepted the card. Emits `RunAgentsCardViewEvent::AcceptRequested`
    /// for the parent `AIBlock` to translate into the
    /// `dispatch_run_agents` flow.
    Accept,
    /// User rejected the card. Emits `RunAgentsCardViewEvent::RejectRequested`
    /// so the parent `AIBlock` can call `cancel_action`.
    Reject,
    /// Open or close the inline editor. Lazily builds the picker views
    /// the first time the editor is opened.
    ToggleEdit,
    /// Close the inline editor (gated by `RUN_AGENTS_EDITOR_OPEN`).
    /// Equivalent to `ToggleEdit` when the editor is already open.
    DiscardEdits,
    /// User flipped the Local/Cloud segmented control.
    ExecutionModeToggled { is_remote: bool },
    /// User selected a different base model in the inline editor.
    ModelChanged { model_id: String },
    /// User selected a different harness in the inline editor.
    HarnessChanged { harness_type: String },
    /// User selected a different environment_id (or "(no environment)"
    /// for an empty value).
    EnvironmentChanged { environment_id: String },
}

/// Events surfaced to `AIBlock` so it can run the cross-cutting flows
/// that don't belong on the per-card view (action-model dispatch,
/// cancellation, focus-stealing).
#[derive(Clone, Debug)]
pub enum RunAgentsCardViewEvent {
    /// The user accepted this card. Parent should resolve the current
    /// `RunAgentsRequest` via [`RunAgentsCardView::current_request`]
    /// and run the dispatch flow.
    AcceptRequested,
    /// The user rejected this card. Parent should cancel the action.
    RejectRequested,
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub struct RunAgentsCardView {
    action_id: AIAgentActionId,
    state: RunAgentsEditState,
    handles: RunAgentsCardHandles,
    spawning: Option<RunAgentsSpawningSnapshot>,

    // Plumbed handles. We store these on the view so dispatch handlers
    // can mutate the action model and read the live block model
    // without going through the parent.
    action_model: ModelHandle<BlocklistAIActionModel>,
    block_model: Rc<dyn AIBlockModel<View = AIBlock>>,
}

impl RunAgentsCardView {
    /// Construct a new card view from the streamed `RunAgentsRequest`.
    /// Eagerly builds the Reject / Edit / Accept buttons so the card
    /// can render them on its first frame; pickers stay lazy until the
    /// user opens the editor.
    pub fn new(
        action_id: AIAgentActionId,
        request: &RunAgentsRequest,
        action_model: ModelHandle<BlocklistAIActionModel>,
        block_model: Rc<dyn AIBlockModel<View = AIBlock>>,
        ctx: &mut ViewContext<Self>,
    ) -> Self {
        // Reject defaults to Ctrl-C (rendered as `⌃C` on Mac, `Ctrl C`
        // elsewhere). When the inline editor is open the Edit button
        // label/keystroke swaps to "Discard edits" / Esc via
        // `sync_card_buttons`.
        let reject_keystroke = CTRL_C_KEYSTROKE.clone();
        let edit_keystroke =
            Keystroke::parse("cmdorctrl-e").expect("orchestrate edit keystroke literal must parse");
        let accept_keystroke = ENTER_KEYSTROKE.clone();

        // Round 7: use `ButtonSize::Small` to match apply-diff's
        // Reject/Edit/Accept buttons exactly. The card itself is
        // hidden during streaming via the `is_streaming` gate in
        // `view_impl::output`, so per-button disabled-state plumbing
        // is unnecessary.
        let reject_button = CompactibleActionButton::new(
            "Reject".to_string(),
            Some(KeystrokeSource::Fixed(reject_keystroke)),
            ButtonSize::Small,
            RunAgentsCardViewAction::Reject,
            Icon::X,
            std::sync::Arc::new(NakedTheme),
            ctx,
        );
        let edit_button = CompactibleActionButton::new(
            "Edit".to_string(),
            Some(KeystrokeSource::Fixed(edit_keystroke)),
            ButtonSize::Small,
            RunAgentsCardViewAction::ToggleEdit,
            Icon::Pencil,
            std::sync::Arc::new(NakedTheme),
            ctx,
        );
        // The chevron-down split-button affordance is visual-only per
        // the Figma; both the primary click and the chevron click route
        // to `Accept`.
        let accept_button = CompactibleSplitActionButton::new(
            "Accept".to_string(),
            Some(KeystrokeSource::Fixed(accept_keystroke)),
            ButtonSize::Small,
            RunAgentsCardViewAction::Accept,
            RunAgentsCardViewAction::Accept,
            Icon::Check,
            true,
            None,
            ctx,
        );

        Self {
            action_id,
            state: RunAgentsEditState::from_request(request),
            handles: RunAgentsCardHandles {
                reject_button: Some(reject_button),
                edit_button: Some(edit_button),
                accept_button: Some(accept_button),
                ..Default::default()
            },
            spawning: None,
            action_model,
            block_model,
        }
    }

    /// Returns the resolved `RunAgentsRequest` reflecting any user
    /// edits. Read by `AIBlock::handle_run_agents_accept` after the
    /// view emits `AcceptRequested`.
    pub fn current_request(&self) -> RunAgentsRequest {
        self.state.to_request()
    }

    /// Records the in-flight dispatch snapshot. Inserted by
    /// `AIBlock::handle_run_agents_accept` immediately before the
    /// async dispatch batch starts; doubles as the idempotency guard
    /// for repeat Accept invocations.
    pub fn set_spawning_snapshot(
        &mut self,
        snapshot: RunAgentsSpawningSnapshot,
        ctx: &mut ViewContext<Self>,
    ) {
        self.spawning = Some(snapshot);
        ctx.notify();
    }

    /// Clears the in-flight dispatch snapshot. Called from the async
    /// outcome callback before the terminal `RunAgentsResult` is
    /// applied so the post-action card replaces the "Spawning…" card
    /// on the next render.
    pub fn clear_spawning_snapshot(&mut self, ctx: &mut ViewContext<Self>) {
        self.spawning = None;
        ctx.notify();
    }

    /// Returns true when this card is currently mid-dispatch. Read by
    /// `AIBlock::handle_run_agents_accept` as an idempotency guard so
    /// repeat invocations (Enter auto-repeat, double-click) don't
    /// dispatch the batch twice.
    pub fn is_spawning(&self) -> bool {
        self.spawning.is_some()
    }

    fn handle_toggle_edit(&mut self, ctx: &mut ViewContext<Self>) {
        self.state.is_editor_open = !self.state.is_editor_open;

        // Lazily build the model/harness/environment picker views the
        // first time the editor is opened. Building them here (rather
        // than at view construction) avoids spinning up dropdown
        // entities for cards the user never edits, and keeps the
        // picker views alive across editor toggles so their internal
        // selection/focus state is preserved.
        if self.state.is_editor_open {
            self.ensure_pickers(ctx);
        }

        // Swap Reject ↔ Discard-edits label + shortcut chip based on
        // whether the editor is open.
        self.sync_card_buttons(ctx);
        ctx.notify();
    }

    /// Update the Edit button label/keystroke to reflect the current
    /// `RunAgentsEditState`. Per Figma 4340:117057, when the inline
    /// editor is open the Edit button becomes "Discard edits" with
    /// an `Esc` shortcut chip; closed, it reverts to "Edit" with
    /// `Cmd/Ctrl-E`.
    fn sync_card_buttons(&mut self, ctx: &mut ViewContext<Self>) {
        let Some(edit_button) = self.handles.edit_button.as_mut() else {
            return;
        };
        let (label, keystroke) = if self.state.is_editor_open {
            (
                "Discard edits".to_string(),
                Keystroke::parse("escape")
                    .expect("orchestrate discard-edits keystroke literal must parse"),
            )
        } else {
            (
                "Edit".to_string(),
                Keystroke::parse("cmdorctrl-e")
                    .expect("orchestrate edit keystroke literal must parse"),
            )
        };
        edit_button.set_label(label, ctx);
        edit_button.set_keybinding(Some(KeystrokeSource::Fixed(keystroke)), ctx);
    }

    /// Lazily construct the model/harness/environment dropdown views.
    /// Idempotent: re-running this with already-populated handles is a
    /// no-op for those entries.
    fn ensure_pickers(&mut self, ctx: &mut ViewContext<Self>) {
        // Figma orchestrate inline-editor picker styling helpers (node
        // 4340:117057). Per the design, each dropdown shares the same
        // 36px-tall pill-styled top bar regardless of which dropdown
        // type backs it; centralising the values here avoids drift
        // between Dropdown<RunAgentsCardViewAction> and
        // FilterableDropdown<...>.
        const RUN_AGENTS_PICKER_HEIGHT: f32 = 36.;
        const ORCHESTRATE_PICKER_RADIUS: f32 = 4.;
        const RUN_AGENTS_PICKER_BORDER_WIDTH: f32 = 1.;
        const RUN_AGENTS_PICKER_FONT_SIZE: f32 = 14.;
        let picker_padding = Coords {
            top: 8.,
            bottom: 8.,
            left: 12.,
            right: 12.,
        };
        let picker_corner_radius =
            CornerRadius::with_all(Radius::Pixels(ORCHESTRATE_PICKER_RADIUS));
        let picker_border_color_warpui: warpui::elements::Fill =
            Fill::Solid(ColorU::new(0x29, 0x29, 0x29, 0xff)).into();
        let picker_font_color = ColorU::new(0xe3, 0xe2, 0xdf, 0xff);
        let picker_background_theme: Fill = Appearance::as_ref(ctx).theme().surface_overlay_1();
        let picker_background_warpui: warpui::elements::Fill = picker_background_theme.into();
        let picker_styles = UiComponentStyles {
            height: Some(RUN_AGENTS_PICKER_HEIGHT),
            background: Some(picker_background_warpui),
            border_color: Some(picker_border_color_warpui),
            border_width: Some(RUN_AGENTS_PICKER_BORDER_WIDTH),
            border_radius: Some(picker_corner_radius),
            font_size: Some(RUN_AGENTS_PICKER_FONT_SIZE),
            font_color: Some(picker_font_color),
            padding: Some(picker_padding),
            ..Default::default()
        };

        let initial_model_id_default = self
            .block_model
            .base_model(ctx)
            .map(|id| id.to_string())
            .unwrap_or_default();
        let state_snapshot = self.state.clone();

        if self.handles.model_picker.is_none() {
            let initial_model_id = if state_snapshot.model_id.trim().is_empty() {
                initial_model_id_default.clone()
            } else {
                state_snapshot.model_id.clone()
            };
            let picker_padding_clone = picker_padding;
            let picker_corner_radius_clone = picker_corner_radius;
            let picker_background_clone = picker_background_warpui;
            let picker_border_color_clone = picker_border_color_warpui;
            let dropdown_handle = ctx.add_typed_action_view(move |ctx_dropdown| {
                let mut dropdown = Dropdown::<RunAgentsCardViewAction>::new(ctx_dropdown);
                dropdown.set_use_overlay_layer(false, ctx_dropdown);
                dropdown.set_main_axis_size(MainAxisSize::Max, ctx_dropdown);
                dropdown.set_style(DropdownStyle::ActionButtonSecondary, ctx_dropdown);
                dropdown.set_top_bar_height(RUN_AGENTS_PICKER_HEIGHT, ctx_dropdown);
                dropdown.set_padding(picker_padding_clone, ctx_dropdown);
                dropdown.set_border_radius(picker_corner_radius_clone, ctx_dropdown);
                dropdown.set_background(picker_background_clone, ctx_dropdown);
                dropdown.set_border_color(picker_border_color_clone, ctx_dropdown);
                dropdown.set_border_width(RUN_AGENTS_PICKER_BORDER_WIDTH, ctx_dropdown);
                dropdown.set_font_size(RUN_AGENTS_PICKER_FONT_SIZE, ctx_dropdown);
                dropdown.set_font_color(picker_font_color, ctx_dropdown);
                dropdown
            });
            dropdown_handle.update(ctx, |dropdown, ctx_dropdown| {
                let llm_prefs = LLMPreferences::as_ref(ctx_dropdown);
                let choices: Vec<_> = llm_prefs.get_base_llm_choices_for_agent_mode().collect();
                let initial_index = choices
                    .iter()
                    .position(|llm| llm.id.to_string() == initial_model_id);
                let items = available_model_menu_items(
                    choices,
                    move |llm| {
                        DropdownAction::SelectActionAndClose(
                            RunAgentsCardViewAction::ModelChanged {
                                model_id: llm.id.to_string(),
                            },
                        )
                    },
                    None,
                    None,
                    false,
                    false,
                    ctx_dropdown,
                );
                dropdown.set_rich_items(items, ctx_dropdown);
                if let Some(idx) = initial_index {
                    dropdown.set_selected_by_index(idx, ctx_dropdown);
                }
            });
            Self::subscribe_picker_close(&dropdown_handle, ctx);
            self.handles.model_picker = Some(dropdown_handle);
        }

        if self.handles.harness_picker.is_none() {
            let initial_harness = state_snapshot.harness_type.clone();
            let picker_padding_clone = picker_padding;
            let picker_corner_radius_clone = picker_corner_radius;
            let picker_background_clone = picker_background_warpui;
            let picker_border_color_clone = picker_border_color_warpui;
            let dropdown_handle = ctx.add_typed_action_view(move |ctx_dropdown| {
                let mut dropdown = Dropdown::<RunAgentsCardViewAction>::new(ctx_dropdown);
                dropdown.set_use_overlay_layer(false, ctx_dropdown);
                dropdown.set_main_axis_size(MainAxisSize::Max, ctx_dropdown);
                dropdown.set_style(DropdownStyle::ActionButtonSecondary, ctx_dropdown);
                dropdown.set_top_bar_height(RUN_AGENTS_PICKER_HEIGHT, ctx_dropdown);
                dropdown.set_padding(picker_padding_clone, ctx_dropdown);
                dropdown.set_border_radius(picker_corner_radius_clone, ctx_dropdown);
                dropdown.set_background(picker_background_clone, ctx_dropdown);
                dropdown.set_border_color(picker_border_color_clone, ctx_dropdown);
                dropdown.set_border_width(RUN_AGENTS_PICKER_BORDER_WIDTH, ctx_dropdown);
                dropdown.set_font_size(RUN_AGENTS_PICKER_FONT_SIZE, ctx_dropdown);
                dropdown.set_font_color(picker_font_color, ctx_dropdown);
                dropdown
            });
            dropdown_handle.update(ctx, |dropdown, ctx_dropdown| {
                let mut items: Vec<MenuItem<DropdownAction<RunAgentsCardViewAction>>> = Vec::new();
                let mut selected_idx = None;
                for (idx, harness) in [Harness::Oz, Harness::Claude, Harness::Gemini]
                    .into_iter()
                    .enumerate()
                {
                    let mut fields = MenuItemFields::new(harness_display::display_name(harness))
                        .with_icon(harness_display::icon_for(harness));
                    if let Some(color) = harness_display::brand_color(harness) {
                        fields = fields.with_override_icon_color(Fill::from(color));
                    }
                    let harness_str = harness.to_string();
                    fields = fields.with_on_select_action(DropdownAction::SelectActionAndClose(
                        RunAgentsCardViewAction::HarnessChanged {
                            harness_type: harness_str.clone(),
                        },
                    ));
                    if harness_str.eq_ignore_ascii_case(&initial_harness) {
                        selected_idx = Some(idx);
                    }
                    items.push(MenuItem::Item(fields));
                }
                dropdown.set_rich_items(items, ctx_dropdown);
                if let Some(idx) = selected_idx {
                    dropdown.set_selected_by_index(idx, ctx_dropdown);
                }
            });
            Self::subscribe_picker_close(&dropdown_handle, ctx);
            self.handles.harness_picker = Some(dropdown_handle);
        }

        if self.handles.environment_picker.is_none() {
            let initial_env = match &state_snapshot.execution_mode {
                RunAgentsExecutionMode::Remote { environment_id, .. } => environment_id.clone(),
                RunAgentsExecutionMode::Local => String::new(),
            };
            let picker_styles_clone = picker_styles;
            let dropdown_handle = ctx.add_typed_action_view(move |ctx_dropdown| {
                let mut dropdown = FilterableDropdown::<RunAgentsCardViewAction>::new(ctx_dropdown);
                dropdown.set_use_overlay_layer(false, ctx_dropdown);
                dropdown.set_main_axis_size(MainAxisSize::Max, ctx_dropdown);
                dropdown.set_button_variant(ButtonVariant::Secondary);
                dropdown.set_style(picker_styles_clone);
                dropdown.set_top_bar_height(RUN_AGENTS_PICKER_HEIGHT, ctx_dropdown);
                dropdown
            });
            dropdown_handle.update(ctx, |dropdown, ctx_dropdown| {
                dropdown.set_menu_width(280.0, ctx_dropdown);
                let envs = AgentConversationsModel::as_ref(ctx_dropdown)
                    .get_all_environment_ids_and_names(ctx_dropdown);
                let mut sorted_envs: Vec<(String, String)> = envs.into_iter().collect();
                sorted_envs.sort_by(|a, b| a.1.cmp(&b.1));

                let mut items: Vec<MenuItem<DropdownAction<RunAgentsCardViewAction>>> = Vec::new();
                let mut selected_name: Option<String> = None;
                if sorted_envs.is_empty() {
                    dropdown.set_menu_header_text_override(|_| {
                        "Environment: loading\u{2026}".to_string()
                    });
                }
                // Round 6 follow-up B1: prepend a "(no environment)"
                // item so the user can deselect a previously chosen
                // environment and revert to the empty-env state.
                items.push(MenuItem::Item(
                    MenuItemFields::new(RUN_AGENTS_ENV_NONE_LABEL).with_on_select_action(
                        DropdownAction::SelectActionAndClose(
                            RunAgentsCardViewAction::EnvironmentChanged {
                                environment_id: String::new(),
                            },
                        ),
                    ),
                ));
                if initial_env.is_empty() {
                    selected_name = Some(RUN_AGENTS_ENV_NONE_LABEL.to_string());
                }
                for (env_id, env_name) in &sorted_envs {
                    if env_id == &initial_env {
                        selected_name = Some(env_name.clone());
                    }
                    let env_id_for_item = env_id.clone();
                    items.push(MenuItem::Item(
                        MenuItemFields::new(env_name).with_on_select_action(
                            DropdownAction::SelectActionAndClose(
                                RunAgentsCardViewAction::EnvironmentChanged {
                                    environment_id: env_id_for_item,
                                },
                            ),
                        ),
                    ));
                }
                dropdown.set_rich_items(items, ctx_dropdown);
                if let Some(name) = selected_name {
                    dropdown.set_selected_by_name(&name, ctx_dropdown);
                }
            });
            // FilterableDropdown variant; subscribe to its Close event
            // separately from the Dropdown helper.
            ctx.subscribe_to_view(&dropdown_handle, |me, _, event, ctx| {
                if let FilterableDropdownEvent::Close = event {
                    ctx.focus_self();
                    me.refocus_after_picker_close(ctx);
                }
            });
            self.handles.environment_picker = Some(dropdown_handle);
        }

        if self.handles.host_picker.is_none() {
            let picker_padding_clone = picker_padding;
            let picker_corner_radius_clone = picker_corner_radius;
            let picker_background_clone = picker_background_warpui;
            let picker_border_color_clone = picker_border_color_warpui;
            let dropdown_handle = ctx.add_typed_action_view(move |ctx_dropdown| {
                let mut dropdown = Dropdown::<RunAgentsCardViewAction>::new(ctx_dropdown);
                dropdown.set_use_overlay_layer(false, ctx_dropdown);
                dropdown.set_main_axis_size(MainAxisSize::Max, ctx_dropdown);
                dropdown.set_style(DropdownStyle::ActionButtonSecondary, ctx_dropdown);
                dropdown.set_top_bar_height(RUN_AGENTS_PICKER_HEIGHT, ctx_dropdown);
                dropdown.set_padding(picker_padding_clone, ctx_dropdown);
                dropdown.set_border_radius(picker_corner_radius_clone, ctx_dropdown);
                dropdown.set_background(picker_background_clone, ctx_dropdown);
                dropdown.set_border_color(picker_border_color_clone, ctx_dropdown);
                dropdown.set_border_width(RUN_AGENTS_PICKER_BORDER_WIDTH, ctx_dropdown);
                dropdown.set_font_size(RUN_AGENTS_PICKER_FONT_SIZE, ctx_dropdown);
                dropdown.set_font_color(picker_font_color, ctx_dropdown);
                dropdown
            });
            dropdown_handle.update(ctx, |dropdown, ctx_dropdown| {
                // Visual-only Host picker. The lone "Warp" item has no
                // `on_select_action`, so clicking it just closes the
                // menu without dispatching anything.
                let item = MenuItemFields::new("Warp".to_string());
                dropdown.set_rich_items(vec![MenuItem::Item(item)], ctx_dropdown);
                dropdown.set_selected_by_index(0, ctx_dropdown);
            });
            Self::subscribe_picker_close(&dropdown_handle, ctx);
            self.handles.host_picker = Some(dropdown_handle);
        }

        // Force the picker top-bar labels to reflect the current state.
        // The Dropdown's internal `MenuEvent::ItemSelected`
        // subscription is unreliable inside this view tree (the top-bar
        // text stays blank even after the menu's selected_row_index is
        // set), so we explicitly drive the displayed selection here and
        // again after every state-mutating action.
        self.sync_picker_selections(ctx);
    }

    fn subscribe_picker_close(
        dropdown_handle: &ViewHandle<Dropdown<RunAgentsCardViewAction>>,
        ctx: &mut ViewContext<Self>,
    ) {
        ctx.subscribe_to_view(dropdown_handle, move |me, _, event, ctx| {
            if let DropdownEvent::Close = event {
                ctx.focus_self();
                me.refocus_after_picker_close(ctx);
            }
        });
    }

    /// Restore focus to the card view after a picker dropdown closes.
    /// Without this, focus is left on the now-hidden Dropdown view and
    /// the card's own keymap context (e.g. the `enter → Accept`
    /// binding) no longer resolves.
    fn refocus_after_picker_close(&self, ctx: &mut ViewContext<Self>) {
        ctx.focus_self();
    }

    /// Re-sync each picker's displayed selection with the authoritative
    /// `RunAgentsEditState`. Called after creating the pickers and
    /// after every state-mutating action so the top-bar label always
    /// matches the underlying state.
    fn sync_picker_selections(&mut self, ctx: &mut ViewContext<Self>) {
        let state = self.state.clone();
        if let Some(model_picker) = self.handles.model_picker.clone() {
            let target_model_id = state.model_id.clone();
            model_picker.update(ctx, |dropdown, ctx_dropdown| {
                let llm_prefs = LLMPreferences::as_ref(ctx_dropdown);
                let choices: Vec<_> = llm_prefs.get_base_llm_choices_for_agent_mode().collect();
                if let Some(idx) = choices
                    .iter()
                    .position(|llm| llm.id.to_string() == target_model_id)
                {
                    dropdown.set_selected_by_index(idx, ctx_dropdown);
                }
            });
        }
        if let Some(harness_picker) = self.handles.harness_picker.clone() {
            let target =
                Harness::parse_orchestration_harness(&state.harness_type).unwrap_or(Harness::Oz);
            let display = harness_display::display_name(target).to_string();
            harness_picker.update(ctx, |dropdown, ctx_dropdown| {
                dropdown.set_selected_by_name(&display, ctx_dropdown);
            });
        }
        if let Some(environment_picker) = self.handles.environment_picker.clone() {
            let env_id = match &state.execution_mode {
                RunAgentsExecutionMode::Remote { environment_id, .. } => environment_id.clone(),
                RunAgentsExecutionMode::Local => String::new(),
            };
            environment_picker.update(ctx, |dropdown, ctx_dropdown| {
                if env_id.is_empty() {
                    dropdown.set_selected_by_name(RUN_AGENTS_ENV_NONE_LABEL, ctx_dropdown);
                    return;
                }
                let envs = AgentConversationsModel::as_ref(ctx_dropdown)
                    .get_all_environment_ids_and_names(ctx_dropdown);
                if let Some((_, name)) = envs.into_iter().find(|(id, _)| id == &env_id) {
                    dropdown.set_selected_by_name(&name, ctx_dropdown);
                }
            });
        }
        if let Some(host_picker) = self.handles.host_picker.clone() {
            host_picker.update(ctx, |dropdown, ctx_dropdown| {
                dropdown.set_selected_by_index(0, ctx_dropdown);
            });
        }
    }
}

impl Entity for RunAgentsCardView {
    type Event = RunAgentsCardViewEvent;
}

impl View for RunAgentsCardView {
    fn ui_name() -> &'static str {
        "RunAgentsCardView"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        let status = self
            .action_model
            .as_ref(app)
            .get_action_status(&self.action_id);

        if let Some(AIActionStatus::Finished(result)) = &status {
            if let AIAgentActionResultType::RunAgents(orchestrate_result) = &result.result {
                return render_terminal_state(orchestrate_result, appearance, app);
            }
            log::error!(
                "Unexpected action result type for orchestrate: {:?}",
                result.result
            );
            return Empty::new().finish();
        }

        // In-flight: the user has accepted the orchestrate card and
        // the async dispatch batch is running. Render the "Spawning N
        // agents…" card until the outcome callback clears the
        // snapshot. This addresses the UX gap where the confirmation
        // card otherwise stays visible for hundreds of ms (Local +
        // harness `create_agent_task` round-trip), tempting users to
        // mash Enter.
        if let Some(snapshot) = &self.spawning {
            return render_spawning_card(snapshot, appearance, app);
        }

        // Restored-from-history but not finished: there's no point in
        // showing an interactive confirmation card because the
        // action's pending dispatch state has been lost on restore.
        // Render as Cancelled, mirroring how `set_restored_file_edits`
        // on the apply-diff tool call marks restored-pending edits as
        // Rejected.
        if self.block_model.is_restored() {
            return render_status_only_card(
                "Spawn agents cancelled".to_string(),
                appearance,
                StatusKind::Cancelled,
                app,
            );
        }

        let is_blocked = matches!(status, Some(AIActionStatus::Blocked));
        render_confirmation_card(&self.state, &self.handles, is_blocked, app)
    }

    fn keymap_context(&self, _app: &AppContext) -> warpui::keymap::Context {
        let mut context = Self::default_keymap_context();
        if self.state.is_editor_open {
            context.set.insert(RUN_AGENTS_EDITOR_OPEN);
        }
        context
    }
}

impl TypedActionView for RunAgentsCardView {
    type Action = RunAgentsCardViewAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            RunAgentsCardViewAction::Accept => {
                ctx.emit(RunAgentsCardViewEvent::AcceptRequested);
            }
            RunAgentsCardViewAction::Reject => {
                ctx.emit(RunAgentsCardViewEvent::RejectRequested);
            }
            RunAgentsCardViewAction::ToggleEdit => {
                self.handle_toggle_edit(ctx);
            }
            RunAgentsCardViewAction::DiscardEdits => {
                if self.state.is_editor_open {
                    self.handle_toggle_edit(ctx);
                }
            }
            RunAgentsCardViewAction::ExecutionModeToggled { is_remote } => {
                self.state.toggle_execution_mode_to_remote(*is_remote);
                // The Local→Cloud transition can programmatically
                // reset OpenCode→Oz; keep the harness dropdown's
                // display in sync with that change so the user sees
                // the active harness.
                self.sync_picker_selections(ctx);
                ctx.notify();
            }
            RunAgentsCardViewAction::ModelChanged { model_id } => {
                self.state.model_id = model_id.clone();
                // Note: do NOT call `sync_picker_selections` here.
                // This action is dispatched synchronously from the
                // model_picker dropdown's `select_action_and_close`
                // while its `update_view` is mid-execution; calling
                // `model_picker.update(...)` from this handler would
                // panic with "Circular view update". The dropdown's
                // own `MenuEvent::ItemSelected` subscription updates
                // the displayed selection after the dispatch chain
                // unwinds.
                ctx.notify();
            }
            RunAgentsCardViewAction::HarnessChanged { harness_type } => {
                self.state.harness_type = harness_type.clone();
                // See note on ModelChanged above.
                ctx.notify();
            }
            RunAgentsCardViewAction::EnvironmentChanged { environment_id } => {
                self.state.set_environment_id(environment_id.clone());
                // See note on ModelChanged above.
                ctx.notify();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free render functions
// ---------------------------------------------------------------------------

fn render_confirmation_card(
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    is_blocked: bool,
    app: &AppContext,
) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();

    let header = render_header(handles, app);
    let body = render_body(state, app);

    let mut content = Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(header)
        .with_child(body);

    if state.is_editor_open {
        content.add_child(render_editor(state, handles, app));
    }

    let border_color = if is_blocked {
        theme.accent()
    } else {
        theme.surface_2()
    };

    Container::new(content.finish())
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
        .with_border(Border::all(1.).with_border_fill(border_color))
        .finish()
        .with_content_item_spacing()
        .finish()
}

fn render_header(handles: &RunAgentsCardHandles, app: &AppContext) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let mut config = HeaderConfig::new(RUN_AGENTS_CARD_TITLE, app)
        .with_icon(icons::run_agents_stop_icon(appearance))
        .with_corner_radius_override(CornerRadius::with_top(Radius::Pixels(8.)));

    if let (Some(reject), Some(edit), Some(accept)) = (
        handles.reject_button.as_ref(),
        handles.edit_button.as_ref(),
        handles.accept_button.as_ref(),
    ) {
        let action_buttons: Vec<Rc<dyn RenderCompactibleActionButton>> = vec![
            Rc::new(reject.clone()),
            Rc::new(edit.clone()),
            Rc::new(accept.clone()),
        ];
        config = config.with_interaction_mode(InteractionMode::ActionButtons {
            action_buttons,
            size_switch_threshold: MEDIUM_SIZE_SWITCH_THRESHOLD,
        });
    }

    config.render(app)
}

fn render_body(state: &RunAgentsEditState, app: &AppContext) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

    column.add_child(render_summary(state, appearance));
    column.add_child(render_agents_section(state, app));

    Container::new(column.finish())
        .with_horizontal_padding(16.)
        .with_vertical_padding(12.)
        .with_background_color(theme.background().into_solid())
        .with_corner_radius(CornerRadius::with_bottom(Radius::Pixels(8.)))
        .finish()
}

fn render_summary(state: &RunAgentsEditState, appearance: &Appearance) -> Box<dyn Element> {
    let theme = appearance.theme();
    let summary = if state.summary.trim().is_empty() {
        format!(
            "Spawn {} agent(s) to address this task.",
            state.agent_run_configs.len()
        )
    } else {
        state.summary.clone()
    };
    let summary_text = Text::new(
        summary,
        appearance.ui_font_family(),
        appearance.monospace_font_size(),
    )
    .with_color(blended_colors::text_main(theme, theme.background()))
    .with_selectable(true)
    .finish();

    Container::new(summary_text)
        .with_margin_bottom(12.)
        .finish()
}

fn render_agents_section(state: &RunAgentsEditState, app: &AppContext) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    let label = Text::new(
        format!("Agents ({})", state.agent_run_configs.len()),
        appearance.ui_font_family(),
        appearance.monospace_font_size() - 1.,
    )
    .with_color(blended_colors::text_disabled(theme, theme.background()))
    .finish();

    let mut pills_row = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Center)
        .with_main_axis_size(MainAxisSize::Min)
        .with_spacing(4.);
    for cfg in &state.agent_run_configs {
        pills_row.add_child(render_static_agent_pill(&cfg.name, app));
    }

    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(Container::new(label).with_margin_bottom(6.).finish())
        .with_child(pills_row.finish())
        .finish()
}

fn render_terminal_state(
    result: &RunAgentsResult,
    appearance: &Appearance,
    app: &AppContext,
) -> Box<dyn Element> {
    let (label, kind) = format_terminal_state(result);
    render_status_only_card(label, appearance, kind, app)
}

/// Maps a terminal `RunAgentsResult` to the user-visible label + the
/// status icon kind shown by `render_status_only_card`. Pure-function
/// extraction so the label-format / pluralization rules can be unit
/// tested without spinning up a view context.
pub(crate) fn format_terminal_state(result: &RunAgentsResult) -> (String, StatusKind) {
    match result {
        RunAgentsResult::Launched { agents, .. } => {
            let total = agents.len();
            let launched = agents
                .iter()
                .filter(|a| matches!(a.kind, RunAgentsAgentOutcomeKind::Launched { .. }))
                .count();
            let label = if launched == total {
                if total == 1 {
                    "Spawned 1 agent".to_string()
                } else {
                    format!("Spawned {total} agents")
                }
            } else {
                format!("Spawned {launched} of {total} agents")
            };
            let kind = if launched == total {
                StatusKind::Success
            } else {
                StatusKind::Mixed
            };
            (label, kind)
        }
        RunAgentsResult::Denied { reason } => {
            let body = if reason.is_empty() {
                "Orchestration is currently disabled. Re-enable on the plan card to launch."
                    .to_string()
            } else {
                format!(
                    "Orchestration is currently disabled. Re-enable on the plan card to launch. ({reason})"
                )
            };
            (body, StatusKind::Cancelled)
        }
        RunAgentsResult::Failure { error } => {
            let label = if error.is_empty() {
                "Failed to start orchestration".to_string()
            } else {
                format!("Failed to start orchestration: {error}")
            };
            (label, StatusKind::Failure)
        }
        RunAgentsResult::Cancelled => ("Spawn agents cancelled".to_string(), StatusKind::Cancelled),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StatusKind {
    Spawning,
    Success,
    Mixed,
    Failure,
    Cancelled,
}

fn render_spawning_card(
    snapshot: &RunAgentsSpawningSnapshot,
    appearance: &Appearance,
    app: &AppContext,
) -> Box<dyn Element> {
    let total = snapshot.agent_count;
    let label = if total == 1 {
        "Spawning 1 agent\u{2026}".to_string()
    } else {
        format!("Spawning {total} agents\u{2026}")
    };
    render_status_only_card(label, appearance, StatusKind::Spawning, app)
}

fn render_status_only_card(
    label: String,
    appearance: &Appearance,
    kind: StatusKind,
    app: &AppContext,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let icon = match kind {
        StatusKind::Spawning | StatusKind::Mixed => icons::yellow_running_icon(appearance).finish(),
        StatusKind::Success => inline_action_icons::green_check_icon(appearance).finish(),
        StatusKind::Failure => inline_action_icons::red_x_icon(appearance).finish(),
        StatusKind::Cancelled => inline_action_icons::cancelled_icon(appearance).finish(),
    };
    let row = render_requested_action_row_for_text(
        label.into(),
        appearance.ui_font_family(),
        Some(icon),
        None,
        false,
        false,
        app,
    );
    Container::new(
        Container::new(row)
            .with_background_color(blended_colors::neutral_2(theme))
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(8.)))
            .finish(),
    )
    .with_margin_left(16.)
    .with_margin_right(16.)
    .finish()
    .with_agent_output_item_spacing(app)
    .finish()
}

fn render_editor(
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    app: &AppContext,
) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

    let divider = Container::new(
        ConstrainedBox::new(Empty::new().finish())
            .with_height(1.)
            .finish(),
    )
    .with_background_color(theme.surface_2().into_solid())
    .finish();
    column.add_child(divider);

    column.add_child(
        Container::new(render_mode_toggle(state, handles, appearance))
            .with_margin_top(12.)
            .finish(),
    );
    column.add_child(render_picker_row_quad(state, handles, appearance));

    if let Some(reason) = state.accept_disabled_reason() {
        column.add_child(render_validation_error(
            reason,
            theme.ui_error_color(),
            appearance,
        ));
    } else if let Some(message) = empty_env_recommendation_message(state, app) {
        column.add_child(render_validation_error(
            message,
            theme.ui_warning_color(),
            appearance,
        ));
    }

    Container::new(column.finish())
        .with_horizontal_padding(16.)
        .with_padding_bottom(12.)
        .with_background_color(theme.background().into_solid())
        .with_corner_radius(CornerRadius::with_bottom(Radius::Pixels(8.)))
        .finish()
}

fn render_picker_row_quad(
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let is_remote = state.execution_mode.is_remote();
    let main_axis_size = if is_remote {
        MainAxisSize::Max
    } else {
        MainAxisSize::Min
    };
    let mut row = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_main_axis_size(main_axis_size)
        .with_main_axis_alignment(MainAxisAlignment::Start)
        .with_spacing(12.);

    const LOCAL_PICKER_WIDTH: f32 = 220.;
    let add_picker = |row: &mut Flex, label: &str, picker: Option<Box<dyn Element>>| {
        let column = render_picker_column(label, picker, appearance);
        if is_remote {
            row.add_child(Expanded::new(1.0, column).finish());
        } else {
            row.add_child(
                ConstrainedBox::new(column)
                    .with_width(LOCAL_PICKER_WIDTH)
                    .finish(),
            );
        }
    };

    add_picker(
        &mut row,
        "Agent harness",
        handles
            .harness_picker
            .as_ref()
            .map(|p| ChildView::new(p).finish()),
    );
    if is_remote {
        add_picker(
            &mut row,
            "Host",
            handles
                .host_picker
                .as_ref()
                .map(|p| ChildView::new(p).finish()),
        );
        add_picker(
            &mut row,
            "Environment",
            handles
                .environment_picker
                .as_ref()
                .map(|p| ChildView::new(p).finish()),
        );
    }
    add_picker(
        &mut row,
        "Base model",
        handles
            .model_picker
            .as_ref()
            .map(|p| ChildView::new(p).finish()),
    );

    Container::new(row.finish()).with_margin_top(12.).finish()
}

fn render_picker_column(
    label: &str,
    picker: Option<Box<dyn Element>>,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let label_el = Text::new(
        label.to_string(),
        appearance.ui_font_family(),
        appearance.monospace_font_size() - 1.,
    )
    .with_color(blended_colors::text_disabled(theme, theme.surface_1()))
    .finish();

    let body: Box<dyn Element> = picker.unwrap_or_else(|| Empty::new().finish());
    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(label_el)
        .with_child(body)
        .finish()
}

fn render_mode_toggle(
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let is_remote = state.execution_mode.is_remote();
    let label = Text::new(
        "Agent location".to_string(),
        appearance.ui_font_family(),
        appearance.monospace_font_size() - 1.,
    )
    .with_color(blended_colors::text_disabled(theme, theme.surface_1()))
    .finish();

    let local_segment = render_segment_button(
        "Local",
        !is_remote,
        RunAgentsCardViewAction::ExecutionModeToggled { is_remote: false },
        handles.local_toggle.clone(),
        appearance,
    );
    let cloud_segment = render_segment_button(
        "Cloud",
        is_remote,
        RunAgentsCardViewAction::ExecutionModeToggled { is_remote: true },
        handles.cloud_toggle.clone(),
        appearance,
    );

    let segment_outer_bg = warp_core::ui::theme::color::internal_colors::fg_overlay_2(theme);
    let segments_row = Flex::row()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_main_axis_alignment(MainAxisAlignment::Start)
        .with_main_axis_size(MainAxisSize::Max)
        .with_child(Expanded::new(1.0, cloud_segment).finish())
        .with_child(Expanded::new(1.0, local_segment).finish())
        .finish();
    let segmented_control = Container::new(segments_row)
        .with_padding_top(4.)
        .with_padding_bottom(4.)
        .with_padding_left(4.)
        .with_padding_right(4.)
        .with_corner_radius(CornerRadius::with_all(Radius::Pixels(6.)))
        .with_background(segment_outer_bg)
        .finish();
    let segmented_control = ConstrainedBox::new(segmented_control)
        .with_width(205.)
        .finish();

    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(Container::new(label).with_margin_bottom(6.).finish())
        .with_child(segmented_control)
        .finish()
}

fn render_segment_button(
    label: &str,
    is_active: bool,
    on_click: RunAgentsCardViewAction,
    mouse_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let label_owned = label.to_string();
    let font_family = appearance.ui_font_family();
    let font_size = appearance.monospace_font_size() + 1.;
    let active_text_color = blended_colors::text_main(theme, theme.surface_1());
    let inactive_text_color = blended_colors::text_disabled(theme, theme.surface_1());
    let segment_active_bg = warp_core::ui::theme::color::internal_colors::fg_overlay_4(theme);
    Hoverable::new(mouse_state, move |_| {
        let text = Text::new(label_owned.clone(), font_family, font_size)
            .with_color(if is_active {
                active_text_color
            } else {
                inactive_text_color
            })
            .finish();
        let centered = warpui::elements::Align::new(text).finish();
        let mut container = Container::new(centered)
            .with_vertical_padding(6.)
            .with_corner_radius(CornerRadius::with_all(Radius::Pixels(4.)));
        if is_active {
            container = container.with_background(segment_active_bg);
        }
        container.finish()
    })
    .on_click(move |ctx, _, _| {
        ctx.dispatch_typed_action(on_click.clone());
    })
    .with_cursor(Cursor::PointingHand)
    .finish()
}

fn render_validation_error(
    reason: impl Into<String>,
    color: ColorU,
    appearance: &Appearance,
) -> Box<dyn Element> {
    Container::new(
        Text::new(
            reason.into(),
            appearance.ui_font_family(),
            appearance.monospace_font_size() - 1.,
        )
        .with_color(color)
        .finish(),
    )
    .with_margin_bottom(8.)
    .finish()
}

fn empty_env_recommendation_message(
    state: &RunAgentsEditState,
    app: &AppContext,
) -> Option<String> {
    let RunAgentsExecutionMode::Remote {
        environment_id,
        worker_host,
        ..
    } = &state.execution_mode
    else {
        return None;
    };
    if !environment_id.trim().is_empty() {
        return None;
    }
    if !worker_host.eq_ignore_ascii_case(RUN_AGENTS_WARP_WORKER_HOST) {
        return None;
    }
    let env_count = AgentConversationsModel::as_ref(app)
        .get_all_environment_ids_and_names(app)
        .len();
    Some(if env_count > 0 {
        "We recommend selecting an environment for cloud agents.".to_string()
    } else {
        "We recommend creating an environment for cloud agents.".to_string()
    })
}

#[cfg(test)]
#[path = "run_agents_card_view_tests.rs"]
mod tests;
