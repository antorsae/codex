//! Fetch picker models without blocking the event loop and keep new-thread defaults in sync.

use super::AppServerSession;
use super::model_preset_from_api_model;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_protocol::openai_models::ModelPreset;
use uuid::Uuid;

impl AppServerSession {
    pub(crate) fn set_available_models(&mut self, models: Vec<ModelPreset>) {
        if let Some(default) = models
            .iter()
            .find(|model| model.is_default)
            .or(models.first())
        {
            self.default_model = Some(default.model.clone());
        }
        self.available_models = models;
    }

    pub(crate) async fn selected_models(&self) -> color_eyre::Result<ModelListResponse> {
        request_models(
            self.request_handle(),
            self.managed_accounts_active,
            self.account_selection.clone(),
            self.account_thread_id.clone(),
        )
        .await
        .map_err(color_eyre::eyre::Report::msg)
    }

    pub(crate) fn fetch_models(&self, request_id: Uuid, app_event_tx: AppEventSender) {
        let request_handle = self.request_handle();
        let managed = self.managed_accounts_active;
        let selection = self.account_selection.clone();
        let thread_id = self.account_thread_id.clone();
        tokio::spawn(async move {
            let result = request_models(request_handle, managed, selection, thread_id)
                .await
                .map(|response| {
                    response
                        .data
                        .into_iter()
                        .map(model_preset_from_api_model)
                        .collect()
                });
            app_event_tx.send(AppEvent::ModelsLoaded { request_id, result });
        });
    }
}

async fn request_models(
    request_handle: codex_app_server_client::AppServerRequestHandle,
    managed: bool,
    selection: Option<codex_protocol::account_pool::AccountSelection>,
    thread_id: Option<String>,
) -> Result<ModelListResponse, String> {
    if managed {
        let response: codex_app_server_protocol::ManagedAccountResponse = request_handle
            .request_typed(ClientRequest::ManagedAccount {
                request_id: RequestId::String(Uuid::new_v4().to_string()),
                params: codex_app_server_protocol::ManagedAccountParams {
                    action: codex_app_server_protocol::ManagedAccountAction::Models,
                    account_selection: selection,
                    thread_id,
                    alias: None,
                    device_auth: None,
                    model: None,
                    cursor: None,
                    limit: None,
                },
            })
            .await
            .map_err(|error| error.to_string())?;
        return Ok(ModelListResponse {
            data: response
                .models
                .ok_or_else(|| "Named account model catalog is unavailable".to_owned())?,
            next_cursor: None,
        });
    }
    request_handle
        .request_typed::<ModelListResponse>(ClientRequest::ModelList {
            request_id: RequestId::String(Uuid::new_v4().to_string()),
            params: ModelListParams {
                cursor: None,
                limit: None,
                include_hidden: Some(true),
            },
        })
        .await
        .map_err(|error| error.to_string())
}
