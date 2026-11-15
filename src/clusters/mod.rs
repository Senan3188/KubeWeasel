//! Cluster connections own isolated application contexts and worker lifetimes.
mod connection;
mod crypto;
mod routes;
mod store;
use crate::{
    error::{Result, invalid},
    git, history, settings,
    state::AppState,
    storage,
};
use rand_core::{OsRng, RngCore};
pub(crate) use routes::routes;
use std::{collections::BTreeMap, path::Path, sync::Arc};
pub(crate) use store::Cluster;
use tokio::{sync::Mutex, task::JoinHandle};
struct Context {
    state: AppState,
    workers: Vec<JoinHandle<()>>,
    activity: Arc<tokio::sync::RwLock<()>>,
}
impl Drop for Context {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.abort();
        }
    }
}
#[derive(Clone)]
pub(crate) struct Manager {
    store: Arc<store::Store>,
    local: AppState,
    contexts: Arc<Mutex<BTreeMap<String, Context>>>,
    gate: Arc<Mutex<()>>,
}
impl Manager {
    pub(crate) fn open(local: AppState, path: &str, key: Option<&Path>) -> Result<Self> {
        Ok(Self {
            store: Arc::new(store::Store::open(path, key, &local.namespace)?),
            local,
            contexts: Arc::new(Mutex::new(BTreeMap::new())),
            gate: Arc::new(Mutex::new(())),
        })
    }
    pub(crate) async fn resolve(&self, id: &str) -> Result<AppState> {
        let _gate = self.gate.lock().await;
        self.resolve_locked(id).await
    }
    async fn resolve_locked(&self, id: &str) -> Result<AppState> {
        let cluster = self.store.get(id)?;
        if cluster.local {
            return Ok(self.local.clone());
        }
        if let Some(context) = self.contexts.lock().await.get(id) {
            return Ok(context.state.clone());
        }
        let raw = self.store.credentials(id)?;
        let (client, _) = connection::client(&raw, &cluster.context, &cluster.namespace).await?;
        let state = AppState {
            client: Some(client),
            namespace: cluster.namespace,
            apps: Default::default(),
            settings: settings::SettingsState::default(),
            storage: storage::StorageState::default(),
            projects: self.local.projects.for_cluster(id),
            build_gate: Default::default(),
        };
        let workers = vec![git::worker(state.clone()), history::worker(state.clone())];
        self.contexts.lock().await.insert(
            id.into(),
            Context {
                state: state.clone(),
                workers,
                activity: Default::default(),
            },
        );
        Ok(state)
    }
    async fn lease(
        &self,
        id: &str,
    ) -> Result<(AppState, Option<tokio::sync::OwnedRwLockReadGuard<()>>)> {
        let _gate = self.gate.lock().await;
        let state = self.resolve_locked(id).await?;
        let activity = self
            .contexts
            .lock()
            .await
            .get(id)
            .map(|c| c.activity.clone());
        let lease = match activity {
            Some(a) => Some(a.read_owned().await),
            None => None,
        };
        Ok((state, lease))
    }
    async fn evict(&self, id: &str) {
        if let Some(context) = self.contexts.lock().await.remove(id) {
            let _idle = context.activity.write().await;
            context.state.settings.stop_all_forwards().await;
        }
    }
    pub(crate) async fn resume(&self) {
        if let Ok(clusters) = self.store.list() {
            for cluster in clusters {
                if !cluster.local
                    && let Err(e) = self.resolve(&cluster.id).await
                {
                    eprintln!("Cluster {} unavailable: {}", cluster.id, e.1);
                }
            }
        }
    }
}
fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(invalid("Cluster name must contain 1–80 characters."));
    }
    Ok(())
}
fn new_id() -> String {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, http::features};
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    async fn call(
        router: &Router,
        path: &str,
        body: Option<Value>,
        method: &str,
    ) -> (axum::http::StatusCode, Value) {
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
        let r = router.clone().oneshot(request).await.unwrap();
        let status = r.status();
        (
            status,
            serde_json::from_slice(&to_bytes(r.into_body(), 1024 * 1024).await.unwrap()).unwrap(),
        )
    }
    #[tokio::test]
    async fn scoped_dispatch_isolates_apps_projects_and_configuration_and_disconnects_safely() {
        let config = Config {
            address: "127.0.0.1:7878".into(),
            auth_db: ":memory:".into(),
            public_origin: None,
            private_origin: None,
            namespace: "same".into(),
            demo: true,
            healthcheck: false,
        };
        let local = AppState::initialize(&config).await.unwrap();
        let manager = Manager::open(local.clone(), ":memory:", None).unwrap();
        for id in ["one", "two"] {
            manager
                .store
                .save(
                    &Cluster {
                        id: id.into(),
                        name: id.into(),
                        namespace: "same".into(),
                        server: "https://example.test".into(),
                        context: String::new(),
                        local: false,
                    },
                    Some("encrypted-fixture"),
                )
                .unwrap();
            let mut state = AppState::initialize(&config).await.unwrap();
            state.projects = local.projects.for_cluster(id);
            manager.contexts.lock().await.insert(
                id.into(),
                Context {
                    state,
                    workers: vec![],
                    activity: Default::default(),
                },
            );
        }
        let router = features().merge(routes(manager.clone())).with_state(local);
        let (_, project) = call(
            &router,
            "/api/clusters/one/projects",
            Some(json!({"name":"Website","description":""})),
            "POST",
        )
        .await;
        let project = project["project"]["id"].as_str().unwrap();
        let (_, other) = call(
            &router,
            "/api/clusters/two/projects",
            Some(json!({"name":"Website","description":""})),
            "POST",
        )
        .await;
        assert_ne!(project, other["project"]["id"].as_str().unwrap());
        let app = json!({"name":"identical","image":"nginx:stable","port":80,"replicas":0,"project_id":project});
        assert_eq!(
            call(&router, "/api/clusters/two/apps", Some(app.clone()), "POST")
                .await
                .0,
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&router, "/api/clusters/one/apps", Some(app), "POST")
                .await
                .0,
            axum::http::StatusCode::OK
        );
        let (_, config) = call(
            &router,
            "/api/clusters/one/apps/identical/config",
            None,
            "GET",
        )
        .await;
        assert_eq!(config["config"]["image"], "nginx:stable");
        assert_eq!(
            call(
                &router,
                "/api/clusters/two/apps/identical/config",
                None,
                "GET"
            )
            .await
            .0,
            axum::http::StatusCode::NOT_FOUND
        );
        assert!(!Arc::ptr_eq(
            &manager.resolve("one").await.unwrap().build_gate,
            &manager.resolve("two").await.unwrap().build_gate
        ));
        let retained = manager.resolve("one").await.unwrap();
        let worker = tokio::spawn(std::future::pending::<()>());
        let worker_status = worker.abort_handle();
        manager
            .contexts
            .lock()
            .await
            .get_mut("one")
            .unwrap()
            .workers
            .push(worker);
        let (_, lease) = manager.lease("one").await.unwrap();
        let deleting_router = router.clone();
        let deleting = tokio::spawn(async move {
            call(
                &deleting_router,
                "/api/clusters/one",
                Some(json!({})),
                "DELETE",
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while manager.store.get("one").is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !deleting.is_finished(),
            "disconnect must drain active requests"
        );
        drop(lease);
        assert_eq!(deleting.await.unwrap().0, axum::http::StatusCode::OK);
        tokio::task::yield_now().await;
        assert!(
            worker_status.is_finished(),
            "disconnect must cancel workers"
        );
        assert!(manager.resolve("one").await.is_err());
        assert!(!manager.contexts.lock().await.contains_key("one"));
        assert!(retained.apps.read().await.contains_key("identical"));
        assert_eq!(
            call(&router, "/api/clusters/local", Some(json!({})), "DELETE")
                .await
                .0,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
}

#[cfg(test)]
mod live_tests;
