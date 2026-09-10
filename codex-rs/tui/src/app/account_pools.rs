//! Native account/pool controls use server-side storage, including for remote TUI connections.

use super::App;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use crate::history_cell::AccountPoolReport;
use crate::history_cell::AccountReportView;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ManagedAccountAction;
use codex_app_server_protocol::ManagedAccountParams;
use codex_app_server_protocol::ManagedAccountResponse;
use codex_app_server_protocol::ManagedPoolAction;
use codex_app_server_protocol::ManagedPoolParams;
use codex_app_server_protocol::ManagedPoolResponse;
use codex_app_server_protocol::RequestId;
use codex_protocol::account_pool::AccountPool;

impl App {
    pub(super) fn manage_accounts(&mut self, server: &AppServerSession, args: &str) {
        let mut words: Vec<_> = args.split_whitespace().collect();
        let device_auth = words.last() == Some(&"--device-auth");
        if device_auth {
            words.pop();
        }
        let (mut action, mut alias) = match words.as_slice() {
            [] | ["list"] => (ManagedAccountAction::Usage, None),
            ["usage"] => (ManagedAccountAction::Usage, None),
            ["usage", alias] => (ManagedAccountAction::Usage, Some((*alias).to_owned())),
            ["add", alias] => (ManagedAccountAction::Add, Some((*alias).to_owned())),
            ["import", alias] => (ManagedAccountAction::Import, Some((*alias).to_owned())),
            ["select", alias] => (ManagedAccountAction::Select, Some((*alias).to_owned())),
            ["remove", alias] => (ManagedAccountAction::Remove, Some((*alias).to_owned())),
            ["redeem", alias] => (ManagedAccountAction::Redeem, Some((*alias).to_owned())),
            ["clear"] => (ManagedAccountAction::Select, None),
            _ => {
                self.chat_widget.add_error_message("Usage: /accounts [list|usage [alias]|add alias|import alias|select alias|remove alias|redeem alias|clear]".to_owned());
                return;
            }
        };
        if device_auth && action != ManagedAccountAction::Add {
            self.chat_widget.add_error_message(
                "--device-auth is only available with /accounts add ALIAS".to_owned(),
            );
            return;
        }
        let handle = server.request_handle();
        let tx = self.app_event_tx.clone();
        let view = if words.first() == Some(&"usage") {
            AccountReportView::Usage
        } else {
            AccountReportView::Summary
        };
        tokio::spawn(async move {
            let result = async {
                let mut data = Vec::new();
                let mut usage = Vec::new();
                let mut cursor = None;
                let mut selection_updated = false;
                loop {
                    let mut response: ManagedAccountResponse = handle
                        .request_typed(ClientRequest::ManagedAccount {
                            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                            params: ManagedAccountParams {
                                action: action.clone(),
                                alias: alias.clone(),
                                account_selection: None,
                                thread_id: None,
                                device_auth: Some(device_auth),
                                model: None,
                                cursor,
                                limit: Some(100),
                            },
                        })
                        .await
                        .map_err(|error| {
                            if selection_updated {
                                format!("Default account updated, but usage could not be refreshed: {error}. Run /accounts to retry.")
                            } else {
                                error.to_string()
                            }
                        })?;
                    if action == ManagedAccountAction::Select {
                        // Selection returns metadata for the selected alias. Read all accounts'
                        // usage next; pagination must never repeat the selection mutation.
                        action = ManagedAccountAction::Usage;
                        alias = None;
                        cursor = None;
                        selection_updated = true;
                        continue;
                    }
                    data.append(&mut response.data);
                    usage.append(&mut response.usage);
                    if action != ManagedAccountAction::Usage || response.next_cursor.is_none() {
                        response.data = data;
                        response.usage = usage;
                        return Ok(AccountPoolReport::accounts(
                            &response,
                            view,
                            chrono::Utc::now().timestamp(),
                        ));
                    }
                    cursor = response.next_cursor;
                }
            }
            .await;
            tx.send(AppEvent::ManagedAccountOutput { result });
        });
    }

    pub(super) fn manage_pools(&mut self, server: &AppServerSession, args: &str) {
        let words: Vec<_> = args.split_whitespace().collect();
        let (action, name, pool) = match words.as_slice() {
            [] | ["list"] => (ManagedPoolAction::List, None, None),
            ["inspect", name] => (ManagedPoolAction::Read, Some((*name).to_owned()), None),
            ["select", name] => (ManagedPoolAction::Select, Some((*name).to_owned()), None),
            ["clear"] => (ManagedPoolAction::Select, None, None),
            ["remove", name] => (ManagedPoolAction::Remove, Some((*name).to_owned()), None),
            [command @ ("create" | "update"), name, accounts @ ..] if !accounts.is_empty() => {
                let pool = AccountPool {
                    name: (*name).to_owned(),
                    accounts: accounts
                        .iter()
                        .filter(|alias| **alias != "--no-auto-reset")
                        .map(|alias| (*alias).to_owned())
                        .collect(),
                    redeem_weekly_resets: !accounts.contains(&"--no-auto-reset"),
                };
                (
                    if *command == "create" {
                        ManagedPoolAction::Create
                    } else {
                        ManagedPoolAction::Update
                    },
                    None,
                    Some(pool),
                )
            }
            _ => {
                self.chat_widget.add_error_message("Usage: /pools [list|inspect NAME|create NAME ALIAS...|update NAME ALIAS...|select NAME|remove NAME|clear]. Add --no-auto-reset to wait for weekly recovery.".to_owned());
                return;
            }
        };
        let handle = server.request_handle();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut cursor = None;
                let mut data = Vec::new();
                loop {
                    let response: ManagedPoolResponse = handle
                        .request_typed(ClientRequest::ManagedPool {
                            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                            params: ManagedPoolParams {
                                action: action.clone(),
                                name: name.clone(),
                                pool: pool.clone(),
                                cursor,
                                limit: Some(100),
                            },
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    data.extend(response.data);
                    if action != ManagedPoolAction::List || response.next_cursor.is_none() {
                        return Ok(AccountPoolReport::pools(&ManagedPoolResponse {
                            data,
                            next_cursor: None,
                            default_selection: response.default_selection,
                        }));
                    }
                    cursor = response.next_cursor;
                }
            }
            .await;
            tx.send(AppEvent::ManagedAccountOutput { result });
        });
    }
}
