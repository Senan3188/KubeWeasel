use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize)]
pub(crate) struct App {
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) replicas: i32,
    pub(crate) ready: i32,
    pub(crate) status: String,
    pub(crate) workload_kind: String,
    pub(crate) project_id: Option<String>,
}
#[derive(Deserialize)]
pub(crate) struct Deploy {
    #[serde(default)]
    pub(crate) workload_kind: String,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    pub(crate) name: String,
    pub(crate) image: String,
    pub(crate) port: u16,
    pub(crate) replicas: i32,
}
#[derive(Deserialize)]
pub(crate) struct Scale {
    pub(crate) replicas: i32,
}
