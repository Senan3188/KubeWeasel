use super::{service, validation::Change};
use crate::{config::Config, state::AppState};
use axum::{
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode},
};
use kube::{Client, client::Body as KubeBody};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::service_fn;
fn input() -> Change {
    Change {
        uid: "node-uid".into(),
        resource_version: "1".into(),
        action: "drain".into(),
        key: String::new(),
        value: String::new(),
        effect: String::new(),
    }
}
async fn state(
    local_data: bool,
    controller_exists: bool,
    allow_eviction: bool,
) -> (AppState, Arc<Mutex<Vec<(String, String, Value)>>>) {
    let seen = Arc::new(Mutex::new(vec![]));
    let evicted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let records = seen.clone();
    let mock = service_fn(move |req: Request<KubeBody>| {
        let records = records.clone();
        let evicted = evicted.clone();
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            let body = to_bytes(Body::new(req.into_body()), 1024 * 1024)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            records
                .lock()
                .unwrap()
                .push((method.clone(), path.clone(), body));
            let (status, data) = match (method.as_str(), path.as_str()) {
                ("GET", "/api/v1/nodes/worker") => (
                    200,
                    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker","uid":"node-uid","resourceVersion":"1"}}),
                ),
                ("PATCH", "/api/v1/nodes/worker") => (
                    200,
                    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker","uid":"node-uid","resourceVersion":"2"},"spec":{"unschedulable":true}}),
                ),
                ("GET", "/api/v1/pods") => {
                    assert!(query.contains("spec.nodeName"));
                    let mut p = json!({"metadata":{"name":"web-0","namespace":"apps","uid":"pod-uid","resourceVersion":"pod-version","ownerReferences":[{"kind":"ReplicaSet","apiVersion":"apps/v1","name":"web","uid":"owner-uid","controller":true}]},"spec":{"nodeName":"worker","containers":[{"name":"web"}]},"status":{"phase":"Running"}});
                    if local_data {
                        p["spec"]["volumes"] = json!([{"name":"data","emptyDir":{}}]);
                    }
                    (
                        200,
                        json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":if evicted.load(std::sync::atomic::Ordering::SeqCst){vec![]}else{vec![p]}}),
                    )
                }
                ("GET", "/apis/apps/v1/namespaces/apps/replicasets/web") => (
                    200,
                    json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"uid":if controller_exists{"owner-uid"}else{"replaced-owner"}}}),
                ),
                ("POST", "/api/v1/namespaces/apps/pods/web-0/eviction") if allow_eviction => {
                    evicted.store(true, std::sync::atomic::Ordering::SeqCst);
                    (
                        200,
                        json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
                    )
                }
                ("POST", "/api/v1/namespaces/apps/pods/web-0/eviction") => (
                    429,
                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"TooManyRequests","message":"Cannot evict pod as it would violate the pod's disruption budget.","code":429}),
                ),
                _ => panic!("Unexpected node-management request {method} {path}"),
            };
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(data.to_string()))
                    .unwrap(),
            )
        }
    });
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: ":memory:".into(),
        public_origin: None,
        namespace: "test".into(),
        demo: true,
        healthcheck: false,
    };
    let mut state = AppState::initialize(&config).await.unwrap();
    state.client = Some(Client::new(mock, "test"));
    (state, seen)
}
#[tokio::test]
async fn drain_respects_pdb_and_uid_preconditions_without_force_deletion() {
    let (s, seen) = state(false, true, false).await;
    let preview = service::preview(&s, "worker").await.unwrap();
    assert_eq!(preview["blocked"], false);
    let result = service::drain(&s, "worker", &input(), &["pod-uid".into()])
        .await
        .unwrap();
    assert_eq!(result["state"], "incomplete");
    assert_eq!(result["cordoned"], true);
    assert!(
        result["failures"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Disruption budget")
    );
    let seen = seen.lock().unwrap();
    assert!(!seen.iter().any(|(method, _, _)| method == "DELETE"));
    let eviction = &seen.iter().find(|(m, _, _)| m == "POST").unwrap().2;
    assert_eq!(eviction["apiVersion"], "policy/v1");
    assert_eq!(eviction["deleteOptions"]["preconditions"]["uid"], "pod-uid");
    assert_eq!(
        eviction["deleteOptions"]["preconditions"]["resourceVersion"],
        "pod-version"
    );
    assert!(
        eviction["deleteOptions"]
            .get("gracePeriodSeconds")
            .is_none()
    );
}
#[tokio::test]
async fn unsafe_or_changed_previews_make_no_cluster_mutations() {
    for (local_data, controller_exists) in [(true, true), (false, false)] {
        let (s, seen) = state(local_data, controller_exists, false).await;
        assert_eq!(
            service::preview(&s, "worker").await.unwrap()["blocked"],
            true
        );
        assert!(
            service::drain(&s, "worker", &input(), &["pod-uid".into()])
                .await
                .is_err()
        );
        assert!(!seen.lock().unwrap().iter().any(|(m, _, _)| m != "GET"));
    }
    let (s, seen) = state(false, true, false).await;
    assert_eq!(
        service::drain(&s, "worker", &input(), &["replaced-pod".into()])
            .await
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    let mut stale = input();
    stale.uid = "replaced-node".into();
    assert_eq!(
        service::change(&s, "worker", &stale).await.unwrap_err().0,
        StatusCode::CONFLICT
    );
    assert!(service::detail(&s, "../../other-api").await.is_err());
    assert!(!seen.lock().unwrap().iter().any(|(m, _, _)| m != "GET"));
}

#[tokio::test]
async fn completed_drain_observes_pods_gone_and_keeps_node_cordoned() {
    let (s, seen) = state(false, true, true).await;
    let result = service::drain(&s, "worker", &input(), &["pod-uid".into()])
        .await
        .unwrap();
    assert_eq!(result["state"], "complete");
    assert_eq!(result["cordoned"], true);
    assert_eq!(result["requested"].as_array().unwrap().len(), 1);
    assert_eq!(result["remaining"].as_array().unwrap().len(), 0);
    let requests = seen.lock().unwrap();
    let patch = &requests.iter().find(|(m, _, _)| m == "PATCH").unwrap().2;
    assert_eq!(patch["metadata"]["uid"], "node-uid");
    assert_eq!(patch["metadata"]["resourceVersion"], "1");
    assert_eq!(patch["spec"]["unschedulable"], true);
}
