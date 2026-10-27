// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Artifact storage: historical command output, old source reads, enormous
//! test logs live in the CAS. The prompt carries `artifact://<hash>` refs
//! plus short summaries — never the unbounded content itself.

use std::sync::Arc;

use faktor_cas::Cas;
use faktor_core::hash::FileHash;
use faktor_core::id::SessionId;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRef {
    /// Present when content fit inside the inline cap.
    pub inline: Option<String>,
    /// `artifact://<hash>` when stored in the CAS.
    pub artifact: Option<String>,
    pub summary: String,
    pub size: usize,
}

impl ArtifactRef {
    pub fn render(&self) -> String {
        if let Some(a) = &self.artifact {
            format!("[artifact: {a}] {}", self.summary)
        } else {
            self.inline.clone().unwrap_or_default()
        }
    }
}

pub struct ArtifactWriter {
    cas: Arc<Cas>,
    session: SessionId,
}

impl ArtifactWriter {
    pub fn new(cas: Arc<Cas>, session: SessionId) -> Self {
        Self { cas, session }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Store bytes: inline if within `max_inline`, otherwise CAS.
    pub fn store(
        &self,
        _kind: &str,
        bytes: &[u8],
        max_inline: usize,
    ) -> faktor_core::Result<ArtifactRef> {
        let size = bytes.len();
        if size <= max_inline {
            let text = String::from_utf8_lossy(bytes).to_string();
            let summary = make_summary(&text);
            return Ok(ArtifactRef {
                inline: Some(text),
                artifact: None,
                summary,
                size,
            });
        }
        let hash = self.cas.put(bytes).map_err(cas_err)?;
        let text = String::from_utf8_lossy(bytes);
        Ok(ArtifactRef {
            inline: None,
            artifact: Some(format!("artifact://{}", hash.to_hex())),
            summary: make_summary(&text),
            size,
        })
    }

    /// Read an artifact back by its `artifact://<hash>` reference.
    pub fn read_ref(&self, artifact_ref: &str) -> faktor_core::Result<Vec<u8>> {
        let hash_hex = artifact_ref
            .strip_prefix("artifact://")
            .ok_or_else(|| faktor_core::error::Error::malformed("bad artifact reference"))?;
        let hash = FileHash::from_hex(hash_hex)
            .ok_or_else(|| faktor_core::error::Error::malformed("bad artifact hash"))?;
        self.cas.get_verified_now(hash).map_err(cas_err)
    }

    /// Verify an artifact reference resolves and hashes correctly.
    pub fn verify(&self, artifact_ref: &str) -> faktor_core::Result<bool> {
        let bytes = self.read_ref(artifact_ref)?;
        let hash = FileHash::from(blake3::hash(&bytes).into());
        let hash_hex = artifact_ref
            .strip_prefix("artifact://")
            .ok_or_else(|| faktor_core::error::Error::malformed("bad artifact reference"))?;
        Ok(hash.to_hex() == hash_hex)
    }
}

fn make_summary(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let summarized = truncate(first, 200);
    if summarized.is_empty() {
        "[empty output]".to_string()
    } else {
        summarized.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn cas_err(e: faktor_cas::CasError) -> faktor_core::error::Error {
    faktor_core::error::Error::new(faktor_core::error::ErrorKind::Store, format!("cas: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::error::ErrorKind;
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, Arc<Cas>, SessionId) {
        let dir = tempdir().unwrap();
        let cas = Arc::new(Cas::open(dir.path().join("cas")).unwrap());
        (dir, cas, SessionId::new(1))
    }

    #[test]
    fn small_content_stays_inline() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let r = w.store("command_output", b"ok", 4096).unwrap();
        assert!(r.inline.is_some());
        assert!(r.artifact.is_none());
        assert_eq!(r.render(), "ok");
        assert_eq!(r.size, 2);
    }

    #[test]
    fn overflow_goes_to_cas_and_roundtrips_exactly() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let blob: Vec<u8> = (0..20_000).map(|i| (i % 251) as u8).collect();
        let r = w.store("log", &blob, 4096).unwrap();
        assert!(r.inline.is_none());
        let artifact = r.artifact.as_ref().expect("artifact ref");
        assert!(artifact.starts_with("artifact://"));
        assert_eq!(artifact.len(), 11 + 64);
        let back = w.read_ref(artifact).unwrap();
        assert_eq!(back, blob);
        assert!(w.verify(artifact).unwrap());
    }

    #[test]
    fn tampered_cas_blob_is_a_typed_store_error_and_never_returns_bytes() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let r = w.store("log", &vec![1u8; 9000], 4096).unwrap();
        let artifact = r.artifact.unwrap();
        // Corrupt the stored blob behind the writer's back: read its bytes
        // from the CAS, then write garbage at the same address by storing
        // the blob path via the shard layout (first two hex chars as dir).
        let hash_hex = artifact.strip_prefix("artifact://").unwrap();
        let path = cas.root().join(&hash_hex[..2]).join(&hash_hex[2..]);
        std::fs::write(path, b"corrupted").unwrap();
        // Catches a reader that returns Ok(partial) or a verify that returns
        // Ok(false) instead of surfacing the corruption as a typed failure.
        let err = w.read_ref(&artifact).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Store, "{err:?}");
        assert!(
            err.message.starts_with("cas:") && err.message.contains("zstd"),
            "the CAS corruption must surface as a typed store error: {err}"
        );
        let err = w.verify(&artifact).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Store, "{err:?}");
    }

    #[test]
    fn identical_artifacts_dedupe() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let blob = vec![7u8; 8000];
        let a = w.store("log", &blob, 100).unwrap();
        let b = w.store("log", &blob, 100).unwrap();
        assert_eq!(a.artifact, b.artifact);
        assert_eq!(
            cas.blob_count(),
            1,
            "ten checkpoints of one file = one copy"
        );
    }

    #[test]
    fn malformed_refs_are_rejected_with_the_exact_reason() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        // Catches a reader that treats any string as a ref or returns a
        // generic store error: the prefix and hex grammar fail separately.
        for (raw, expected) in [
            ("artifact://zz", "bad artifact hash"),
            ("https://evil.com/x", "bad artifact reference"),
            ("", "bad artifact reference"),
            ("artifact://", "bad artifact hash"),
        ] {
            let err = w.read_ref(raw).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Malformed, "{raw:?}: {err:?}");
            assert_eq!(err.message, expected, "{raw:?}: {err}");
        }
    }

    #[test]
    fn empty_and_binary_artifacts() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let r = w.store("cmd", b"", 10).unwrap();
        assert_eq!(r.render(), "");
        // Binary garbage > inline cap: summary never panics on invalid UTF-8.
        let bytes: Vec<u8> = (0..=255).cycle().take(10_000).collect();
        let r = w.store("cmd", &bytes, 100).unwrap();
        assert!(r.artifact.is_some());
        assert!(!r.summary.is_empty());
        assert!(w.verify(r.artifact.as_ref().unwrap()).unwrap());
    }

    #[test]
    fn summary_is_bounded() {
        let (_d, cas, session) = fixture();
        let w = ArtifactWriter::new(cas.clone(), session);
        let long = format!("line1\n{}", "y".repeat(10_000));
        let r = w.store("cmd", long.as_bytes(), 4096).unwrap();
        assert!(
            r.summary.len() <= 204,
            "summary bounded: {}",
            r.summary.len()
        );
        assert!(r.summary.starts_with("line1"));
    }

    #[test]
    fn artifact_writer_reserves_before_publishing_and_refuses_over_budget() {
        use faktor_cas::faktor_storage::{StorageClass, StorageGovernor, StorageGovernorConfig};
        let dir = tempdir().unwrap();
        let config = StorageGovernorConfig {
            total_budget_bytes: 100_000,
            watermark_permille: 900,
            reservation_lease_ms: 60_000,
            quotas: StorageClass::ALL
                .into_iter()
                .map(|class| {
                    (
                        class,
                        if class == StorageClass::MainCas {
                            1_000
                        } else {
                            20_000
                        },
                    )
                })
                .collect(),
        }
        .validated()
        .unwrap();
        let governor = Arc::new(StorageGovernor::open(dir.path(), config).unwrap());
        let mut cas = Cas::open_in_class(dir.path().join("cas"), StorageClass::MainCas).unwrap();
        cas.attach_governor(governor.clone(), StorageClass::MainCas);
        let cas = Arc::new(cas);
        let writer = ArtifactWriter::new(cas.clone(), SessionId::new(1));
        let mut noise_state = 0x9E37_79B9_7F4A_7C15u64;
        let noise: Vec<u8> = (0..2_000)
            .map(|_| {
                noise_state ^= noise_state << 13;
                noise_state ^= noise_state >> 7;
                noise_state ^= noise_state << 17;
                noise_state as u8
            })
            .collect();
        let error = writer
            .store("log", &noise, 0)
            .expect_err("over-budget artifact must be refused");
        assert_eq!(error.kind, ErrorKind::Store);
        assert_eq!(cas.blob_count(), 0);
        assert_eq!(governor.counters().unwrap().refusals, 1);
        let stored = writer.store("log", b"small artifact", 0).unwrap();
        assert!(stored.artifact.is_some());
        assert_eq!(cas.blob_count(), 1);
    }
}
