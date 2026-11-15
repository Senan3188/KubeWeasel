use crate::{auth, config::Config, git, health, history, http, state::AppState};
pub(crate) async fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env();
    if config.healthcheck {
        return health::check(&config).await;
    }
    let auth = auth::AuthState::open_with_private(
        &config.auth_db,
        &config.address,
        config.public_origin.clone(),
        config.private_origin.clone(),
    )?;
    let state = AppState::initialize(&config).await?;
    let key_path = std::env::var("KUBEWEASEL_CLUSTER_KEY_FILE")
        .unwrap_or_else(|_| format!("{}.cluster-key", config.auth_db));
    let manager = crate::clusters::Manager::open(
        state.clone(),
        if config.demo {
            ":memory:"
        } else {
            &config.auth_db
        },
        if config.demo {
            None
        } else {
            Some(std::path::Path::new(&key_path))
        },
    )?;
    manager.resume().await;
    let _git_worker = git::worker(state.clone());
    let _history_worker = history::worker(state.clone());
    let router = http::router_with_clusters(state, auth, manager);
    let listener = tokio::net::TcpListener::bind(&config.address).await?;
    println!(
        "KubeWeasel → http://{} ({})",
        config.address,
        if config.demo { "demo" } else { "live" }
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
