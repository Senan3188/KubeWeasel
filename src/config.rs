//! Runtime configuration is read once at process startup.
pub(crate) struct Config {
    pub(crate) address: String,
    pub(crate) auth_db: String,
    pub(crate) public_origin: Option<String>,
    pub(crate) private_origin: Option<String>,
    pub(crate) namespace: String,
    pub(crate) demo: bool,
    pub(crate) healthcheck: bool,
}
impl Config {
    pub(crate) fn from_env() -> Self {
        let args: Vec<_> = std::env::args().collect();
        Self {
            address: std::env::var("KUBEWEASEL_BIND").unwrap_or_else(|_| "127.0.0.1:7878".into()),
            auth_db: std::env::var("KUBEWEASEL_AUTH_DB").unwrap_or_else(|_| "kubeweasel.db".into()),
            public_origin: std::env::var("KUBEWEASEL_PUBLIC_ORIGIN").ok(),
            private_origin: std::env::var("KUBEWEASEL_PRIVATE_ORIGIN").ok(),
            namespace: std::env::var("KUBEWEASEL_NAMESPACE").unwrap_or_else(|_| "default".into()),
            demo: args.iter().any(|a| a == "--demo"),
            healthcheck: args.iter().any(|a| a == "--healthcheck"),
        }
    }
}
