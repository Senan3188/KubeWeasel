use super::*;
use crate::{auth, config::Config, http};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn request(
    router: &Router,
    path: &str,
    body: Option<Value>,
    method: &str,
    cookie: &str,
    csrf: &str,
) -> (StatusCode, Value, String) {
    let mut req = Request::builder()
        .uri(path)
        .method(method)
        .header("host", "127.0.0.1:7878");
    if !cookie.is_empty() {
        req = req.header("cookie", cookie);
    }
    if body.is_some() {
        req = req
            .header("content-type", "application/json")
            .header("x-kubeweasel-request", "dashboard")
            .header("x-kubeweasel-csrf", csrf);
    }
    let res = router
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .to_string();
    let bytes = to_bytes(res.into_body(), 1_000_000).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_default(),
        cookie,
    )
}
#[tokio::test]
async fn owner_can_share_multiple_named_connections_without_exposing_credentials() {
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: ":memory:".into(),
        public_origin: None,
        private_origin: None,
        namespace: "test".into(),
        demo: true,
        healthcheck: false,
    };
    let state = AppState::initialize(&config).await.unwrap();
    let router = http::router(state.clone(), auth::test_state()).layer(axum::Extension(
        ConnectInfo("127.0.0.1:10000".parse::<std::net::SocketAddr>().unwrap()),
    ));
    for path in [
        "/api/projects/project/connections",
        "/api/apps/web-frontend/connections",
        "/api/clusters/local/apps/web-frontend/connections",
    ] {
        assert_eq!(
            request(&router, path, None, "GET", "", "").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status,setup,cookie)=request(&router,"/api/auth/setup",Some(json!({"setup_token":"test-setup-token","email":"owner@example.test","password":"a-test-password-long-enough"})),"POST","","").await;
    assert_eq!(status, StatusCode::OK);
    let csrf = setup["csrf"].as_str().unwrap();
    let (_, p, _) = request(
        &router,
        "/api/projects",
        Some(json!({"name":"Shop"})),
        "POST",
        &cookie,
        csrf,
    )
    .await;
    let project = p["project"]["id"].as_str().unwrap();
    for app in ["web-frontend", "payments-api"] {
        let (status, _, _) = request(
            &router,
            &format!("/api/apps/{app}/project"),
            Some(json!({"project_id":project})),
            "POST",
            &cookie,
            csrf,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let path = format!("/api/clusters/local/projects/{project}/connections");
    let input = json!({"name":"Postgres","kind":"postgresql","project_id":project,"values":{"host":"postgres","password":"never-return-this-password"}});
    assert_eq!(
        request(&router, &path, Some(input.clone()), "POST", &cookie, "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, pg, _) = request(&router, &path, Some(input.clone()), "POST", &cookie, csrf).await;
    assert_eq!(status, StatusCode::OK, "{pg}");
    assert!(!pg.to_string().contains("never-return"));
    let id = pg["connection"]["id"].as_str().unwrap();
    assert_eq!(
        request(&router, &path, Some(input), "POST", &cookie, csrf)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status,mysql,_)=request(&router,&path,Some(json!({"name":"MySQL","kind":"mysql","project_id":project,"values":{"url":"mysql://private-value"}})),"POST",&cookie,csrf).await;
    assert_eq!(status, StatusCode::OK, "{mysql}");
    let mysql_id = mysql["connection"]["id"].as_str().unwrap();
    let (status, _, _) = request(&router, &path, Some(json!({"name":"Reporting Postgres","kind":"postgresql","project_id":project,"values":{"url":"postgres://fake-reporting"}})), "POST", &cookie, csrf).await;
    assert_eq!(status, StatusCode::OK);

    for (app, conn, mappings) in [
        (
            "web-frontend",
            id,
            json!({"POSTGRES_PASSWORD":"password","POSTGRES_HOST":"host"}),
        ),
        ("web-frontend", mysql_id, json!({"MYSQL_URL":"url"})),
        ("payments-api", id, json!({"DB_PASSWORD":"password"})),
    ] {
        let (_, list, _) = request(
            &router,
            &format!("/api/apps/{app}/connections"),
            None,
            "GET",
            &cookie,
            csrf,
        )
        .await;
        let connview = list["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == conn)
            .unwrap();
        let payload = json!({"mappings":mappings,"version":connview["version"],"workload_version":connview["workload_version"]});
        let endpoint = format!("/api/apps/{app}/connections/{conn}");
        assert_eq!(
            request(
                &router,
                &endpoint,
                Some(payload.clone()),
                "POST",
                &cookie,
                csrf
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(&router, &endpoint, Some(payload), "POST", &cookie, csrf)
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    let (_, list, _) = request(
        &router,
        "/api/apps/web-frontend/connections",
        None,
        "GET",
        &cookie,
        csrf,
    )
    .await;
    assert_eq!(list["connections"].as_array().unwrap().len(), 3);
    assert!(!list.to_string().contains("private-value"));
    assert!(!list.to_string().contains("never-return"));
    assert_eq!(
        list["connections"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| !v["mappings"].as_object().unwrap().is_empty())
            .count(),
        2
    );
    let (_, other, _) = request(
        &router,
        "/api/projects",
        Some(json!({"name":"Other"})),
        "POST",
        &cookie,
        csrf,
    )
    .await;
    let other_project = other["project"]["id"].as_str().unwrap();
    let (_, isolated, _) = request(
        &router,
        &format!("/api/projects/{other_project}/connections"),
        None,
        "GET",
        &cookie,
        csrf,
    )
    .await;
    assert_eq!(isolated["connections"], json!([]));
    assert_eq!(
        request(
            &router,
            "/api/apps/background-worker/project",
            Some(json!({"project_id":other_project})),
            "POST",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            &format!("/api/apps/background-worker/connections/{id}"),
            Some(
                json!({"mappings":{"DB_PASSWORD":"password"},"version":"1","workload_version":"0"})
            ),
            "POST",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &router,
            "/api/apps/web-frontend/environment",
            Some(json!({"env":{"POSTGRES_PASSWORD":"must-not-overwrite-reference"}})),
            "POST",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );

    let connected = catalog(&state, project, None).await.unwrap();
    let pg = connected.iter().find(|c| c.id == id).unwrap();
    assert_eq!(pg.consumers.len(), 2);
    assert!(pg.consumers.iter().all(|c| !c.restart_required));
    let endpoint = format!("{path}/{id}");
    assert_eq!(
        request(
            &router,
            &endpoint,
            Some(json!({"version":"1"})),
            "DELETE",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &router,
            "/api/apps/web-frontend/project",
            Some(json!({"project_id":null})),
            "POST",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status,rotated,_)=request(&router,&endpoint,Some(json!({"name":"Postgres","kind":"postgresql","project_id":project,"version":"1","values":{"password":"new-private-password"}})),"POST",&cookie,csrf).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    assert!(!rotated.to_string().contains("new-private"));
    let secret = read(&state, id).await.unwrap();
    assert_eq!(secret.data.as_ref().unwrap()["host"].0, b"postgres");
    assert_eq!(
        secret.data.as_ref().unwrap()["password"].0,
        b"new-private-password"
    );
    let connected = catalog(&state, project, None).await.unwrap();
    assert!(
        connected
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .consumers
            .iter()
            .all(|c| c.restart_required)
    );
    assert!(
        connected
            .iter()
            .find(|c| c.id == mysql_id)
            .unwrap()
            .consumers
            .iter()
            .all(|c| !c.restart_required)
    );
    // Ordinary app restart acknowledges its versions independently of the other consumer.
    crate::applications::service::restart(&state, "web-frontend")
        .await
        .unwrap();
    let consumers = catalog(&state, project, None)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap()
        .consumers;
    assert!(
        !consumers
            .iter()
            .find(|c| c.name == "web-frontend")
            .unwrap()
            .restart_required
    );
    assert!(
        consumers
            .iter()
            .find(|c| c.name == "payments-api")
            .unwrap()
            .restart_required
    );
    // One app uses two changed database connections; one restart reloads both.
    let rotation=serde_json::from_value(json!({"name":"MySQL","kind":"mysql","project_id":project,"version":"1","values":{"url":"mysql://changed-private-value"}})).unwrap();
    save(&state, project, Some(mysql_id), rotation)
        .await
        .unwrap();
    assert!(
        catalog(&state, project, None)
            .await
            .unwrap()
            .iter()
            .find(|c| c.id == mysql_id)
            .unwrap()
            .consumers[0]
            .restart_required
    );
    let (status, restart, _) = request(
        &router,
        &format!("{endpoint}/restart"),
        Some(json!({"version":"2"})),
        "POST",
        &cookie,
        csrf,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restart["restarted"].as_array().unwrap().len(), 2);
    assert!(
        catalog(&state, project, None)
            .await
            .unwrap()
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .consumers
            .iter()
            .all(|c| !c.restart_required)
    );
    assert!(
        !catalog(&state, project, None)
            .await
            .unwrap()
            .iter()
            .find(|c| c.id == mysql_id)
            .unwrap()
            .consumers[0]
            .restart_required
    );
    let original = usage::revision(&read(&state, mysql_id).await.unwrap());
    let unchanged=serde_json::from_value(json!({"name":"MySQL renamed","kind":"mysql","project_id":project,"version":"2","values":{"url":"mysql://changed-private-value"}})).unwrap();
    save(&state, project, Some(mysql_id), unchanged)
        .await
        .unwrap();
    assert_eq!(
        original,
        usage::revision(&read(&state, mysql_id).await.unwrap())
    );
    for app in ["web-frontend", "payments-api"] {
        let (_, list, _) = request(
            &router,
            &format!("/api/apps/{app}/connections"),
            None,
            "GET",
            &cookie,
            csrf,
        )
        .await;
        let current = list["connections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .unwrap();
        let payload = json!({"mappings":{},"version":current["version"],"workload_version":current["workload_version"]});
        assert_eq!(
            request(
                &router,
                &format!("/api/apps/{app}/connections/{id}"),
                Some(payload),
                "DELETE",
                &cookie,
                csrf
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(
            &router,
            &endpoint,
            Some(json!({"version":"2"})),
            "DELETE",
            &cookie,
            csrf
        )
        .await
        .0,
        StatusCode::OK
    );
    let remote = state.projects.for_cluster("another-cluster");
    assert!(remote.require("test", Some(project)).is_err());
}
#[test]
fn mappings_preserve_external_references_and_reject_collisions() {
    let env: Vec<EnvVar> = serde_json::from_value(json!([
        {"name":"OTHER","valueFrom":{"configMapKeyRef":{"name":"settings","key":"a"}}},
        {"name":"PLAIN","value":"keep"},
        {"name":"OLD","valueFrom":{"secretKeyRef":{"name":"kw-conn-a","key":"password"}}}
    ]))
    .unwrap();
    let keys = BTreeSet::from(["password".into()]);
    let mut input = Attachment {
        mappings: BTreeMap::from([("OTHER".into(), "password".into())]),
        version: "1".into(),
        workload_version: "1".into(),
    };
    assert!(mapped_env(env.clone(), "kw-conn-a", &keys, &input, false).is_err());
    input.mappings = BTreeMap::from([("DB_PASSWORD".into(), "password".into())]);
    let output = mapped_env(env, "kw-conn-a", &keys, &input, false).unwrap();
    assert_eq!(output.len(), 3);
    assert!(
        output
            .iter()
            .any(|e| e.name == "OTHER"
                && e.value_from.as_ref().unwrap().config_map_key_ref.is_some())
    );
    assert_eq!(
        output
            .iter()
            .find(|e| e.name == "PLAIN")
            .unwrap()
            .value
            .as_deref(),
        Some("keep")
    );
    assert!(!output.iter().any(|e| e.name == "OLD"));
    assert!(references(
        &json!({"spec":{"template":{"spec":{"initContainers":[{"envFrom":[{"secretRef":{"name":"kw-conn-a"}}]}],"volumes":[{"secret":{"secretName":"kw-conn-b"}}]}}}}),
        "kw-conn-a"
    ));
    assert!(references(
        &json!({"volumes":[{"projected":{"sources":[{"secret":{"name":"kw-conn-b"}}]}}]}),
        "kw-conn-b"
    ));
}

#[tokio::test]
async fn kubernetes_secret_operations_are_scoped_versioned_and_redacted() {
    use axum::http::Response;
    use kube::{Client, client::Body as KubeBody};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: ":memory:".into(),
        public_origin: None,
        private_origin: None,
        namespace: "test".into(),
        demo: true,
        healthcheck: false,
    };
    let mut state = AppState::initialize(&config).await.unwrap();
    // Create project metadata through the feature router before adding the mock client.
    let router = crate::http::features().with_state(state.clone());
    let (_, p, _) = request(
        &router,
        "/api/projects",
        Some(json!({"name":"Mock"})),
        "POST",
        "",
        "",
    )
    .await;
    let project = p["project"]["id"].as_str().unwrap().to_owned();
    let secret = Arc::new(Mutex::new(None::<Value>));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let used = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(false));
    let (store, requests, occupied, unavailable) =
        (secret.clone(), recorded.clone(), used.clone(), fail.clone());
    let mock = tower::service_fn(move |req: Request<KubeBody>| {
        let (store, requests, occupied, unavailable) = (
            store.clone(),
            requests.clone(),
            occupied.clone(),
            unavailable.clone(),
        );
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let body = to_bytes(Body::new(req.into_body()), 1_000_000)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or_default();
            requests
                .lock()
                .unwrap()
                .push((method.clone(), path.clone(), body.clone()));
            let mut store = store.lock().unwrap();
            let (status, data) = if unavailable.load(Ordering::SeqCst) {
                (
                    403,
                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","message":"do-not-leak-password","reason":"Forbidden","code":403}),
                )
            } else if path.ends_with("/secrets") {
                if method == "POST" {
                    let mut v = body;
                    v["metadata"]["resourceVersion"] = json!("1");
                    v["metadata"]["uid"] = json!("secret-uid");
                    *store = Some(v.clone());
                    (201, v)
                } else {
                    (
                        200,
                        json!({"apiVersion":"v1","kind":"SecretList","metadata":{},"items":store.clone().into_iter().collect::<Vec<_>>()}),
                    )
                }
            } else if path.contains("/secrets/") {
                match method.as_str() {
                    "GET" => (200, store.clone().unwrap()),
                    "PUT" => {
                        assert_eq!(body["metadata"]["resourceVersion"], "1");
                        assert_eq!(body["metadata"]["uid"], "secret-uid");
                        let mut v = body;
                        v["metadata"]["resourceVersion"] = json!("2");
                        *store = Some(v.clone());
                        (200, v)
                    }
                    "DELETE" => {
                        assert_eq!(body["preconditions"]["uid"], "secret-uid");
                        assert_eq!(body["preconditions"]["resourceVersion"], "2");
                        *store = None;
                        (
                            200,
                            json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
                        )
                    }
                    _ => panic!("Unexpected Secret method"),
                }
            } else {
                let items = if occupied.load(Ordering::SeqCst) && path.ends_with("/cronjobs") {
                    vec![
                        json!({"apiVersion":"batch/v1","kind":"CronJob","metadata":{"name":"nightly"},"spec":{"jobTemplate":{"spec":{"template":{"spec":{"containers":[{"name":"task","envFrom":[{"secretRef":{"name":store.as_ref().unwrap()["metadata"]["name"]}}]}]}}}}}}),
                    ]
                } else {
                    vec![]
                };
                (
                    200,
                    json!({"apiVersion":"v1","kind":"List","metadata":{},"items":items}),
                )
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
    state.client = Some(Client::new(mock, "test"));
    let input = |version: Option<&str>, values: Value| {
        serde_json::from_value::<Input>(json!({"name":"Postgres","kind":"postgresql","project_id":project,"version":version,"values":values})).unwrap()
    };
    let created = save(
        &state,
        &project,
        None,
        input(None, json!({"password":"mock-secret","host":"db"})),
    )
    .await
    .unwrap();
    let listing = catalog(&state, &project, None).await.unwrap();
    assert!(
        !serde_json::to_string(&listing)
            .unwrap()
            .contains("mock-secret")
    );
    let id = created.id;
    save(
        &state,
        &project,
        Some(&id),
        input(Some("1"), json!({"password":"rotated-secret"})),
    )
    .await
    .unwrap();
    let saved: Secret = serde_json::from_value(secret.lock().unwrap().clone().unwrap()).unwrap();
    assert_eq!(saved.data.unwrap()["host"].0, b"db");
    used.store(true, Ordering::SeqCst);
    assert!(remove(&state, &project, &id, "2").await.is_err());
    assert!(!recorded.lock().unwrap().iter().any(|r| r.0 == "DELETE"));
    used.store(false, Ordering::SeqCst);
    remove(&state, &project, &id, "2").await.unwrap();
    assert!(
        recorded
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.1.contains("/namespaces/test/"))
    );
    fail.store(true, Ordering::SeqCst);
    let err = catalog(&state, &project, None).await.err().unwrap();
    assert!(!err.to_string().contains("do-not-leak"));
}

#[tokio::test]
async fn live_attachment_uses_references_and_workload_preconditions() {
    use axum::http::Response;
    use kube::{Client, client::Body as KubeBody};
    use std::sync::Mutex;
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: ":memory:".into(),
        public_origin: None,
        private_origin: None,
        namespace: "test".into(),
        demo: true,
        healthcheck: false,
    };
    let mut state = AppState::initialize(&config).await.unwrap();
    let router = crate::http::features().with_state(state.clone());
    let (_, p, _) = request(
        &router,
        "/api/projects",
        Some(json!({"name":"Live attachment"})),
        "POST",
        "",
        "",
    )
    .await;
    let project = p["project"]["id"].as_str().unwrap();
    let id = "kw-conn-fixture";
    let description =
        json!({"name":"Postgres","kind":"postgresql","project_id":project}).to_string();
    let secret = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":id,"uid":"secret-uid","resourceVersion":"1","labels":{"app.kubernetes.io/managed-by":"kubeweasel",KIND:"true",PROJECT_LABEL:project},"annotations":{META:description}},"type":"Opaque","data":{"password":"bmV2ZXItZXhwb3NlLW1l"}});
    let workload = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","uid":"app-uid","resourceVersion":"7","labels":{"app.kubernetes.io/managed-by":"kubeweasel",PROJECT_LABEL:project}},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"app","image":"nginx","env":[{"name":"PLAIN","value":"keep"},{"name":"EXTERNAL","valueFrom":{"configMapKeyRef":{"name":"settings","key":"feature"}}}]}]}}}});
    let patches = Arc::new(Mutex::new(Vec::<Value>::new()));
    let recorded = patches.clone();
    let mock = tower::service_fn(move |req: Request<KubeBody>| {
        let (secret, workload, recorded) = (secret.clone(), workload.clone(), recorded.clone());
        async move {
            let method = req.method().to_string();
            let path = req.uri().path().to_string();
            let bytes = to_bytes(Body::new(req.into_body()), 1_000_000)
                .await
                .unwrap();
            let response =
                if path.contains("/secrets/") {
                    secret
                } else if method == "PATCH" {
                    let patch: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(patch["metadata"]["resourceVersion"], "7");
                    assert_eq!(
                        patch
                            .pointer(&format!(
                                "/spec/template/metadata/annotations/{}",
                                usage::marker(id).replace('~', "~0").replace('/', "~1")
                            ))
                            .unwrap(),
                        "legacy:1"
                    );
                    assert!(!patch.to_string().contains("never-expose-me"));
                    let env = patch
                        .pointer("/spec/template/spec/containers/0/env")
                        .unwrap()
                        .as_array()
                        .unwrap();
                    assert!(env.iter().any(|e| e["name"] == "DB_PASSWORD"
                        && e["valueFrom"]["secretKeyRef"]["name"] == id));
                    assert!(env.iter().any(|e| e["name"] == "EXTERNAL"
                        && e["valueFrom"]["configMapKeyRef"]["name"] == "settings"));
                    assert!(
                        env.iter()
                            .any(|e| e["name"] == "PLAIN" && e["value"] == "keep")
                    );
                    let mut updated = workload;
                    updated["spec"]["template"]["spec"]["containers"][0]["env"] = json!(
                        env.iter()
                            .filter(|e| e.get("name").is_some())
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                    recorded.lock().unwrap().push(patch);
                    updated
                } else {
                    workload
                };
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(response.to_string()))
                    .unwrap(),
            )
        }
    });
    state.client = Some(Client::new(mock, "test"));
    let input = || Attachment {
        mappings: BTreeMap::from([("DB_PASSWORD".into(), "password".into())]),
        version: "1".into(),
        workload_version: "7".into(),
    };
    attach(&state, "web", id, input(), false).await.unwrap();
    assert_eq!(patches.lock().unwrap().len(), 1);
    let mut stale = input();
    stale.workload_version = "6".into();
    assert!(attach(&state, "web", id, stale, false).await.is_err());
    let mut collision = input();
    collision.mappings = BTreeMap::from([("EXTERNAL".into(), "password".into())]);
    assert!(attach(&state, "web", id, collision, false).await.is_err());
    assert_eq!(patches.lock().unwrap().len(), 1);
}

#[test]
fn only_inactive_deployment_revisions_are_excluded_from_consumers() {
    let mut object:DynamicObject=serde_json::from_value(json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"old","ownerReferences":[{"apiVersion":"apps/v1","kind":"Deployment","name":"web","uid":"web-uid","controller":true}]},"spec":{"replicas":0}})).unwrap();
    assert!(historical_revision("ReplicaSet", &object));
    assert!(!historical_revision("Deployment", &object));
    object.data["spec"]["replicas"] = json!(1);
    assert!(!historical_revision("ReplicaSet", &object));
    object.data["spec"]["replicas"] = json!(0);
    object.metadata.owner_references = None;
    assert!(!historical_revision("ReplicaSet", &object));
}

#[tokio::test]
async fn failed_restart_stays_stale_and_missing_usage_is_not_an_empty_list() {
    use axum::http::Response;
    use kube::{Client, client::Body as KubeBody};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    let config = Config {
        address: "127.0.0.1:7878".into(),
        auth_db: ":memory:".into(),
        public_origin: None,
        private_origin: None,
        namespace: "test".into(),
        demo: true,
        healthcheck: false,
    };
    let mut state = AppState::initialize(&config).await.unwrap();
    let router = crate::http::features().with_state(state.clone());
    let (_, p, _) = request(
        &router,
        "/api/projects",
        Some(json!({"name":"Restart test"})),
        "POST",
        "",
        "",
    )
    .await;
    let project = p["project"]["id"].as_str().unwrap();
    let id = "kw-conn-restart-test";
    let description =
        json!({"name":"Postgres","kind":"postgresql","project_id":project}).to_string();
    let secret = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":id,"resourceVersion":"2","labels":{"app.kubernetes.io/managed-by":"kubeweasel",KIND:"true",PROJECT_LABEL:project},"annotations":{META:description,(usage::REVISION):"r2"}},"type":"Opaque","data":{"password":"ZmFrZQ=="}});
    let apps = Arc::new(Mutex::new(BTreeMap::<String, Value>::new()));
    for name in ["good", "bad"] {
        apps.lock().unwrap().insert(name.into(),json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":name,"uid":format!("uid-{name}"),"resourceVersion":"7","generation":2,"labels":{"app.kubernetes.io/managed-by":"kubeweasel",PROJECT_LABEL:project}},"spec":{"replicas":1,"selector":{},"template":{"metadata":{"annotations":{(usage::marker(id)):"r1"}},"spec":{"containers":[{"name":"app","env":[{"name":"PASSWORD","valueFrom":{"secretKeyRef":{"name":id,"key":"password"}}}]}]}}},"status":{"observedGeneration":1,"replicas":1,"readyReplicas":1,"updatedReplicas":1}}));
    }
    let denied = Arc::new(AtomicBool::new(false));
    let unavailable = denied.clone();
    let data = apps.clone();
    let mock = tower::service_fn(move |req: Request<KubeBody>| {
        let (secret, data, unavailable) = (secret.clone(), data.clone(), unavailable.clone());
        async move {
            let path = req.uri().path().to_string();
            let method = req.method().to_string();
            let bytes = to_bytes(Body::new(req.into_body()), 1_000_000)
                .await
                .unwrap();
            let mut data = data.lock().unwrap();
            let (status, value) = if path.ends_with("/secrets") {
                (
                    200,
                    json!({"apiVersion":"v1","kind":"SecretList","items":[secret]}),
                )
            } else if path.contains("/secrets/") {
                (200, secret)
            } else if unavailable.load(Ordering::SeqCst) {
                (
                    403,
                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","message":"private-cluster-error","code":403}),
                )
            } else if path.ends_with("/deployments") {
                (
                    200,
                    json!({"apiVersion":"apps/v1","kind":"DeploymentList","items":data.values().cloned().collect::<Vec<_>>()}),
                )
            } else if path.ends_with("/statefulsets") {
                (
                    200,
                    json!({"apiVersion":"apps/v1","kind":"StatefulSetList","items":[]}),
                )
            } else {
                let name = path.rsplit('/').next().unwrap();
                if method == "PATCH" && name == "bad" {
                    (
                        403,
                        json!({"apiVersion":"v1","kind":"Status","status":"Failure","message":"private-patch-error","code":403}),
                    )
                } else {
                    let app = data.get_mut(name).unwrap();
                    if method == "PATCH" {
                        let patch: Value = serde_json::from_slice(&bytes).unwrap();
                        app["spec"]["template"]["metadata"]["annotations"] =
                            patch["spec"]["template"]["metadata"]["annotations"].clone();
                    }
                    (200, app.clone())
                }
            };
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(value.to_string()))
                    .unwrap(),
            )
        }
    });
    state.client = Some(Client::new(mock, "test"));
    let result = restart(&state, project, id, "2").await.unwrap();
    assert_eq!(result["restarted"], json!(["good"]));
    assert_eq!(result["failed"], json!(["bad"]));
    let catalog = catalog(&state, project, None).await.unwrap();
    let consumers = &catalog[0].consumers;
    assert!(
        consumers
            .iter()
            .find(|c| c.name == "bad")
            .unwrap()
            .restart_required
    );
    let good = consumers.iter().find(|c| c.name == "good").unwrap();
    assert!(!good.restart_required);
    assert_eq!(good.status, "Restart requested · rollout pending");
    denied.store(true, Ordering::SeqCst);
    let catalog = super::catalog(&state, project, None).await.unwrap();
    assert!(catalog[0].usage_error.is_some());
    assert!(
        !serde_json::to_string(&catalog)
            .unwrap()
            .contains("private-cluster-error")
    );
}
