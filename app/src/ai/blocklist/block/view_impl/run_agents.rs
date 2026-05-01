//! Renders the inline confirmation card for an `orchestrate` tool call.
//!
//! Visual structure (Figma-driven):
//!  - A code-block-style outer shell: 1px border + rounded 8px corners. The
//!    border switches to the theme accent when the card is blocked on user
//!    confirmation, mirroring `requested_command.rs`.
//!  - A header bar (rendered via [`HeaderConfig`]) containing the static
//!    "Can I add additional agents to this task?" title, a leading
//!    `stop-filled` accent icon, and the Reject / Edit / Accept action
//!    cluster on the trailing edge.
//!  - A body region (`theme.background()` fill) holding, in order: an
//!    optional validation-error line (theme error color), the LLM-supplied
//!    `summary` text, an `Agents (N)` label, and a horizontal row of
//!    static agent pills.
//!  - When the inline editor is open, an inset surface_1 panel is appended
//!    below the body containing the Local/Cloud toggle and a four-column
//!    row of dropdown pickers (Agent harness, Host, Environment, Base
//!    model) per Figma node 4340:117057.
//!
//! Spec references: TECH.md §8, §9; PRODUCT.md "Confirmation card",
//! "Post-action card states", "Invariants".

use ai::agent::action::{RunAgentsExecutionMode, RunAgentsRequest};
use ai::agent::action_result::{RunAgentsAgentOutcomeKind, RunAgentsResult};
use pathfinder_color::ColorU;
use std::rc::Rc;
use warpui::elements::{
    Border, ChildView, ConstrainedBox, Container, CornerRadius, CrossAxisAlignment, Empty,
    Expanded, Flex, Hoverable, MainAxisAlignment, MainAxisSize, MouseStateHandle, ParentElement,
    Radius, Text,
};
use warpui::platform::Cursor;
use warpui::{AppContext, Element, SingletonEntity};

use crate::ai::agent::icons;
use crate::ai::agent::{AIAgentActionId, AIAgentActionResultType};
use crate::ai::agent_conversations_model::AgentConversationsModel;
use crate::ai::blocklist::action_model::AIActionStatus;
use crate::ai::blocklist::agent_view::orchestration_pill_bar::render_static_agent_pill;
use crate::ai::blocklist::block::{
    AIBlockAction, RunAgentsCardHandles, RunAgentsEditState, RunAgentsSpawningSnapshot,
};
use crate::ai::blocklist::inline_action::inline_action_header::{HeaderConfig, InteractionMode};
use crate::ai::blocklist::inline_action::inline_action_icons;
use crate::ai::blocklist::inline_action::requested_action::render_requested_action_row_for_text;
use crate::appearance::Appearance;
use crate::ui_components::blended_colors;
use crate::view_components::compactible_action_button::{
    RenderCompactibleActionButton, MEDIUM_SIZE_SWITCH_THRESHOLD,
};

/// Round 6 follow-up B3: canonical worker-host value (lowercase) used
/// throughout the orchestrate edit state. The recommendation copy in
/// `render_editor` is gated on this so non-Warp hosts — where the
/// environment concept doesn't apply — don't surface the recommendation.
const RUN_AGENTS_WARP_WORKER_HOST: &str = "warp";

use super::output::Props;
use super::WithContentItemSpacing;

/// Static title rendered in the orchestrate confirmation card header. Per
/// spec §8 this is invariant client copy; the LLM-supplied `summary` field
/// is repurposed as the body description.
const RUN_AGENTS_CARD_TITLE: &str = "Can I add additional agents to this task?";

