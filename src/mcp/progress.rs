//! MCP progress heartbeat for long-running tool calls.
//!
//! rmcp's session worker idle-timeouts after 300s of *event-loop* silence, and
//! Claude Code aborts a tool after 300s with no progress. A silent
//! `codebase-retrieval` hits both clocks: the worker dies mid-call (handle
//! stays in the map → next POST is `Session service terminated`) and the client
//! aborts. Heartbeats are concurrent with the work future so a hung embed/query
//! still pings.

use std::future::Future;
use std::time::Duration;

use rmcp::model::{Meta, NumberOrString, ProgressNotificationParam, ProgressToken};
use rmcp::{Peer, RoleServer};
use tokio_util::sync::CancellationToken;

/// Interval used by `codebase-retrieval` / `file-retrieval`. Well under both
/// 300s clocks; not so chatty that we flood the session channel.
pub const MCP_PROGRESS_HEARTBEAT: Duration = Duration::from_secs(15);

/// Fallback token when the client did not send `_meta.progressToken`. Progress
/// still traverses the session worker (resets keep_alive) even if the client
/// ignores an unknown token.
fn heartbeat_token(meta: &Meta) -> ProgressToken {
    meta.get_progress_token().unwrap_or_else(|| {
        ProgressToken(NumberOrString::String(std::sync::Arc::from(
            "context-engine-heartbeat",
        )))
    })
}

/// Run `work` while emitting MCP progress every `interval`.
///
/// The ticker shares this task via `select!` (no spawned task to leak).
/// `ct` cancel or a `notify_progress` error stops ticks and still returns
/// `work`'s result — a dropped client must not fail the tool.
pub async fn with_progress_heartbeat<F, T>(
    peer: Peer<RoleServer>,
    meta: &Meta,
    ct: CancellationToken,
    interval: Duration,
    work: F,
) -> T
where
    F: Future<Output = T>,
{
    let progress_token = heartbeat_token(meta);
    heartbeat_loop(ct, interval, work, move |n| {
        let peer = peer.clone();
        let progress_token = progress_token.clone();
        async move {
            match peer
                .notify_progress(
                    ProgressNotificationParam::new(progress_token, n).with_message("working"),
                )
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "MCP progress heartbeat send failed; stopping ticks, tool continues"
                    );
                    false
                }
            }
        }
    })
    .await
}

/// Drive `work` against a tick callback. `on_tick` returning `false` stops
/// further ticks (progress send failed) but still waits for `work`.
async fn heartbeat_loop<F, T, S, Fut>(
    ct: CancellationToken,
    interval: Duration,
    work: F,
    mut on_tick: S,
) -> T
where
    F: Future<Output = T>,
    S: FnMut(f64) -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::pin!(work);
    let mut n = 0.0_f64;
    loop {
        tokio::select! {
            biased;
            result = &mut work => return result,
            _ = ct.cancelled() => return work.await,
            _ = tokio::time::sleep(interval) => {
                n += 1.0;
                if !on_tick(n).await {
                    return work.await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn missing_client_progress_token_still_fabricates_one() {
        // Empty Meta = client sent no `_meta.progressToken`. We must still emit
        // (fabricated token) so the session worker sees FromHandler traffic.
        let token = heartbeat_token(&Meta::default());
        match token.0 {
            NumberOrString::String(s) => assert_eq!(&*s, "context-engine-heartbeat"),
            NumberOrString::Number(n) => panic!("expected fabricated string token, got {n}"),
        }
    }

    #[tokio::test]
    async fn returns_work_result_when_work_finishes_first() {
        let ticks = AtomicU32::new(0);
        let result = heartbeat_loop(
            CancellationToken::new(),
            Duration::from_millis(50),
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                7_u8
            },
            |_| {
                ticks.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
        )
        .await;
        assert_eq!(result, 7);
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            0,
            "work shorter than interval"
        );
    }

    #[tokio::test]
    async fn ticks_concurrently_with_long_work() {
        let ticks = AtomicU32::new(0);
        let result = heartbeat_loop(
            CancellationToken::new(),
            Duration::from_millis(30),
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                "ok"
            },
            |_| {
                ticks.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
        )
        .await;
        assert_eq!(result, "ok");
        assert!(
            ticks.load(Ordering::SeqCst) >= 2,
            "long work must be ticked concurrently, got {}",
            ticks.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn cancel_stops_ticks_and_still_returns_work() {
        let ct = CancellationToken::new();
        let ticks = AtomicU32::new(0);
        ct.cancel();
        let result = heartbeat_loop(
            ct,
            Duration::from_millis(10),
            async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                1_u8
            },
            |_| {
                ticks.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
        )
        .await;
        assert_eq!(result, 1);
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            0,
            "cancelled token must not tick"
        );
    }

    #[tokio::test]
    async fn tick_failure_stops_ticks_and_still_returns_work() {
        let ticks = AtomicU32::new(0);
        let result = heartbeat_loop(
            CancellationToken::new(),
            Duration::from_millis(20),
            async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                3_u8
            },
            |_| {
                let n = ticks.fetch_add(1, Ordering::SeqCst) + 1;
                async move { n < 1 } // first tick returns false
            },
        )
        .await;
        assert_eq!(result, 3);
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            1,
            "failed tick must not be retried"
        );
    }
}
