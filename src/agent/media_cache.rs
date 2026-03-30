use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::multimodal::parse_image_markers;
use crate::providers::traits::ChatMessage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CACHED_IMAGE_PREFIX: &str = "[CACHED_IMAGE:";

// ---------------------------------------------------------------------------
// MediaCache
// ---------------------------------------------------------------------------

/// Caches image data extracted from conversation messages before they are
/// pruned or compressed, allowing later re-read via filesystem markers.
pub struct MediaCache {
    cache_dir: PathBuf,
}

impl MediaCache {
    /// Create a new media cache rooted at `cache_dir`.
    /// The directory is created lazily on the first write.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Derive a default cache directory from the zeroclaw config root.
    /// Returns `~/.zeroclaw/media_cache/`.
    pub fn default_dir() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(PathBuf::from(home).join(".zeroclaw").join("media_cache"))
    }

    /// Scan `messages` for `[IMAGE:...]` markers, save referenced local files
    /// to the cache directory (deduplicating by content hash), and return
    /// `(original_ref, cached_path)` pairs for each saved image.
    pub async fn save_images(&self, messages: &[ChatMessage]) -> Result<Vec<(String, PathBuf)>> {
        let mut saved = Vec::new();

        for msg in messages {
            let (_, refs) = parse_image_markers(&msg.content);
            for image_ref in refs {
                if let Some(cached_path) = self.cache_single(&image_ref).await? {
                    saved.push((image_ref, cached_path));
                }
            }
        }

        Ok(saved)
    }

    /// Replace `[IMAGE:...]` markers in the given messages with compact
    /// `[Previously seen image cached at {path}]` references for any images
    /// that were successfully cached. Messages are modified in place.
    pub fn rewrite_markers(messages: &mut [ChatMessage], cached: &[(String, PathBuf)]) {
        if cached.is_empty() {
            return;
        }
        for msg in messages.iter_mut() {
            for (original_ref, cached_path) in cached {
                let marker = format!("[IMAGE:{original_ref}]");
                if msg.content.contains(&marker) {
                    let replacement = format!(
                        "[Previously seen image cached at {}]",
                        cached_path.display()
                    );
                    msg.content = msg.content.replace(&marker, &replacement);
                }
            }
        }
    }

    /// Create a re-read marker string for a cached image path.
    pub fn create_reread_marker(cached_path: &Path) -> String {
        format!("{CACHED_IMAGE_PREFIX}{}]", cached_path.display())
    }

    /// Delete cache entries older than `max_age`.
    pub async fn cleanup_old(&self, max_age: Duration) -> Result<usize> {
        if !self.cache_dir.exists() {
            return Ok(0);
        }

        let mut removed = 0usize;
        let now = std::time::SystemTime::now();
        let mut entries = tokio::fs::read_dir(&self.cache_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(now);
            if now.duration_since(modified).is_ok_and(|age| age > max_age)
                && tokio::fs::remove_file(entry.path()).await.is_ok()
            {
                removed += 1;
            }
        }

        Ok(removed)
    }

    // ── Internal ────────────────────────────────────────────────────

    /// Attempt to cache a single image reference. Returns `Some(cached_path)`
    /// on success, `None` if the reference cannot be cached (e.g. remote URL
    /// or data URI without payload).
    async fn cache_single(&self, image_ref: &str) -> Result<Option<PathBuf>> {
        let bytes = if image_ref.starts_with("data:") {
            decode_data_uri(image_ref)
        } else if image_ref.starts_with("http://") || image_ref.starts_with("https://") {
            // Remote URLs are not cached to disk — they can be re-fetched.
            return Ok(None);
        } else {
            // Local file path
            tokio::fs::read(image_ref).await.ok()
        };

        let Some(bytes) = bytes else {
            return Ok(None);
        };

        if bytes.is_empty() {
            return Ok(None);
        }

        let hash = sha256_hex(&bytes);
        let extension = guess_extension(&bytes);
        let filename = format!("{hash}{extension}");
        let cached_path = self.cache_dir.join(&filename);

        // Deduplicate: skip write if the file already exists.
        if cached_path.exists() {
            return Ok(Some(cached_path));
        }

        tokio::fs::create_dir_all(&self.cache_dir).await?;
        tokio::fs::write(&cached_path, &bytes).await?;

        Ok(Some(cached_path))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn guess_extension(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 8
        && bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
    {
        ".png"
    } else if bytes.len() >= 3 && bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ".jpg"
    } else if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        ".gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        ".webp"
    } else {
        ".bin"
    }
}

