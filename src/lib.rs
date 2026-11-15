//! KubeWeasel server and feature modules.
mod applications;
mod auth;
mod clusters;
mod config;
mod diagnostics;
mod error;
mod git;
mod health;
mod history;
mod http;
mod https;
mod monitoring;
mod nodes;
mod projects;
mod registry;
mod server;
mod settings;
mod state;
mod storage;
mod validation;
mod workloads;

/// Start the dashboard, or run the side-effect-free health-check command.
pub async fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    server::run().await
}
