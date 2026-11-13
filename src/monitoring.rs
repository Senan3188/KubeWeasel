use super::*;
use axum::http::Request;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/monitoring", get(snapshot))
        .route("/api/monitoring/nodes", get(nodes))
}
fn quantity(value: &str, cpu: bool) -> Option<f64> {
    let suffixes: &[(&str, f64)] = if cpu {
        &[("n", 1e-9), ("u", 1e-6), ("m", 1e-3)]
    } else {
        &[
            ("Ki", 1024.),
            ("Mi", 1048576.),
            ("Gi", 1073741824.),
            ("Ti", 1099511627776.),
            ("K", 1e3),
            ("M", 1e6),
            ("G", 1e9),
            ("k", 1e3),
        ]
    };
    let (number, factor) = suffixes
        .iter()
        .find_map(|(s, f)| value.strip_suffix(s).map(|n| (n, *f)))
        .unwrap_or((value, 1.));
    let n = number.parse::<f64>().ok()? * factor;
    (n.is_finite() && n >= 0.).then_some(n)
}
fn usage(item: &Value) -> Option<(f64, f64)> {
    let containers = item.get("containers")?.as_array()?;
    if containers.is_empty() {
        return None;
    }
    let mut cpu = 0.;
    let mut memory = 0.;
    for c in containers {
        cpu += quantity(c.get("usage")?.get("cpu")?.as_str()?, true)?;
        memory += quantity(c.get("usage")?.get("memory")?.as_str()?, false)?;
    }
    Some((cpu, memory))
}
fn metrics_error(error: &kube::Error) -> (&'static str, &'static str) {
    match error {
        kube::Error::Api(e) if e.code == 403 => (
            "forbidden",
            "KubeWeasel cannot read pod metrics. Grant get/list on pods in metrics.k8s.io for this namespace.",
        ),
        kube::Error::Api(e) if e.code == 404 => (
            "missing",
            "Metrics Server is not installed or its metrics API is not registered. Install Metrics Server to see live CPU and memory usage.",
        ),
        _ => (
            "unavailable",
            "The Kubernetes metrics API is temporarily unavailable. Check Metrics Server readiness and kubelet connectivity.",
        ),
    }
}
async fn snapshot(State(s): State<AppState>) -> Result<Json<Value>> {
    let sampled_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let Some(client) = s.client.clone() else {
        return Ok(Json(
            json!({"mode":"demo","namespace":s.namespace,"sampled_at":sampled_at,"metrics_state":"demo","message":"Live usage requires a cluster connection. Demo mode does not simulate CPU or memory measurements.","pods":[],"cpu_cores":null,"memory_bytes":null,"measured_pods":0,"total_pods":0,"ready_pods":0,"restarts":0}),
        ));
    };
    let pods: Api<Pod> = Api::namespaced(client.clone(), &s.namespace);
    let list = pods.list(&ListParams::default().labels(LABEL)).await?;
    let request = Request::get(format!(
        "/apis/metrics.k8s.io/v1beta1/namespaces/{}/pods",
        s.namespace
    ))
    .body(Vec::new())
    .map_err(|_| invalid("Invalid metrics request"))?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        client.request::<Value>(request),
    )
    .await;
    let (state, message, items) = match result {
        Ok(Ok(v)) => (
            "available",
            "Live readings from Kubernetes Metrics Server.",
            v.get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        Ok(Err(e)) => {
            let (state, message) = metrics_error(&e);
            (state, message, Vec::new())
        }
        Err(_) => (
            "unavailable",
            "The metrics API timed out. Check Metrics Server health.",
            Vec::new(),
        ),
    };
    let mut total_cpu = 0.;
    let mut total_memory = 0.;
    let mut measured = 0;
    let mut ready_count = 0;
    let mut restarts = 0;
    let mut rows = Vec::new();
    for pod in list.items {
        let name = pod.name_any();
        let statuses = pod
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref());
        let ready = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        let restart_count = statuses
            .map(|s| s.iter().map(|c| c.restart_count as i64).sum::<i64>())
            .unwrap_or(0);
        restarts += restart_count;
        if ready {
            ready_count += 1;
        }
        let item = items
            .iter()
            .find(|i| i.pointer("/metadata/name").and_then(Value::as_str) == Some(&name));
        let reading = item.and_then(usage);
        if let Some((c, m)) = reading {
            measured += 1;
            total_cpu += c;
            total_memory += m;
        }
        rows.push(json!({"name":name,"app":pod.labels().get("app.kubernetes.io/name"),"phase":pod.status.as_ref().and_then(|s|s.phase.as_deref()).unwrap_or("Unknown"),"ready":ready,"restarts":restart_count,"cpu_cores":reading.map(|r|r.0),"memory_bytes":reading.map(|r|r.1),"metrics_timestamp":item.and_then(|i|i.get("timestamp")),"window":item.and_then(|i|i.get("window"))}));
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({"mode":"live","namespace":s.namespace,"sampled_at":sampled_at,"metrics_state":state,"message":message,"pods":rows,"cpu_cores":if measured>0{Some(total_cpu)}else{None},"memory_bytes":if measured>0{Some(total_memory)}else{None},"measured_pods":measured,"total_pods":rows.len(),"ready_pods":ready_count,"restarts":restarts}),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_kubernetes_usage() {
        assert_eq!(quantity("250m", true), Some(0.25));
        assert_eq!(quantity("100000000n", true), Some(0.1));
        assert!((quantity("100u", true).unwrap() - 0.0001).abs() < 1e-12);
        assert_eq!(quantity("64Mi", false), Some(67108864.));
        assert_eq!(quantity("-1", true), None);
        assert_eq!(quantity("NaN", true), None);
        assert_eq!(quantity("garbage", false), None);
    }
    #[test]
    fn aggregates_all_containers() {
        let item = json!({"containers":[{"usage":{"cpu":"100m","memory":"1Mi"}},{"usage":{"cpu":"200m","memory":"2Mi"}}]});
        let (c, m) = usage(&item).unwrap();
        assert!((c - 0.3).abs() < 1e-9);
        assert_eq!(m, 3145728.);
        assert!(usage(&json!({"containers":[{"usage":{"cpu":"0"}}]})).is_none());
    }
    #[test]
    fn distinguishes_permissions_and_missing_provider() {
        let status: kube::core::Status = serde_json::from_value(
            json!({"status":"Failure","message":"missing","reason":"NotFound","code":404}),
        )
        .unwrap();
        let missing = kube::Error::Api(Box::new(status));
        assert_eq!(metrics_error(&missing).0, "missing");
    }
}

