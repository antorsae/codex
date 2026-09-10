use std::sync::Arc;

use super::trim_function_call_history_to_fit_context_window;
use crate::Prompt;
use crate::client::CompactConversationRequestSettings;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_protocol::auth::AuthMode;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_rollout_trace::CompactionTraceContext;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(super) struct RemoteCompactAttempt {
    pub(super) new_history: Vec<ResponseItem>,
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
}

pub(super) async fn run_remote_compact_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: &mut ModelClientSession,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    cancellation: &CancellationToken,
) -> CodexResult<RemoteCompactAttempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let base_instructions = sess.get_prompt_base_instructions().await;
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut history,
            turn_context.as_ref(),
            &base_instructions,
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction"
        );
    }
    if estimated_deleted_tokens > 0 {
        let max_local_deleted_tokens = sess
            .estimated_tokens_after_last_model_generated_item()
            .await;
        analytics_details.active_context_tokens_before = analytics_details
            .active_context_tokens_before
            .map(|active_context_tokens_before| {
                active_context_tokens_before
                    .saturating_sub(estimated_deleted_tokens.min(max_local_deleted_tokens))
            });
    }
    let trace_input_history = compaction_trace
        .is_enabled()
        .then(|| history.raw_items().cloned().collect());
    let prompt_input = history.for_prompt(&turn_context.model_info().input_modalities);
    let tool_router = &step_context.tool_router;
    let prompt = Prompt {
        input: prompt_input,
        tools: tool_router.model_visible_specs(),
        parallel_tool_calls: true,
        base_instructions,
        output_schema: None,
        output_schema_strict: true,
        cyber_access_program: turn_context.cyber_access_program,
    };
    let responses_metadata = sess
        .responses_metadata(
            turn_context.as_ref(),
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        )
        .await;
    crate::account_pools::before_request(
        sess,
        turn_context,
        &turn_context.model_info().slug,
        client_session,
        cancellation,
    )
    .await?;
    let new_history = loop {
        let result = sess
            .services
            .model_client
            .compact_conversation_history(
                &prompt,
                turn_context.model_info(),
                Some(client_session.turn_state()),
                CompactConversationRequestSettings {
                    effort: turn_context.reasoning_effort().cloned(),
                    summary: turn_context.reasoning_summary(),
                    service_tier: if sess.services.auth_manager.auth_mode()
                        == Some(AuthMode::ApiKey)
                    {
                        None
                    } else {
                        step_context.settings.service_tier.clone()
                    },
                },
                &turn_context.session_telemetry,
                compaction_trace,
                &responses_metadata,
            )
            .await;
        match result {
            Ok(history) => break history,
            Err(error) if matches!(error.details(), CodexErrorDetails::UsageLimitReached(_)) => {
                if !crate::account_pools::recover(
                    sess,
                    turn_context,
                    &turn_context.model_info().slug,
                    client_session,
                    cancellation,
                )
                .await?
                {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    };
    Ok(RemoteCompactAttempt {
        new_history,
        trace_input_history,
    })
}
