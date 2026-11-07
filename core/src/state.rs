use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;
use crate::clock::Clock;

#[derive(Clone)]
pub struct AppState {
    pub owner_pool: sqlx::PgPool,   // control-plane writes + worker (system), bypasses per-request tenant ctx
    pub rt_pool: sqlx::PgPool,      // app_rt role: RLS enforced, used for tenant-scoped requests
    pub clock: Arc<Clock>,
    pub sessions: Arc<Mutex<HashMap<String, Session>>>,
    pub bus: broadcast::Sender<TenantEvent>,
    pub http: reqwest::Client,
    pub goalert_base: Option<String>,
    pub control_host: String,
    pub base_domain: String,
}

#[derive(Clone)]
pub struct Session {
    pub user_id: Uuid,
    pub tenant_id: Option<Uuid>,  // None = superadmin / control plane
    pub email: String,
    pub role: String,             // "superadmin" for control plane
    pub is_super: bool,
}

#[derive(Clone, Debug)]
pub struct TenantEvent {
    pub tenant_id: Uuid,
    pub payload: serde_json::Value,
}
