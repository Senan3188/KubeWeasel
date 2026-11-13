use crate::{applications::models::App, config::Config, projects, settings, storage};
use kube::Client;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) client: Option<Client>,
    pub(crate) namespace: String,
    pub(crate) apps: Arc<RwLock<BTreeMap<String, App>>>,
    pub(crate) settings: settings::SettingsState,
    pub(crate) projects: projects::Store,
    pub(crate) storage: storage::StorageState,
}
impl AppState {
    pub(crate) async fn initialize(
        config: &Config,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let client = if config.demo {
            None
        } else {
            Some(Client::try_default().await?)
        };
        let mut apps = BTreeMap::new();
        if config.demo {
            for (name, image, replicas) in [
                ("web-frontend", "nginx:stable", 2),
                ("payments-api", "ghcr.io/example/payments:1.4.2", 3),
                ("background-worker", "ghcr.io/example/worker:0.8.0", 0),
            ] {
                apps.insert(
                    name.into(),
                    App {
                        workload_kind: "Deployment".into(),
                        project_id: None,
                        name: name.into(),
                        image: image.into(),
                        replicas,
                        ready: replicas,
                        status: if replicas == 0 { "Paused" } else { "Healthy" }.into(),
                    },
                );
            }
        }
        Ok(Self {
            client,
            namespace: config.namespace.clone(),
            apps: Arc::new(RwLock::new(apps)),
            settings: settings::SettingsState::default(),
            storage: storage::StorageState::default(),
            projects: if config.demo {
                projects::Store::open(":memory:")?
            } else {
                projects::Store::open(&config.auth_db)?
            },
        })
    }
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn https_client_initializes_crypto_provider() {
        let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
        assert!(kube::Client::try_from(config).is_ok());
    }
}
