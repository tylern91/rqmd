//! Background model download at server start.
//!
//! The first `query` otherwise downloads the models inside the tool's
//! deadline. This only fills the hub cache; nothing is loaded into memory, and
//! the lazy path in `RqmdServer::ml` still downloads on demand if this failed.

use std::future::Future;

use rqmd_llm::BackendKind;

/// Set to `1` or `true` to skip the background download.
pub(crate) const NO_PREFETCH_ENV: &str = "RQMD_NO_MODEL_PREFETCH";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    OptedOut,
    NotNeeded,
    Downloaded,
    Failed,
}

fn opted_out() -> bool {
    std::env::var(NO_PREFETCH_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Decide and run one prefetch. `needed` and `download` are parameters so the
/// policy is testable without a network or a model cache.
pub(crate) async fn run<D, Fut>(
    opted_out: bool,
    needed: impl FnOnce() -> bool,
    download: D,
) -> Outcome
where
    D: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    if opted_out {
        return Outcome::OptedOut;
    }
    if !needed() {
        return Outcome::NotNeeded;
    }
    eprintln!("[rqmd-mcp] Downloading models in the background...");
    match download().await {
        Ok(()) => {
            eprintln!("[rqmd-mcp] Model download finished.");
            Outcome::Downloaded
        }
        Err(e) => {
            eprintln!(
                "[rqmd-mcp] Background model download failed (will retry on first query): {e:#}"
            );
            Outcome::Failed
        }
    }
}

/// Start the download on the runtime and return at once.
pub(crate) fn spawn() {
    tokio::spawn(async {
        let kind = BackendKind::from_env();
        run(
            opted_out(),
            || rqmd_llm::prefetch_needed(&kind),
            || rqmd_llm::prefetch_models(&kind),
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn counter() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let c = Arc::new(AtomicUsize::new(0));
        (c.clone(), c)
    }

    #[tokio::test]
    async fn downloads_without_any_tool_call() {
        let (seen, c) = counter();
        let out = run(
            false,
            || true,
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(out, Outcome::Downloaded);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn opt_out_starts_no_download() {
        let (seen, c) = counter();
        let out = run(
            true,
            || true,
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(out, Outcome::OptedOut);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn offline_or_cached_starts_no_download() {
        let (seen, c) = counter();
        let out = run(
            false,
            || false,
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(out, Outcome::NotNeeded);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failure_is_tolerated() {
        let out = run(false, || true, || async { anyhow::bail!("network down") }).await;
        assert_eq!(out, Outcome::Failed);
    }

    #[test]
    fn opt_out_env_values() {
        // Parsing is exercised through the real env reader in one test to
        // avoid racing other tests on the process environment.
        // SAFETY: this is the only test that touches NO_PREFETCH_ENV.
        unsafe {
            std::env::remove_var(NO_PREFETCH_ENV);
            assert!(!opted_out());
            std::env::set_var(NO_PREFETCH_ENV, "1");
            assert!(opted_out());
            std::env::set_var(NO_PREFETCH_ENV, "0");
            assert!(!opted_out());
            std::env::remove_var(NO_PREFETCH_ENV);
        }
    }

    #[tokio::test]
    async fn spawn_returns_while_download_is_still_running() {
        // A download that never finishes must not hold the caller: the server
        // keeps answering while models are still downloading.
        let task = tokio::spawn(run(
            false,
            || true,
            std::future::pending::<anyhow::Result<()>>,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        task.abort();
    }
}
