use super::crypto::Cipher;
use crate::error::{Error, Result};
use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{path::Path, sync::Mutex};
#[derive(Clone, Serialize)]
pub(crate) struct Cluster {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub server: String,
    pub local: bool,
    pub context: String,
}
pub(super) struct Store {
    db: Mutex<Connection>,
    cipher: Cipher,
}
fn failure() -> Error {
    Error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Cluster connection storage is unavailable.".into(),
    )
}
impl Store {
    pub(super) fn open(path: &str, key_path: Option<&Path>, namespace: &str) -> Result<Self> {
        let db = Connection::open(path).map_err(|_| failure())?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| failure())?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS cluster_connections(id TEXT PRIMARY KEY,name TEXT NOT NULL,namespace TEXT NOT NULL,server TEXT NOT NULL,context TEXT NOT NULL,credentials BLOB);").map_err(|_|failure())?;
        let encrypted: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM cluster_connections WHERE credentials IS NOT NULL)",
                [],
                |r| r.get(0),
            )
            .map_err(|_| failure())?;
        let cipher = Cipher::open(key_path, encrypted)?;
        db.execute("INSERT INTO cluster_connections(id,name,namespace,server,context) VALUES('local','Local cluster',?1,'Current installation','') ON CONFLICT(id) DO UPDATE SET namespace=excluded.namespace",[namespace]).map_err(|_|failure())?;
        let store = Self {
            db: Mutex::new(db),
            cipher,
        };
        // Validate the key on startup; never silently replace inaccessible credentials.
        for cluster in store.list()? {
            if !cluster.local {
                let _ = store.credentials(&cluster.id)?;
            }
        }
        Ok(store)
    }
    pub(super) fn list(&self) -> Result<Vec<Cluster>> {
        let db = self.db.lock().map_err(|_| failure())?;
        let mut q=db.prepare("SELECT id,name,namespace,server,context FROM cluster_connections ORDER BY CASE WHEN id='local' THEN 0 ELSE 1 END,name COLLATE NOCASE").map_err(|_|failure())?;
        q.query_map([], |r| {
            let id: String = r.get(0)?;
            Ok(Cluster {
                local: id == "local",
                id,
                name: r.get(1)?,
                namespace: r.get(2)?,
                server: r.get(3)?,
                context: r.get(4)?,
            })
        })
        .map_err(|_| failure())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| failure())
    }
    pub(super) fn get(&self, id: &str) -> Result<Cluster> {
        self.list()?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| Error::not_found("Cluster connection not found."))
    }
    pub(super) fn credentials(&self, id: &str) -> Result<String> {
        let data: Option<Vec<u8>> = self
            .db
            .lock()
            .map_err(|_| failure())?
            .query_row(
                "SELECT credentials FROM cluster_connections WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| failure())?
            .flatten();
        self.cipher.decrypt(id, &data.ok_or_else(failure)?)
    }
    pub(super) fn save(&self, cluster: &Cluster, raw: Option<&str>) -> Result<()> {
        let encrypted = raw
            .map(|raw| self.cipher.encrypt(&cluster.id, raw))
            .transpose()?;
        let db = self.db.lock().map_err(|_| failure())?;
        if raw.is_some() {
            db.execute("INSERT INTO cluster_connections(id,name,namespace,server,context,credentials) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,server=excluded.server,context=excluded.context,credentials=excluded.credentials",params![cluster.id,cluster.name,cluster.namespace,cluster.server,cluster.context,encrypted]).map_err(|_|failure())?;
        } else {
            db.execute(
                "UPDATE cluster_connections SET name=?1 WHERE id=?2",
                params![cluster.name, cluster.id],
            )
            .map_err(|_| failure())?;
        }
        Ok(())
    }
    pub(super) fn remove(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .map_err(|_| failure())?
            .execute("DELETE FROM cluster_connections WHERE id=?1", [id])
            .map_err(|_| failure())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_encrypted_persisted_and_require_the_matching_key() {
        let dir = std::env::temp_dir().join(format!("kw-cluster-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("account.db");
        let key = dir.join("account.cluster-key");
        let path = db_path.to_str().unwrap();
        let cluster = Cluster {
            id: "remote".into(),
            name: "Remote".into(),
            namespace: "apps".into(),
            server: "https://cluster.test".into(),
            local: false,
            context: "test".into(),
        };
        let secret = "private-token-that-must-not-appear";
        let store = Store::open(path, Some(&key), "local-apps").unwrap();
        store.save(&cluster, Some(secret)).unwrap();
        assert_eq!(store.credentials("remote").unwrap(), secret);
        assert!(
            !serde_json::to_string(&store.list().unwrap())
                .unwrap()
                .contains(secret)
        );
        drop(store);
        assert!(
            !std::fs::read(&db_path)
                .unwrap()
                .windows(secret.len())
                .any(|w| w == secret.as_bytes())
        );
        let reopened = Store::open(path, Some(&key), "local-apps").unwrap();
        assert_eq!(reopened.credentials("remote").unwrap(), secret);
        drop(reopened);
        let original = std::fs::read(&key).unwrap();
        std::fs::write(&key, [42u8; 32]).unwrap();
        assert!(Store::open(path, Some(&key), "local-apps").is_err());
        std::fs::remove_file(&key).unwrap();
        assert!(Store::open(path, Some(&key), "local-apps").is_err());
        assert!(!key.exists());
        std::fs::write(&key, original).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let recovered = Store::open(path, Some(&key), "local-apps").unwrap();
        recovered.remove("remote").unwrap();
        assert!(recovered.get("remote").is_err());
        drop(recovered);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
