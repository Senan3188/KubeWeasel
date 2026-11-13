use crate::{auth, config::Config, git, health, history, http, state::AppState};
pub(crate) async fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env();
    if config.healthcheck {
        return health::check(&config).await;
    }
    let auth = auth::AuthState::open(
        &config.auth_db,
        &config.address,
        config.public_origin.clone(),
    )?;
    let state = AppState::initialize(&config).await?;
    git::worker(state.clone());
    history::worker(state.clone());
    let router = http::router(state, auth);
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