fn resource(map: Option<&Value>, key: &str) -> f64 {
    map.and_then(|m| m.get(key))
        .and_then(Value::as_str)
        .and_then(|s| quantity(s, key == "cpu"))
        .unwrap_or(0.)
}
// Effective scheduling requests include restartable init sidecars, the largest
// sequential init stage, pod-level overrides, and pod overhead.
fn pod_request(spec: &Value, key: &str) -> f64 {
    let container_value = |c: &Value| resource(c.pointer("/resources/requests"), key);
    let app = spec["containers"]
        .as_array()
        .map(|cs| cs.iter().map(container_value).sum::<f64>())
        .unwrap_or(0.);
    let mut sidecars = 0.;
    let mut init: f64 = 0.;
    if let Some(containers) = spec["initContainers"].as_array() {
        for c in containers {
            let v = container_value(c);
            if c["restartPolicy"] == "Always" {
                sidecars += v;
                init = init.max(sidecars);
            } else {
                init = init.max(sidecars + v);
            }
        }
    }
    let mut total = (app + sidecars).max(init);
    if let Some(v) = spec
        .pointer("/resources/requests")
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .and_then(|v| quantity(v, key == "cpu"))
    {
        total = v;
    }
    total + resource(spec.get("overhead"), key)
}
fn configured_limits(spec: &Value, key: &str) -> (f64, usize) {
    let mut total = 0.;
    let mut unbounded = 0;
    for field in ["containers", "initContainers"] {
        if let Some(cs) = spec[field].as_array() {
            for c in cs {
                if field == "initContainers" && c["restartPolicy"] != "Always" {
                    continue;
                }
                match c
                    .pointer("/resources/limits")
                    .and_then(|m| m.get(key))
                    .and_then(Value::as_str)
                    .and_then(|v| quantity(v, key == "cpu"))
                {
                    Some(v) => total += v,
                    None => unbounded += 1,
                }
            }
        }
    }
    if let Some(v) = spec
        .pointer("/resources/limits")
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .and_then(|v| quantity(v, key == "cpu"))
    {
        return (v, 0);
    }
    (total, unbounded)
}
fn percent(used: Option<f64>, capacity: Option<f64>) -> Option<f64> {
    used.zip(capacity)
        .and_then(|(u, c)| (c > 0.).then_some(u / c * 100.))
}
async fn nodes(State(s): State<AppState>) -> Result<Json<Value>> {
    use k8s_openapi::api::core::v1::Node;
    let sampled_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let Some(client) = s.client.clone() else {
        return Ok(Json(
            json!({"mode":"demo","sampled_at":sampled_at,"nodes":[],"message":"Node monitoring requires a live cluster; no host usage is simulated."}),
        ));
    };
    let node_api: Api<Node> = Api::all(client.clone());
    let list = match node_api.list(&ListParams::default()).await {
        Ok(v) => v,
        Err(kube::Error::Api(e)) if e.code == 403 => {
            return Ok(Json(
                json!({"mode":"live","sampled_at":sampled_at,"nodes":[],"message":"Node monitoring requires cluster-scoped get/list access to nodes. Enable nodeMonitoring in the Helm chart or ask your administrator.","state":"forbidden"}),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let request = Request::get("/apis/metrics.k8s.io/v1beta1/nodes")
        .body(Vec::new())
        .map_err(|_| invalid("Invalid metrics request"))?;
    let metrics = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        client.request::<Value>(request),
    )
    .await;
    let (metrics_state, message, items) = match metrics {
        Ok(Ok(v)) => (
            "available",
            "Node CPU and memory usage from Metrics Server.",
            v["items"].as_array().cloned().unwrap_or_default(),
        ),
        Ok(Err(kube::Error::Api(e))) if e.code == 403 => (
            "forbidden",
            "Cannot read node metrics. Grant get/list on nodes in metrics.k8s.io.",
            vec![],
        ),
        _ => (
            "unavailable",
            "Node metrics unavailable. Check Metrics Server readiness and node-metrics permissions.",
            vec![],
        ),
    };
    let pods: Api<Pod> = Api::all(client);
    let allocations = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        pods.list(&ListParams::default()),
    )
    .await;
    let mut reservations: BTreeMap<String, Value> = BTreeMap::new();
    let allocations_available = matches!(&allocations, Ok(Ok(_)));
    if let Ok(Ok(pods)) = allocations {
        for pod in pods.items {
            if pod
                .status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .is_some_and(|p| p == "Succeeded" || p == "Failed")
            {
                continue;
            }
            let Some(spec) = pod.spec else {
                continue;
            };
            let Some(node) = spec.node_name.clone() else {
                continue;
            };
            let spec = serde_json::to_value(spec).map_err(|_| invalid("Invalid pod spec"))?;
            let row=reservations.entry(node).or_insert(json!({"cpu_requests":0.,"memory_requests":0.,"cpu_limits":0.,"memory_limits":0.,"cpu_unbounded":0,"memory_unbounded":0,"pods":0}));
            for (key, prefix) in [("cpu", "cpu"), ("memory", "memory")] {
                let req = format!("{prefix}_requests");
                row[&req] = json!(row[&req].as_f64().unwrap_or(0.) + pod_request(&spec, key));
                let (limits, unbounded) = configured_limits(&spec, key);
                let limit = format!("{prefix}_limits");
                row[&limit] = json!(row[&limit].as_f64().unwrap_or(0.) + limits);
                let count = format!("{prefix}_unbounded");
                row[&count] = json!(row[&count].as_u64().unwrap_or(0) + unbounded as u64);
            }
            row["pods"] = json!(row["pods"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut rows = Vec::new();
    for node in list.items {
        let name = node.name_any();
        let ready = node
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"));
        let alloc = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
        let capacity = |key: &str| {
            alloc
                .and_then(|m| m.get(key))
                .and_then(|q| quantity(&q.0, key == "cpu"))
        };
        let item = items
            .iter()
            .find(|i| i.pointer("/metadata/name").and_then(Value::as_str) == Some(&name));
        let cpu = item
            .and_then(|i| i.pointer("/usage/cpu"))
            .and_then(Value::as_str)
            .and_then(|q| quantity(q, true));
        let memory = item
            .and_then(|i| i.pointer("/usage/memory"))
            .and_then(Value::as_str)
            .and_then(|q| quantity(q, false));
        let reservation = if allocations_available {
            Some(reservations.remove(&name).unwrap_or(json!({"cpu_requests":0.,"memory_requests":0.,"cpu_limits":0.,"memory_limits":0.,"cpu_unbounded":0,"memory_unbounded":0,"pods":0})))
        } else {
            None
        };
        rows.push(json!({"name":name,"ready":ready,"cpu_cores":cpu,"memory_bytes":memory,"allocatable_cpu":capacity("cpu"),"allocatable_memory":capacity("memory"),"cpu_percent":percent(cpu,capacity("cpu")),"memory_percent":percent(memory,capacity("memory")),"metrics_timestamp":item.and_then(|i|i.get("timestamp")),"allocation":reservation}));
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({"mode":"live","sampled_at":sampled_at,"nodes":rows,"state":metrics_state,"message":message,"allocations_available":allocations_available}),
    ))
}
#[cfg(test)]
mod node_tests {
    use super::*;
    #[test]
    fn request_includes_init_overhead_and_sidecars() {
        let spec = json!({"containers":[{"resources":{"requests":{"cpu":"500m"}}}],"initContainers":[{"resources":{"requests":{"cpu":"100m"}},"restartPolicy":"Always"},{"resources":{"requests":{"cpu":"1"}}}],"overhead":{"cpu":"50m"}});
        assert!((pod_request(&spec, "cpu") - 1.15).abs() < 1e-9);
    }
    #[test]
    fn missing_limits_are_explicit() {
        let spec = json!({"containers":[{}, {"resources":{"limits":{"cpu":"1"}}}]});
        assert_eq!(configured_limits(&spec, "cpu"), (1., 1));
    }
    #[test]
    fn percentage_does_not_hide_overcommit() {
        assert_eq!(percent(Some(3.), Some(2.)), Some(150.));
        assert_eq!(percent(None, Some(2.)), None);
        assert_eq!(percent(Some(1.), Some(0.)), None);
    }
}