/// Decode a `data:...;base64,...` URI into raw bytes.
fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let comma_idx = uri.find(',')?;
    let payload = uri[comma_idx + 1..].trim();
    if payload.is_empty() {
        return None;
    }
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.decode(payload).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn sha256_hex_deterministic() {
        let hash1 = sha256_hex(b"hello");
        let hash2 = sha256_hex(b"hello");
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }

    #[test]
    fn sha256_hex_differs_for_different_input() {
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }

    #[test]
    fn guess_extension_png() {
        let png = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        assert_eq!(guess_extension(&png), ".png");
    }

    #[test]
    fn guess_extension_jpg() {
        let jpg = [0xff, 0xd8, 0xff, 0xe0];
        assert_eq!(guess_extension(&jpg), ".jpg");
    }

    #[test]
    fn guess_extension_unknown() {
        assert_eq!(guess_extension(b"random bytes"), ".bin");
    }

    #[test]
    fn decode_data_uri_valid() {
        // base64("hello") = "aGVsbG8="
        let uri = "data:image/png;base64,aGVsbG8=";
        let bytes = decode_data_uri(uri).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn decode_data_uri_empty_payload() {
        let uri = "data:image/png;base64,";
        assert!(decode_data_uri(uri).is_none());
    }

    #[test]
    fn decode_data_uri_no_comma() {
        assert!(decode_data_uri("data:image/png;base64").is_none());
    }

    #[test]
    fn create_reread_marker_format() {
        let path = PathBuf::from("/tmp/media_cache/abc123.png");
        let marker = MediaCache::create_reread_marker(&path);
        assert_eq!(marker, "[CACHED_IMAGE:/tmp/media_cache/abc123.png]");
    }

    #[tokio::test]
    async fn save_and_cleanup_cycle() {
        let temp = tempfile::tempdir().unwrap();
        let cache_dir = temp.path().join("media_cache");
        let cache = MediaCache::new(cache_dir.clone());

        // Create a local image file to reference.
        let img_path = temp.path().join("test.png");
        let png_bytes = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0x00];
        tokio::fs::write(&img_path, &png_bytes).await.unwrap();

        let messages = vec![msg(
            "user",
            &format!("Check this [IMAGE:{}]", img_path.display()),
        )];

        // Save images from messages
        let saved = cache.save_images(&messages).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1.exists());
        assert!(saved[0].1.extension().unwrap() == "png");

        // Verify content hash deduplication — saving again returns same path
        let saved2 = cache.save_images(&messages).await.unwrap();
        assert_eq!(saved[0].1, saved2[0].1);

        // Cleanup with zero max_age should remove the file
        let removed = cache.cleanup_old(Duration::from_secs(0)).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!saved[0].1.exists());
    }

    #[tokio::test]
    async fn save_images_skips_remote_urls() {
        let temp = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(temp.path().join("mc"));

        let messages = vec![msg("user", "Look at [IMAGE:https://example.com/photo.png]")];

        let saved = cache.save_images(&messages).await.unwrap();
        assert!(saved.is_empty());
    }

    #[tokio::test]
    async fn save_images_handles_data_uri() {
        use base64::Engine as _;

        let temp = tempfile::tempdir().unwrap();
        let cache = MediaCache::new(temp.path().join("mc"));

        // base64 of PNG header bytes
        let b64 = base64::engine::general_purpose::STANDARD
            .encode([0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
        let messages = vec![msg("user", &format!("[IMAGE:data:image/png;base64,{b64}]"))];

        let saved = cache.save_images(&messages).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1.exists());
    }

    #[test]
    fn rewrite_markers_replaces_image_refs() {
        let cached = vec![(
            "/tmp/photo.png".to_string(),
            PathBuf::from("/cache/abc123.png"),
        )];
        let mut messages = vec![msg("user", "See [IMAGE:/tmp/photo.png] here")];

        MediaCache::rewrite_markers(&mut messages, &cached);

        assert!(
            messages[0]
                .content
                .contains("[Previously seen image cached at /cache/abc123.png]")
        );
        assert!(!messages[0].content.contains("[IMAGE:"));
    }

    #[test]
    fn rewrite_markers_noop_when_empty() {
        let mut messages = vec![msg("user", "no images here")];
        MediaCache::rewrite_markers(&mut messages, &[]);
        assert_eq!(messages[0].content, "no images here");
    }

    #[tokio::test]
    async fn cleanup_old_tolerates_missing_dir() {
        let cache = MediaCache::new(PathBuf::from("/nonexistent/media_cache"));
        let removed = cache.cleanup_old(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn default_dir_returns_some() {
        // This test may fail in environments without HOME or USERPROFILE, but
        // should work on all CI runners.
        if std::env::var("HOME").is_ok() || std::env::var("USERPROFILE").is_ok() {
            let dir = MediaCache::default_dir();
            assert!(dir.is_some());
            let dir = dir.unwrap();
            assert!(dir.ends_with("media_cache"));
        }
    }
}
