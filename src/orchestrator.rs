//! Composition root (PLAN §1.6) — the only place the three domains meet.
//!
//! [`Session`] owns the `Arc<Overlay>`, the [`ParseCache`], and the
//! feature-gated [`LangRegistry`]; one session per CLI invocation
//! (stateless across invocations in v0.1, ADR-0007). `scan` is the first
//! command wired here (F4.6); `apply` (F5) and the memory commands (F6)
//! land in their phases.

use std::sync::Arc;

use crate::ast::cache::ParseCache;
use crate::ast::lang::LangRegistry;
use crate::error::JanitorError;
use crate::scan::{self, ScanParams, ScanReport};
use crate::vfs::{Os, Overlay};

/// One composition root per invocation (PLAN §1.6 sketch, F4 shape).
pub struct Session {
    overlay: Arc<Overlay>,
    cache: Arc<ParseCache>,
    langs: LangRegistry,
}

impl Session {
    /// A session over the real disk (`Os` base, empty overlay — the CLI's
    /// one-shot shape).
    pub fn new() -> Self {
        Self::with_overlay(Arc::new(Overlay::new(Arc::new(Os))))
    }

    /// A session over a caller-provided overlay — the staged-edit flows
    /// (F5's apply-then-scan) and the tests' virtual-file setups.
    pub fn with_overlay(overlay: Arc<Overlay>) -> Self {
        Self {
            overlay,
            cache: Arc::new(ParseCache::new()),
            langs: LangRegistry::new(),
        }
    }

    /// The session's overlay (shared, `Arc`).
    pub fn overlay(&self) -> &Arc<Overlay> {
        &self.overlay
    }

    /// The session's parse cache (streaming discipline: empty between
    /// scans, ADR-0007).
    pub fn cache(&self) -> &Arc<ParseCache> {
        &self.cache
    }

    /// The session's language registry.
    pub fn registry(&self) -> &LangRegistry {
        &self.langs
    }

    /// Run a structural scan (see [`scan`] for the pipeline).
    pub fn scan(&self, params: &ScanParams) -> Result<ScanReport, JanitorError> {
        scan::scan(&self.overlay, &self.cache, &self.langs, params)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session is the composition root: one overlay, one cache, one
    /// registry — and scans run against all three at once.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn session_composes_the_three_domains() {
        let session = Session::new();
        assert!(session.cache().is_empty());
        assert!(!session.registry().is_empty());

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("s.rs"),
            "fn s() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n",
        )
        .expect("corpus");
        let report = session
            .scan(&ScanParams::new(
                "(under block (seq (kind let_declaration) (kind let_declaration)))",
                tmp.path(),
            ))
            .expect("scan composes");
        assert_eq!(report.matches.len(), 1);
    }
}
