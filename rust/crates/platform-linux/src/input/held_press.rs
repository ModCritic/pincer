//! A press that leaves its button held (mouse_button_down's foreground
//! route), run on a blocking thread under a time budget.
//!
//! The blocking work cannot be stopped once it started. When the call stops
//! waiting (its budget ran out, or the request was cancelled and its future
//! dropped), the press can still land later, and a hold the call never
//! recorded is a button nothing releases: on the foreground route, the core
//! pointer's real button. So the task records the hold itself as the press
//! lands, unless the call already gave up; then it releases the button at
//! once and records nothing. [`HeldPressGate`] decides which, once.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

fn timeout_error(label: &str, budget: Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: {label} did not complete within {budget:?}; the X server or the target \
         application may be wedged. No button is held: a press that still lands is \
         released at once. Take a screenshot before retrying",
        super::foreground::CODE_TIMEOUT
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HeldPress {
    #[default]
    Pending,
    /// The press landed while the call still waited, and the hold is recorded.
    Landed,
    /// The call stopped waiting first: a press that lands is released.
    Abandoned,
}

/// Who owns a press that lands: the call that still waits for it, or nobody.
#[derive(Default)]
struct HeldPressGate(Mutex<HeldPress>);

impl HeldPressGate {
    fn state(&self) -> std::sync::MutexGuard<'_, HeldPress> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The task, right after its press landed: run `record` and return
    /// `true` while the call still waits; return `false` and record nothing
    /// once it gave up.
    fn land(&self, record: impl FnOnce()) -> bool {
        let mut state = self.state();
        if *state == HeldPress::Abandoned {
            return false;
        }
        record();
        *state = HeldPress::Landed;
        true
    }

    /// The call stops waiting: `true` when the press already landed and its
    /// hold is recorded, `false` when a press that lands from now on is not
    /// the call's.
    fn abandon(&self) -> bool {
        let mut state = self.state();
        if *state == HeldPress::Landed {
            return true;
        }
        *state = HeldPress::Abandoned;
        false
    }
}

/// Abandons the gate when the waiting call's future is dropped (a cancelled
/// request); harmless once the task answered.
struct AbandonOnDrop(Arc<HeldPressGate>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        self.0.abandon();
    }
}

/// Run `press` on a blocking thread under `budget`. When it lands while the
/// call still waits, `record` stores the hold, on that thread, before the
/// call hears of it. When it lands after the call stopped waiting, or fails
/// (it may have landed before the error), `release` runs instead; `release`
/// must do nothing when the button is not down. A press that lands just as
/// the budget runs out counts as landed: the call reports success.
pub(crate) async fn press_and_hold_bounded(
    label: &'static str,
    budget: Duration,
    press: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    release: impl FnOnce() + Send + 'static,
    record: impl FnOnce() + Send + 'static,
) -> Result<anyhow::Result<()>, tokio::task::JoinError> {
    let gate = Arc::new(HeldPressGate::default());
    let task_gate = Arc::clone(&gate);
    let _abandon_if_dropped = AbandonOnDrop(Arc::clone(&gate));
    let task = tokio::task::spawn_blocking(move || {
        if let Err(error) = press() {
            release();
            return Err(error);
        }
        if task_gate.land(record) {
            Ok(())
        } else {
            release();
            Err(anyhow::anyhow!(
                "{label}: the press landed after the call stopped waiting and was released"
            ))
        }
    });
    match tokio::time::timeout(budget, task).await {
        Ok(joined) => joined,
        Err(_elapsed) if gate.abandon() => Ok(Ok(())),
        Err(_elapsed) => Ok(Err(timeout_error(label, budget))),
    }
}

#[cfg(test)]
mod tests {
    // A press that leaves its button held runs on a blocking thread under a
    // time budget. Whatever happens to the call (its budget runs out, or the
    // request is cancelled), a press that lands must end up either recorded as
    // a hold or released again, never down with no record.

    use super::{press_and_hold_bounded, HeldPressGate};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct FakeButton {
        down: AtomicBool,
        recorded: AtomicBool,
        releases: AtomicUsize,
    }

    impl FakeButton {
        /// Down with no hold record: nothing would ever release it.
        fn stuck(&self) -> bool {
            self.down.load(Ordering::SeqCst) && !self.recorded.load(Ordering::SeqCst)
        }
    }

    /// press, release and record closures over `button`; the press takes
    /// `press_takes` and then lands.
    fn fake_press(
        button: &Arc<FakeButton>,
        press_takes: Duration,
    ) -> (
        impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        impl FnOnce() + Send + 'static,
        impl FnOnce() + Send + 'static,
    ) {
        let (pressed, released, recorded) = (button.clone(), button.clone(), button.clone());
        (
            move || {
                std::thread::sleep(press_takes);
                pressed.down.store(true, Ordering::SeqCst);
                Ok(())
            },
            move || {
                released.releases.fetch_add(1, Ordering::SeqCst);
                released.down.store(false, Ordering::SeqCst);
            },
            move || recorded.recorded.store(true, Ordering::SeqCst),
        )
    }

    fn wait_until(deadline: Duration, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() && started.elapsed() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_press_that_lands_after_the_budget_is_released() {
        let button = Arc::new(FakeButton::default());
        let (press, release, record) = fake_press(&button, Duration::from_millis(300));
        let result = press_and_hold_bounded(
            "test press",
            Duration::from_millis(50),
            press,
            release,
            record,
        )
        .await
        .expect("joined");
        assert!(result.is_err(), "the call reports its budget ran out");
        // The blocking press still lands, 250 ms after the call gave up.
        wait_until(Duration::from_secs(2), || {
            button.releases.load(Ordering::SeqCst) > 0
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !button.stuck(),
            "the late press stayed down with no hold record"
        );
        assert!(!button.recorded.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_call_releases_a_press_that_lands_later() {
        let button = Arc::new(FakeButton::default());
        let (press, release, record) = fake_press(&button, Duration::from_millis(300));
        let call = press_and_hold_bounded(
            "test press",
            Duration::from_secs(10),
            press,
            release,
            record,
        );
        // The request is cancelled: its future is dropped while the press runs.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), call)
                .await
                .is_err(),
            "the press is still running"
        );
        wait_until(Duration::from_secs(2), || {
            button.releases.load(Ordering::SeqCst) > 0
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !button.stuck(),
            "the late press stayed down with no hold record"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_press_within_the_budget_is_recorded_and_kept() {
        let button = Arc::new(FakeButton::default());
        let (press, release, record) = fake_press(&button, Duration::from_millis(10));
        press_and_hold_bounded("test press", Duration::from_secs(5), press, release, record)
            .await
            .expect("joined")
            .expect("pressed");
        assert!(button.down.load(Ordering::SeqCst));
        assert!(button.recorded.load(Ordering::SeqCst));
        assert_eq!(button.releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_gate_gives_a_press_to_whichever_side_comes_first() {
        let gate = HeldPressGate::default();
        let mut recorded = false;
        assert!(gate.land(|| recorded = true));
        assert!(recorded);
        assert!(gate.abandon(), "a landed press stays the call's");

        let gate = HeldPressGate::default();
        assert!(!gate.abandon());
        let mut recorded = false;
        assert!(
            !gate.land(|| recorded = true),
            "a late press is not recorded"
        );
        assert!(!recorded);
    }
}
