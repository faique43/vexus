use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

pub struct ModelFile {
    pub name: &'static str,
    pub sha256: &'static str,
    pub url_path: &'static str,
    /// Rough size shown in the download message ("~150 MB"), so a first-run
    /// user knows why the terminal went quiet. `""` omits the hint.
    pub size_hint: &'static str,
}

pub struct ModelManifest {
    pub id: &'static str,
    pub dim: usize,
    pub files: &'static [ModelFile],
}

/// Pinned revision resolved 2026-07-25 via
/// `curl -s https://huggingface.co/api/models/jinaai/jina-embeddings-v2-base-code`
/// (returns the repo's current `sha` field).
pub const DEFAULT_BASE_URL: &str =
    "https://huggingface.co/jinaai/jina-embeddings-v2-base-code/resolve/516f4baf13dec4ddddda8631e019b5737c8bc250/";

pub static JINA_CODE_V2: ModelManifest = ModelManifest {
    id: "jina-code-v2-q",
    dim: 768,
    files: &[
        ModelFile {
            name: "model_quantized.onnx",
            sha256: "ed45870251c9f0cf656e78aab0d37a23489066df8a222bb1c8caf8a45f2cb16d",
            url_path: "onnx/model_quantized.onnx",
            size_hint: "~160 MB",
        },
        ModelFile {
            name: "tokenizer.json",
            sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
            url_path: "tokenizer.json",
            size_hint: "~2 MB",
        },
    ],
};

fn base_url() -> String {
    std::env::var("VEXUS_MODEL_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn fetch(url: &str, dest: &Path) -> Result<()> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, dest).with_context(|| format!("copy {path}"))?;
        return Ok(());
    }
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_reader();
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut reader, &mut out)?;
    Ok(())
}

