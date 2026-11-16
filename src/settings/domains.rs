//! Independent application routes: one Ingress and TLS policy per domain entry.
use crate::{
    error::{Error, Result, invalid},
    http::request::mutation,
    https,
    settings::{self, Domain},
    state::AppState,
    validation::valid_name,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, post},
};
use k8s_openapi::api::networking::v1::Ingress;
use kube::{
    Api, ResourceExt,
    api::{
        DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions, PropagationPolicy,
    },
};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::{Value, json};
const APP_LABEL: &str = "kubeweasel.io/application";
const MANAGED: &str = "app.kubernetes.io/managed-by";
const LIMIT: usize = 20;
#[derive(Default, Deserialize)]
pub(crate) struct Selection {
    pub(crate) domain: Option<String>,
}
pub(crate) fn selected<'a>(config: &'a settings::Config, id: Option<&str>) -> Result<&'a Domain> {
    match id {
        None => Ok(&config.domain),
        Some(id) => config
            .domains
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| Error::not_found("Domain route not found.")),
    }
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/{name}/domains", post(create))
        .route(
            "/api/apps/{name}/domains/{domain}",
            post(update).merge(delete(remove)),
        )
}
fn belongs(i: &Ingress, app: &str) -> bool {
    i.labels().get(MANAGED).map(String::as_str) == Some("kubeweasel")
        && (i.labels().get(APP_LABEL).map(String::as_str) == Some(app)
            || (i.name_any() == app && !i.labels().contains_key(APP_LABEL)))
}
fn read(i: Ingress) -> Option<Domain> {
    let mut d = Domain {
        id: i.name_any(),
        ..Default::default()
    };
    https::read_annotations(&mut d, &i.metadata.annotations.unwrap_or_default());
    let spec = i.spec?;
    d.ingress_class = spec.ingress_class_name.unwrap_or_default();
    let rule = spec.rules?.into_iter().next()?;
    d.host = rule.host?;
    d.path = rule
        .http?
        .paths
        .into_iter()
        .next()?
        .path
        .unwrap_or_else(|| "/".into());
    d.tls_secret = spec
        .tls
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.hosts.as_ref().is_none_or(|hosts| hosts.contains(&d.host)))
        .and_then(|t| t.secret_name)
        .unwrap_or_default();
    Some(d)
}
pub(super) async fn load(s: &AppState, app: &str) -> Result<Vec<Domain>> {
    let api: Api<Ingress> = Api::namespaced(s.client.clone().unwrap(), &s.namespace);
    let mut result = Vec::new();
    if let Some(i) = api.get_opt(app).await?
        && belongs(&i, app)
        && let Some(d) = read(i)
    {
        result.push(d);
    }
    let selector = format!("{MANAGED}=kubeweasel,{APP_LABEL}={app}");
    for i in api
        .list(&ListParams::default().labels(&selector))
        .await?
        .items
    {
        if i.name_any() != app
            && belongs(&i, app)
            && let Some(d) = read(i)
        {
            result.push(d);
        }
    }
    result.sort_by_key(|d| (d.id != app, d.id.clone()));
    Ok(result)
}
fn new_id(app: &str) -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    format!(
        "{}-domain-{}",
        app.chars()
            .take(20)
            .collect::<String>()
            .trim_end_matches('-'),
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn validate_entry(d: &Domain, entries: &[Domain], legacy: bool) -> Result<()> {
    settings::validate_domain(d)?;
    if !legacy && d.host.is_empty() {
        return Err(invalid(
            "Enter a hostname; use Remove domain to delete a route.",
        ));
    }
    if entries
        .iter()
        .any(|e| e.id != d.id && e.host == d.host && e.path == d.path)
    {
        return Err(invalid(
            "This hostname and path already have a route for this application.",
        ));
    }
    if !d.host.is_empty() && !entries.iter().any(|e| e.id == d.id) && entries.len() >= LIMIT {
        return Err(invalid("An application can have at most 20 domain routes."));
    }
    Ok(())
}
async fn create(
    State(s): State<AppState>,
    Path(app): Path<String>,
    headers: HeaderMap,
    Json(mut d): Json<Domain>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _guard = s.settings.domain_gate.lock().await;
    d.id = new_id(&app);
    save(&s, &app, d, false).await.map(saved)
}
async fn update(
    State(s): State<AppState>,
    Path((app, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(mut d): Json<Domain>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _guard = s.settings.domain_gate.lock().await;
    let cfg = settings::load(&s, &app).await?;
    selected(&cfg, Some(&id))?;
    d.id = id;
    save(&s, &app, d, false).await.map(saved)
}
async fn remove(
    State(s): State<AppState>,
    Path((app, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _guard = s.settings.domain_gate.lock().await;
    let cfg = settings::load(&s, &app).await?;
    let mut d = selected(&cfg, Some(&id))?.clone();
    d.host.clear();
    save(&s, &app, d, true).await.map(saved)
}
fn saved(id: String) -> Json<Value> {
    Json(json!({"ok":true,"id":id}))
}

pub(super) async fn save(s: &AppState, app: &str, mut d: Domain, legacy: bool) -> Result<String> {
    if !valid_name(&d.id) {
        return Err(invalid("Invalid domain route ID."));
    }
    let mut cfg = settings::load(s, app).await?;
    validate_entry(&d, &cfg.domains, legacy)?;
    let previous = cfg
        .domains
        .iter()
        .find(|e| e.id == d.id)
        .cloned()
        .unwrap_or_default();
    https::normalize(&d.id.clone(), &mut d, &previous)?;
    https::require_automatic(s, &d).await?;
    if let Some(client) = s.client.clone() {
        let api: Api<Ingress> = Api::namespaced(client, &s.namespace);
        let existing = api.get_opt(&d.id).await?;
        if existing.as_ref().is_some_and(|i| !belongs(i, app)) {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "This Ingress does not belong to this application.".into(),
            ));
        }
        if d.host.is_empty() {
            if let Some(i) = existing {
                api.delete(
                    &d.id,
                    &DeleteParams {
                        preconditions: Some(Preconditions {
                            uid: i.metadata.uid,
                            resource_version: i.metadata.resource_version,
                        }),
                        propagation_policy: Some(PropagationPolicy::Orphan),
                        ..Default::default()
                    },
                )
                .await?;
            }
        } else {
            let mut value = settings::ingress(app, &d, settings::service_port(s, app).await?);
            value["metadata"]["name"] = json!(d.id);
            value["metadata"]["labels"][APP_LABEL] = json!(app);
            if let Some(i) = existing {
                // Merge only our routing fields and issuer annotations, preserving other metadata.
                let patch = json!({"metadata":{"uid":i.metadata.uid,"resourceVersion":i.metadata.resource_version,"labels":{APP_LABEL:app},"annotations":https::annotations(&d)},"spec":{"rules":value["spec"]["rules"],"ingressClassName":if d.ingress_class.is_empty(){Value::Null}else{json!(d.ingress_class)},"tls":if d.tls_secret.is_empty(){Value::Null}else{value["spec"]["tls"].clone()}}});
                api.patch(&d.id, &PatchParams::default(), &Patch::Merge(patch))
                    .await?;
            } else {
                let object: Ingress =
                    serde_json::from_value(value).map_err(|_| invalid("Invalid Ingress"))?;
                api.create(&PostParams::default(), &object).await?;
            }
        }
    } else {
        cfg.domains.retain(|e| e.id != d.id);
        if !d.host.is_empty() {
            cfg.domains.push(d.clone());
        }
        cfg.domains.sort_by_key(|e| (e.id != app, e.id.clone()));
        cfg.domain = cfg.domains.first().cloned().unwrap_or_default();
        s.settings.demo.write().await.insert(app.into(), cfg);
    }
    Ok(d.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_ownership_and_legacy_tls_are_preserved() {
        let value = settings::ingress(
            "web",
            &Domain {
                host: "public.example.test".into(),
                path: "/".into(),
                tls_secret: "retained-tls".into(),
                ..Default::default()
            },
            8080,
        );
        let mut original: Ingress = serde_json::from_value(value).unwrap();
        assert!(belongs(&original, "web"));
        assert!(!belongs(&original, "other"));
        let domain = read(original.clone()).unwrap();
        assert_eq!(domain.id, "web");
        assert_eq!(domain.tls_secret, "retained-tls");
        original
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(APP_LABEL.into(), "other".into());
        assert!(!belongs(&original, "web"));
        original.metadata.labels.as_mut().unwrap().remove(MANAGED);
        assert!(!belongs(&original, "other"));
    }
    #[test]
    fn routes_are_bounded_and_certificate_defaults_are_independent() {
        let mut public = Domain {
            id: new_id("web"),
            host: "public.example.test".into(),
            path: "/".into(),
            ingress_class: "traefik".into(),
            tls_mode: "automatic".into(),
            issuer: "staging".into(),
            issuer_kind: "Issuer".into(),
            ..Default::default()
        };
        let mut internal = Domain {
            id: new_id("web"),
            host: "web.internal".into(),
            ..public.clone()
        };
        assert_ne!(public.id, internal.id);
        https::normalize("web", &mut public, &Domain::default()).unwrap();
        https::normalize("web", &mut internal, &Domain::default()).unwrap();
        assert_ne!(public.tls_secret, internal.tls_secret);
        let entries = vec![public.clone()];
        let duplicate = Domain {
            id: new_id("web"),
            ..public.clone()
        };
        assert!(validate_entry(&duplicate, &entries, false).is_err());
        assert!(validate_entry(&public, &entries, false).is_ok());
        assert!(validate_entry(&internal, &vec![public.clone(); LIMIT], false).is_err());
        assert!(validate_entry(&Domain::default(), &[], false).is_err());
    }
}
