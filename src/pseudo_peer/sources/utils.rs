//! Shared utilities for block sources

use futures::{StreamExt, TryStreamExt};
use std::{future::Future, time::Duration};

/// Runs `f` up to `max_attempts` times, doubling the delay after each failure.
/// `on_retry` is called with the failed attempt number and its error before each retry.
pub async fn retry_with_backoff<T, F, Fut>(
    max_attempts: u32,
    initial_backoff: Duration,
    mut f: F,
    mut on_retry: impl FnMut(u32, &eyre::Report),
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = eyre::Result<T>>,
{
    let mut backoff = initial_backoff;
    let mut attempt = 1;
    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < max_attempts => {
                on_retry(attempt, &err);
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Fetches every height with `tokio::spawn`, keeping at most `concurrency` fetches in flight
/// (a sliding window, so one slow fetch does not hold back the rest), and returns the
/// results in input order. Fails on the first fetch error.
pub async fn fetch_ordered<T, F, Fut>(
    heights: Vec<u64>,
    concurrency: usize,
    fetch: F,
) -> eyre::Result<Vec<T>>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = eyre::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    futures::stream::iter(heights)
        .map(|height| tokio::spawn(fetch(height)))
        .buffered(concurrency.max(1))
        .map(|joined| joined.map_err(|e| eyre::eyre!(e))?)
        .try_collect()
        .await
}

/// Finds the file/directory with the largest number in its name from a list of files
pub fn name_with_largest_number(files: &[String], is_dir: bool) -> Option<(u64, String)> {
    let mut files = files
        .iter()
        .filter_map(|file_raw| {
            let file = file_raw.strip_suffix("/").unwrap_or(file_raw);
            let file = file.split("/").last().unwrap();
            let stem = if is_dir { file } else { file.strip_suffix(".rmp.lz4")? };
            stem.parse::<u64>().ok().map(|number| (number, file_raw.to_string()))
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return None;
    }
    files.sort_by_key(|(number, _)| *number);
    files.last().cloned()
}

/// Generates the RMP file path for a given block height
pub fn rmp_path(height: u64) -> String {
    let f = ((height - 1) / 1_000_000) * 1_000_000;
    let s = ((height - 1) / 1_000) * 1_000;
    format!("{f}/{s}/{height}.rmp.lz4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    };

    const TEST_BACKOFF: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let calls = AtomicU32::new(0);
        let mut retried = Vec::new();
        let result = retry_with_backoff(
            6,
            TEST_BACKOFF,
            || {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move { if call < 3 { Err(eyre::eyre!("stalled")) } else { Ok(call) } }
            },
            |attempt, _| retried.push(attempt),
        )
        .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(retried, vec![1, 2]);
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let result: eyre::Result<()> = retry_with_backoff(
            4,
            TEST_BACKOFF,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(eyre::eyre!("stalled")) }
            },
            |_, _| {},
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fetch_ordered_keeps_order_and_bounds_in_flight() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let heights: Vec<u64> = (1..=50).collect();
        let results = fetch_ordered(heights.clone(), 4, |height| {
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(now, Ordering::SeqCst);
                // Later heights finish sooner, so completion order differs from input order.
                tokio::time::sleep(Duration::from_millis(50 - height)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(height)
            }
        })
        .await
        .unwrap();
        assert_eq!(results, heights);
        assert!(max_in_flight.load(Ordering::SeqCst) <= 4);
    }

    #[tokio::test]
    async fn fetch_ordered_fails_on_a_failed_fetch() {
        let result = fetch_ordered((1..=10).collect(), 4, |height| async move {
            if height == 7 { Err(eyre::eyre!("missing block {height}")) } else { Ok(height) }
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_ordered_with_retry_survives_a_transient_failure() {
        let failed_once = Arc::new(AtomicU32::new(0));
        let results = fetch_ordered((1..=10).collect(), 4, |height| {
            let failed_once = failed_once.clone();
            async move {
                retry_with_backoff(
                    3,
                    TEST_BACKOFF,
                    || {
                        let fail = height == 7 && failed_once.fetch_add(1, Ordering::SeqCst) == 0;
                        async move { if fail { Err(eyre::eyre!("stalled")) } else { Ok(height) } }
                    },
                    |_, _| {},
                )
                .await
            }
        })
        .await
        .unwrap();
        assert_eq!(results, (1..=10).collect::<Vec<_>>());
        assert_eq!(failed_once.load(Ordering::SeqCst), 2);
    }
}
