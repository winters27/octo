//! Whether the lookups behind one radio station answered. A station build runs each station's
//! lookups inside [`watch`], and what goes wrong in there marks that scope:
//!
//! - `core`: a Last.fm request that got no answer while a key is set (no connection, a 408, 429
//!   or 5xx, one of Last.fm's try-again error codes, a body that is not Last.fm's JSON), and a
//!   lookup the build's time budget cut off;
//! - `sources`: no radio source answered cleanly, because each one asked failed, timed out or
//!   met trouble of its own, or every source left is resting.
//!
//! Outside any scope nothing is marked, so search, scrobbling and the station walk-on are never
//! affected. An answer that came back short is an answer, not trouble.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What went wrong inside one scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Trouble {
    pub core: bool,
    pub sources: bool,
}

impl Trouble {
    /// Either kind.
    pub fn any(self) -> bool {
        self.core || self.sources
    }
}

#[derive(Default)]
struct Flags {
    core: AtomicBool,
    sources: AtomicBool,
}

tokio::task_local! {
    static SCOPE: Arc<Flags>;
}

/// Marks `core` trouble in the current scope.
pub fn note_core() {
    let _ = SCOPE.try_with(|flags| flags.core.store(true, Ordering::Relaxed));
}

/// Marks `sources` trouble in the current scope.
pub fn note_sources() {
    let _ = SCOPE.try_with(|flags| flags.sources.store(true, Ordering::Relaxed));
}

/// Runs `work` in a scope of its own: its output, and what went wrong inside it. A scope inside
/// another one hides its trouble from the outer one; the caller passes on what should count.
pub async fn watch<F: Future>(work: F) -> (F::Output, Trouble) {
    let flags = Arc::new(Flags::default());
    let output = SCOPE.scope(Arc::clone(&flags), work).await;
    let trouble = Trouble {
        core: flags.core.load(Ordering::Relaxed),
        sources: flags.sources.load(Ordering::Relaxed),
    };
    (output, trouble)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_scope_sees_only_its_own_trouble() {
        note_core();
        let ((), clean) = watch(async {}).await;
        assert_eq!(clean, Trouble::default());

        let ((), core) = watch(async { note_core() }).await;
        assert_eq!(
            core,
            Trouble {
                core: true,
                sources: false
            }
        );

        let ((), outer) = watch(async {
            let ((), inner) = watch(async { note_sources() }).await;
            assert!(inner.sources);
        })
        .await;
        assert!(!outer.any(), "an inner scope keeps its trouble to itself");

        let ((), joined) = watch(async {
            futures::future::join(async { note_sources() }, async {}).await;
        })
        .await;
        assert!(joined.sources && !joined.core);
    }
}
