//! Local-filesystem implementation of [`crate::storage::Handler`] — what
//! `make dev-local` and `make local-mail` use instead of S3.
//!
//! Writes each object as a file under `MOCK_STORAGE_DIR` (default
//! `../local/mail-out/storage`, gitignored — relative to `api/`, where the
//! server/CLI run from), mirroring the bucket's own key structure as a
//! relative path. Presigned URLs are not real HTTP URLs — nothing serves
//! this directory over HTTP — they are a `file://` path a developer can open
//! directly, which is enough for local debugging and for `replyToTicket`'s
//! pending-key validation (which only inspects the key string, never
//! dereferences the URL).

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

use crate::storage;

pub const DIR_VAR: &str = "MOCK_STORAGE_DIR";
const DEFAULT_DIR: &str = "../local/mail-out/storage";

pub struct Storage {
    dir: PathBuf,
}

impl Storage {
    pub fn new() -> Self {
        Self::from_env()
    }

    /// An instance rooted at an explicit directory, bypassing [`DIR_VAR`] —
    /// what integration tests use to get an isolated temp directory per
    /// test rather than sharing `MOCK_STORAGE_DIR`/[`DEFAULT_DIR`] across a
    /// parallel `cargo test` run.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Honours [`DIR_VAR`]; falls back to [`DEFAULT_DIR`].
    pub fn from_env() -> Self {
        let dir = std::env::var_os(DIR_VAR)
            .map(PathBuf::from)
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR));
        Self { dir }
    }

    /// Resolve a bucket-relative key to a local path, refusing anything that
    /// could escape `dir` — every key this module ever receives is built by
    /// `inbound::attachments`, never taken verbatim from an external input,
    /// but this is cheap insurance against a future caller that isn't as
    /// careful.
    fn path_for(&self, key: &str) -> Result<PathBuf> {
        if key.contains("..") {
            return Err(anyhow!("refusing to write a key containing '..': {key:?}"));
        }
        Ok(self.dir.join(key))
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self::new()
    }
}

impl storage::Handler for Storage {
    async fn put_bytes(&self, key: &str, bytes: &[u8], _content_type: &str) -> Result<()> {
        let path = self.path_for(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.path_for(key)?;
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
    }

    async fn move_object(&self, from_key: &str, to_key: &str) -> Result<()> {
        let from = self.path_for(from_key)?;
        let to = self.path_for(to_key)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        Ok(())
    }

    async fn object_size(&self, key: &str) -> Result<u64> {
        let path = self.path_for(key)?;
        let meta =
            std::fs::metadata(&path).with_context(|| format!("stat-ing {}", path.display()))?;
        Ok(meta.len())
    }

    async fn presign_put(&self, key: &str, _content_type: &str) -> Result<String> {
        let path = self.path_for(key)?;
        Ok(format!("file://{}", path.display()))
    }

    /// The local stand-in for the public bucket is the same directory —
    /// `email-assets/…` keys don't collide with the mail bucket's.
    async fn presign_put_public_asset(
        &self,
        key: &str,
        _content_type: &str,
        _content_length: u64,
    ) -> Result<String> {
        let path = self.path_for(key)?;
        Ok(format!("file://{}", path.display()))
    }

    async fn presign_get(&self, key: &str) -> Result<String> {
        let path = self.path_for(key)?;
        Ok(format!("file://{}", path.display()))
    }

    /// Same shape as [`Self::presign_get`] — there's no real HTTP response
    /// here to attach a `Content-Disposition` header to, so `filename` is
    /// unused beyond validating it the same way the real implementation
    /// does (cheap insurance against a caller relying on this mock to catch
    /// a filename it never sanitised).
    async fn presign_get_download(&self, key: &str, filename: &str) -> Result<String> {
        let _ = storage::sanitize_download_filename(filename);
        let path = self.path_for(key)?;
        Ok(format!("file://{}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use storage::Handler as _;

    fn temp_storage() -> (Storage, PathBuf) {
        let dir = std::env::temp_dir().join(format!("toolbox-mockstorage-{}", nanoid::nanoid!(8)));
        (Storage { dir: dir.clone() }, dir)
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let (s, dir) = temp_storage();
        s.put_bytes("attachments/t1/m1/0/file.txt", b"hello", "text/plain")
            .await
            .unwrap();
        let read = s.get_bytes("attachments/t1/m1/0/file.txt").await.unwrap();
        assert_eq!(read, b"hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn move_object_renames_the_file() {
        let (s, dir) = temp_storage();
        s.put_bytes("pending/inst1/abc/file.txt", b"hi", "text/plain")
            .await
            .unwrap();
        s.move_object("pending/inst1/abc/file.txt", "attachments/t1/m1/0/file.txt")
            .await
            .unwrap();
        assert!(s.get_bytes("pending/inst1/abc/file.txt").await.is_err());
        assert_eq!(
            s.get_bytes("attachments/t1/m1/0/file.txt").await.unwrap(),
            b"hi"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn refuses_a_traversal_key() {
        let (s, _dir) = temp_storage();
        assert!(
            s.put_bytes("../../etc/passwd", b"x", "text/plain")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn object_size_reports_the_byte_count() {
        let (s, dir) = temp_storage();
        s.put_bytes(
            "attachments/t1/m1/0/f.bin",
            b"hello world",
            "application/octet-stream",
        )
        .await
        .unwrap();
        let size = s.object_size("attachments/t1/m1/0/f.bin").await.unwrap();
        assert_eq!(size, 11);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn presign_urls_are_file_urls() {
        let (s, _dir) = temp_storage();
        let put = s
            .presign_put("pending/inst1/x/f.txt", "text/plain")
            .await
            .unwrap();
        let get = s.presign_get("attachments/t1/m1/0/f.txt").await.unwrap();
        let download = s
            .presign_get_download("invoices/inst1/inv1/Invoice-008.pdf", "Invoice-008.pdf")
            .await
            .unwrap();
        assert!(put.starts_with("file://"), "{put}");
        assert!(get.starts_with("file://"), "{get}");
        assert!(download.starts_with("file://"), "{download}");
    }
}
