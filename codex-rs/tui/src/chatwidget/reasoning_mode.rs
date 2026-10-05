//! `/pro` toggles the thread's `reasoning.mode` between Standard and Pro.

use super::ChatWidget;
use crate::app_event::AppEvent;
use codex_protocol::config_types::ReasoningMode;

impl ChatWidget {
    /// Shows a mode applied by the app, for example after `/pro` updated the thread.
    pub(crate) fn set_reasoning_mode(&mut self, mode: ReasoningMode) {
        self.config.model_reasoning_mode = Some(mode);
        self.refresh_status_line();
    }

    pub(super) fn toggle_reasoning_mode(&self) {
        let next = match self.config.model_reasoning_mode.unwrap_or_default() {
            ReasoningMode::Standard => ReasoningMode::Pro,
            ReasoningMode::Pro => ReasoningMode::Standard,
        };
        self.app_event_tx.send(AppEvent::UpdateReasoningMode(next));
    }
}
