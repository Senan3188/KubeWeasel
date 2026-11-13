#[cfg(test)]
use crate::{
    applications::{self, models::Deploy},
    settings, storage,
};
use crate::{
    error::{Error, Result, invalid},
    http::request::mutation,
    state::AppState,
    workloads::{self, LABEL, managed},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use kube::api::{Patch, PatchParams};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use tokio::sync::RwLock;

pub const PROJECT_LABEL: &str = "kubeweasel.io/project";
#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
    pub gate: Arc<tokio::sync::Mutex<()>>,
}
#[derive(Clone, Serialize)]
struct Project {
    id: String,
    name: String,
    description: String,
}
#[derive(Deserialize)]
struct ProjectInput {
    name: String,
    #[serde(default)]
    description: String,
}
#[derive(Deserialize)]
struct Assignment {
    #[serde(default)]
    project_id: Option<String>,
}
fn storage() -> Error {
    Error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Project storage unavailable.".into(),
    )
}
fn validate_project(p: &ProjectInput) -> Result<()> {
    if p.name.trim().is_empty()
        || p.name.chars().count() > 80
        || p.description.chars().count() > 500
        || p.name.chars().any(char::is_control)
    {
        return Err(invalid(
            "Use a project name of 1–80 characters and a description of up to 500 characters.",
        ));
    }
    Ok(())
}
impl Store {
    pub fn open(path: &str) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY,namespace TEXT NOT NULL,name TEXT NOT NULL,description TEXT NOT NULL,UNIQUE(namespace,name COLLATE NOCASE));")?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    fn list(&self, ns: &str) -> Result<Vec<Project>> {
        let db = self.db.lock().map_err(|_| storage())?;
        let mut statement=db.prepare("SELECT id,name,description FROM projects WHERE namespace=?1 ORDER BY name COLLATE NOCASE").map_err(|_|storage())?;
        statement
            .query_map([ns], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                })
            })
            .map_err(|_| storage())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| storage())
    }
    pub fn require(&self, ns: &str, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            let found: bool = self
                .db
                .lock()
                .map_err(|_| storage())?
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM projects WHERE namespace=?1 AND id=?2)",
                    params![ns, id],
                    |r| r.get(0),
                )
                .map_err(|_| storage())?;
            if !found {
                return Err(Error(StatusCode::NOT_FOUND, "Project not found.".into()));
            }
        }
        Ok(())
    }
    fn save(&self, ns: &str, id: Option<&str>, input: ProjectInput) -> Result<Project> {
        validate_project(&input)?;
        self.require(ns, id)?;
        let id = id.map(String::from).unwrap_or_else(|| {
            let mut b = [0u8; 16];
            OsRng.fill_bytes(&mut b);
            b.iter().map(|b| format!("{b:02x}")).collect()
        });
        let name = input.name.trim().to_string();
        let db = self.db.lock().map_err(|_| storage())?;
        let result = if id_exists(&db, ns, &id)? {
            db.execute(
                "UPDATE projects SET name=?1,description=?2 WHERE namespace=?3 AND id=?4",
                params![name, input.description, ns, id],
            )
        } else {
            db.execute(
                "INSERT INTO projects(id,namespace,name,description) VALUES(?1,?2,?3,?4)",
                params![id, ns, name, input.description],
            )
        };
        result.map_err(|e|if matches!(e,rusqlite::Error::SqliteFailure(ref error,_)if error.code==rusqlite::ErrorCode::ConstraintViolation){Error(StatusCode::CONFLICT,"A project with this name already exists.".into())}else{storage()})?;
        Ok(Project {
            id,
            name,
            description: input.description,
        })
    }
}
fn id_exists(db: &Connection, ns: &str, id: &str) -> Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE namespace=?1 AND id=?2)",
        params![ns, id],
        |r| r.get(0),
    )
    .optional()
    .map(|v| v.unwrap_or(false))
    .map_err(|_| storage())
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects", get(list).post(create))
        .route("/api/projects/{id}", post(update).delete(remove))
        .route("/api/apps/{name}/project", post(assign))
}
async fn list(State(s): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(json!({"projects":s.projects.list(&s.namespace)?})))
}
async fn create(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(p): Json<ProjectInput>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.projects.gate.lock().await;
    Ok(Json(
        json!({"project":s.projects.save(&s.namespace,None,p)?}),
    ))
}
async fn update(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(p): Json<ProjectInput>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.projects.gate.lock().await;
    Ok(Json(
        json!({"project":s.projects.save(&s.namespace,Some(&id),p)?}),
    ))
}
async fn remove(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.projects.gate.lock().await;
    s.projects.require(&s.namespace, Some(&id))?;
    let assigned = if let Some(client) = s.client {
        !workloads::list(
            client,
            &s.namespace,
            &format!("{LABEL},{PROJECT_LABEL}={id}"),
        )
        .await?
        .is_empty()
    } else {
        s.apps
            .read()
            .await
            .values()
            .any(|a| a.project_id.as_deref() == Some(&id))
    };
    if assigned {
        return Err(Error(
            StatusCode::CONFLICT,
            "Move the project's applications elsewhere before deleting this folder.".into(),
        ));
    }
    s.projects
        .db
        .lock()
        .map_err(|_| storage())?
        .execute(
            "DELETE FROM projects WHERE namespace=?1 AND id=?2",
            params![s.namespace, id],
        )
        .map_err(|_| storage())?;
    Ok(Json(json!({"ok":true})))
}
async fn assign(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(a): Json<Assignment>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.projects.gate.lock().await;
    s.projects.require(&s.namespace, a.project_id.as_deref())?;
    if s.client.is_some() {
        managed(&s, &name)
            .await?
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({"metadata":{"labels":{PROJECT_LABEL:a.project_id}}})),
            )
            .await?;
    } else {
        let mut apps = s.apps.write().await;
        apps.get_mut(&name)
            .ok_or_else(|| Error(StatusCode::NOT_FOUND, "App not found".into()))?
            .project_id = a.project_id;
    }
    Ok(Json(json!({"ok":true})))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_validation_namespaces_and_duplicate_names() {
        let store = Store::open(":memory:").unwrap();
        let first = store
            .save(
                "a",
                None,
                ProjectInput {
                    name: "Website".into(),
                    description: "Production website".into(),
                },
            )
            .unwrap();
        assert_eq!(first.id.len(), 32);
        assert!(store.require("b", Some(&first.id)).is_err());
        assert!(
            store
                .save(
                    "a",
                    None,
                    ProjectInput {
                        name: "website".into(),
                        description: String::new()
                    }
                )
                .is_err()
        );
        let edited = store
            .save(
                "a",
                Some(&first.id),
                ProjectInput {
                    name: "Storefront".into(),
                    description: String::new(),
                },
            )
            .unwrap();
        assert_eq!(first.id, edited.id);
        assert_eq!(store.list("a").unwrap()[0].name, "Storefront");
        assert!(
            validate_project(&ProjectInput {
                name: "   ".into(),
                description: String::new()
            })
            .is_err()
        );
    }
    #[test]
    fn project_persists_on_reopen() {
        let path =
            std::env::temp_dir().join(format!("kubeweasel-project-{}.db", std::process::id()));
        {
            let store = Store::open(path.to_str().unwrap()).unwrap();
            store
                .save(
                    "test",
                    None,
                    ProjectInput {
                        name: "Persisted".into(),
                        description: String::new(),
                    },
                )
                .unwrap();
        }
        let store = Store::open(path.to_str().unwrap()).unwrap();
        assert_eq!(store.list("test").unwrap().len(), 1);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[tokio::test]
    async fn assignment_deploy_and_nonempty_deletion() {
        let store = Store::open(":memory:").unwrap();
        let s = AppState {
            client: None,
            namespace: "test".into(),
            apps: Arc::new(RwLock::new(BTreeMap::new())),
            settings: settings::SettingsState::default(),
            storage: storage::StorageState::default(),
            projects: store,
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-kubeweasel-request", "dashboard".parse().unwrap());
        let project = create(
            State(s.clone()),
            headers.clone(),
            Json(ProjectInput {
                name: "Website".into(),
                description: String::new(),
            }),
        )
        .await
        .unwrap()
        .0;
        let id = project["project"]["id"].as_str().unwrap().to_string();
        applications::service::deploy(
            &s,
            Deploy {
                workload_kind: String::new(),
                project_id: Some(id.clone()),
                name: "test-app".into(),
                image: "nginx:stable".into(),
                port: 80,
                replicas: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            s.apps.read().await["test-app"].project_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            remove(State(s.clone()), Path(id.clone()), headers.clone())
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            assign(
                State(s.clone()),
                Path("test-app".into()),
                headers.clone(),
                Json(Assignment {
                    project_id: Some("missing".into())
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
        let _ = assign(
            State(s.clone()),
            Path("test-app".into()),
            headers.clone(),
            Json(Assignment { project_id: None }),
        )
        .await
        .unwrap();
        let _ = remove(State(s.clone()), Path(id.clone()), headers)
            .await
            .unwrap();
        assert!(s.projects.require("test", Some(&id)).is_err());
        assert_eq!(s.apps.read().await["test-app"].replicas, 1);
    }
}
