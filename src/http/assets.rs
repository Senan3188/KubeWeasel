//! Dashboard assets are embedded at build time, with no runtime file server.
use crate::state::AppState;
use axum::{
    Router,
    response::Html,
    routing::{MethodRouter, get},
};
fn javascript(contents: &'static str) -> MethodRouter<AppState> {
    get(move || async move { ([("content-type", "text/javascript")], contents) })
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../dashboard/index.html")) }),
        )
        .route(
            "/dashboard.css",
            get(|| async {
                (
                    [("content-type", "text/css")],
                    include_str!("../../dashboard/dashboard.css"),
                )
            }),
        )
        .route(
            "/dashboard.js",
            javascript(include_str!("../../dashboard/dashboard.js")),
        )
        .route(
            "/settings.js",
            javascript(include_str!("../../dashboard/settings.js")),
        )
        .route(
            "/history.js",
            javascript(include_str!("../../dashboard/history.js")),
        )
        .route(
            "/git.js",
            javascript(include_str!("../../dashboard/git.js")),
        )
        .route(
            "/storage.js",
            javascript(include_str!("../../dashboard/storage.js")),
        )
        .route(
            "/projects.js",
            javascript(include_str!("../../dashboard/projects.js")),
        )
        .route(
            "/https.js",
            javascript(include_str!("../../dashboard/https.js")),
        )
        .route(
            "/monitoring.js",
            javascript(include_str!("../../dashboard/monitoring.js")),
        )
        .route(
            "/auth.js",
            javascript(include_str!("../../dashboard/auth.js")),
        )
}