/// Renders the full orchestrate confirmation card.
///
/// Dispatched from the tool-call view dispatcher in `output.rs`. The card
/// is gated on `FeatureFlag::RunAgentsTool` at the dispatcher level; when
/// the flag is off this function is never reached.
pub(super) fn render_run_agents(
    props: Props,
    action_id: &AIAgentActionId,
    req: &RunAgentsRequest,
    app: &AppContext,
) -> Box<dyn Element> {
    let appearance = Appearance::as_ref(app);
    let status = props.action_model.as_ref(app).get_action_status(action_id);

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

    // In-flight: the user has accepted the orchestrate card and the
    // async dispatch batch is running. Render the "Spawning N agents…"
    // card (visually mirrors the terminal `Launched` success card)
    // until the outcome callback removes the snapshot and applies the
    // terminal result. This addresses the UX gap where the
    // confirmation card otherwise stays visible for hundreds of ms
    // (Local+harness `create_agent_task` round-trip), tempting users
    // to mash Enter.
    if let Some(snapshot) = props.run_agents_spawning.get(action_id) {
        return render_spawning_card(snapshot, appearance, app);
    }

    // Restored-from-history but not finished: there's no point in
    // showing an interactive confirmation card because the action's
    // pending dispatch state has been lost on restore. Render as
    // Cancelled, mirroring how `set_restored_file_edits` on the
    // apply-diff tool call marks restored-pending edits as Rejected
    // (block.rs ~line 3241).
    if props.model.is_restored() {
        return render_status_only_card(
            "Spawn agents cancelled".to_string(),
            appearance,
            StatusKind::Cancelled,
            app,
        );
    }

    // Pre-dispatch confirmation layout. Pulls per-action edit state +
    // button handles from the AIBlock; the LLM-supplied request is the
    // source of truth until the user clicks Edit. Per the polish round
    // (P2.4) we no longer render a separate "Preparing orchestration..."
    // placeholder during streaming \u2014 the confirmation card stands in for
    // that intermediate state, mirroring how the edit/apply-diff
    // tool-call card behaves before the user accepts.
    let display_state = props
        .run_agents_edit_states
        .get(action_id)
        .cloned()
        .unwrap_or_else(|| RunAgentsEditState::from_request(req));
    let handles = props
        .run_agents_card_handles
        .get(action_id)
        .cloned()
        .unwrap_or_default();

    let is_blocked = matches!(status, Some(AIActionStatus::Blocked));
    render_confirmation_card(action_id, &display_state, &handles, is_blocked, app)
}

fn render_confirmation_card(
    action_id: &AIAgentActionId,
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
        content.add_child(render_editor(action_id, state, handles, app));
    }

    let border_color = if is_blocked {
        theme.accent()
    } else {
        theme.surface_2()
    };

    // P5.6: active confirmation card spans the full block-list width
    // (no agent-output indent), matching how the apply-diff card
    // renders when it's blocked on user confirmation. We use
    // `with_content_item_spacing()` (CONTENT_HORIZONTAL_PADDING on
    // both sides + standard bottom margin) which is the same wrapper
    // apply-diff uses in `render_requested_edits_output_message`'s
    // `Embedded` branch when the action is blocked.
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

    // P4.6: validation error moved out of the body and rendered
    // *below* the picker row inside `render_editor`, so it appears
    // adjacent to the offending field rather than at the top of the
    // body. The body now only carries summary + agents.
    column.add_child(render_summary_with_edit_chip(state, appearance));
    column.add_child(render_agents_section(state, app));

    Container::new(column.finish())
        .with_horizontal_padding(16.)
        .with_vertical_padding(12.)
        .with_background_color(theme.background().into_solid())
        .with_corner_radius(CornerRadius::with_bottom(Radius::Pixels(8.)))
        .finish()
}

