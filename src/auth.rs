use super::*;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    extract::{ConnectInfo, Request},
    middleware::Next,
    response::Response,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
const LIFETIME: i64 = 12 * 60 * 60;
const IDLE: i64 = 30 * 60;
#[derive(Clone)]
pub struct AuthState {
    db: Arc<Mutex<Connection>>,
    setup_hash: Arc<Mutex<Option<String>>>,
    origins: Vec<String>,
    hosts: Vec<String>,
    secure: bool,
    attempts: Arc<Mutex<BTreeMap<String, (i64, u32)>>>,
    gate: Arc<tokio::sync::Semaphore>,
}
fn internal() -> Error {
    Error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Authentication storage unavailable.".into(),
    )
}
fn unauthorized() -> Error {
    Error(StatusCode::UNAUTHORIZED, "Please sign in.".into())
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
fn csrf(token: &str) -> String {
    digest(&format!("csrf:{token}"))
}
fn session(headers: &HeaderMap, secure: bool) -> Option<String> {
    let key = if secure {
        "__Host-kubeweasel-session"
    } else {
        "kubeweasel-session"
    };
    let s = headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix(&format!("{key}=")))?;
    (s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())).then(|| s.to_string())
}
impl AuthState {
    pub fn open(
        path: &str,
        address: &str,
        origin: Option<String>,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let bind: SocketAddr = address.parse()?;
        let mut origins = Vec::new();
        let mut hosts = Vec::new();
        let secure = origin.is_some();
        if let Some(origin) = origin {
            let url = reqwest::Url::parse(&origin)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("KUBEWEASEL_PUBLIC_ORIGIN must be an HTTPS origin, such as https://deploy.example.com".into());
            }
            let origin = url.origin().ascii_serialization();
            hosts.push(origin.trim_start_matches("https://").to_string());
            origins.push(origin);
        } else if !bind.ip().is_loopback() {
            return Err("Non-loopback binding requires KUBEWEASEL_PUBLIC_ORIGIN=https://your-dashboard-host. Configure HTTPS on your reverse proxy.".into());
        }
        if bind.ip().is_loopback() {
            for host in [bind.to_string(), format!("localhost:{}", bind.port())] {
                hosts.push(host.clone());
                origins.push(format!("http://{host}"));
            }
        }
        #[cfg(unix)]
        if path != ":memory:" {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let _file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS owner(id INTEGER PRIMARY KEY CHECK(id=1),email TEXT NOT NULL,password_hash TEXT NOT NULL); CREATE TABLE IF NOT EXISTS sessions(hash TEXT PRIMARY KEY,expires INTEGER NOT NULL,last_seen INTEGER NOT NULL); PRAGMA user_version=1;")?;
        let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM owner)", [], |r| r.get(0))?;
        let setup_hash = if exists {
            None
        } else {
            let value = token();
            eprintln!(
                "KubeWeasel first-run setup token: {value}\nOpen the dashboard and enter this token to create your owner account. It changes on restart until setup is complete."
            );
            Some(digest(&value))
        };
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            setup_hash: Arc::new(Mutex::new(setup_hash)),
            origins,
            hosts,
            secure,
            attempts: Arc::new(Mutex::new(BTreeMap::new())),
            gate: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    fn owner(&self) -> Result<Option<(String, String)>> {
        self.db
            .lock()
            .map_err(|_| internal())?
            .query_row(
                "SELECT email,password_hash FROM owner WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| internal())
    }
    fn valid_session(&self, headers: &HeaderMap) -> Result<Option<String>> {
        let Some(raw) = session(headers, self.secure) else {
            return Ok(None);
        };
        let db = self.db.lock().map_err(|_| internal())?;
        let time = now();
        let valid:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE hash=?1 AND expires>?2 AND last_seen>?3)",params![digest(&raw),time,time-IDLE],|r|r.get(0)).map_err(|_|internal())?;
        if !valid {
            return Ok(None);
        }
        db.execute(
            "UPDATE sessions SET last_seen=?1 WHERE hash=?2",
            params![time, digest(&raw)],
        )
        .map_err(|_| internal())?;
        Ok(Some(raw))
    }
    fn rate_limit(&self, key: &str) -> Result<()> {
        let mut attempts = self.attempts.lock().map_err(|_| internal())?;
        let time = now();
        attempts.retain(|_, (start, _)| time - *start < 900);
        if attempts.len() >= 4096 && !attempts.contains_key(key) {
            return Err(Error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in attempts. Try again later.".into(),
            ));
        }
        let entry = attempts.entry(key.into()).or_insert((time, 0));
        if entry.1 >= 10 {
            return Err(Error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in attempts. Wait 15 minutes.".into(),
            ));
        }
        entry.1 += 1;
        Ok(())
    }
    fn set_cookie(&self, value: &str, age: i64) -> String {
        format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            if self.secure {
                "__Host-kubeweasel-session"
            } else {
                "kubeweasel-session"
            },
            if self.secure { "; Secure" } else { "" }
        )
    }
    fn new_session(&self, headers: &HeaderMap) -> Result<Response> {
        let raw = token();
        let time = now();
        let mut db = self.db.lock().map_err(|_| internal())?;
        let tx = db.transaction().map_err(|_| internal())?;
        tx.execute(
            "DELETE FROM sessions WHERE expires<=?1 OR last_seen<=?2",
            params![time, time - IDLE],
        )
        .map_err(|_| internal())?;
        if let Some(old) = session(headers, self.secure) {
            tx.execute("DELETE FROM sessions WHERE hash=?1", [digest(&old)])
                .map_err(|_| internal())?;
        }
        tx.execute(
            "INSERT INTO sessions(hash,expires,last_seen) VALUES(?1,?2,?3)",
            params![digest(&raw), time + LIFETIME, time],
        )
        .map_err(|_| internal())?;
        tx.commit().map_err(|_| internal())?;
        let mut response = Json(json!({"ok":true,"csrf":csrf(&raw)})).into_response();
        response.headers_mut().insert(
            "set-cookie",
            self.set_cookie(&raw, LIFETIME)
                .parse()
                .map_err(|_| internal())?,
        );
        Ok(response)
    }
}
pub fn routes(state: AuthState) -> Router<AppState> {
    Router::new()
        .route("/api/auth/status", get(status))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/logout-all", post(logout_all))
        .with_state(state)
}
// The UI is a public shell; all application API data and mutations require a session.
pub async fn guard(State(s): State<AuthState>, request: Request, next: Next) -> Response {
    let checked = (|| -> Result<()> {
        let headers = request.headers();
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !s.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "Unrecognized dashboard host.".into(),
            ));
        }
        let write = !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
        );
        if write {
            mutation(headers)?;
            if let Some(origin) = headers.get("origin") {
                let origin = origin.to_str().unwrap_or("");
                if !s.origins.iter().any(|o| o == origin) {
                    return Err(Error(
                        StatusCode::FORBIDDEN,
                        "Cross-origin request rejected.".into(),
                    ));
                }
            }
        }
        let path = request.uri().path();
        let public = matches!(
            path,
            "/api/auth/status" | "/api/auth/login" | "/api/auth/setup"
        );
        if path.starts_with("/api/") && !public {
            let raw = s.valid_session(headers)?.ok_or_else(unauthorized)?;
            if write
                && !headers
                    .get("x-kubeweasel-csrf")
                    .and_then(|h| h.to_str().ok())
                    .is_some_and(|h| equal(h, &csrf(&raw)))
            {
                return Err(Error(
                    StatusCode::FORBIDDEN,
                    "Invalid session request token. Reload and sign in again.".into(),
                ));
            }
        }
        Ok(())
    })();
    let mut response = match checked {
        Ok(()) => next.run(request).await,
        Err(e) => e.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("referrer-policy", "same-origin".parse().unwrap());
    if s.secure {
        headers.insert(
            "strict-transport-security",
            "max-age=31536000".parse().unwrap(),
        );
    }
    response
}
async fn status(State(s): State<AuthState>, headers: HeaderMap) -> Result<Json<Value>> {
    let raw = s.valid_session(&headers)?;
    let owner = s.owner()?;
    Ok(Json(
        json!({"setup_required":owner.is_none(),"authenticated":raw.is_some(),"email":if raw.is_some(){owner.map(|o|o.0)}else{None},"csrf":raw.as_ref().map(|r|csrf(r))}),
    ))
}
#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
    #[serde(default)]
    setup_token: String,
}
fn credentials(c: &Credentials) -> Result<()> {
    if c.email.len() > 254
        || !c.email.contains('@')
        || c.email.chars().any(char::is_whitespace)
        || c.password.len() < 12
        || c.password.len() > 1024
    {
        return Err(invalid(
            "Enter an email address and a password of 12–1024 characters.",
        ));
    }
    Ok(())
}
async fn setup(
    State(s): State<AuthState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(c): Json<Credentials>,
) -> Result<Response> {
    s.rate_limit(&peer.ip().to_string())?;
    if s.owner()?.is_some() {
        return Err(Error(
            StatusCode::CONFLICT,
            "Owner already configured. Please sign in.".into(),
        ));
    }
    credentials(&c)?;
    let matches = s
        .setup_hash
        .lock()
        .map_err(|_| internal())?
        .as_ref()
        .is_some_and(|hash| equal(hash, &digest(&c.setup_token)));
    if !matches {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Invalid setup token. Use the token printed when KubeWeasel started.".into(),
        ));
    }
    let _permit = s.gate.clone().try_acquire_owned().map_err(|_| {
        Error(
            StatusCode::TOO_MANY_REQUESTS,
            "Sign-in is busy. Try again shortly.".into(),
        )
    })?;
    let password = c.password;
    let hash = tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|h| h.to_string())
    })
    .await
    .map_err(|_| internal())?
    .map_err(|_| internal())?;
    s.db.lock()
        .map_err(|_| internal())?
        .execute(
            "INSERT INTO owner(id,email,password_hash) VALUES(1,?1,?2)",
            params![c.email.trim().to_lowercase(), hash],
        )
        .map_err(|_| {
            Error(
                StatusCode::CONFLICT,
                "Owner already configured. Please sign in.".into(),
            )
        })?;
    *s.setup_hash.lock().map_err(|_| internal())? = None;
    s.new_session(&headers)
}
async fn login(
    State(s): State<AuthState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(c): Json<Credentials>,
) -> Result<Response> {
    s.rate_limit(&peer.ip().to_string())?;
    if c.password.len() > 1024 || c.email.len() > 254 {
        return Err(unauthorized());
    }
    let owner = s
        .owner()?
        .ok_or_else(|| invalid("Complete owner setup first."))?;
    let same_email = equal(&digest(&c.email.trim().to_lowercase()), &digest(&owner.0));
    let _permit = s.gate.clone().try_acquire_owned().map_err(|_| {
        Error(
            StatusCode::TOO_MANY_REQUESTS,
            "Sign-in is busy. Try again shortly.".into(),
        )
    })?;
    let verified = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&owner.1).is_ok_and(|h| {
            Argon2::default()
                .verify_password(c.password.as_bytes(), &h)
                .is_ok()
        })
    })
    .await
    .map_err(|_| internal())?;
    if !same_email || !verified {
        return Err(Error(
            StatusCode::UNAUTHORIZED,
            "Email or password is incorrect.".into(),
        ));
    }
    s.attempts
        .lock()
        .map_err(|_| internal())?
        .remove(&peer.ip().to_string());
    s.new_session(&headers)
}
async fn logout(State(s): State<AuthState>, headers: HeaderMap) -> Result<Response> {
    if let Some(raw) = session(&headers, s.secure) {
        s.db.lock()
            .map_err(|_| internal())?
            .execute("DELETE FROM sessions WHERE hash=?1", [digest(&raw)])
            .map_err(|_| internal())?;
    }
    let mut response = Json(json!({"ok":true})).into_response();
    response.headers_mut().insert(
        "set-cookie",
        s.set_cookie("", 0).parse().map_err(|_| internal())?,
    );
    Ok(response)
}
async fn logout_all(State(s): State<AuthState>) -> Result<Response> {
    s.db.lock()
        .map_err(|_| internal())?
        .execute("DELETE FROM sessions", [])
        .map_err(|_| internal())?;
    let mut response = Json(json!({"ok":true})).into_response();
    response.headers_mut().insert(
        "set-cookie",
        s.set_cookie("", 0).parse().map_err(|_| internal())?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request as HttpRequest,
    };
    use tower::ServiceExt;
    fn state() -> AuthState {
        let s = AuthState::open(":memory:", "127.0.0.1:7878", None).unwrap();
        *s.setup_hash.lock().unwrap() = Some(digest("test-setup-token"));
        s
    }
    fn router(s: AuthState) -> Router {
        let app = AppState {
            client: None,
            namespace: "test".into(),
            apps: Arc::new(RwLock::new(BTreeMap::new())),
            settings: settings::SettingsState::default(),
            projects: projects::Store::open(":memory:").unwrap(),
        };
        Router::new()
            .route(
                "/api/protected",
                get(|| async { "private" }).post(|| async { "changed" }),
            )
            .merge(routes(s.clone()))
            .layer(axum::middleware::from_fn_with_state(s, guard))
            .layer(axum::Extension(ConnectInfo(
                "127.0.0.1:10000".parse::<SocketAddr>().unwrap(),
            )))
            .with_state(app)
    }
    async fn request(
        r: &Router,
        path: &str,
        body: Option<Value>,
        cookie: Option<&str>,
        csrf_token: Option<&str>,
        origin: Option<&str>,
    ) -> Response {
        let mut req = HttpRequest::builder()
            .uri(path)
            .header("host", "127.0.0.1:7878");
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        if let Some(csrf) = csrf_token {
            req = req.header("x-kubeweasel-csrf", csrf);
        }
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        let req = if let Some(body) = body {
            req.method("POST")
                .header("content-type", "application/json")
                .header("x-kubeweasel-request", "dashboard")
                .body(Body::from(body.to_string()))
                .unwrap()
        } else {
            req.body(Body::empty()).unwrap()
        };
        r.clone().oneshot(req).await.unwrap()
    }
    async fn value(r: Response) -> Value {
        serde_json::from_slice(&to_bytes(r.into_body(), 65536).await.unwrap()).unwrap()
    }
    #[tokio::test]
    async fn owner_lifecycle_and_api_boundary() {
        let s = state();
        let r = router(s.clone());
        assert_eq!(
            request(&r, "/api/protected", None, None, None, None)
                .await
                .status(),
            401
        );
        let credentials = json!({"email":"owner@example.test","password":"a-test-password-long-enough","setup_token":"test-setup-token"});
        let bad = json!({"email":"owner@example.test","password":"a-test-password-long-enough","setup_token":"wrong"});
        assert_eq!(
            request(&r, "/api/auth/setup", Some(bad), None, None, None)
                .await
                .status(),
            403
        );
        let response = request(
            &r,
            "/api/auth/setup",
            Some(credentials.clone()),
            None,
            None,
            None,
        )
        .await;
        assert_eq!(response.status(), 200);
        let full = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(full.contains("HttpOnly") && full.contains("SameSite=Strict"));
        let cookie = full.split(';').next().unwrap();
        let csrf = value(response).await["csrf"].as_str().unwrap().to_string();
        assert_eq!(
            request(
                &r,
                "/api/auth/setup",
                Some(credentials.clone()),
                None,
                None,
                None
            )
            .await
            .status(),
            409
        );
        assert_eq!(
            request(&r, "/api/protected", None, Some(cookie), None, None)
                .await
                .status(),
            200
        );
        assert_eq!(
            request(
                &r,
                "/api/protected",
                Some(json!({})),
                Some(cookie),
                None,
                None
            )
            .await
            .status(),
            403
        );
        assert_eq!(
            request(
                &r,
                "/api/protected",
                Some(json!({})),
                Some(cookie),
                Some(&csrf),
                Some("https://evil.test")
            )
            .await
            .status(),
            403
        );
        assert_eq!(
            request(
                &r,
                "/api/protected",
                Some(json!({})),
                Some(cookie),
                Some(&csrf),
                None
            )
            .await
            .status(),
            200
        );
        assert_ne!(s.owner().unwrap().unwrap().1, "a-test-password-long-enough");
        assert!(s.owner().unwrap().unwrap().1.starts_with("$argon2id$"));
        assert_eq!(
            request(
                &r,
                "/api/auth/logout",
                Some(json!({})),
                Some(cookie),
                Some(&csrf),
                None
            )
            .await
            .status(),
            200
        );
        assert_eq!(
            request(&r, "/api/protected", None, Some(cookie), None, None)
                .await
                .status(),
            401
        );
        let response = request(&r, "/api/auth/login", Some(credentials), None, None, None).await;
        assert_eq!(response.status(), 200);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let csrf = value(response).await["csrf"].as_str().unwrap().to_string();
        assert_eq!(
            request(
                &r,
                "/api/auth/logout-all",
                Some(json!({})),
                Some(&cookie),
                Some(&csrf),
                None
            )
            .await
            .status(),
            200
        );
        assert_eq!(
            request(&r, "/api/protected", None, Some(&cookie), None, None)
                .await
                .status(),
            401
        );
    }
    #[test]
    fn public_binding_requires_https_origin() {
        assert!(AuthState::open(":memory:", "0.0.0.0:7878", None).is_err());
        assert!(
            AuthState::open(
                ":memory:",
                "0.0.0.0:7878",
                Some("http://example.test".into())
            )
            .is_err()
        );
        let s = AuthState::open(
            ":memory:",
            "0.0.0.0:7878",
            Some("https://deploy.example.test".into()),
        )
        .unwrap();
        assert!(s.set_cookie("example", 60).contains("; Secure"));
        assert!(s.set_cookie("example", 60).starts_with("__Host-"));
    }
    #[test]
    fn rate_limit_and_expiry() {
        let s = state();
        for _ in 0..10 {
            s.rate_limit("test")
                .unwrap_or_else(|_| panic!("early rate limit"));
        }
        assert!(s.rate_limit("test").is_err());
        let raw = token();
        s.db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions VALUES(?1,?2,?3)",
                params![digest(&raw), now() - 1, now()],
            )
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("kubeweasel-session={raw}").parse().unwrap(),
        );
        assert!(s.valid_session(&headers).unwrap().is_none());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    #[test]
    fn account_and_hashed_session_survive_reopen() {
        let path = std::env::temp_dir().join(format!("kubeweasel-auth-test-{}.db", token()));
        let path = path.to_str().unwrap();
        let raw = token();
        {
            let s = AuthState::open(path, "127.0.0.1:7878", None).unwrap();
            let db = s.db.lock().unwrap();
            db.execute(
                "INSERT INTO owner VALUES(1,?1,?2)",
                params!["test@example.test", "test-hash"],
            )
            .unwrap();
            db.execute(
                "INSERT INTO sessions VALUES(?1,?2,?3)",
                params![digest(&raw), now() + LIFETIME, now()],
            )
            .unwrap();
            let stored: String = db
                .query_row("SELECT hash FROM sessions", [], |r| r.get(0))
                .unwrap();
            assert_ne!(stored, raw);
        }
        let reopened = AuthState::open(path, "127.0.0.1:7878", None).unwrap();
        assert!(reopened.setup_hash.lock().unwrap().is_none());
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("kubeweasel-session={raw}").parse().unwrap(),
        );
        assert!(reopened.valid_session(&headers).unwrap().is_some());
        drop(reopened);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }
}