// Thread-local (not a shared global) so tests running concurrently in other
// threads can't pollute a given test's count of *its own* hash calls.
#[cfg(test)]
thread_local! {
    static HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn file_sha256(path: &Path) -> Result<String> {
    #[cfg(test)]
    HASH_CALLS.with(|c| c.set(c.get() + 1));
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Path of the "already verified" sentinel for `file` in `dir`.
fn sentinel_path(dir: &Path, file: &ModelFile) -> PathBuf {
    dir.join(format!("{}.verified", file.name))
}

/// A sentinel is trusted only when its content is exactly the expected
/// sha256 — anything missing, unreadable, or stale (from a previous
/// manifest revision) is treated as unverified, falling back to a full
/// hash check like before this optimization existed.
fn sentinel_matches(path: &Path, expected_sha256: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.trim() == expected_sha256)
        .unwrap_or(false)
}

fn write_sentinel(path: &Path, sha256: &str) -> Result<()> {
    std::fs::write(path, sha256)?;
    Ok(())
}

/// Returns the local model dir, downloading+verifying any missing/corrupt file.
/// Base URL override: env `VEXUS_MODEL_URL` (supports `file://` for tests/air-gap).
/// Never partial: downloads to `<name>.part`, verifies sha256, atomic-renames.
///
/// Re-hashing a ~150MB model file on every single CLI invocation (`vexus
/// search`, in particular) is wasted work once it's already been verified
/// once — a `<name>.verified` sentinel containing the expected sha256 lets a
/// later call skip straight past the hash check. The sentinel is written
/// only right after a successful verify, so it can't outlive the file it
/// describes without also getting invalidated below whenever the file is
/// re-fetched.
pub fn ensure_model(manifest: &ModelManifest, models_root: &Path) -> Result<PathBuf> {
    let dir = models_root.join(manifest.id);
    std::fs::create_dir_all(&dir)?;
    let base = base_url();
    for file in manifest.files {
        let dest = dir.join(file.name);
        let sentinel = sentinel_path(&dir, file);
        if dest.exists() && sentinel_matches(&sentinel, file.sha256) {
            continue; // trusted from a prior verify: skip the re-hash
        }
        if dest.exists() && file_sha256(&dest)? == file.sha256 {
            write_sentinel(&sentinel, file.sha256)?;
            continue; // present + verified
        }
        let part = dir.join(format!("{}.part", file.name));
        let url = format!("{base}{}", file.url_path);
        let size = if file.size_hint.is_empty() {
            String::new()
        } else {
            format!(" ({}, one-time)", file.size_hint)
        };
        eprintln!("vexus: downloading {}{size} …", file.name);
        let result = (|| -> Result<()> {
            fetch(&url, &part)?;
            let got = file_sha256(&part)?;
            if got != file.sha256 {
                bail!(
                    "checksum mismatch for {}: expected {}, got {got}",
                    file.name,
                    file.sha256
                );
            }
            std::fs::rename(&part, &dest)?;
            write_sentinel(&sentinel, file.sha256)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&dest); // never leave an unverified file
            let _ = std::fs::remove_file(&sentinel);
            return result.map(|_| unreachable!());
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(data))
    }

    #[test]
    fn ensure_model_downloads_verifies_and_is_idempotent() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("onnx")).unwrap();
        std::fs::write(src.path().join("onnx/model.bin"), b"weights").unwrap();
        std::fs::write(src.path().join("tok.json"), b"tokens").unwrap();

        let files = vec![
            ModelFile {
                name: "model.bin",
                sha256: Box::leak(sha256_hex(b"weights").into_boxed_str()),
                url_path: "onnx/model.bin",
                size_hint: "",
            },
            ModelFile {
                name: "tok.json",
                sha256: Box::leak(sha256_hex(b"tokens").into_boxed_str()),
                url_path: "tok.json",
                size_hint: "",
            },
        ];
        let manifest = ModelManifest {
            id: "testmodel",
            dim: 4,
            files: Box::leak(files.into_boxed_slice()),
        };

        std::env::set_var(
            "VEXUS_MODEL_URL",
            format!("file://{}/", src.path().display()),
        );
        let root = tempfile::tempdir().unwrap();

        let dir = ensure_model(&manifest, root.path()).unwrap();
        assert_eq!(std::fs::read(dir.join("model.bin")).unwrap(), b"weights");
        assert_eq!(std::fs::read(dir.join("tok.json")).unwrap(), b"tokens");

        // corrupt one file -> re-fetched. Must also drop its now-stale
        // "verified" sentinel: with it left in place, `ensure_model` would
        // trust the sentinel and skip re-hashing (that's the whole point of
        // the optimization — see `sentinel_skips_rehash_when_file_unchanged`
        // below), so this test simulates corruption discovered with no
        // sentinel to (wrongly) vouch for the file, same as before the
        // sentinel existed.
        std::fs::remove_file(dir.join("model.bin.verified")).unwrap();
        std::fs::write(dir.join("model.bin"), b"corrupt").unwrap();
        let dir = ensure_model(&manifest, root.path()).unwrap();
        assert_eq!(std::fs::read(dir.join("model.bin")).unwrap(), b"weights");

        // sentinel written after that re-verify; content is the expected sha256
        let sentinel = std::fs::read_to_string(dir.join("model.bin.verified")).unwrap();
        assert_eq!(sentinel.trim(), manifest.files[0].sha256);

        // unchanged file + valid sentinel -> next call must NOT re-hash it.
        // (Thread-local counter: safe against other tests' concurrent hashing.)
        let before = HASH_CALLS.with(|c| c.get());
        let dir = ensure_model(&manifest, root.path()).unwrap();
        let after = HASH_CALLS.with(|c| c.get());
        assert_eq!(
            before, after,
            "a valid sentinel must let ensure_model skip the re-hash entirely"
        );
        assert_eq!(std::fs::read(dir.join("model.bin")).unwrap(), b"weights");

        // bad checksum in manifest -> Err, no .part left behind
        let bad = ModelManifest {
            id: "badmodel",
            dim: 4,
            files: Box::leak(
                vec![ModelFile {
                    name: "model.bin",
                    sha256: "00",
                    url_path: "onnx/model.bin",
                    size_hint: "",
                }]
                .into_boxed_slice(),
            ),
        };
        assert!(ensure_model(&bad, root.path()).is_err());
        assert!(!root.path().join("badmodel/model.bin").exists());
        std::env::remove_var("VEXUS_MODEL_URL");
    }
}