fn render_summary_with_edit_chip(
    state: &RunAgentsEditState,
    appearance: &Appearance,
) -> Box<dyn Element> {
    // Per polish round 2 P2.3: the summary text is not editable, so the
    // `\u2318E` keyboard chip that originally appeared next to it has been
    // dropped. Render the LLM-supplied summary alone.
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

    // P5.8: 12px between summary text and the "Agents (N)" label
    // below it (was 8px), matching the Figma 4340:117104 column gap.
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
fn format_terminal_state(result: &RunAgentsResult) -> (String, StatusKind) {
    match result {
        RunAgentsResult::Launched { agents, .. } => {
            let total = agents.len();
            let launched = agents
                .iter()
                .filter(|a| matches!(a.kind, RunAgentsAgentOutcomeKind::Launched { .. }))
                .count();
            // Per P2.3, all-success uses "Spawned N agent(s)" with proper
            // pluralization; mixed uses "Spawned X of Y agents".
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
enum StatusKind {
    /// In-flight orchestrate dispatch — children are being spawned.
    /// Renders with `yellow_running_icon` to convey "in progress".
    Spawning,
    Success,
    Mixed,
    Failure,
    Cancelled,
}

/// Renders the in-flight "Spawning N agents…" card shown after the
/// user clicks Accept and before the async dispatch batch resolves.
/// Visually identical to the terminal `Launched` success card except
/// for the label text and the running-state icon.
fn render_spawning_card(
    snapshot: &RunAgentsSpawningSnapshot,
    appearance: &Appearance,
    app: &AppContext,
) -> Box<dyn Element> {
    let total = snapshot.agent_count;
    let label = if total == 1 {
        "Spawning 1 agent…".to_string()
    } else {
        format!("Spawning {total} agents…")
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
    // P3.2: post-action card uses the indented "narrow" inline-action
    // lane. The active confirmation card spans the full block width
    // (set by `render_confirmation_card`); once the action transitions
    // to a terminal state we render this status row indented from
    // both edges so it visually matches the post-action presentation
    // of other tool-call cards (e.g. apply-diff).
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
    action_id: &AIAgentActionId,
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    app: &AppContext,
) -> Box<dyn Element> {
    // Per Figma 4340:117057 the editor is a Local/Cloud segmented control
    // followed by a single horizontal row of four equally-distributed
    // dropdown columns: Agent harness, Host, Environment, Base model.
    // Each column renders a small grey label above the dropdown body.
    //
    // P4.7 / P5.2: the editor area no longer renders the gray surface_1
    // panel with rounded corners. Instead the editor is separated from
    // the body above by a 1px top divider element (inset 16px on the
    // left and 12px on the right per the Figma mock) and uses the
    // default block background.
    let appearance = Appearance::as_ref(app);
    let theme = appearance.theme();
    let mut column = Flex::column().with_cross_axis_alignment(CrossAxisAlignment::Stretch);

    // Top divider as a 1px-tall element. Per the Figma mock the
    // divider spans the full inner width of the card (no horizontal
    // inset) \u2014 the earlier 16px/12px insets (P5.2) were too
    // aggressive and left visible gaps next to the card's vertical
    // border.
    let divider = Container::new(
        ConstrainedBox::new(Empty::new().finish())
            .with_height(1.)
            .finish(),
    )
    .with_background_color(theme.surface_2().into_solid())
    .finish();
    column.add_child(divider);

    column.add_child(
        Container::new(render_mode_toggle(action_id, state, handles, appearance))
            .with_margin_top(12.)
            .finish(),
    );
    column.add_child(render_picker_row_quad(state, handles, appearance));

    // P4.6: hard validation error sits below the picker row, inside the
    // editor area, so it appears adjacent to the offending field. Round
    // 6 follow-up: only OpenCode+Cloud remains a hard error here
    // (rendered in `ui_error_color`); empty-env is handled below as a
    // soft recommendation.
    if let Some(reason) = state.accept_disabled_reason() {
        column.add_child(render_validation_error(
            reason,
            theme.ui_error_color(),
            appearance,
        ));
    } else if let Some(message) = empty_env_recommendation_message(state, app) {
        // Round 6 follow-up B2/B3: soft warning when Cloud mode is
        // selected with no environment_id and the worker host is
        // "warp". Doesn't block Accept.
        column.add_child(render_validation_error(
            message,
            theme.ui_warning_color(),
            appearance,
        ));
    }

    // P5.4: add rounded bottom corners that match the outer card's
    // 8px radius so the editor's solid background respects the
    // card's rounded shape (without this the editor's bg paints into
    // the corner pixels and the card's bottom-left/bottom-right
    // corners look truncated by the inset selection outline).
    Container::new(column.finish())
        .with_horizontal_padding(16.)
        .with_padding_bottom(12.)
        .with_background_color(theme.background().into_solid())
        .with_corner_radius(CornerRadius::with_bottom(Radius::Pixels(8.)))
        .finish()
}

/// Renders the dropdown row beneath the Local/Cloud toggle. Per spec
/// PRODUCT.md "Default state and prepopulation rules", Local mode shows
/// only `Agent harness` + `Base model`; Cloud mode adds `Host` +
/// `Environment` between them. Wraps each picker in `Expanded::new(1.0,
/// \u2026)` so the columns share the available width equally; the parent
/// `Flex::row` is set to `MainAxisSize::Max` to opt the children into
/// flexible sizing.
fn render_picker_row_quad(
    state: &RunAgentsEditState,
    handles: &RunAgentsCardHandles,
    appearance: &Appearance,
) -> Box<dyn Element> {
    // P4.5: in Local mode there are only two pickers (Agent harness +
    // Base model). The Figma calls for these to pack at their natural
    // width on the leading edge instead of stretching across the row.
    // Cloud mode keeps the four-column distributed layout.
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

    // Fixed width for Local-mode pickers. Cloud pickers use Expanded so
    // they share the available row width equally; Local mode packs
    // left at this fixed width per the Figma update (P4.5).
    const LOCAL_PICKER_WIDTH: f32 = 220.;
    let add_picker = |row: &mut Flex, label: &str, picker: Option<Box<dyn Element>>| {
        let column = render_picker_column(label, picker, appearance);
        if is_remote {
            row.add_child(Expanded::new(1.0, column).finish());
        } else {
            // Fixed-width column so the picker doesn't expand to fill
            // the row in Local mode.
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
    // Host + Environment only render in Cloud (Remote) mode. Local mode
    // hides them per PRODUCT.md \u00a7"Default state and prepopulation rules";
    // the underlying `RunAgentsExecutionMode::Local` variant has no
    // host or environment fields.
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

    // P4.8: 12px between the segmented control bottom and the picker
    // row (was 8px).
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
    // P5.3: drop the explicit margin_bottom on the label container.
    // The picker's top padding (P5.3 reduced to 4 in block.rs) is now
    // the only contributor to the label-to-text gap, which matches
    // the Figma mock.
    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Stretch)
        .with_child(label_el)
        .with_child(body)
        .finish()
}

/// Renders the "Agent location" label above a Local/Cloud segmented
/// control. Per Figma node 4340:117057 the segmented control is a
/// single rounded container with a translucent overlay background; the
/// selected option has a lighter rounded fill, and the unselected
/// option is fully transparent with muted text \u2014 only the labels are
/// visible side-by-side, with no border between them.
fn render_mode_toggle(
    action_id: &AIAgentActionId,
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
        AIBlockAction::RunAgentsExecutionModeToggled {
            action_id: action_id.clone(),
            is_remote: false,
        },
        handles.local_toggle.clone(),
        appearance,
    );
    let cloud_segment = render_segment_button(
        "Cloud",
        is_remote,
        AIBlockAction::RunAgentsExecutionModeToggled {
            action_id: action_id.clone(),
            is_remote: true,
        },
        handles.cloud_toggle.clone(),
        appearance,
    );

    // Single segmented-control container. Figma 4340:117057 specifies a
    // ~10% foreground overlay background with 4px inner padding around
    // two equal-width segments.
    //
    // P5.5: the segmented control is exactly 205px wide per Figma. The
    // two segments split that width evenly via Expanded so each is
    // ~98px (after 4px padding on each side of the outer container).
    // Order: Cloud first, then Local (per Figma 4340:117134).
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

    // 6px gap between the "Agent location" label and the segmented
    // control \u2014 enough breathing room so the label doesn't crowd
    // the buttons. The picker labels in `render_picker_column` keep
    // their tighter 0px gap because the pickers themselves carry
    // 8px of internal top padding.
    Flex::column()
        .with_cross_axis_alignment(CrossAxisAlignment::Start)
        .with_child(Container::new(label).with_margin_bottom(6.).finish())
        .with_child(segmented_control)
        .finish()
}

fn render_segment_button(
    label: &str,
    is_active: bool,
    on_click: AIBlockAction,
    mouse_state: MouseStateHandle,
    appearance: &Appearance,
) -> Box<dyn Element> {
    let theme = appearance.theme();
    let label_owned = label.to_string();
    let font_family = appearance.ui_font_family();
    // P4.9: bump font size and segment width per Figma. Body copy
    // uses `monospace_font_size()` (~13); segments need to be slightly
    // larger and roomier so they read as a control rather than inline
    // text.
    let font_size = appearance.monospace_font_size() + 1.;
    // Selected segment: lighter rounded background. Unselected: fully
    // transparent with muted text, blending into the outer container.
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
        // P5.5: each segment is centered horizontally inside the
        // Expanded(1.0) flex slot. Drop the wide horizontal padding
        // and let Flex fill divide the 205px-wide outer container
        // equally between segments; vertical padding still keeps the
        // 36px outer height.
        // `Align::new` defaults to center alignment.
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

/// Round 6 follow-up A: render the validation/recommendation line below
/// the picker row. Color is now caller-supplied so the same slot can
/// surface either a hard `ui_error_color` block (OpenCode+Cloud) or a
/// softer `ui_warning_color` recommendation (Cloud without env on
/// Warp host). Font dropped from 13px → 12px (`monospace_font_size() - 1.`)
/// to align with the surrounding field labels and "Agents (N)" label.
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

/// Round 6 follow-up B2/B3: returns the recommendation copy for the
/// Cloud-mode editor when the user has not selected an environment AND
/// the worker host is "warp". Returns `None` for Local mode, when an
/// environment is selected, or when a non-Warp worker host is in use
/// (environments don't apply outside the Warp host so the warning is
/// suppressed). Two variants based on whether any environments are
/// available to choose from.
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
#[path = "run_agents_tests.rs"]
mod tests;
