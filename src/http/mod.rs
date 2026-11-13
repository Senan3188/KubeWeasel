//! HTTP composition. The authentication layer covers every merged feature API.
mod assets;
pub(crate) mod request;
use crate::{
    applications, auth, diagnostics, git, history, https, monitoring, projects, registry, settings,
    state::AppState, storage,
};
use axum::{Router, routing::get};
pub(crate) fn router(state: AppState, auth: auth::AuthState) -> Router {
    assets::routes()
        .merge(applications::routes())
        .merge(settings::routes())
        .merge(storage::routes())
        .merge(projects::routes())
        .merge(diagnostics::routes())
        .merge(https::routes())
        .merge(monitoring::routes())
        .merge(registry::routes())
        .merge(git::routes())
        .merge(history::routes())
        .route("/healthz", get(|| async { "ok" }))
        .merge(auth::routes(auth.clone()))
        .layer(axum::middleware::from_fn_with_state(auth, auth::guard))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{Request, StatusCode},
        response::Response,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    async fn application() -> Router {
        let config = Config {
            address: "127.0.0.1:7878".into(),
            auth_db: ":memory:".into(),
            public_origin: None,
            namespace: "test".into(),
            demo: true,
            healthcheck: false,
        };
        router(
            AppState::initialize(&config).await.unwrap(),
            auth::test_state(),
        )
        .layer(axum::Extension(ConnectInfo(
            "127.0.0.1:10000".parse::<std::net::SocketAddr>().unwrap(),
        )))
    }
    async fn request(
        router: &Router,
        path: &str,
        body: Option<Value>,
        session: Option<(&str, &str)>,
    ) -> Response {
        let mut request = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7878");
        if let Some((cookie, csrf)) = session {
            request = request.header("cookie", cookie);
            if !csrf.is_empty() {
                request = request.header("x-kubeweasel-csrf", csrf);
            }
        }
        let request = if let Some(body) = body {
            request
                .method("POST")
                .header("content-type", "application/json")
                .header("x-kubeweasel-request", "dashboard")
                .body(Body::from(body.to_string()))
                .unwrap()
        } else {
            request.body(Body::empty()).unwrap()
        };
        router.clone().oneshot(request).await.unwrap()
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
    }
    #[tokio::test]
    async fn composed_router_preserves_asset_access_and_protects_every_api() {
        let app = application().await;
        let response = request(&app, "/", None, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        for script in html
            .split("src=\"/")
            .skip(1)
            .map(|s| s.split('"').next().unwrap())
        {
            let response = request(&app, &format!("/{script}"), None, None).await;
            assert_eq!(response.status(), StatusCode::OK, "{script}");
            assert_eq!(response.headers()["content-type"], "text/javascript");
        }
        assert_eq!(
            request(&app, "/dashboard.css", None, None).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, "/healthz", None, None).await.status(),
            StatusCode::OK
        );
        for path in [
            "/api/overview",
            "/api/projects",
            "/api/monitoring",
            "/api/monitoring/nodes",
            "/api/apps/web/config",
            "/api/apps/web/source",
            "/api/apps/web/storage",
            "/api/apps/web/history",
        ] {
            assert_eq!(
                request(&app, path, None, None).await.status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
        let wrong_host = Request::builder()
            .uri("/healthz")
            .header("host", "attacker.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(wrong_host).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    #[tokio::test]
    async fn owner_can_deploy_scale_and_change_source_through_the_composed_router() {
        let app = application().await;
        let response=request(&app,"/api/auth/setup",Some(json!({"setup_token":"test-setup-token","email":"owner@example.test","password":"a-test-password-long-enough"})),None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let csrf = json_body(response).await["csrf"]
            .as_str()
            .unwrap()
            .to_string();
        let session = Some((cookie.as_str(), csrf.as_str()));
        let deployment =
            json!({"name":"smoke-app","image":"nginx:stable","port":8080,"replicas":1});
        assert_eq!(
            request(
                &app,
                "/api/apps",
                Some(deployment.clone()),
                Some((&cookie, ""))
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, "/api/apps", Some(deployment), session)
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "/api/apps/smoke-app/scale",
                Some(json!({"replicas":0})),
                session
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "/api/apps/smoke-app/image",
                Some(json!({"registry":"github","image":"owner/web:v1"})),
                session
            )
            .await
            .status(),
            StatusCode::OK
        );
        let config =
            json_body(request(&app, "/api/apps/smoke-app/config", None, session).await).await;
        assert_eq!(config["config"]["image"], "ghcr.io/owner/web:v1");
        assert_eq!(config["config"]["replicas"], 0);
        assert_eq!(config["config"]["port"], 8080);
        assert_eq!(
            request(
                &app,
                "/api/apps/smoke-app/restart",
                Some(json!({})),
                session
            )
            .await
            .status(),
            StatusCode::OK
        );
        let logs = json_body(request(&app, "/api/apps/smoke-app/logs", None, session).await).await;
        assert!(logs["logs"].as_str().unwrap().contains("[demo] smoke-app"));
        let overview = json_body(request(&app, "/api/overview", None, session).await).await;
        let created = overview["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "smoke-app")
            .unwrap();
        assert_eq!(created["status"], "Paused");
    }
}
