//! Cancellation and progress for the request a tool call runs in.
//!
//! A transport that can tell a request was cancelled (an MCP
//! `notifications/cancelled`, the client closing stdin, a termination signal,
//! a proxy closing its daemon connection) runs the request inside
//! [`RequestControl::scope`]. Nested registry dispatch (run_script and
//! run_steps calling other tools, replay_trajectory replaying turns) stays in
//! the same task, so it sees the same control.
//!
//! Long tools ask [`is_cancelled`] between steps, or race a wait against
//! [`cancelled`], and stop early with a result whose structured `code` is
//! [`CANCELLED_CODE`]. A single action already under way is not cut off: a
//! tool stops at its next step boundary. When the request asked for progress,
//! [`report_progress`] reaches the transport, which sends MCP
//! `notifications/progress`. Outside a scope nothing is ever cancelled and
//! progress goes nowhere, so tools behave as before for in-process callers.

use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

/// Structured `code` of a result a tool returned because its request was
/// cancelled.
pub const CANCELLED_CODE: &str = "cancelled";

/// A cancel flag shared by a request's transport and the tool running it.
/// Cloning shares the flag; the first [`CancelToken::cancel`] wins and its
/// reason is kept.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<CancelState>,
}

#[derive(Debug, Default)]
struct CancelState {
    cancelled: AtomicBool,
    reason: Mutex<Option<String>>,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel with a short reason for the tool's result, such as "the client
    /// cancelled the request". Returns false when it was already cancelled.
    pub fn cancel(&self, reason: impl Into<String>) -> bool {
        let mut slot = self
            .inner
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return false;
        }
        *slot = Some(reason.into());
        self.inner.cancelled.store(true, Ordering::SeqCst);
        drop(slot);
        self.inner.notify.notify_waiters();
        true
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// The reason given to the first [`CancelToken::cancel`].
    pub fn reason(&self) -> Option<String> {
        self.inner
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Resolve once the token is cancelled (at once if it already is).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            // Register before checking, so a cancel between the check and
            // the await still wakes this waiter.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// One progress report from a tool: `progress` out of `total` (when known),
/// with a short human-readable message.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: String,
}

/// Where a request's progress reports go. Set by the transport only when the
/// request asked for progress.
pub type ProgressSink = Arc<dyn Fn(Progress) + Send + Sync>;

/// What a transport hands to the tool call it runs.
#[derive(Clone, Default)]
pub struct RequestControl {
    cancel: CancelToken,
    progress: Option<ProgressSink>,
}

impl std::fmt::Debug for RequestControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestControl")
            .field("cancel", &self.cancel)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

tokio::task_local! {
    static CURRENT: RequestControl;
}

impl RequestControl {
    pub fn new(cancel: CancelToken) -> Self {
        Self {
            cancel,
            progress: None,
        }
    }

    pub fn with_progress(mut self, sink: ProgressSink) -> Self {
        self.progress = Some(sink);
        self
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Run `future` (a request's whole handling) under this control.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }
}

/// The cancel token of the request running on this task, if a transport
/// installed one.
pub fn current_cancel_token() -> Option<CancelToken> {
    CURRENT.try_with(|control| control.cancel.clone()).ok()
}

/// Whether the request running on this task was cancelled.
pub fn is_cancelled() -> bool {
    CURRENT
        .try_with(|control| control.cancel.is_cancelled())
        .unwrap_or(false)
}

/// Why the request running on this task was cancelled, if it was.
pub fn cancel_reason() -> Option<String> {
    CURRENT
        .try_with(|control| control.cancel.reason())
        .ok()
        .flatten()
}

/// Resolve when the request running on this task is cancelled; never
/// resolves outside a [`RequestControl::scope`].
pub async fn cancelled() {
    match current_cancel_token() {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Report progress for the request running on this task. `message` is built
/// only when the request asked for progress, so a call is cheap otherwise.
pub fn report_progress(progress: f64, total: Option<f64>, message: impl FnOnce() -> String) {
    let Ok(Some(sink)) = CURRENT.try_with(|control| control.progress.clone()) else {
        return;
    };
    sink(Progress {
        progress,
        total,
        message: message(),
    });
}

/// "cancelled (reason)" for a tool's result text, with a fallback when the
/// transport gave no reason.
pub fn describe_cancel() -> String {
    format!(
        "cancelled ({})",
        cancel_reason().unwrap_or_else(|| "the request was cancelled".to_owned())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn the_first_cancel_wins_and_wakes_waiters() {
        let token = CancelToken::new();
        let waiter = tokio::spawn({
            let token = token.clone();
            async move { token.cancelled().await }
        });
        tokio::task::yield_now().await;
        assert!(!token.is_cancelled());
        assert!(token.cancel("first"));
        assert!(!token.cancel("second"));
        assert_eq!(token.reason().as_deref(), Some("first"));
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("a cancel wakes a waiter")
            .unwrap();
        // Already cancelled: resolves at once.
        tokio::time::timeout(Duration::from_millis(50), token.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn outside_a_scope_nothing_is_cancelled_and_progress_is_dropped() {
        assert!(!is_cancelled());
        assert!(cancel_reason().is_none());
        report_progress(1.0, None, || unreachable!("no sink, no message"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), cancelled())
                .await
                .is_err(),
            "cancelled() must stay pending without a scope"
        );
    }

    #[tokio::test]
    async fn a_scope_carries_cancel_and_progress_into_the_request() {
        let token = CancelToken::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink: ProgressSink = {
            let seen = seen.clone();
            Arc::new(move |progress: Progress| seen.lock().unwrap().push(progress))
        };
        let control = RequestControl::new(token.clone()).with_progress(sink);
        let outcome = control
            .scope(async {
                report_progress(1.0, Some(2.0), || "step 1 of 2".into());
                assert!(!is_cancelled());
                token.cancel("the client cancelled the request");
                cancelled().await;
                (is_cancelled(), describe_cancel())
            })
            .await;
        assert_eq!(
            outcome,
            (
                true,
                "cancelled (the client cancelled the request)".to_owned()
            )
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Progress {
                progress: 1.0,
                total: Some(2.0),
                message: "step 1 of 2".into()
            }]
        );
    }
}
