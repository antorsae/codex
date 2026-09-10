use crate::JsonSchema;
use crate::TS;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ManagedAccountAction {
    Add,
    Import,
    List,
    Select,
    Remove,
    Usage,
    Redeem,
    Resolve,
    Models,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedAccountParams {
    pub action: ManagedAccountAction,
    /// Session selection for `resolve`; omitted selection uses saved state or the user default.
    #[ts(optional = nullable)]
    pub account_selection: Option<AccountSelection>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub alias: Option<String>,
    /// Use device-code login for `add`; defaults to browser OAuth.
    #[ts(optional = nullable)]
    pub device_auth: Option<bool>,
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedAccountResponse {
    /// Null means this selection uses legacy authentication.
    #[schemars(
        schema_with = "crate::protocol::serde_helpers::nullable_embedded_response_schema::<super::GetAccountResponse>"
    )]
    pub resolved: Option<super::GetAccountResponse>,
    pub models: Option<Vec<super::Model>>,
    pub data: Vec<ManagedAccount>,
    pub next_cursor: Option<String>,
    pub usage: Vec<ManagedAccountUsage>,
    #[schemars(
        schema_with = "crate::protocol::serde_helpers::nullable_embedded_response_schema::<super::LoginAccountResponse>"
    )]
    pub login: Option<super::LoginAccountResponse>,
    pub default_selection: Option<AccountSelection>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ManagedPoolAction {
    Create,
    List,
    Read,
    Update,
    Select,
    Remove,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedPoolParams {
    pub action: ManagedPoolAction,
    #[ts(optional = nullable)]
    pub name: Option<String>,
    #[ts(optional = nullable)]
    pub pool: Option<AccountPool>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedPoolResponse {
    pub data: Vec<AccountPool>,
    pub next_cursor: Option<String>,
    pub default_selection: Option<AccountSelection>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountPoolNotification {
    pub thread_id: String,
    pub event: AccountPoolEvent,
}
