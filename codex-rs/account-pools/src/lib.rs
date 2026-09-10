//! Local account storage and quota recovery shared by every Codex execution surface.

mod backend;
mod coordinator;
mod login;
mod policy;
mod redemption;
mod storage;

pub(crate) use backend::AccountBackend;
pub use backend::ManagedBackend;
pub use coordinator::PoolSession;
pub use login::AccountLogin;
pub use storage::AccountStore;

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "process_tests.rs"]
mod process_tests;
