//! Project-scoped named connections backed by retained Kubernetes Secrets.
mod models;
mod routes;
mod service;
mod usage;
pub(crate) use routes::routes;
pub(crate) use service::{
    State, app_project, has_bindings, has_connections, restart_application, validate_literals,
};
