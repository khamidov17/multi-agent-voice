//! Debounce timer for batching rapid messages before Claude API calls.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;
use tracing::warn;

/// Debounce timer that triggers a callback after a period of inactivity.
///
/// Each call to `trigger()` resets the timer. When the timer expires
/// (no triggers for the specified duration), the callback is executed.
pub struct Debouncer {
    /// Channel to signal reset
    reset_tx: mpsc::Sender<()>,
    /// Notify to cancel the timer
    cancel: Arc<Notify>,
}

impl Debouncer {
    /// Create a new debouncer with the given duration.
    ///
    /// The callback will be called after `duration` of inactivity.
    pub fn new<F>(duration: Duration, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let (reset_tx, mut reset_rx) = mpsc::channel::<()>(16);
        let cancel = Arc::new(Notify::new());
        let cancel_clone = cancel.clone();
        let callback = Arc::new(callback);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    _ = cancel_clone.notified() => {
                        // Cancelled, exit the loop
                        break;
                    }
                    result = reset_rx.recv() => {
                        if result.is_none() {
                            // Channel closed, exit
                            break;
                        }

                        // Debounce loop: keep resetting while triggers come in
                        loop {
                            tokio::select! {
                                biased;

                                result = reset_rx.recv() => {
                                    if result.is_none() {
                                        // Channel closed
                                        return;
                                    }
                                    // Reset received, restart the timer
                                }
                                _ = sleep(duration) => {
                                    // Timer expired, call callback
                                    callback();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });

        Self { reset_tx, cancel }
    }

    /// Trigger/reset the debounce timer.
    ///
    /// If the timer is running, it will be reset.
    /// If the timer is not running, it will start.
    pub async fn trigger(&self) {
        if self.reset_tx.send(()).await.is_err() {
            warn!("Debounce channel closed");
        }
    }
}

impl Drop for Debouncer {
    fn drop(&mut self) {
        self.cancel.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_debounce_triggers_after_duration() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let debouncer = Debouncer::new(Duration::from_millis(50), move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Trigger once
        debouncer.trigger().await;

        // Should not fire immediately
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Wait for debounce
        sleep(Duration::from_millis(100)).await;

        // Should have fired once
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_debounce_resets_on_trigger() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let debouncer = Debouncer::new(Duration::from_millis(50), move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Trigger multiple times rapidly
        for _ in 0..5 {
            debouncer.trigger().await;
            sleep(Duration::from_millis(20)).await;
        }

        // Should not have fired yet (timer keeps resetting)
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Wait for final debounce
        sleep(Duration::from_millis(100)).await;

        // Should have fired exactly once
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_debounce_multiple_cycles() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let debouncer = Debouncer::new(Duration::from_millis(30), move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        // First cycle
        debouncer.trigger().await;
        sleep(Duration::from_millis(60)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second cycle
        debouncer.trigger().await;
        sleep(Duration::from_millis(60)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_debounce_drop_cancels() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let debouncer = Debouncer::new(Duration::from_millis(50), move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        debouncer.trigger().await;
        drop(debouncer); // Drop cancels the debouncer

        // Wait past when it would have fired
        sleep(Duration::from_millis(100)).await;

        // Should not have fired due to drop
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
