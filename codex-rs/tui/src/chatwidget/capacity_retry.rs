//! Bounded, unattended retries of capacity failures without adding user messages.

use super::*;

const MAX_CAPACITY_RETRIES: u8 = 10;

#[derive(Default)]
pub(super) struct CapacityRetryState {
    pub(super) attempts: u8,
    last_failed_turn: Option<String>,
    pub(super) pending: Option<PendingCapacityRetry>,
}

pub(super) struct PendingCapacityRetry {
    pub(super) deadline: tokio::time::Instant,
    thread_id: ThreadId,
    turn_id: String,
    model: String,
}

impl CapacityRetryState {
    pub(super) fn reset(&mut self) {
        self.attempts = 0;
        self.pending = None;
        // Keep the dedupe key so a repeated completion cannot undo cancellation.
    }
}

fn item_has_model_progress(item: &ThreadItem) -> bool {
    match item {
        ThreadItem::AgentMessage { text, delivery, .. } => {
            delivery.is_none() && !text.trim().is_empty()
        }
        ThreadItem::Plan { text, .. } => !text.trim().is_empty(),
        ThreadItem::Reasoning {
            summary, content, ..
        } => summary
            .iter()
            .chain(content)
            .any(|text| !text.trim().is_empty()),
        ThreadItem::CommandExecution { source, .. } => *source != ExecCommandSource::UserShell,
        ThreadItem::FileChange { .. }
        | ThreadItem::McpToolCall { .. }
        | ThreadItem::DynamicToolCall { .. }
        | ThreadItem::CollabAgentToolCall { .. }
        | ThreadItem::FunctionCallOutput { .. }
        | ThreadItem::WebSearch(_)
        | ThreadItem::ImageView { .. }
        | ThreadItem::Sleep(_)
        | ThreadItem::ImageGeneration(_) => true,
        // These can occur before inference or come from another agent while this model is blocked.
        ThreadItem::UserMessage { .. }
        | ThreadItem::HookPrompt { .. }
        | ThreadItem::SubAgentActivity { .. }
        | ThreadItem::EnteredReviewMode { .. }
        | ThreadItem::ExitedReviewMode { .. }
        | ThreadItem::ContextCompaction { .. } => false,
    }
}

impl ChatWidget {
    pub(super) fn reset_capacity_retry_on_progress(&mut self, notification: &ServerNotification) {
        if self.capacity_retry.attempts == 0 || !self.is_agent_turn_running() {
            return;
        }
        let (thread_id, turn_id, progressed) = match notification {
            ServerNotification::AgentMessageDelta(event) => (
                &event.thread_id,
                &event.turn_id,
                !event.delta.trim().is_empty(),
            ),
            ServerNotification::PlanDelta(event) => (
                &event.thread_id,
                &event.turn_id,
                !event.delta.trim().is_empty(),
            ),
            ServerNotification::ReasoningSummaryTextDelta(event) => (
                &event.thread_id,
                &event.turn_id,
                !event.delta.trim().is_empty(),
            ),
            ServerNotification::ReasoningTextDelta(event) => (
                &event.thread_id,
                &event.turn_id,
                !event.delta.trim().is_empty(),
            ),
            ServerNotification::ItemStarted(event) => (
                &event.thread_id,
                &event.turn_id,
                item_has_model_progress(&event.item),
            ),
            ServerNotification::ItemCompleted(event) => (
                &event.thread_id,
                &event.turn_id,
                item_has_model_progress(&event.item),
            ),
            _ => return,
        };
        if progressed
            && self.turn_lifecycle.last_turn_id.as_ref() == Some(turn_id)
            && self
                .thread_id
                .is_some_and(|id| id.to_string() == *thread_id)
        {
            // Count failures without model progress, even when a later inference in this turn fails.
            self.capacity_retry.reset();
        }
    }

    pub(crate) fn cancel_capacity_retry_on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release || self.capacity_retry.pending.is_none() {
            return false;
        }
        self.cancel_capacity_retry();
        self.chat_keymap.interrupt_turn.is_pressed(key)
            || key_hint::ctrl(KeyCode::Char('c')).is_press(key)
    }

    fn capacity_retry_input_is_idle(&self) -> bool {
        self.is_session_configured()
            && !self.blocks_direct_input
            && !self.is_user_turn_pending_or_running()
            && !self.has_misalignment_policy_violation()
            && !self.has_queued_follow_up_messages()
            && self.input_queue.pending_steers.is_empty()
            && !self.input_queue.suppress_queue_autosend
            && !self.input_queue.rate_limit_recovery_pending
            && !self.input_queue.recovered_queue
            && self.bottom_pane.composer_is_empty()
            && !self.bottom_pane.is_in_paste_burst()
            && self.bottom_pane.no_modal_or_popup_active()
            && self.external_editor_state == ExternalEditorState::Closed
    }

    pub(super) fn schedule_capacity_retry(&mut self, turn_id: String) {
        if self.capacity_retry.last_failed_turn.as_ref() == Some(&turn_id) {
            return;
        }
        self.capacity_retry.last_failed_turn = Some(turn_id.clone());
        if !self.capacity_retry_input_is_idle()
            || self.turn_lifecycle.last_turn_id.as_ref() != Some(&turn_id)
        {
            self.capacity_retry.reset();
            return;
        }
        if self.capacity_retry.attempts >= MAX_CAPACITY_RETRIES {
            self.add_info_message(
                "Automatic capacity retries stopped after 10 attempts.".to_string(),
                /*hint*/ None,
            );
            return;
        }
        let Some(thread_id) = self.thread_id else {
            return;
        };
        let delay_secs = rand::random_range(10..=60);
        let delay = Duration::from_secs(delay_secs);
        let attempt = self.capacity_retry.attempts + 1;
        self.capacity_retry.pending = Some(PendingCapacityRetry {
            deadline: tokio::time::Instant::now() + delay,
            thread_id,
            turn_id,
            model: self.current_model().to_string(),
        });
        self.add_info_message(
            format!(
                "Auto-retry {attempt}/{MAX_CAPACITY_RETRIES}: retrying in {delay_secs}s. Typing or Esc cancels."
            ),
            /*hint*/ None,
        );
        self.frame_requester.schedule_frame_in(delay);
    }

    pub(super) fn cancel_capacity_retry(&mut self) {
        if self.capacity_retry.pending.is_some() {
            self.capacity_retry.reset();
            self.add_info_message(
                "Automatic capacity retry canceled.".to_string(),
                /*hint*/ None,
            );
        }
    }

    pub(super) fn retry_capacity_if_due(&mut self) {
        let Some(pending) = self.capacity_retry.pending.as_ref() else {
            return;
        };
        if self.thread_id != Some(pending.thread_id)
            || self.turn_lifecycle.last_turn_id.as_ref() != Some(&pending.turn_id)
            || self.current_model() != pending.model
            || !self.capacity_retry_input_is_idle()
        {
            self.cancel_capacity_retry();
            return;
        }
        let remaining = pending
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if !remaining.is_zero() {
            self.frame_requester.schedule_frame_in(remaining);
            return;
        }

        // Empty turn input resumes inference from the existing conversation without
        // rendering a user cell or recording a synthetic prompt in message history.
        self.capacity_retry.pending = None;
        let op = self.user_turn_command(uuid::Uuid::new_v4().to_string(), Vec::new());
        if self.submit_op(op) {
            self.capacity_retry.attempts += 1;
            self.input_queue.user_turn_pending_start = true;
            self.dismiss_backend_banner_for_new_turn();
        } else {
            self.capacity_retry.reset();
        }
    }
}
