use crate::error::{Error, Result};
use axum::http::StatusCode;
use rand_core::{OsRng, RngCore};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
};
fn failure() -> Error {
    Error(StatusCode::INTERNAL_SERVER_ERROR,"Cluster credentials cannot be decrypted. Restore the matching cluster key file; credentials were not replaced.".into())
}
pub(super) struct Cipher(LessSafeKey);
impl Cipher {
    pub(super) fn open(path: Option<&Path>, has_credentials: bool) -> Result<Self> {
        let mut key = [0u8; 32];
        if let Some(path) = path {
            match OpenOptions::new().read(true).open(path) {
                Ok(mut f) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if f.metadata().map_err(|_| failure())?.permissions().mode() & 0o077 != 0 {
                            return Err(Error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Cluster key file must be private (chmod 600).".into(),
                            ));
                        }
                    }
                    f.read_exact(&mut key).map_err(|_| failure())?;
                    let mut extra = [0];
                    if f.read(&mut extra).map_err(|_| failure())? != 0 {
                        return Err(failure());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !has_credentials => {
                    OsRng.fill_bytes(&mut key);
                    let mut options = OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    let mut file = options.open(path).map_err(|_| failure())?;
                    file.write_all(&key)
                        .and_then(|_| file.sync_all())
                        .map_err(|_| failure())?;
                }
                Err(_) => return Err(failure()),
            }
        } else {
            OsRng.fill_bytes(&mut key);
        }
        Ok(Self(LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &key).map_err(|_| failure())?,
        )))
    }
    pub(super) fn encrypt(&self, id: &str, plain: &str) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let mut bytes = plain.as_bytes().to_vec();
        self.0
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(id.as_bytes()),
                &mut bytes,
            )
            .map_err(|_| failure())?;
        let mut result = nonce.to_vec();
        result.extend(bytes);
        Ok(result)
    }
    pub(super) fn decrypt(&self, id: &str, encrypted: &[u8]) -> Result<String> {
        if encrypted.len() < 28 {
            return Err(failure());
        }
        let nonce: [u8; 12] = encrypted[..12].try_into().map_err(|_| failure())?;
        let mut data = encrypted[12..].to_vec();
        let plain = self
            .0
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(id.as_bytes()),
                &mut data,
            )
            .map_err(|_| failure())?;
        String::from_utf8(plain.to_vec()).map_err(|_| failure())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authenticated_encryption_binds_cluster_identity() {
        let cipher = Cipher::open(None, false).unwrap();
        let encrypted = cipher.encrypt("one", "private-kubeconfig").unwrap();
        assert!(!encrypted.windows(7).any(|w| w == b"private"));
        assert_eq!(
            cipher.decrypt("one", &encrypted).unwrap(),
            "private-kubeconfig"
        );
        assert!(cipher.decrypt("two", &encrypted).is_err());
        let mut corrupt = encrypted;
        corrupt[15] ^= 1;
        assert!(cipher.decrypt("one", &corrupt).is_err());
    }
}
