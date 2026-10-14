use std::sync::Arc;

use crate::auth::AuthState;
use crate::config::Config;
use crate::db::Db;
use crate::grpc::registry::Registry;
use crate::pki::Pki;
use crate::repos::ReposState;
use crate::secrets::Sealer;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub sealer: Sealer,
    pub pki: Arc<Pki>,
    pub registry: Registry,
    pub auth: AuthState,
    pub repos: ReposState,
}
