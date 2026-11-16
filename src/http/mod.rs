//! HTTP composition. The authentication layer covers every merged feature API.
mod assets;
pub(crate) mod request;
use crate::{
    applications, auth, diagnostics, git, history, https, monitoring, nodes, projects, registry,
    settings, state::AppState, storage,
};
use axum::{Router, routing::get};
pub(crate) fn features() -> Router<AppState> {
    Router::new()
        .merge(applications::routes())
        .merge(settings::routes())
        .merge(storage::routes())
        .merge(projects::routes())
        .merge(diagnostics::routes())
        .merge(https::routes())
        .merge(monitoring::routes())
        .merge(nodes::routes())
        .merge(registry::routes())
        .merge(git::routes())
        .merge(history::routes())
}
pub(crate) fn router_with_clusters(
    state: AppState,
    auth: auth::AuthState,
    manager: crate::clusters::Manager,
) -> Router {
    assets::routes()
        .merge(features())
        .merge(crate::clusters::routes(manager))
        .route("/healthz", get(|| async { "ok" }))
        .merge(auth::routes(auth.clone()))
        .layer(axum::middleware::from_fn_with_state(auth, auth::guard))
        .with_state(state)
}
#[cfg(test)]
pub(crate) fn router(state: AppState, auth: auth::AuthState) -> Router {
    let manager = crate::clusters::Manager::open(state.clone(), ":memory:", None).unwrap();
    router_with_clusters(state, auth, manager)
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
            private_origin: None,
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
            "/api/nodes",
            "/api/clusters/local/nodes",
            "/api/clusters/local/nodes/worker/drain-preview",
            "/api/overview",
            "/api/clusters",
            "/api/clusters/local/overview",
            "/api/clusters/local/apps/web/config",
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
        assert_eq!(
            request(
                &app,
                "/api/clusters/local/nodes/worker",
                Some(json!({"uid":"u","resource_version":"1","action":"cordon"})),
                Some((&cookie, ""))
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "/api/clusters/local/nodes/worker",
                Some(json!({"uid":"u","resource_version":"1","action":"cordon"})),
                session
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let clusters = json_body(request(&app, "/api/clusters", None, session).await).await;
        assert_eq!(clusters["clusters"][0]["id"], "local");
        assert_eq!(
            request(
                &app,
                "/api/clusters/local",
                Some(json!({"name":"Renamed"})),
                Some((&cookie, ""))
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "/api/clusters/local",
                Some(json!({"name":"Renamed"})),
                session
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, "/api/clusters/unknown/overview", None, session)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
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
                "/api/clusters/local/apps/smoke-app/scale",
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
        let config = json_body(
            request(
                &app,
                "/api/clusters/local/apps/smoke-app/config",
                None,
                session,
            )
            .await,
        )
        .await;
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
        // Preserve the old domain endpoint and add independent scoped routes.
        let legacy = json!({"host":"public.example.test","path":"/","ingress_class":"traefik","tls_mode":"existing","tls_secret":"public-tls"});
        assert_eq!(
            request(&app, "/api/apps/smoke-app/domain", Some(legacy), session)
                .await
                .status(),
            StatusCode::OK
        );
        let internal = json!({"host":"app.saturn.internal","path":"/","ingress_class":"traefik","tls_mode":"http","tls_secret":""});
        let path = "/api/clusters/local/apps/smoke-app/domains";
        assert_eq!(
            request(&app, path, Some(internal.clone()), Some((&cookie, "")))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = request(&app, path, Some(internal.clone()), session).await;
        assert_eq!(response.status(), StatusCode::OK);
        let id = json_body(response).await["id"].as_str().unwrap().to_owned();
        assert_eq!(
            request(&app, path, Some(internal), session).await.status(),
            StatusCode::BAD_REQUEST
        );
        let cfg = json_body(request(&app, "/api/apps/smoke-app/config", None, session).await).await;
        assert_eq!(cfg["config"]["domains"].as_array().unwrap().len(), 2);
        assert_eq!(cfg["config"]["domain"]["tls_secret"], "public-tls");
        assert_eq!(cfg["config"]["port"], 8080);
        let status = json_body(
            request(
                &app,
                &format!("/api/apps/smoke-app/https?domain={id}"),
                None,
                session,
            )
            .await,
        )
        .await;
        assert_eq!(status["host"], "app.saturn.internal");
        let endpoint = format!("/api/clusters/local/apps/smoke-app/domains/{id}");
        let response = request(&app, &endpoint, Some(json!({"host":"renamed.saturn.internal","path":"/","ingress_class":"traefik","tls_mode":"existing","tls_secret":"internal-tls"})), session).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            request(
                &app,
                "/api/apps/smoke-app/domains/unknown",
                Some(
                    json!({"host":"bad.test","path":"/","ingress_class":"traefik","tls_secret":""})
                ),
                session
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let delete_request = Request::builder()
            .uri(&endpoint)
            .method("DELETE")
            .header("host", "127.0.0.1:7878")
            .header("cookie", &cookie)
            .header("x-kubeweasel-request", "dashboard")
            .header("x-kubeweasel-csrf", &csrf)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(delete_request).await.unwrap().status(),
            StatusCode::OK
        );
        let cfg = json_body(request(&app, "/api/apps/smoke-app/config", None, session).await).await;
        assert_eq!(cfg["config"]["domains"].as_array().unwrap().len(), 1);
        assert_eq!(cfg["config"]["domain"]["host"], "public.example.test");
        assert_eq!(cfg["config"]["domain"]["tls_secret"], "public-tls");
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
