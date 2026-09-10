use super::AccountRequestProcessor;
use super::ActiveLogin;
use super::LOGIN_CHATGPT_TIMEOUT;
use crate::error_code::invalid_params;
use codex_account_pools::AccountLogin;
use codex_account_pools::AccountStore;
use codex_account_pools::ManagedBackend;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::ManagedAccountAction;
use codex_app_server_protocol::ManagedAccountParams;
use codex_app_server_protocol::ManagedAccountResponse;
use codex_app_server_protocol::ManagedPoolAction;
use codex_app_server_protocol::ManagedPoolParams;
use codex_app_server_protocol::ManagedPoolResponse;
use codex_app_server_protocol::ServerNotification;
use codex_protocol::account_pool::AccountSelection;
use uuid::Uuid;

impl AccountRequestProcessor {
    pub(crate) async fn managed_account(
        &self,
        params: ManagedAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.managed_account_response(params)
            .await
            .map(|value| Some(value.into()))
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn managed_account_response(
        &self,
        params: ManagedAccountParams,
    ) -> anyhow::Result<ManagedAccountResponse> {
        validate_page_limit(params.limit)?;
        let mut config = self.load_latest_config().await;
        let store = AccountStore::from_config(&config);
        let alias = || {
            params
                .alias
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("An account alias is required"))
        };
        let mut login_response = None;
        let mut resolved = None;
        let mut selected_account = None;
        let mut models = None;
        let mut usage = Vec::new();
        let mut imported_alias = None;
        match params.action {
            ManagedAccountAction::Resolve | ManagedAccountAction::Models => {
                config.account_selection = params.account_selection.clone();
                if let Some(pool) = codex_core::initialize_account_pool(
                    &config,
                    &self.auth_manager,
                    params.thread_id.as_deref(),
                )
                .await?
                {
                    let provider = codex_model_provider::create_model_provider(
                        config.model_provider.clone(),
                        Some(pool.auth_manager()),
                    );
                    if params.action == ManagedAccountAction::Models {
                        models = Some(provider.models_manager_without_cache(config.model_catalog.clone())
                            .list_models(codex_models_manager::manager::RefreshStrategy::OnlineIfUncached, config.http_client_factory()).await
                            .into_iter().map(crate::models::model_from_preset).collect());
                    }
                    let state = provider.account_state()?;
                    selected_account = Some(pool.selected_account().await);
                    resolved = Some(codex_app_server_protocol::GetAccountResponse {
                        account: state.account.map(codex_app_server_protocol::Account::from),
                        requires_openai_auth: state.requires_openai_auth,
                    });
                }
            }
            ManagedAccountAction::Add => {
                let login = AccountLogin::new(store.clone(), alias()?.to_owned())?;
                let mut options = login.options();
                options.open_browser = false;
                let login_id = Uuid::new_v4();
                let cancel = tokio_util::sync::CancellationToken::new();
                let (response, active, completion): (
                    _,
                    _,
                    std::pin::Pin<
                        Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>,
                    >,
                ) = if params.device_auth == Some(true) {
                    let code = codex_login::request_device_code(&options).await?;
                    let response = LoginAccountResponse::ChatgptDeviceCode {
                        login_id: login_id.to_string(),
                        verification_url: code.verification_url.clone(),
                        user_code: code.user_code.clone(),
                    };
                    let active = ActiveLogin::Managed {
                        cancel: cancel.clone(),
                        shutdown_handle: None,
                        login_id,
                    };
                    let cancel = cancel.clone();
                    let completion = Box::pin(async move {
                        tokio::select! {
                            _ = cancel.cancelled() => Err(std::io::Error::other("Login cancelled")),
                            result = codex_login::complete_device_code_login(options, code) => result,
                        }
                    });
                    (response, active, completion)
                } else {
                    let server = codex_login::run_login_server(options)?;
                    let response = LoginAccountResponse::Chatgpt {
                        login_id: login_id.to_string(),
                        auth_url: server.auth_url.clone(),
                    };
                    let active = ActiveLogin::Managed {
                        shutdown_handle: Some(server.cancel_handle()),
                        cancel: cancel.clone(),
                        login_id,
                    };
                    (response, active, Box::pin(server.block_until_done()))
                };
                *self.active_login.lock().await = Some(active);
                login_response = Some(response);
                let outgoing = self.outgoing.clone();
                let active_login = self.active_login.clone();
                tokio::spawn(async move {
                    let result = tokio::time::timeout(LOGIN_CHATGPT_TIMEOUT, async {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => Err(anyhow::anyhow!("Login cancelled")),
                            result = async { completion.await?; login.finish().await } => result,
                        }
                    })
                    .await;
                    let success = matches!(result, Ok(Ok(_)));
                    outgoing
                        .send_server_notification(ServerNotification::AccountLoginCompleted(
                            AccountLoginCompletedNotification {
                                login_id: Some(login_id.to_string()),
                                success,
                                error: (!success).then(|| {
                                    "Named account login was cancelled or failed".to_owned()
                                }),
                                onboarding_entrypoint: None,
                            },
                        ))
                        .await;
                    let mut guard = active_login.lock().await;
                    if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
                        *guard = None;
                    }
                });
            }
            ManagedAccountAction::Import => {
                imported_alias = Some(store.import(alias()?, &config.codex_home).await?.alias);
            }
            ManagedAccountAction::Select => {
                store
                    .select_default(params.alias.clone().map(AccountSelection::Account))
                    .await?;
            }
            ManagedAccountAction::Remove => {
                store.remove_account(alias()?).await?;
            }
            ManagedAccountAction::Redeem => {
                usage.push(store.redeem(alias()?, params.model.as_deref()).await?);
            }
            ManagedAccountAction::List | ManagedAccountAction::Usage => {}
        }
        let mut state = store.read()?;
        state.accounts.sort_by(|a, b| a.alias.cmp(&b.alias));
        if let Some(alias) = imported_alias.as_ref().or(params.alias.as_ref()) {
            state.accounts.retain(|account| &account.alias == alias);
            if state.accounts.is_empty()
                && matches!(
                    params.action,
                    ManagedAccountAction::Usage | ManagedAccountAction::List
                )
            {
                anyhow::bail!("Unknown account: {alias}");
            }
        }
        let (data, next_cursor) = page(state.accounts, params.cursor, params.limit, |account| {
            &account.alias
        })?;
        if params.action == ManagedAccountAction::Usage {
            let backend = ManagedBackend::new(store);
            for account in &data {
                usage.push(backend.usage(account, params.model.as_deref()).await);
            }
        }
        Ok(ManagedAccountResponse {
            resolved,
            selected_account,
            models,
            data,
            next_cursor,
            usage,
            login: login_response,
            default_selection: state.default_selection,
        })
    }

    pub(crate) async fn managed_pool(
        &self,
        params: ManagedPoolParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.managed_pool_response(params)
            .await
            .map(|value| Some(value.into()))
            .map_err(|error| invalid_params(error.to_string()))
    }

    async fn managed_pool_response(
        &self,
        params: ManagedPoolParams,
    ) -> anyhow::Result<ManagedPoolResponse> {
        validate_page_limit(params.limit)?;
        let store = AccountStore::from_config(&self.load_latest_config().await);
        let name = || {
            params
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("A pool name is required"))
        };
        match params.action {
            ManagedPoolAction::Create | ManagedPoolAction::Update => {
                let pool = params
                    .pool
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("A pool definition is required"))?;
                store
                    .put_pool(pool, params.action == ManagedPoolAction::Create)
                    .await?;
            }
            ManagedPoolAction::Select => {
                store
                    .select_default(params.name.clone().map(AccountSelection::Pool))
                    .await?;
            }
            ManagedPoolAction::Remove => {
                store.remove_pool(name()?).await?;
            }
            ManagedPoolAction::List | ManagedPoolAction::Read => {}
        }
        let mut state = store.read()?;
        if params.action == ManagedPoolAction::Read {
            let name = name()?;
            state.pools.retain(|pool| pool.name == name);
            if state.pools.is_empty() {
                anyhow::bail!("Unknown pool: {name}");
            }
        }
        state.pools.sort_by(|a, b| a.name.cmp(&b.name));
        let (data, next_cursor) =
            page(state.pools, params.cursor, params.limit, |pool| &pool.name)?;
        Ok(ManagedPoolResponse {
            data,
            next_cursor,
            default_selection: state.default_selection,
        })
    }
}

fn page<T>(
    mut data: Vec<T>,
    cursor: Option<String>,
    limit: Option<u32>,
    name: impl Fn(&T) -> &str,
) -> anyhow::Result<(Vec<T>, Option<String>)> {
    let limit = limit.unwrap_or(25);
    validate_page_limit(Some(limit))?;
    data.retain(|item| {
        cursor
            .as_ref()
            .is_none_or(|cursor| name(item) > cursor.as_str())
    });
    let next = (data.len() > limit as usize).then(|| name(&data[limit as usize - 1]).to_owned());
    data.truncate(limit as usize);
    Ok((data, next))
}

fn validate_page_limit(limit: Option<u32>) -> anyhow::Result<()> {
    if limit.is_some_and(|limit| !(1..=100).contains(&limit)) {
        anyhow::bail!("limit must be between 1 and 100");
    }
    Ok(())
}
