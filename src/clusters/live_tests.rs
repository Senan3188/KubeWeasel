//! Opt-in connection lifecycle check. Only temporary namespaces are modified.
use super::*;
use crate::{config::Config, http::features};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Namespace, PersistentVolumeClaim},
};
use kube::{
    Api,
    api::{DeleteParams, PostParams},
};
use serde_json::{Value, json};
use tower::ServiceExt;
async fn call(router: &Router, path: &str, body: Option<Value>, method: &str) -> Value {
    let request = Request::builder()
        .uri(path)
        .method(method)
        .header("content-type", "application/json")
        .header("x-kubeweasel-request", "dashboard")
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let result: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert!(status.is_success(), "{path}: {status}: {result}");
    result
}
#[tokio::test]
#[ignore = "connects to Docker Desktop using embedded credentials; creates two temporary namespaces, paused workloads and an unbound PVC"]
async fn connections_persist_and_disconnect_preserves_workloads_and_volumes() {
    assert_eq!(
        std::env::var("KUBEWEASEL_RUN_CLUSTERS_TEST").as_deref(),
        Ok("1")
    );
    let context = std::process::Command::new("kubectl")
        .args(["config", "current-context"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(context.stdout).unwrap().trim(),
        "docker-desktop"
    );
    let output = std::process::Command::new("kubectl")
        .args([
            "--context",
            "docker-desktop",
            "config",
            "view",
            "--raw",
            "--flatten",
            "--minify",
            "-o",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let raw = String::from_utf8(output.stdout).unwrap();
    let (client, _) = connection::client(&raw, "", "default").await.unwrap();
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let prefix = format!("kw-clusters-{}", std::process::id());
    let first = format!("{prefix}-one");
    let second = format!("{prefix}-two");
    for ns in [&first, &second] {
        namespaces
            .create(
                &PostParams::default(),
                &serde_json::from_value(json!({"metadata":{"name":ns}})).unwrap(),
            )
            .await
            .unwrap();
    }
    let dir = std::env::temp_dir().join(&prefix);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("test.db");
    let key = dir.join("test.cluster-key");
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: db.to_str().unwrap().into(),
        public_origin: None,
        private_origin: None,
        namespace: "kubeweasel-test".into(),
        demo: false,
        healthcheck: false,
    };
    let local = AppState::initialize(&config).await.unwrap();
    let manager = Manager::open(local.clone(), db.to_str().unwrap(), Some(&key)).unwrap();
    let router = features()
        .merge(routes(manager.clone()))
        .with_state(local.clone());
    let mut ids = Vec::new();
    for ns in [&first, &second] {
        let created = call(
            &router,
            "/api/clusters",
            Some(json!({"name":ns,"namespace":ns,"kubeconfig":raw})),
            "POST",
        )
        .await;
        assert_eq!(created["connection"]["connected"], true);
        let id = created["cluster"]["id"].as_str().unwrap().to_string();
        let project = call(
            &router,
            &format!("/api/clusters/{id}/projects"),
            Some(json!({"name":"Website","description":"test"})),
            "POST",
        )
        .await;
        call(&router,&format!("/api/clusters/{id}/apps"),Some(json!({"name":"same-name","image":"nginx:stable","port":80,"replicas":0,"project_id":project["project"]["id"]})),"POST").await;
        let config = call(
            &router,
            &format!("/api/clusters/{id}/apps/same-name/config"),
            None,
            "GET",
        )
        .await;
        assert_eq!(config["config"]["replicas"], 0);
        ids.push(id);
    }
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &first);
    claims.create(&PostParams::default(),&serde_json::from_value(json!({"metadata":{"name":"retained-data"},"spec":{"accessModes":["ReadWriteOnce"],"storageClassName":"","resources":{"requests":{"storage":"1Mi"}}}})).unwrap()).await.unwrap();
    drop(router);
    drop(manager);
    let reopened = Manager::open(local.clone(), db.to_str().unwrap(), Some(&key)).unwrap();
    reopened.resume().await;
    let router = features().merge(routes(reopened.clone())).with_state(local);
    for id in &ids {
        let projects = call(
            &router,
            &format!("/api/clusters/{id}/projects"),
            None,
            "GET",
        )
        .await;
        assert_eq!(projects["projects"].as_array().unwrap().len(), 1);
    }
    call(
        &router,
        &format!("/api/clusters/{}", ids[0]),
        Some(json!({})),
        "DELETE",
    )
    .await;
    assert!(reopened.resolve(&ids[0]).await.is_err());
    assert!(claims.get("retained-data").await.is_ok());
    assert!(
        Api::<Deployment>::namespaced(client.clone(), &first)
            .get("same-name")
            .await
            .is_ok()
    );
    let overview = call(
        &router,
        &format!("/api/clusters/{}/overview", ids[1]),
        None,
        "GET",
    )
    .await;
    assert_eq!(overview["apps"].as_array().unwrap().len(), 1);
    drop(router);
    drop(reopened);
    for ns in [&first, &second] {
        namespaces
            .delete(ns, &DeleteParams::default())
            .await
            .unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}
