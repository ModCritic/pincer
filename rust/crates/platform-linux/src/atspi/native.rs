//! Native AT-SPI access over D-Bus via the `atspi` crate (zbus).
//!
//! Replaces the previous `python3 -c "import pyatspi; ..."` subprocess bridge:
//! no Python, `pyatspi`, or GObject-introspection typelibs are needed at
//! runtime. The zbus calls are async, so each public entry point drives a
//! small shared Tokio runtime via `block_on` (callers already invoke these
//! from `tokio::task::spawn_blocking`, so blocking here is safe).
//!
//! Element indices match the markdown produced by [`walk_tree_bounded`] for
//! one window: a depth-first, pre-order traversal of that window, numbering
//! the nodes accepted by the shared [`is_indexable`] capability predicate.
//! `perform_action`, `set_value`, `scroll_element`, `focus_element` and
//! `get_element_bounds_for_window` index into that same ordered set, so they
//! always take the window an index belongs to.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{CoordType, Interface, State, StateSet};
use atspi_connection::AccessibilityConnection;
use pincer_core::menu_label::{menu_label_key, menu_label_matches};

use super::{AtspiIdentity, AtspiNode};

pub mod hit;
pub(crate) mod hit_test;

/// Per-call D-Bus timeout: a single unresponsive accessible (common in large,
/// lazily-built trees like Chromium's) must not stall the whole walk.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Overall budget for one tree walk / operation.
const OP_TIMEOUT: Duration = Duration::from_secs(25);
/// Startup may run before `serve` binds its socket or MCP reads stdin. A
/// reachable but wedged accessibility bus must not hold either entry point
/// forever. The worker is deliberately left running after this readiness
/// budget so a late registry reply can still establish the process-lifetime
/// listener.
const LISTENER_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

tokio::task_local! {
    /// Wall-clock deadline for the AT-SPI operation currently running on the
    /// runtime. Every [`call`] clamps its per-call timeout to what is left of
    /// it, so a 1 s tool budget really is 1 s even when a single node would
    /// otherwise be allowed the full [`CALL_TIMEOUT`]. Set via [`bounded_for`]
    /// / [`walk_tree_bounded_with_timeout`]; absent for legacy callers.
    static OP_DEADLINE: tokio::time::Instant;
}

/// Time left on the current operation's deadline, or [`CALL_TIMEOUT`] when no
/// deadline is in scope.
fn remaining_budget() -> Duration {
    OP_DEADLINE
        .try_with(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .unwrap_or(CALL_TIMEOUT)
}

/// True once the current operation's deadline has passed (never for callers
/// that did not set one).
fn deadline_passed() -> bool {
    OP_DEADLINE
        .try_with(|deadline| tokio::time::Instant::now() >= *deadline)
        .unwrap_or(false)
}

/// Run `fut` with [`CALL_TIMEOUT`] (clamped to the operation deadline when one
/// is in scope); `None` on timeout so the caller can skip the node and keep
/// walking rather than blocking forever.
async fn call<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    let budget = remaining_budget().min(CALL_TIMEOUT);
    if budget.is_zero() {
        return None;
    }
    tokio::time::timeout(budget, fut).await.ok()
}

/// Run a *synchronous, blocking* closure (typically a raw X11 round-trip via
/// `x11rb::RustConnection`, which has no built-in socket timeout) off the
/// current task and bound how long the caller waits for it.
///
/// This exists because `tokio::time::timeout` around an `.await` only helps
/// when the wrapped future actually yields control back to the runtime at
/// its await points; a plain synchronous call made directly inside an async
/// fn (no `.await` inside it) blocks the executor thread for its full
/// duration regardless of any `timeout`/`timeout_at` wrapped around the
/// *outer* future, because that outer future never gets polled again until
/// the blocking call returns. `get_window_state`'s bounds phase hit exactly
/// this on LibreOffice: `window_to_screen_offset`/`x11_window_origin` open a
/// fresh X11 connection and block on `GetGeometry`/`TranslateCoordinates`
/// replies with no timeout, and when the X server is slow to answer (busy
/// servicing LibreOffice's own heavy repaint traffic) that single call could
/// run for many seconds — well past `timeout_ms` — with none of the AT-SPI
/// D-Bus deadline machinery able to see or bound it (#42).
///
/// `spawn_blocking` moves the closure to a dedicated blocking-pool thread so
/// the timeout here can make the *caller* proceed with `None`; the orphaned
/// thread may still be blocked on the X server afterward (there is no way to
/// cancel a live X11 socket read), but that no longer holds up the walk or
/// consumes the operation's wall-clock budget.
async fn bounded_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let budget = remaining_budget().min(CALL_TIMEOUT);
    if budget.is_zero() {
        return None;
    }
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(value)) => Some(value),
        _ => None,
    }
}

async fn before_snapshot_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl std::future::Future<Output = T>,
) -> std::result::Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout_at(deadline, work).await
}

/// Drive an AT-SPI op `work` on the runtime, bounded by [`OP_TIMEOUT`].
///
/// Individual interface calls are each bounded by [`call`], and `app_for_pid` /
/// `collect_visited` carry their own deadlines — but not every internal await is
/// wrapped (e.g. the EditableText writes in `write_into_editable`, the proxy
/// builds in `app_for_pid`), and an app that holds a modal grab can leave one of
/// those unwrapped round-trips pending indefinitely. `walk_tree` already guards
/// itself this way; this helper applies the same backstop to every other public
/// entry point so a modal/wedged app can never hang the caller (the daemon, an
/// MCP client) past OP_TIMEOUT (#1936). On timeout it yields `on_timeout` — the
/// graceful "couldn't complete" value, so e.g. `type_text` falls back to XTEST.
fn bounded<T>(
    work: impl std::future::Future<Output = Result<T>>,
    on_timeout: impl FnOnce() -> Result<T>,
) -> Result<T> {
    bounded_for(OP_TIMEOUT, work, on_timeout)
}

/// [`bounded`] with an explicit budget. The budget is also installed as the
/// operation deadline, so every inner [`call`] shortens itself to what is
/// left and the walk returns *partial* results before the outer timeout fires
/// (the outer timeout is a backstop for un-wrapped awaits only).
fn bounded_for<T>(
    budget: Duration,
    work: impl std::future::Future<Output = Result<T>>,
    on_timeout: impl FnOnce() -> Result<T>,
) -> Result<T> {
    runtime().block_on(async move {
        let deadline = tokio::time::Instant::now() + budget;
        let backstop = deadline + Duration::from_millis(500);
        match tokio::time::timeout_at(backstop, OP_DEADLINE.scope(deadline, work)).await {
            Ok(r) => r,
            Err(_) => on_timeout(),
        }
    })
}

/// Budget for AT-SPI queries that run *inside* an input action (hit-testing a
/// pixel click, classifying the focused widget, locating an editable). These
/// must never stall the action on a slow app: a miss simply falls through to
/// the next delivery rung.
pub(crate) const INPUT_QUERY_BUDGET: Duration = Duration::from_millis(1500);
/// Budget for resolving an element index against a fresh walk when no cached
/// snapshot answers it. Larger than [`INPUT_QUERY_BUDGET`] because the caller
/// named a specific element and a miss is an error, not a fallback.
pub(crate) const INDEX_RESOLVE_BUDGET: Duration = Duration::from_secs(6);

/// Emit a one-line diagnostic to stderr when `PINCER_ATSPI_DEBUG` is set. The
/// driver's stderr is surfaced in the test logs, so this is how we see what the
/// native walk actually found in CI.
fn dbg_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PINCER_ATSPI_DEBUG").is_some())
}
macro_rules! dlog {
    ($($arg:tt)*) => {
        if dbg_enabled() { eprintln!("[pincer-atspi] {}", format!($($arg)*)); }
    };
}

/// Shared multi-threaded Tokio runtime for the blocking AT-SPI entry points.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build AT-SPI tokio runtime")
    })
}

static SHARED_CONNECTION: tokio::sync::OnceCell<AccessibilityConnection> =
    tokio::sync::OnceCell::const_new();

/// Keep one AT-SPI connection and registry registration alive for the daemon
/// lifetime. WebKitGTK only publishes its WebProcess accessibility subtree
/// while the registry reports an interested listener.
async fn shared_connection() -> Result<&'static AccessibilityConnection> {
    SHARED_CONNECTION
        .get_or_try_init(|| async {
            let conn = AccessibilityConnection::new()
                .await
                .map_err(|error| anyhow!("AT-SPI connect failed: {error}"))?;
            if let Err(error) = conn.add_registry_event::<atspi::ObjectEvents>().await {
                dlog!("AT-SPI object-event registration failed: {error}");
            }
            // Subscribe to focus changes so `focused_is_editable` / typing can
            // find the focused widget in O(1) instead of walking the tree.
            if let Err(error) = conn
                .register_event::<atspi::events::object::StateChangedEvent>()
                .await
            {
                dlog!("AT-SPI state-changed subscription failed: {error}");
            }
            // Grids (LibreOffice Calc) keep the focus on the table and report
            // the current cell as its active descendant.
            if let Err(error) = conn
                .register_event::<atspi::events::object::ActiveDescendantChangedEvent>()
                .await
            {
                dlog!("AT-SPI active-descendant subscription failed: {error}");
            }
            Ok(conn)
        })
        .await
        .inspect(|conn| {
            static TRACKER: OnceLock<()> = OnceLock::new();
            TRACKER.get_or_init(|| spawn_focus_tracker(conn));
        })
}

/// Object path of the accessible that most recently reported `focused=true`,
/// keyed by the emitting application's bus name. Maintained by
/// [`spawn_focus_tracker`]; consulted by the focus-aware typing paths.
fn focus_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static MAP: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Object path of the active descendant most recently reported per bus
/// (`object:active-descendant-changed`): the current cell of a focused grid.
fn active_descendant_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static MAP: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record the focused-object path for `bus` from one `object:state-changed:focused` event.
fn note_focus_event(bus: &str, path: &str, enabled: bool) {
    let mut map = focus_map().lock().unwrap();
    if enabled {
        map.insert(bus.to_owned(), path.to_owned());
    } else if map.get(bus).is_some_and(|current| current == path) {
        map.remove(bus);
    }
}

/// Consume the shared connection's event stream for the daemon lifetime,
/// remembering the last focused accessible per application bus.
fn spawn_focus_tracker(conn: &'static AccessibilityConnection) {
    runtime().spawn(async move {
        use futures_util::StreamExt;
        let stream = conn.event_stream();
        futures_util::pin_mut!(stream);
        while let Some(event) = stream.next().await {
            if let Ok(atspi::Event::Object(atspi::ObjectEvents::ActiveDescendantChanged(active))) =
                &event
            {
                if let Some(bus) = active.item.name_as_str() {
                    active_descendant_map()
                        .lock()
                        .unwrap()
                        .insert(bus.to_owned(), active.descendant.path_as_str().to_owned());
                }
                continue;
            }
            let Ok(atspi::Event::Object(atspi::ObjectEvents::StateChanged(changed))) = event else {
                continue;
            };
            if changed.state != State::Focused {
                continue;
            }
            let Some(bus) = changed.item.name_as_str() else {
                continue;
            };
            note_focus_event(bus, changed.item.path_as_str(), changed.enabled);
        }
        dlog!("AT-SPI focus tracker stream ended");
    });
}

/// The accessible that currently holds keyboard focus inside `pid`'s
/// application, resolved from the focus-event log (no tree walk). `None` when
/// no focus event was seen for the app or the recorded object no longer
/// reports `Focused`.
async fn focused_accessible_via_events<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
) -> Option<(AccessibleProxy<'a>, ObjectRef)> {
    let app = app_for_pid(conn, pid).await.ok()??;
    let bus = app.inner().destination().to_string();
    let path = focus_map().lock().unwrap().get(&bus).cloned()?;
    let oref = RawObjectRef {
        name: bus.clone(),
        path: path.clone(),
    };
    let acc = call(accessible_for(conn, &oref)).await?.ok()?;
    let state = call(acc.get_state()).await?.ok()?;
    if !state.contains(State::Focused) {
        dlog!("focus log for {bus} is stale ({path} no longer focused)");
        return None;
    }
    Some((acc, ObjectRef { bus, path }))
}

/// Establish the process-lifetime listener before accessibility-aware apps are
/// launched. Idempotent; later calls reuse the same connection.
pub fn ensure_listener_active() -> Result<()> {
    wait_for_listener_startup(LISTENER_STARTUP_TIMEOUT, || {
        runtime().block_on(async { shared_connection().await.map(|_| ()) })
    })
}

fn wait_for_listener_startup(
    timeout: Duration,
    connect: impl FnOnce() -> Result<()> + Send + 'static,
) -> Result<()> {
    let (completed_tx, completed_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pincer-atspi-listener".into())
        .spawn(move || {
            let _ = completed_tx.send(connect());
        })
        .map_err(|error| anyhow!("could not spawn AT-SPI listener initialization: {error}"))?;

    match completed_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "AT-SPI listener initialization did not complete within {} ms; continuing in the background",
            timeout.as_millis()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow!("AT-SPI listener initialization thread panicked"))
        }
    }
}

#[cfg(test)]
mod listener_startup_tests {
    use super::wait_for_listener_startup;
    use anyhow::anyhow;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn healthy_listener_initialization_completes_before_readiness_returns() {
        let initialized = Arc::new(AtomicBool::new(false));
        let initialized_in_worker = initialized.clone();

        wait_for_listener_startup(Duration::from_secs(1), move || {
            initialized_in_worker.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect("healthy listener startup");

        assert!(initialized.load(Ordering::SeqCst));
    }

    #[test]
    fn unreachable_listener_initialization_returns_its_error() {
        let error = wait_for_listener_startup(Duration::from_secs(1), || {
            Err(anyhow!("synthetic AT-SPI connection failure"))
        })
        .expect_err("unreachable listener must fail");

        assert!(error
            .to_string()
            .contains("synthetic AT-SPI connection failure"));
    }

    #[test]
    fn stalled_listener_is_bounded_and_keeps_initializing_in_background() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_in_worker = gate.clone();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_in_worker = completed.clone();
        let started_at = Instant::now();

        let error = wait_for_listener_startup(Duration::from_millis(50), move || {
            let (lock, ready) = &*gate_in_worker;
            let released = lock.lock().expect("listener gate lock");
            drop(
                ready
                    .wait_while(released, |released| !*released)
                    .expect("listener gate wait"),
            );
            completed_in_worker.store(true, Ordering::SeqCst);
            Ok(())
        })
        .expect_err("stalled listener must exceed the readiness budget");

        assert!(error.to_string().contains("continuing in the background"));
        assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "stalled initialization exceeded its bounded wait"
        );
        let (lock, ready) = &*gate;
        *lock.lock().expect("release listener gate") = true;
        ready.notify_one();

        let completion_deadline = Instant::now() + Duration::from_secs(1);
        while !completed.load(Ordering::SeqCst) && Instant::now() < completion_deadline {
            std::thread::yield_now();
        }
        assert!(
            completed.load(Ordering::SeqCst),
            "timed-out initialization worker was not allowed to finish"
        );
    }
}

/// The `value` of one walked node: its Value-interface value, else, for an
/// editable text widget without a Value interface, its non-empty Text
/// content (the typed string). A SpinButton implements Text and Value, so it
/// keeps its numeric value. Callers pass `has_editable: false` for password
/// fields.
fn editable_text_value(
    has_editable: bool,
    has_value: bool,
    value: Option<String>,
    text_content: &str,
) -> Option<String> {
    if value.is_some() || has_value || !has_editable || text_content.is_empty() {
        return value;
    }
    Some(text_content.to_owned())
}

#[cfg(test)]
mod editable_text_value_tests {
    use super::editable_text_value;

    #[test]
    fn editable_text_reports_its_content_as_value() {
        assert_eq!(
            editable_text_value(true, false, None, "jev-use native note").as_deref(),
            Some("jev-use native note")
        );
    }

    #[test]
    fn empty_or_non_editable_text_has_no_value() {
        assert_eq!(editable_text_value(true, false, None, ""), None);
        assert_eq!(
            editable_text_value(false, false, None, "static label"),
            None
        );
    }

    #[test]
    fn value_interface_wins() {
        assert_eq!(
            editable_text_value(true, true, Some("3".into()), "3.0").as_deref(),
            Some("3")
        );
        assert_eq!(editable_text_value(true, true, None, "3.0"), None);
    }
}

/// A node discovered during the pre-order walk, with its proxy retained so the
/// per-index operations can act on it without re-walking the tree.
struct Visited<'a> {
    depth: usize,
    role: String,
    /// Display text: the accessible `name`, or — for editable/text widgets that
    /// expose no name — the Text-interface content (where typed text lives).
    name: String,
    /// AT-SPI `Description`, read for controls only: Qt publishes a control's
    /// tooltip there ("Play"/"Pause", "Look in"), GTK its tooltip text.
    description: String,
    value: Option<String>,
    checked: Option<bool>,
    enabled: Option<bool>,
    selected: Option<bool>,
    /// States the Markdown row shows ([`row_state_marks`]), so a re-read or
    /// a `since` diff shows a toggle.
    marks: Vec<&'static str>,
    /// The field's whole length in characters (AT-SPI `CharacterCount`) when
    /// `value` is its text and holds only the first [`TEXT_READ_CHARS`].
    value_total_chars: Option<usize>,
    selectable: bool,
    /// False when the toolkit's state set is known and marks the widget
    /// neither `Showing` nor `Visible`: it exists in the tree but is hidden
    /// (a widget on an unmapped page). Unknown state sets count as showing.
    showing: bool,
    /// `Visible` but not `Showing`: listed and indexed, but not on screen
    /// right now (GTK3 drops `Showing` for a control scrolled out of its
    /// viewport, and for a revealer child such as Nautilus' hover-only eject
    /// buttons). The tree says so, so the caller can scroll it into view or
    /// prefer an on-screen control.
    offscreen: bool,
    /// Children this walk deliberately did not enumerate: the items of a
    /// menu that is not open (every closed LibreOffice menu would otherwise
    /// put ~500 frameless entries ahead of the document).
    collapsed_children: Option<usize>,
    actions: Vec<String>,
    has_editable: bool,
    has_value: bool,
    has_component: bool,
    focused: bool,
    /// True when an ancestor is a web document (e.g. role "document web"),
    /// i.e. this node is page content rather than browser chrome.
    in_web_doc: bool,
    /// True when this node is exported by a separate WebKit WebProcess bus.
    /// Chromium keeps its document on the application's ordinary AT-SPI bus,
    /// where descendant Window extents already include the document origin.
    on_web_process_bus: bool,
    /// Position of the application top-level (frame/window) this node descends
    /// from, in `app.get_children()` order. AT-SPI exposes one application per
    /// process, so a multi-window app publishes every window's controls in one
    /// tree; this is what lets a caller that named an exact native window prove
    /// which of those windows a node actually lives in.
    frame_ordinal: usize,
    /// D-Bus identity (bus name + object path) of this accessible, so a later
    /// per-index action can re-open it directly from a cached snapshot instead
    /// of re-walking the application.
    object_ref: ObjectRef,
    identity: Option<AtspiIdentity>,
    acc: AccessibleProxy<'a>,
}

/// Owned D-Bus identity of an accessible object: `(bus name, object path)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRef {
    pub bus: String,
    pub path: String,
}

/// Why a tree walk stopped before exhausting the application's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncation {
    /// Machine-readable reason: `timeout`, `node_budget`, `app_unresponsive`,
    /// `app_lookup_timeout`.
    pub reason: &'static str,
    /// Nodes fully visited before the walk stopped.
    pub visited: usize,
    /// Nodes discovered but not yet visited when the walk stopped (lower bound
    /// on what is missing — their descendants are unknown).
    pub pending: usize,
}

/// Result of one bounded collection pass.
struct Collected<'a> {
    visited: Vec<Visited<'a>>,
    scoped_frame: Option<usize>,
    truncation: Option<Truncation>,
    /// The visited nodes are a role search's matches under the scoped
    /// top-level, not a walk of it.
    searched_roles: bool,
}

/// Role names that denote embedded web/document content. An editable beneath
/// one of these is page content (the field a user means when typing into a
/// background browser) rather than browser chrome like the address bar.
fn is_document_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.contains("document") || r == "embedded"
}

fn is_web_process_bus(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("webkit") || name.contains("webprocess")
}

/// Build an `AccessibleProxy` for an arbitrary (bus name, path) in the tree.
/// Uses owned `String`s for destination/path so the resulting `BusName`/
/// `ObjectPath` are `'static` and the proxy borrows only the connection.
///
/// `cache_properties(No)` is load-bearing, not just an optimization: with the
/// zbus default (`Lazily`) the first property read on the proxy — our
/// `acc.name()` during the walk — makes zbus issue
/// `org.freedesktop.DBus.Properties.GetAll` (one argument: the interface name)
/// to warm the cache. Qt5's AT-SPI bridge (`AtSpiAdaptor::handleMessage`)
/// assumes every Properties call is `Get`/`Set` and unconditionally reads
/// `message.arguments().at(1)`; for a one-argument `GetAll` that index is out
/// of range, so the following `QVariant::toString()` dereferences garbage and
/// the Qt5 app *segfaults* (observed crash: `libQt5Core` via
/// `AtSpiAdaptor::handleMessage`). Qt6's bridge handles `GetAll`, which is why
/// only Qt5 crashed. Forcing `No` makes zbus issue per-property `Get` calls
/// (two arguments) instead, which Qt5 handles correctly — so the Qt5 window can
/// be walked and written without killing the app. Other toolkits are
/// unaffected (they already tolerate `GetAll`), and the sub-interface proxies
/// from `proxies()` already use `CacheProperties::No`.
/// Build the proxy on the shared bus connection, never the peer socket.
async fn accessible_via_bus<'a>(
    conn: &'a AccessibilityConnection,
    oref: &RawObjectRef,
) -> Result<AccessibleProxy<'a>> {
    AccessibleProxy::builder(conn.connection())
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .destination(oref.name.clone())
        .map_err(|e| anyhow!("bad a11y destination: {e}"))?
        .path(oref.path.clone())
        .map_err(|e| anyhow!("bad a11y path: {e}"))?
        .build()
        .await
        .map_err(|e| anyhow!("AccessibleProxy build failed: {e}"))
}

/// The interface proxies of one object whose interface set is already
/// known. atspi's `ProxyExt::proxies` asks GetInterfaces again before it
/// hands out the same proxies; this builds them the way it does (uncached,
/// on the object's connection, destination and path), from the set the
/// caller read.
struct KnownInterfaces<'p, 'a> {
    acc: &'p AccessibleProxy<'a>,
    interfaces: atspi::InterfaceSet,
}

impl<'p, 'a> KnownInterfaces<'p, 'a> {
    fn new(acc: &'p AccessibleProxy<'a>, interfaces: atspi::InterfaceSet) -> Self {
        Self { acc, interfaces }
    }

    /// Like `Proxies::action` and the rest: `InterfaceNotAvailable` when the
    /// set lacks `interface`.
    async fn build<T>(&self, interface: Interface, name: &'static str) -> atspi::Result<T>
    where
        T: From<atspi::zbus::Proxy<'a>> + atspi::zbus::proxy::Defaults,
    {
        if !self.interfaces.contains(interface) {
            return Err(atspi::AtspiError::InterfaceNotAvailable(name));
        }
        let inner = self.acc.inner();
        Ok(atspi::zbus::proxy::Builder::<T>::new(inner.connection())
            .cache_properties(atspi::zbus::proxy::CacheProperties::No)
            .destination(inner.destination())?
            .path(inner.path())?
            .build()
            .await?)
    }

    async fn action(&self) -> atspi::Result<atspi::proxy::action::ActionProxy<'a>> {
        self.build(Interface::Action, "Action").await
    }

    async fn value(&self) -> atspi::Result<atspi::proxy::value::ValueProxy<'a>> {
        self.build(Interface::Value, "Value").await
    }

    async fn text(&self) -> atspi::Result<atspi::proxy::text::TextProxy<'a>> {
        self.build(Interface::Text, "Text").await
    }
}

async fn accessible_for<'a>(
    conn: &'a AccessibilityConnection,
    oref: &RawObjectRef,
) -> Result<AccessibleProxy<'a>> {
    // Always the session a11y bus. WebKitGTK's late WebProcess children use a
    // well-known name that only the explicit bus builder can address, and the
    // crate's peer-to-peer route is disabled (see Cargo.toml).
    accessible_via_bus(conn, oref).await
}

/// AT-SPI's `(so)` object references are documented as unique bus names, but
/// WebKitGTK uses its well-known WebProcess name for the embedded web tree.
/// Keep the wire values as strings while walking so zbus does not reject that
/// validly addressable well-known name before we can call it.
#[derive(Clone, Debug)]
struct RawObjectRef {
    name: String,
    path: String,
}

impl RawObjectRef {
    fn from_atspi(oref: &atspi::ObjectRefOwned) -> Option<Self> {
        Some(Self {
            name: oref.name_as_str()?.to_owned(),
            path: oref.path_as_str().to_owned(),
        })
    }
}

/// Containers with more children than this are not expanded. LibreOffice
/// Calc's spreadsheet accessible reports the whole sheet (rows x columns, in
/// the billions); a `GetChildren` on it makes the application enumerate every
/// cell, which stalls its accessibility bridge for minutes and wedges every
/// later call from this connection behind it. pyatspi/Orca never enumerate
/// such containers wholesale either. A caller-supplied `max_elements` above
/// this raises the cap (the walk cannot exceed the budget anyway).
pub(crate) const CHILD_ENUMERATION_CAP: usize = 2048;

/// Accessible.ChildCount: one cheap property read that tells a leaf (skip the
/// GetChildren round-trip entirely) from a container that must not be expanded.
async fn raw_child_count(conn: &atspi::zbus::Connection, oref: &RawObjectRef) -> Result<i32> {
    // `cache_properties(No)` is load-bearing (see `accessible_via_bus`): a
    // zbus proxy with the default lazy cache answers this one `ChildCount`
    // read with `Properties.GetAll`, which segfaults Qt5's AT-SPI bridge
    // (VLC died on the first get_window_state walk).
    let proxy = atspi::zbus::proxy::Builder::<atspi::zbus::Proxy<'_>>::new(conn)
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .destination(oref.name.as_str())
        .map_err(|e| anyhow!("bad a11y destination: {e}"))?
        .path(oref.path.as_str())
        .map_err(|e| anyhow!("bad a11y path: {e}"))?
        .interface("org.a11y.atspi.Accessible")
        .map_err(|e| anyhow!("bad a11y interface: {e}"))?
        .build()
        .await
        .map_err(|e| anyhow!("Accessible proxy unavailable: {e}"))?;
    proxy
        .get_property::<i32>("ChildCount")
        .await
        .map_err(|e| anyhow!("Accessible.ChildCount failed: {e}"))
}

/// Pin well-known names to their current unique owner. A missing owner is
/// discovery-only: indexed clicks cannot use an unproven persistent identity.
async fn identity_ref(
    conn: &AccessibilityConnection,
    raw: &RawObjectRef,
    owners: &mut std::collections::HashMap<String, Option<String>>,
) -> Option<RawObjectRef> {
    let owner = if raw.name.starts_with(':') {
        raw.name.clone()
    } else if let Some(owner) = owners.get(&raw.name) {
        owner.clone()?
    } else {
        let owner = async {
            let bus = atspi::zbus::fdo::DBusProxy::new(conn.connection())
                .await
                .ok()?;
            let name = atspi::zbus::names::BusName::try_from(raw.name.as_str()).ok()?;
            call(bus.get_name_owner(name))
                .await?
                .ok()
                .map(|name| name.to_string())
        }
        .await;
        owners.insert(raw.name.clone(), owner.clone());
        owner?
    };
    Some(RawObjectRef {
        name: owner,
        path: raw.path.clone(),
    })
}

/// Read Accessible.GetChildren without deserializing the bus-name field as a
/// `UniqueName`. WebKitGTK's embedded WebProcess exposes a well-known name
/// containing a UUID; D-Bus can address it, but the stricter AT-SPI wrapper
/// rejects it as an invalid unique name.
async fn raw_children(
    conn: &atspi::zbus::Connection,
    oref: &RawObjectRef,
) -> Result<Vec<RawObjectRef>> {
    let proxy = atspi::zbus::Proxy::new(
        conn,
        oref.name.as_str(),
        oref.path.as_str(),
        "org.a11y.atspi.Accessible",
    )
    .await
    .map_err(|e| anyhow!("Accessible proxy unavailable: {e}"))?;
    let refs: Vec<(String, atspi::zbus::zvariant::OwnedObjectPath)> = proxy
        .call("GetChildren", &())
        .await
        .map_err(|e| anyhow!("Accessible.GetChildren failed: {e}"))?;
    Ok(refs
        .into_iter()
        .map(|(name, path)| RawObjectRef {
            name,
            path: path.to_string(),
        })
        .collect())
}

/// Resolve the process id behind an application accessible's D-Bus name.
async fn pid_of(
    dbus: &atspi::zbus::fdo::DBusProxy<'_>,
    oref: &atspi::ObjectRefOwned,
) -> Option<u32> {
    let bus = atspi::zbus::names::BusName::try_from(oref.name_as_str()?.to_owned()).ok()?;
    dbus.get_connection_unix_process_id(bus).await.ok()
}

/// Keep the first matching application as a compatibility fallback, but allow
/// a later registration with a real child tree to win. Some Qt processes
/// publish an empty application object before their populated one (#2678,
/// #2706).
struct ApplicationSelection<T> {
    target_pid: u32,
    fallback: Option<T>,
    populated: Vec<T>,
}

impl<T> ApplicationSelection<T> {
    fn new(target_pid: u32) -> Self {
        Self {
            target_pid,
            fallback: None,
            populated: Vec::new(),
        }
    }

    fn matches_pid(&self, candidate_pid: Option<u32>) -> bool {
        candidate_pid == Some(self.target_pid)
    }

    /// Retain every populated exact-PID candidate so resolution can reject an
    /// ambiguous registry instead of silently choosing whichever entry sorted
    /// first. The first childless candidate remains the compatibility fallback
    /// for applications that genuinely expose no top-level accessibles.
    fn consider_matching(&mut self, candidate: T, has_children: bool) {
        if has_children {
            self.populated.push(candidate);
        } else if self.fallback.is_none() {
            self.fallback = Some(candidate);
        }
    }

    fn into_selected(mut self) -> std::result::Result<Option<T>, usize> {
        match self.populated.len() {
            0 => Ok(self.fallback),
            1 => Ok(self.populated.pop()),
            count => Err(count),
        }
    }
}

/// An application accessible plus the top-level children its selection probe
/// already fetched (`None` when that probe timed out or failed), so callers do
/// not pay a second `GetChildren` round-trip — on an application that never
/// answers it, that was a second full CALL_TIMEOUT.
struct ResolvedApp<'a> {
    app: AccessibleProxy<'a>,
    children: Option<Vec<atspi::ObjectRefOwned>>,
}

/// Locate the application accessible whose backing process is `pid`.
async fn app_for_pid<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
) -> Result<Option<AccessibleProxy<'a>>> {
    Ok(resolve_app_for_pid(conn, pid).await?.map(|r| r.app))
}

/// Best-effort executable/process name for `pid`, read from `/proc/<pid>/comm`
/// (Linux only; falls back to `None` on any error, e.g. the process already
/// exited or `/proc` is unavailable in a sandbox).
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn resolve_app_for_pid<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
) -> Result<Option<ResolvedApp<'a>>> {
    let zconn = conn.connection();
    // Every AT-SPI round-trip below can block on an app whose main loop isn't
    // servicing D-Bus — most commonly one holding a modal grab (an "Add/Edit/
    // Preferences" dialog). Without a bound, `GetConnectionUnixProcessID` stalls
    // on the zbus default (~25s) per such app, which made get_window_state and
    // type_text hang on real apps (#1936). Bound each step with CALL_TIMEOUT and
    // skip/return instead of stalling — for type_text this returns fast so the
    // tool falls back to XTEST, which still types into the focused dialog field.
    let root = match call(conn.root_accessible_on_registry()).await {
        Some(Ok(r)) => r,
        Some(Err(e)) => return Err(anyhow!("registry root unavailable: {e}")),
        None => {
            dlog!("registry root lookup timed out");
            return Ok(None);
        }
    };
    let dbus = atspi::zbus::fdo::DBusProxy::new(zconn)
        .await
        .map_err(|e| anyhow!("DBus proxy unavailable: {e}"))?;

    let apps = match call(root.get_children()).await {
        Some(r) => r.unwrap_or_default(),
        None => {
            dlog!("registry get_children timed out");
            return Ok(None);
        }
    };
    dlog!(
        "registry root has {} application(s); seeking pid {pid}",
        apps.len()
    );
    let mut selection = ApplicationSelection::new(pid);
    // Retained for a name-based fallback pass below (#2706-adjacent gap): a
    // blind prelaunched app whose AT-SPI application object registered under
    // a *different* pid than the one the caller targets (e.g. a launcher
    // shim re-execs into the real binary, or a toolkit forks once before
    // registering with the a11y bus) is otherwise invisible even though its
    // process name matches exactly.
    let mut other_apps: Vec<(atspi::ObjectRefOwned, u32)> = Vec::new();
    for child in apps {
        // A modal-grabbed app can't answer the pid query; skip it after
        // CALL_TIMEOUT rather than blocking the whole walk on it.
        let cpid = match call(pid_of(&dbus, &child)).await {
            Some(p) => p,
            None => {
                dlog!(
                    "  pid_of timed out for bus={:?}, skipping",
                    child.name_as_str()
                );
                continue;
            }
        };
        dlog!("  app bus={:?} pid={:?}", child.name_as_str(), cpid);
        if !selection.matches_pid(cpid) {
            if let Some(cpid) = cpid {
                other_apps.push((child, cpid));
            }
            continue;
        }
        let child = match RawObjectRef::from_atspi(&child) {
            Some(child) => child,
            None => continue,
        };
        let app = match call(accessible_for(conn, &child)).await {
            Some(Ok(app)) => app,
            Some(Err(error)) => {
                dlog!("  accessible_for failed for pid {pid}: {error:#}");
                continue;
            }
            None => {
                dlog!("  accessible_for timed out for pid {pid}");
                continue;
            }
        };
        let children = match call(app.get_children()).await {
            Some(Ok(children)) => Some(children),
            Some(Err(error)) => {
                dlog!("  get_children failed for pid {pid}: {error:#}");
                None
            }
            None => {
                dlog!("  get_children timed out for pid {pid}");
                None
            }
        };
        let has_children = children.as_ref().is_some_and(|c| !c.is_empty());
        dlog!("  matching app has_children={has_children}");
        selection.consider_matching(ResolvedApp { app, children }, has_children);
    }
    match selection.into_selected() {
        Ok(Some(app)) => Ok(Some(app)),
        Ok(None) => {
            dlog!("no application accessible matched pid {pid} by exact pid; trying WM_CLASS/process-name fallback");
            // Exact-pid match failed. Fall back to matching by process
            // executable name across every OTHER AT-SPI-registered
            // application; accept it only when exactly one candidate shares
            // the target's process name, so an ambiguous match still
            // refuses rather than guessing.
            let Some(target_name) = process_name(pid) else {
                return Ok(None);
            };
            let mut name_matches: Vec<(atspi::ObjectRefOwned, u32)> = Vec::new();
            for (child, cpid) in other_apps {
                if process_name(cpid).as_deref() == Some(target_name.as_str()) {
                    name_matches.push((child, cpid));
                }
            }
            if name_matches.len() != 1 {
                dlog!(
                    "process-name fallback for pid {pid} ({target_name}) found {} candidate(s); refusing",
                    name_matches.len()
                );
                return Ok(None);
            }
            let (child, fallback_pid) = name_matches.into_iter().next().unwrap();
            dlog!("process-name fallback matched pid {pid} ({target_name}) to AT-SPI app pid {fallback_pid}");
            let child = match RawObjectRef::from_atspi(&child) {
                Some(child) => child,
                None => return Ok(None),
            };
            let app = match call(accessible_for(conn, &child)).await {
                Some(Ok(app)) => app,
                _ => return Ok(None),
            };
            let children = match call(app.get_children()).await {
                Some(Ok(children)) => Some(children),
                _ => None,
            };
            Ok(Some(ResolvedApp { app, children }))
        }
        Err(count) => Err(anyhow!(
            "ambiguous AT-SPI application selection for pid {pid}: \
             {count} populated application accessibles matched"
        )),
    }
}

/// Depth-first, pre-order walk of window `xid` (`0`: every window of the
/// application). Mirrors the old pyatspi `walk`/`collect` traversal, and is
/// the walk a `get_window_state` read of `xid` numbers its elements by, so an
/// element index from that read names the same element here. There is no
/// application-wide variant for indices: it numbered from the application's
/// first top-level, where index 8 of a dialog's read is some control of the
/// main window.
async fn collect_window_visited<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
) -> Result<Option<Vec<Visited<'a>>>> {
    // A window that has closed has no index space left, but the walk would
    // still find one: with a single top-level left it resolves any window id
    // to that one, and otherwise walks every top-level. An index from the
    // closed window's read then named another window's element (a closed
    // dialog's [8] resolved to the main window's list item "Health" on the
    // Qt probe, 2026-11-09).
    if xid != 0 && !crate::wayland::is_wayland() {
        let presence = bounded_blocking(move || crate::x11::window_presence(xid))
            .await
            .flatten();
        if window_index_space_is_gone(presence) {
            return Err(WindowClosed { xid }.into());
        }
    }
    collect_visited_bounded(conn, pid, xid, None, None)
        .await
        .map(|walked| walked.map(|collected| collected.visited))
}

/// An element index named a window that has closed: its read's indices name
/// nothing now. Tools report it as a stale element token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowClosed {
    pub xid: u64,
}

impl std::fmt::Display for WindowClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "window {} has closed, so element indices from its read name nothing; nothing \
             was done. Read the window that is open now with get_window_state",
            self.xid
        )
    }
}

impl std::error::Error for WindowClosed {}

/// Whether an index walk of a window must be refused: the X server says the
/// window was destroyed, or it is a popup that closed. A window that still
/// exists (even minimized), or one the server could not be asked about,
/// keeps the walk.
fn window_index_space_is_gone(presence: Option<crate::x11::WindowPresence>) -> bool {
    matches!(
        presence,
        Some(crate::x11::WindowPresence::Gone | crate::x11::WindowPresence::ClosedPopup)
    )
}

/// Whether an AT-SPI role name is a top-level window's. Qt's `GetRoleName`
/// answers "alert message" (its name for QAccessible::AlertMessage) for a
/// QMessageBox, where libatspi and GTK say "alert"; without it a message box
/// never correlated with its X window and its read walked the whole
/// application (seen on the Qt probe, 2026-11-09).
pub(crate) fn is_window_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "frame" | "window" | "dialog" | "alert" | "alert message" | "file chooser"
    )
}

/// Screen-space distance between an AT-SPI frame's extents and a native
/// window's geometry. Lower is a better correspondence; `None` when the frame
/// reports no usable extents.
fn frame_geometry_distance(
    frame: (i32, i32, i32, i32),
    window: &crate::x11::WindowInfo,
) -> Option<u64> {
    let (fx, fy, fw, fh) = frame;
    if fw <= 0 || fh <= 0 {
        return None;
    }
    let dx = i64::from(fx) - i64::from(window.x);
    let dy = i64::from(fy) - i64::from(window.y);
    let dw = i64::from(fw) - i64::from(window.width);
    let dh = i64::from(fh) - i64::from(window.height);
    Some(dx.unsigned_abs() + dy.unsigned_abs() + dw.unsigned_abs() + dh.unsigned_abs())
}

/// Server-side decorations offset a frame's reported origin from the native
/// window's outer geometry, so an exact match is not required. The correlation
/// must still be unambiguous: the best candidate has to be within this budget
/// AND beat the runner-up by [`FRAME_MATCH_MARGIN_PX`].
const FRAME_MATCH_TOLERANCE_PX: u64 = 160;

/// How decisively the best frame must beat the second-best. Two windows of
/// genuinely similar geometry are not disambiguated by this heuristic, and a
/// caller that needs proof of window identity must get a refusal instead of a
/// coin flip.
const FRAME_MATCH_MARGIN_PX: u64 = 24;

/// An AT-SPI frame ordinal and its screen extents (x, y, width, height).
type FrameExtents = (usize, (i32, i32, i32, i32));

/// Pick the unique application top-level that corresponds to native window
/// `xid`, or `None` when the correspondence cannot be proven.
///
/// AT-SPI publishes one application per process: every window of a multi-window
/// app shares a single tree, and the protocol exposes no window handle to join
/// on. Geometry is the available bridge — `Component.GetExtents` in screen
/// coordinates against the X11 outer geometry the caller already named. This
/// refuses ties rather than guessing, because callers use the result to decide
/// which window they are about to act inside.
///
/// One exception to the tie rule: Qt publishes a QMessageBox as two top-level
/// accessibles with the same name and extents, both driving the one dialog
/// (pressing Cancel in either closed it, checked 2026-11-09). When the tied
/// frames report identical extents and the process has no other native
/// window of that geometry (`native_twins` is 0), they are one window
/// published twice, and the first is as good as the other. Two real windows
/// stacked exactly still refuse, because each one is a native twin of the
/// other.
fn correlate_frame_to_window(
    candidates: &[FrameExtents],
    window: &crate::x11::WindowInfo,
    native_twins: usize,
) -> Option<usize> {
    // Each candidate with usable extents, with its distance to the window.
    let mut scored: Vec<(u64, FrameExtents)> = candidates
        .iter()
        .filter_map(|candidate| {
            frame_geometry_distance(candidate.1, window).map(|distance| (distance, *candidate))
        })
        .collect();
    scored.sort_by_key(|(distance, (ordinal, _))| (*distance, *ordinal));
    let (best_distance, (best_ordinal, best_extents)) = *scored.first()?;
    if best_distance > FRAME_MATCH_TOLERANCE_PX {
        return None;
    }
    let tied: Vec<&(u64, FrameExtents)> = scored
        .iter()
        .skip(1)
        .filter(|(distance, _)| distance.saturating_sub(best_distance) < FRAME_MATCH_MARGIN_PX)
        .collect();
    let published_twice = native_twins == 0
        && tied
            .iter()
            .all(|(_, (_, extents))| *extents == best_extents);
    if !tied.is_empty() && !published_twice {
        return None;
    }
    Some(best_ordinal)
}

/// How many of `windows` other than `window` have its geometry (within the
/// tie margin): real windows a duplicate frame could also belong to.
fn native_twins(window: &crate::x11::WindowInfo, windows: &[crate::x11::WindowInfo]) -> usize {
    windows
        .iter()
        .filter(|other| other.xid != window.xid)
        .filter(|other| {
            frame_geometry_distance(
                (
                    other.x,
                    other.y,
                    i32::try_from(other.width).unwrap_or(i32::MAX),
                    i32::try_from(other.height).unwrap_or(i32::MAX),
                ),
                window,
            )
            .is_some_and(|distance| distance < FRAME_MATCH_MARGIN_PX)
        })
        .count()
}

#[derive(Debug)]
struct HyprlandFrame {
    ordinal: usize,
    title: String,
    browser_root: bool,
    size: Option<(i32, i32)>,
}

fn chromium_title_matches(accessible: &str, native: &str) -> bool {
    if native.is_empty() {
        return false;
    }
    accessible == native
        || accessible
            .strip_prefix(native)
            .and_then(|suffix| suffix.strip_prefix(" - "))
            .is_some_and(|profile| {
                !profile.trim().is_empty() && !profile.chars().any(char::is_control)
            })
}

fn correlate_hyprland_frame(
    frames: &[HyprlandFrame],
    clients: &[(&str, u32, u32)],
    target: usize,
) -> Option<usize> {
    let &(title, _, _) = clients.get(target)?;
    if title.is_empty() || clients.iter().filter(|client| client.0 == title).count() != 1 {
        return None;
    }
    let matches = |frame: &HyprlandFrame, client: &(&str, u32, u32)| {
        frame.title == client.0
            || (frame.browser_root
                && frame.size.is_some_and(|(width, height)| {
                    width > 0
                        && height > 0
                        && i64::from(width) == i64::from(client.1)
                        && i64::from(height) == i64::from(client.2)
                })
                && chromium_title_matches(&frame.title, client.0))
    };
    let mut candidates = frames
        .iter()
        .filter(|frame| matches(frame, &clients[target]));
    let frame = candidates.next()?;
    if candidates.next().is_some()
        || clients
            .iter()
            .filter(|client| matches(frame, client))
            .count()
            != 1
    {
        return None;
    }
    Some(frame.ordinal)
}

#[cfg(test)]
mod hyprland_frame_tests {
    use super::*;

    fn frame(title: &str) -> HyprlandFrame {
        HyprlandFrame {
            ordinal: 7,
            title: title.into(),
            browser_root: true,
            size: Some((800, 600)),
        }
    }

    #[test]
    fn binds_unique_chromium_profile_title() {
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Pincer Test")],
                &[("Page - Google Chrome", 800, 600), ("Other", 800, 600)],
                0,
            ),
            Some(7)
        );
    }

    #[test]
    fn rejects_generic_prefixes_unknown_roots_and_wrong_sizes() {
        assert!(!chromium_title_matches("Pageant", "Page"));
        assert!(!chromium_title_matches("Page - ", "Page"));
        let mut candidate = frame("Page - Google Chrome - Pincer Test");
        candidate.browser_root = false;
        assert_eq!(
            correlate_hyprland_frame(&[candidate], &[("Page - Google Chrome", 800, 600)], 0),
            None
        );
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Pincer Test")],
                &[("Page - Google Chrome", 801, 600)],
                0,
            ),
            None
        );
    }

    #[test]
    fn rejects_ambiguous_native_and_accessibility_candidates() {
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Pincer Test")],
                &[
                    ("Page - Google Chrome", 800, 600),
                    ("Page - Google Chrome - Pincer Test", 800, 600),
                ],
                0,
            ),
            None
        );
        assert_eq!(
            correlate_hyprland_frame(
                &[
                    frame("Page - Google Chrome"),
                    frame("Page - Google Chrome - Pincer Test"),
                ],
                &[("Page - Google Chrome", 800, 600)],
                0,
            ),
            None
        );
    }

    #[test]
    fn preserves_exact_non_chromium_matching() {
        let mut candidate = frame("Editor");
        candidate.browser_root = false;
        candidate.size = None;
        assert_eq!(
            correlate_hyprland_frame(&[candidate], &[("Editor", 800, 600)], 0),
            Some(7)
        );
        assert_eq!(
            correlate_hyprland_frame(&[frame("")], &[("", 800, 600)], 0),
            None
        );
    }
}

/// Resolve native window `xid` to the ordinal of the application top-level that
/// renders it, or `None` when that cannot be proven. `None` means the walk stays
/// application-wide: callers that merely want a tree carry on, and callers that
/// need window identity must refuse.
async fn resolve_window_frame(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    seeds: &[RawObjectRef],
) -> Option<usize> {
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let windows = crate::wayland::hyprland::list_windows().ok()?;
        let owned: Vec<_> = windows.iter().filter(|window| window.pid == pid).collect();
        let target = owned.iter().position(|window| window.address == xid)?;
        let chromium = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .is_some_and(|name| crate::browser_platform::is_chromium_identity(&name));
        let mut frames = Vec::new();
        for (ordinal, oref) in seeds.iter().enumerate() {
            let acc = call(accessible_for(conn, oref)).await?.ok()?;
            let role = call(acc.get_role_name()).await?.ok()?;
            if !is_window_role(&role) {
                continue;
            }
            let title = call(acc.name()).await?.ok()?;
            let browser_root = if chromium {
                call(acc.get_attributes())
                    .await?
                    .ok()?
                    .get("class")
                    .is_some_and(|class| class == "BrowserRootView")
            } else {
                false
            };
            let size = if browser_root {
                let proxies = call(acc.proxies()).await?.ok()?;
                let component = call(proxies.component()).await?.ok()?;
                let (_, _, width, height) =
                    call(component.get_extents(CoordType::Screen)).await?.ok()?;
                Some((width, height))
            } else {
                None
            };
            frames.push(HyprlandFrame {
                ordinal,
                title,
                browser_root,
                size,
            });
        }
        let clients = owned
            .iter()
            .map(|window| (window.title.as_str(), window.width, window.height))
            .collect::<Vec<_>>();
        return correlate_hyprland_frame(&frames, &clients, target);
    }
    if seeds.len() == 1 && crate::input::popup_window_info(xid).is_none() {
        // One top-level: the caller's window is the only thing this
        // application could be showing, and no geometry round-trip can make
        // that more certain. A popup is the exception: Qt publishes no frame
        // for its menus, so Kate's open Projects menu resolved to the main
        // window and its read printed the whole main tree (2026-11-03).
        return Some(0);
    }
    // A popup menu / popover is an override-redirect window the WM's client
    // list never carries: describe it from the server directly, and let it
    // correlate with the toolkit's popup toplevels (role `window` for GTK
    // menus, `menu` / `popup menu` elsewhere).
    let pid_windows = crate::x11::list_windows(Some(pid));
    let (window, popup) = match pid_windows
        .iter()
        .find(|candidate| candidate.xid == xid)
        .cloned()
    {
        Some(window) => (window, false),
        None => (
            crate::x11::window_info(xid).filter(|w| w.is_on_screen)?,
            true,
        ),
    };
    let twins = native_twins(&window, &pid_windows);
    let mut candidates: Vec<(usize, (i32, i32, i32, i32))> = Vec::new();
    for (ordinal, oref) in seeds.iter().enumerate() {
        let Some(Ok(acc)) = call(accessible_for(conn, oref)).await else {
            continue;
        };
        // Menus, tooltips and other transients are top-level accessibles too;
        // only real windows can correspond to a managed native window id.
        let role = match call(acc.get_role_name()).await {
            Some(Ok(role)) => role,
            _ => continue,
        };
        let real_window = is_window_role(&role);
        let popup_toplevel = matches!(role.as_str(), "window" | "menu" | "popup menu");
        if !(real_window || (popup && popup_toplevel)) {
            continue;
        }
        let Some(Ok(proxies)) = call(acc.proxies()).await else {
            continue;
        };
        let Some(Ok(component)) = call(proxies.component()).await else {
            continue;
        };
        if let Some(Ok(extents)) = call(component.get_extents(CoordType::Screen)).await {
            candidates.push((ordinal, extents));
        }
    }
    let resolved = correlate_frame_to_window(&candidates, &window, twins);
    if resolved.is_none() {
        dlog!(
            "could not correlate xid {xid} to one of pid {pid}'s {} top-level frame(s); \
             walk stays application-scoped",
            candidates.len()
        );
    }
    resolved
}

/// The nodes a walk starts from, in visiting order, as (node, depth,
/// top-level ordinal).
///
/// The caller's window comes FIRST when it resolved (`scope`): under a short
/// budget an application-wide pre-order (main window, then dialogs) could
/// exhaust the budget before ever reaching the dialog the caller asked
/// about. Ordinals stay the application's own, so `render` still scopes by
/// the same identity, and the window's element indices are the same whether
/// or not the other top-levels follow. They follow only with
/// `other_windows`. A popup's item container (`popup_root`) replaces its
/// top-level; a role search's `matches` are visited one level below it.
fn walk_seeds<T: Clone>(
    seeds: &[T],
    scope: Option<usize>,
    popup_root: Option<T>,
    matches: Option<Vec<T>>,
    other_windows: bool,
) -> Vec<(T, usize, usize)> {
    let mut ordered: Vec<(T, usize, usize)> = seeds
        .iter()
        .cloned()
        .enumerate()
        .map(|(ordinal, seed)| (seed, 0, ordinal))
        .collect();
    let Some(scope) = scope else {
        return ordered;
    };
    if let Some(container) = popup_root {
        return vec![(container, 0, scope)];
    }
    if let Some(position) = ordered.iter().position(|(_, _, ordinal)| *ordinal == scope) {
        let scoped = ordered.remove(position);
        ordered.insert(0, scoped);
    }
    if let Some(matches) = matches {
        ordered.truncate(1);
        ordered.extend(matches.into_iter().map(|found| (found, 1, scope)));
    } else if !other_windows {
        ordered.truncate(1);
    }
    ordered
}

/// The roles a `get_window_state` query names, searched through AT-SPI
/// Collection instead of filtering a walk ([`role_search_for`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleSearch {
    /// AT-SPI role numbers.
    roles: Vec<u32>,
}

/// Role names Qt's `GetRoleName` gives where libatspi's differ (read with
/// GetRole and GetRoleName over D-Bus on Qt 6.12, 2026-11-09). Rows show the
/// application's own name, so a query may use either.
const QT_ROLE_NAMES: &[(atspi::Role, &str)] = &[
    (atspi::Role::Alert, "alert message"),
    (atspi::Role::Image, "graphic"),
    (atspi::Role::Button, "push button"),
    (atspi::Role::TableCell, "cell"),
    (atspi::Role::TableColumnHeader, "column header"),
    (atspi::Role::TableRowHeader, "row header"),
];

/// The roles `query` names: those with a name of which the query is a run of
/// whole words ("button" names push, toggle and radio buttons; "combo box"
/// names the combo box). `None` when the query is not role words, so it
/// filters a walk instead.
pub fn role_search_for(query: &str) -> Option<RoleSearch> {
    let query = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if query.is_empty() {
        return None;
    }
    let names_a_role = |name: &str| {
        let words: Vec<&str> = name.split(' ').collect();
        words
            .windows(query.len())
            .any(|run| run == query.as_slice())
    };
    let roles: Vec<u32> = (1u32..256)
        .map_while(|number| atspi::Role::try_from(number).ok())
        .filter(|role| {
            names_a_role(role.name())
                || QT_ROLE_NAMES
                    .iter()
                    .any(|(qt_role, name)| qt_role == role && names_a_role(name))
        })
        .map(|role| role as u32)
        .collect();
    (!roles.is_empty()).then_some(RoleSearch { roles })
}

/// A Collection match rule's role set: a bitset of five int32 words, the
/// form libatspi sends.
fn role_bitset(roles: &[u32]) -> Vec<i32> {
    let mut words = vec![0u32; 5];
    for &role in roles {
        if let Some(word) = words.get_mut(role as usize / 32) {
            *word |= 1 << (role % 32);
        }
    }
    words.into_iter().map(|word| word as i32).collect()
}

/// A Collection match rule's state set: a bitset of two int32 words.
fn state_bitset(states: StateSet) -> Vec<i32> {
    let bits = states.bits();
    vec![bits as u32 as i32, (bits >> 32) as u32 as i32]
}

/// `org.a11y.atspi.Collection.GetMatches` below `oref`: every descendant with
/// one of `roles` that is showing or visible, in canonical (walk) order. The
/// rule has libatspi's wire form, as dbus-monitor showed it: role and state
/// bitsets, and match type ALL with empty attribute and interface sets (Qt
/// 6.12 matches nothing when a match type is INVALID, checked 2026-11-09).
/// A rule cannot match names: Collection has no name criterion.
async fn get_matches(
    conn: &atspi::zbus::Connection,
    oref: &RawObjectRef,
    roles: &[u32],
) -> Result<Vec<RawObjectRef>> {
    const MATCH_ALL: i32 = 1;
    const MATCH_ANY: i32 = 2;
    const SORT_CANONICAL: u32 = 1;
    let proxy = atspi::zbus::Proxy::new(
        conn,
        oref.name.as_str(),
        oref.path.as_str(),
        "org.a11y.atspi.Collection",
    )
    .await
    .map_err(|e| anyhow!("Collection proxy unavailable: {e}"))?;
    let rule = (
        state_bitset(StateSet::new(State::Showing | State::Visible)),
        MATCH_ANY,
        std::collections::HashMap::<String, String>::new(),
        MATCH_ALL,
        role_bitset(roles),
        MATCH_ANY,
        Vec::<String>::new(),
        MATCH_ALL,
        false,
    );
    let refs: Vec<(String, atspi::zbus::zvariant::OwnedObjectPath)> = proxy
        .call("GetMatches", &(rule, SORT_CANONICAL, 0i32, true))
        .await
        .map_err(|e| anyhow!("Collection.GetMatches failed: {e}"))?;
    Ok(refs
        .into_iter()
        .map(|(name, path)| RawObjectRef {
            name,
            path: path.as_str().to_owned(),
        })
        .collect())
}

/// Whether the application's toolkit answers Collection searches the way
/// Pincer uses them. Only Qt is checked (Qt 6.12, 2026-11-09: matches by role
/// and state in about a millisecond for a 2,300-node window); other toolkits
/// keep the walk.
async fn is_collection_toolkit_app(app: &AccessibleProxy<'_>) -> bool {
    let Some(Ok(proxies)) = call(app.proxies()).await else {
        return false;
    };
    let Some(Ok(application)) = call(proxies.application()).await else {
        return false;
    };
    let Some(Ok(name)) = call(application.toolkit_name()).await else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("qt")
}

/// When `xid` is a popup of a Qt application, the container its items hang
/// under and the ordinal of the top-level that holds it
/// ([`popup_item_container`]); `None` for anything else.
async fn qt_popup_item_container(
    conn: &AccessibilityConnection,
    app: &AccessibleProxy<'_>,
    seeds: &[RawObjectRef],
    xid: u64,
) -> Option<(usize, RawObjectRef)> {
    let popup = crate::input::popup_window_info(xid)?;
    if !is_collection_toolkit_app(app).await {
        return None;
    }
    popup_item_container(conn.connection(), seeds, &popup).await
}

/// The container a Qt popup's items hang under (a combo box's list, an open
/// menu) and the ordinal of the top-level it belongs to: a showing container
/// whose screen rectangle is the popup's. Qt publishes no top-level for a
/// popup, so this is what a popup read walks.
async fn popup_item_container(
    conn: &atspi::zbus::Connection,
    seeds: &[RawObjectRef],
    popup: &crate::input::PopupWindow,
) -> Option<(usize, RawObjectRef)> {
    let roles = [
        atspi::Role::List,
        atspi::Role::Menu,
        atspi::Role::PopupMenu,
        atspi::Role::Tree,
        atspi::Role::Table,
        atspi::Role::TreeTable,
    ]
    .map(|role| role as u32);
    let mut candidates = Vec::new();
    for (ordinal, seed) in seeds.iter().enumerate() {
        let Some(Ok(found)) = call(get_matches(conn, seed, &roles)).await else {
            continue;
        };
        for container in found {
            let Some(Ok(proxy)) = call(raw_component(conn, &container)).await else {
                continue;
            };
            let Some(Ok(extents)) = call(proxy.get_extents(CoordType::Screen)).await else {
                continue;
            };
            candidates.push((ordinal, container, extents));
        }
    }
    let rects: Vec<_> = candidates.iter().map(|(_, _, extents)| *extents).collect();
    let rect = (popup.x, popup.y, popup.width as i32, popup.height as i32);
    let pick = pick_popup_container(&rects, rect)?;
    let (ordinal, container, _) = candidates.swap_remove(pick);
    Some((ordinal, container))
}

/// The candidate whose screen rectangle is the popup's: every edge within a
/// few pixels (Qt's combo list sits one pixel inside its popup's border),
/// the closest one when several are.
fn pick_popup_container(
    candidates: &[(i32, i32, i32, i32)],
    popup: (i32, i32, i32, i32),
) -> Option<usize> {
    const EDGE_SLACK: i32 = 4;
    let edges = |(x, y, w, h): (i32, i32, i32, i32)| [x, y, x + w, y + h];
    let target = edges(popup);
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, rect)| {
            let distances = edges(*rect)
                .iter()
                .zip(target)
                .map(|(edge, wanted)| (edge - wanted).abs())
                .collect::<Vec<_>>();
            distances
                .iter()
                .all(|distance| *distance <= EDGE_SLACK)
                .then(|| (distances.iter().sum::<i32>(), index))
        })
        .min()
        .map(|(_, index)| index)
}

/// A Component proxy for an arbitrary (bus name, path), uncached like every
/// other proxy the walk builds.
async fn raw_component<'a>(
    conn: &atspi::zbus::Connection,
    oref: &RawObjectRef,
) -> Result<atspi::proxy::component::ComponentProxy<'a>> {
    atspi::proxy::component::ComponentProxy::builder(conn)
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .destination(oref.name.clone())
        .map_err(|e| anyhow!("bad a11y destination: {e}"))?
        .path(oref.path.clone())
        .map_err(|e| anyhow!("bad a11y path: {e}"))?
        .build()
        .await
        .map_err(|e| anyhow!("Component proxy unavailable: {e}"))
}

/// `collect_visited` with caller-supplied caps.
/// - `max_elements = None` keeps the historical 5 000-node budget.
/// - `max_depth = None` keeps depth uncapped (the historical behaviour);
///   `Some(d)` skips enqueueing children whose depth would exceed `d`.
///
/// Issue #22865: caps protect against Electron / large web apps that produce
/// 10k+ element trees and blow context windows.
async fn collect_visited_bounded<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
) -> Result<Option<Collected<'a>>> {
    collect_visited_bounded_opts(
        conn,
        pid,
        xid,
        max_elements,
        max_depth,
        WalkOptions::default(),
    )
    .await
}

/// How a walk for a native window reaches into the application.
#[derive(Clone, Copy, Debug, Default)]
struct WalkOptions<'s> {
    /// Descend into menus the snapshot walk keeps collapsed: a targeted walk
    /// (invoke_menu resolving a path segment by segment) needs them, since a
    /// toolkit that never marks its entries SHOWING (gail) cannot show the
    /// walk that a `doAction` opened one.
    walk_closed_menus: bool,
    /// Walk the application's other top-levels after the caller's window.
    /// Off, a walk for a window whose top-level (or, for a Qt popup, whose
    /// item container) resolves visits only that subtree. Its element indices
    /// are the same either way: the caller's window is always walked first.
    other_windows: bool,
    /// Search these roles below the caller's top-level through AT-SPI
    /// Collection instead of walking it, where the toolkit supports it.
    role_search: Option<&'s RoleSearch>,
    /// Characters of a text field's content to read; `None` reads
    /// [`TEXT_READ_CHARS`]. `full_output` reads asks for
    /// [`FULL_TEXT_READ_CHARS`].
    text_read_chars: Option<usize>,
}

/// [`collect_visited_bounded`] with explicit [`WalkOptions`].
async fn collect_visited_bounded_opts<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
    options: WalkOptions<'_>,
) -> Result<Option<Collected<'a>>> {
    let walk_closed_menus = options.walk_closed_menus;
    let ResolvedApp {
        app,
        children: probed,
    } = match resolve_app_for_pid(conn, pid).await? {
        Some(a) => a,
        None => {
            if deadline_passed() {
                // The registry / application did not answer inside the caller's
                // budget. Distinguish that from "no such application" so the
                // caller can report a timeout instead of an absent tree.
                return Err(anyhow!("app_lookup_timeout"));
            }
            return Ok(None);
        }
    };
    if first_application_walk(pid) {
        let _ = call(app.get_attributes()).await;
    }
    let zconn = conn.connection();
    // Mapped popups of this pid, read once and only when a menubar menu's
    // open state cannot be told from AT-SPI states alone (gail, focus elsewhere).
    let mut pid_popups: Option<Vec<crate::input::PopupWindow>> = None;

    // Stack of (object ref, depth, in_web_doc, frame_ordinal). Seed with the
    // app's windows; push children reversed so siblings pop left-to-right and
    // each subtree completes before the next sibling (pre-order). `in_web_doc`
    // is inherited from ancestors so editables in page content can be told from
    // chrome. `frame_ordinal` is the seed's position in `get_children()` order
    // and is likewise inherited, so every node carries the identity of the
    // top-level window it belongs to.
    let (seeds, app_answered): (Vec<RawObjectRef>, bool) = match probed {
        Some(children) => (
            children
                .iter()
                .filter_map(RawObjectRef::from_atspi)
                .collect(),
            true,
        ),
        None => match call(app.get_children()).await {
            Some(Ok(children)) => (
                children
                    .iter()
                    .filter_map(RawObjectRef::from_atspi)
                    .collect(),
                true,
            ),
            _ => (Vec::new(), false),
        },
    };
    if !app_answered {
        // The application registered with AT-SPI but never answered
        // `GetChildren`: LibreOffice with its bridge half-initialised, or a
        // modal-grabbed app. Report it as unresponsive rather than as an
        // application without windows — the caller can retry later.
        dlog!("application for pid {pid} did not answer GetChildren");
        return Ok(Some(Collected {
            visited: Vec::new(),
            scoped_frame: None,
            truncation: Some(Truncation {
                reason: "app_unresponsive",
                visited: 0,
                pending: 0,
            }),
            searched_roles: false,
        }));
    }

    // Resolve which seed is the caller's window before walking, from the same
    // child list the walk is about to seed from. Re-reading `get_children()`
    // later could observe a different window set, and an ordinal resolved
    // against one list but applied to another names the wrong window.
    let mut scoped_frame = if xid == 0 {
        None
    } else {
        resolve_window_frame(conn, pid, xid, &seeds).await
    };

    // A Qt popup (a combo list, a menu) has no top-level of its own: its
    // items hang under a container in the window that opened it, so without
    // that container the read would walk the whole application to find them.
    let mut popup_root = None;
    if scoped_frame.is_none() && xid != 0 && !options.other_windows {
        if let Some((ordinal, container)) = qt_popup_item_container(conn, &app, &seeds, xid).await {
            scoped_frame = Some(ordinal);
            popup_root = Some(container);
        }
    }

    // A role search asks the toolkit for the matching elements below the
    // caller's top-level (one Collection.GetMatches) instead of walking it.
    // Any failure leaves the ordinary walk.
    let mut search_matches = None;
    if let (Some(search), Some(scope), None) = (options.role_search, scoped_frame, &popup_root) {
        if is_collection_toolkit_app(&app).await {
            match call(get_matches(zconn, &seeds[scope], &search.roles)).await {
                Some(Ok(found)) => search_matches = Some(found),
                Some(Err(error)) => dlog!("role search for pid {pid} failed: {error:#}"),
                None => dlog!("role search for pid {pid} timed out"),
            }
        }
    }

    // A role search visits its matches without walking into them.
    let flat = search_matches.is_some();
    let ordered = walk_seeds(
        &seeds,
        scoped_frame,
        popup_root,
        search_matches,
        options.other_windows,
    );
    let mut stack: Vec<(RawObjectRef, usize, bool, usize)> = ordered
        .into_iter()
        .rev()
        .map(|(oref, depth, ordinal)| (oref, depth, false, ordinal))
        .collect();
    // Direct children of a `menu bar`: a top-level menu whose popup is not
    // open keeps its child count but is not descended into.
    let mut menubar_children: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    let mut visited: Vec<Visited<'a>> = Vec::new();
    let mut identity_owners = std::collections::HashMap::new();
    // Guard against pathological/looping trees. Defaults to 5 000 (the
    // historical hard-coded budget); callers can override via max_elements.
    let mut budget = max_elements.unwrap_or(5000usize);
    // Time budget alongside the node budget: when an app is unresponsive to
    // AT-SPI (most commonly because it holds a modal grab and isn't servicing
    // D-Bus), every per-node `call()` burns the full CALL_TIMEOUT before being
    // skipped, so the walk would otherwise grind for minutes. Callers that lack
    // their own OP_TIMEOUT (snapshot bounds, insert_text) relied on this
    // never happening — bound it here so the walk returns partial within
    // OP_TIMEOUT for every caller, instead of hanging get_window_state/type_text
    // on modal dialogs (#1936).
    // The caller's operation deadline (when one is in scope) wins over the
    // historical OP_TIMEOUT so a 1 s tool budget bounds the whole walk.
    let deadline = {
        let legacy = tokio::time::Instant::now() + OP_TIMEOUT;
        OP_DEADLINE.try_with(|d| (*d).min(legacy)).unwrap_or(legacy)
    };
    // Fast bail for an app that has stopped answering AT-SPI entirely (modal
    // grab): if several consecutive nodes each burn the full CALL_TIMEOUT, the
    // app is unresponsive and the remaining ~OP_TIMEOUT of walking would all
    // time out too. Give up after a few so type_text falls back to XTEST in a
    // few seconds rather than ~25s.
    let mut consecutive_timeouts = 0u32;
    let mut truncation: Option<&'static str> = None;
    // Containers left unexpanded because their child count exceeded the cap:
    // (child count, position in `visited` of the container).
    let mut huge_containers: Vec<(usize, usize)> = Vec::new();
    let child_cap = max_elements
        .map(|cap| cap.max(CHILD_ENUMERATION_CAP))
        .unwrap_or(CHILD_ENUMERATION_CAP);

    while let Some((oref, depth, inherited_web_doc, frame_ordinal)) = stack.pop() {
        if budget == 0 {
            dlog!("node budget exhausted; truncating walk");
            truncation = Some("node_budget");
            stack.push((oref, depth, inherited_web_doc, frame_ordinal));
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            dlog!("collect_visited time budget exhausted; returning partial walk");
            truncation = Some("timeout");
            stack.push((oref, depth, inherited_web_doc, frame_ordinal));
            break;
        }
        // WebKitGTK publishes its embedded page on a distinct WebProcess
        // D-Bus peer and can expose blank role names for the entire subtree.
        // The peer identity is therefore the reliable document boundary when
        // role-based AT-SPI discovery cannot identify one.
        let in_web_doc = inherited_web_doc || is_web_process_bus(&oref.name);

        // accessible_for builds a proxy whose first use round-trips to the
        // target app. On a modal-grabbed (AT-SPI-unresponsive) app this is the
        // await that actually hangs, so it MUST carry the per-call timeout —
        // otherwise the loop never returns to the deadline check at the top and
        // the walk stalls past OP_TIMEOUT for callers without an outer guard
        // (snapshot bounds, insert_text). That was the residual #1936 hang.
        let object_identity = identity_ref(conn, &oref, &mut identity_owners).await;
        let frame_identity = identity_ref(conn, &seeds[frame_ordinal], &mut identity_owners).await;
        let acc = match call(accessible_for(
            conn,
            object_identity.as_ref().unwrap_or(&oref),
        ))
        .await
        {
            Some(Ok(a)) => a,
            Some(Err(error)) => {
                dlog!("  accessible_for failed: {error:#}");
                continue;
            }
            None => {
                if tokio::time::Instant::now() >= deadline {
                    truncation = Some("timeout");
                    stack.push((oref, depth, inherited_web_doc, frame_ordinal));
                    break;
                }
                consecutive_timeouts += 1;
                if consecutive_timeouts >= 3 {
                    dlog!(
                        "{} consecutive AT-SPI timeouts (accessible_for); app unresponsive, bailing walk",
                        consecutive_timeouts
                    );
                    truncation = Some("app_unresponsive");
                    stack.push((oref, depth, inherited_web_doc, frame_ordinal));
                    break;
                }
                continue;
            }
        };

        // Interfaces gate every other query; if even this times out the node is
        // unreachable, so skip it rather than stall.
        let ifaces = match call(acc.get_interfaces()).await {
            Some(Ok(i)) => {
                consecutive_timeouts = 0;
                i
            }
            // A completed-but-errored call is node-specific; keep walking.
            Some(Err(error)) => {
                dlog!("  get_interfaces failed: {error:#}");
                continue;
            }
            // A timeout means the app didn't answer in CALL_TIMEOUT. A run of
            // these means the whole app is wedged — bail so callers fall back.
            None => {
                if tokio::time::Instant::now() >= deadline {
                    truncation = Some("timeout");
                    stack.push((oref, depth, inherited_web_doc, frame_ordinal));
                    break;
                }
                consecutive_timeouts += 1;
                if consecutive_timeouts >= 3 {
                    dlog!(
                        "{} consecutive AT-SPI timeouts; app unresponsive, bailing walk",
                        consecutive_timeouts
                    );
                    truncation = Some("app_unresponsive");
                    stack.push((oref, depth, inherited_web_doc, frame_ordinal));
                    break;
                }
                continue;
            }
        };
        let has_action = ifaces.contains(Interface::Action);
        let has_editable = ifaces.contains(Interface::EditableText);
        let has_value = ifaces.contains(Interface::Value);
        let has_component = ifaces.contains(Interface::Component);
        let has_text = ifaces.contains(Interface::Text);

        // A toplevel transient of role `tool tip` is never read or entered:
        // gail (GTK2, e.g. GIMP 2.10) answers `name` on a tooltip-role popup
        // whose child is not a plain label with a `g_message` ("ATK_ROLE_TOOLTIP
        // object found, but doesn't look like a tooltip"), which GIMP shows as
        // a "GIMP Message" dialog. Such a popup exists only after the real
        // pointer hovered a widget (foreground delivery); it carries no
        // actionable element, so skipping it changes nothing else. One extra
        // round-trip per toplevel, not per node.
        if depth == 0 {
            if let Some(Ok(role)) = call(acc.get_role_name()).await {
                if is_tooltip_role(&role) {
                    dlog!("skipping tooltip toplevel {}", oref.path);
                    continue;
                }
            }
        }
        // These four are independent — issue them concurrently to cut the
        // per-node round-trip cost (large trees like Chromium's have hundreds
        // of nodes, so sequential reads dominate the walk time).
        let (role_r, name_r, state_r, count_r) = tokio::join!(
            call(acc.get_role_name()),
            call(acc.name()),
            call(acc.get_state()),
            call(raw_child_count(zconn, &oref)),
        );
        // Fetch the child list only for real containers within the cap: leaves
        // (the majority of nodes) save a round-trip, and a huge container is
        // never enumerated (see CHILD_ENUMERATION_CAP).
        let children_r: Option<Result<Vec<RawObjectRef>>> = match count_r {
            Some(Ok(count)) if count <= 0 => Some(Ok(Vec::new())),
            Some(Ok(count)) if count as usize > child_cap => {
                huge_containers.push((count as usize, visited.len()));
                Some(Ok(Vec::new()))
            }
            Some(Ok(_)) => call(raw_children(zconn, &oref)).await,
            // Count unavailable (older bridges, error): fall back to the
            // historical unconditional fetch.
            Some(Err(_)) => call(raw_children(zconn, &oref)).await,
            None => None,
        };
        let role = match role_r {
            Some(Ok(r)) => r,
            _ => String::new(),
        };
        let mut name = match name_r {
            Some(Ok(n)) => n,
            _ => String::new(),
        };
        let focused = matches!(state_r.as_ref(), Some(Ok(s)) if s.contains(State::Focused));
        let role_lower = role.to_ascii_lowercase();
        let checked = if role_lower.contains("check") {
            state_r
                .as_ref()
                .and_then(|state| state.as_ref().ok())
                .map(|state| state.contains(State::Checked))
        } else {
            None
        };
        let enabled = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .map(is_enabled_state);
        let selectable = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .is_some_and(|state| state.contains(State::Selectable));
        let (showing, offscreen) = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .map_or((true, false), |state| listing_state(&role_lower, state));
        let expanded = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .is_some_and(|state| state.contains(State::Expanded));
        let marks = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .map(|state| row_state_marks(&role_lower, state))
            .unwrap_or_default();
        let under_menubar = menubar_children.remove(&(oref.name.clone(), oref.path.clone()));
        let selected = if role_lower.contains("check") {
            checked
        } else if role_lower.contains("radio")
            || role_lower.contains("list item")
            || role_lower.contains("menu item")
            || matches!(role_lower.as_str(), "tab" | "page tab" | "tab item")
        {
            state_r
                .as_ref()
                .and_then(|state| state.as_ref().ok())
                .map(|state| state.contains(State::Selected) || state.contains(State::Checked))
        } else {
            None
        };

        // Collect action names, numeric value, and (crucially) Text-interface
        // content. Only touch `proxies` when an interface is actually present,
        // and drop the borrow before `acc` moves into `visited`.
        let mut actions: Vec<String> = Vec::new();
        let mut value: Option<String> = None;
        let mut text_content = String::new();
        let mut text_total: Option<usize> = None;
        if has_action || has_value || has_text {
            // The interface set read above, not `acc.proxies()`, which would
            // ask GetInterfaces again.
            let proxies = KnownInterfaces::new(&acc, ifaces);
            if has_action {
                if let Some(Ok(ap)) = call(proxies.action()).await {
                    let n = call(ap.n_actions()).await.and_then(|r| r.ok()).unwrap_or(0);
                    for i in 0..n {
                        // Preserve the AT-SPI action index even when an
                        // individual name lookup fails. `do_action` takes
                        // this original index, so compacting the vector
                        // could otherwise actuate a different action than
                        // the name we selected.
                        actions.push(
                            call(ap.get_name(i))
                                .await
                                .and_then(|result| result.ok())
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            if has_value {
                if let Some(Ok(vp)) = call(proxies.value()).await {
                    value = call(vp.current_value())
                        .await
                        .and_then(|r| r.ok())
                        .map(format_value);
                }
            }
            // Text content is where editable/entry text (the typed string)
            // lives; `name` is usually empty for such widgets.
            if has_text {
                if let Some(Ok(tp)) = call(proxies.text()).await {
                    let count = call(tp.character_count())
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or(0);
                    if count > 0 {
                        let limit = options.text_read_chars.unwrap_or(TEXT_READ_CHARS);
                        let end = count.min(i32::try_from(limit).unwrap_or(i32::MAX));
                        if let Some(Ok(t)) = call(tp.get_text(0, end)).await {
                            text_content = t;
                        }
                        if count > end {
                            text_total = usize::try_from(count).ok();
                        }
                    }
                }
            }
        }

        // An editable text widget's typed content is its value, reported
        // separately from its name (parity with macOS AXValue and the Windows
        // ValuePattern). Without it a named entry ("Note") hides what it holds
        // from `elements[].value` and `verify_state`.
        // A password field's content is never surfaced.
        let editable_text = has_editable && !role_lower.contains("password");
        value = editable_text_value(editable_text, has_value, value, &text_content);
        let value_total_chars =
            text_total.filter(|_| value.as_deref() == Some(text_content.as_str()));

        // Surface Text content as the display name when the widget has no
        // name — for text widgets. A value control (GtkSpinButton implements
        // AtkText with its formatted number) would otherwise be named after
        // its own value, and four spin buttons then all read "0.0".
        if name.trim().is_empty() && !text_content.trim().is_empty() && !has_value {
            name = text_content;
        }

        // AT-SPI `Description` for controls (one `Get` per actionable node,
        // never for passive containers/labels): Qt reports tooltips there.
        let is_control = has_action
            || has_value
            || has_editable
            || (has_component && is_control_role(&role_lower));
        let mut description = String::new();
        if is_control {
            if let Some(Ok(text)) = call(acc.description()).await {
                description = text.trim().to_owned();
            }
        }
        // An unnamed value/editable control: its `LABELLED_BY` relation names
        // it (GTK grids, Qt forms with buddies). One relation read plus one
        // name read, only for the controls that need it.
        if name.trim().is_empty() && (has_value || has_editable || is_control_role(&role_lower)) {
            if let Some(label) = labelled_by_name(conn, &acc).await {
                name = label;
            }
        }

        // Children inherit web-document context, plus this node's own role.
        let child_in_web_doc = in_web_doc || is_document_role(&role);

        // Enqueue children (fetched above) before moving `acc` into `visited`.
        // Honor max_depth (#22865): skip enqueueing descendants whose depth
        // would exceed the cap.
        let descend = !flat && max_depth.map(|d| depth < d).unwrap_or(true);
        // A menu that is not open (a menubar entry that is not expanded, or
        // any menu that is not showing) keeps its child count and is not
        // walked: its items are hidden, never indexed, and cost a round-trip
        // each. Open menus (expanded / showing popup contents) still descend.
        // Qt has no `menu` role here: its menubar entries are `menu item`
        // and each one's items sit in a `popup menu`, SHOWING only while it
        // is open. Without that case a Qt app's closed menus were walked in
        // full: KWrite's took a read from 1,583 calls to 5,977.
        let mut closed_menu = !walk_closed_menus
            && ((role_lower == "menu" && (!showing || (under_menubar && !expanded)))
                || (role_lower == "popup menu" && !showing));
        if closed_menu && showing && under_menubar {
            // LibreOffice VCL never sets EXPANDED on an open menubar menu; its
            // items are simply SHOWING. gail (GTK2, e.g. GIMP 2.10) never sets
            // EXPANDED either and never marks a menu entry SHOWING, open or
            // not (its entries are VISIBLE only, closed or open, and the
            // leading tearoff item carries no state at all); what it does
            // publish is FOCUSED on the menubar menu that is popped up. So
            // an open menu is one that is FOCUSED itself, or whose leading
            // children are SHOWING: a few extra state reads per menubar
            // menu. Everything else stays collapsed.
            if focused {
                closed_menu = false;
            } else if let Some(Ok(children)) = &children_r {
                let mut leading: Vec<AccessibleProxy<'_>> = Vec::new();
                for child_ref in children.iter().take(MENU_PEEK_CHILDREN) {
                    if let Some(Ok(child)) = call(accessible_for(conn, child_ref)).await {
                        if let Some(Ok(state)) = call(child.get_state()).await {
                            if is_showing_state(&state) {
                                closed_menu = false;
                                break;
                            }
                        }
                        leading.push(child);
                    }
                }
                if closed_menu {
                    // gail with the focus elsewhere (the caller's window is
                    // not active, so the popped-up menu is not FOCUSED): the
                    // open menu is a mapped override-redirect window of this
                    // pid, and its entries' screen extents lie inside it.
                    if pid_popups.is_none() {
                        let popups =
                            tokio::task::spawn_blocking(crate::input::mapped_popup_windows)
                                .await
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|popup| popup.pid.is_none_or(|popup_pid| popup_pid == pid))
                                .collect::<Vec<_>>();
                        pid_popups = Some(popups);
                    }
                    if let Some(popups) = pid_popups.as_ref().filter(|popups| !popups.is_empty()) {
                        for child in &leading {
                            let Some(Ok(proxies)) = call(child.proxies()).await else {
                                continue;
                            };
                            let Some(Ok(component)) = call(proxies.component()).await else {
                                continue;
                            };
                            let Some(Ok((ex, ey, ew, eh))) =
                                call(component.get_extents(CoordType::Screen)).await
                            else {
                                continue;
                            };
                            if ew <= 0 || eh <= 0 || ex < -100_000 || ey < -100_000 {
                                continue;
                            }
                            let (cx, cy) = (ex + ew / 2, ey + eh / 2);
                            if popups.iter().any(|popup| {
                                cx >= popup.x
                                    && cy >= popup.y
                                    && cx < popup.x + popup.width as i32
                                    && cy < popup.y + popup.height as i32
                            }) {
                                closed_menu = false;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let mut collapsed_children = None;
        if descend && closed_menu {
            if let Some(Ok(children)) = &children_r {
                collapsed_children = Some(children.len());
            }
        }
        if descend && !closed_menu {
            match children_r {
                Some(Ok(children)) => {
                    if role_lower == "menu bar" {
                        for c in &children {
                            menubar_children.insert((c.name.clone(), c.path.clone()));
                        }
                    }
                    for c in children.into_iter().rev() {
                        stack.push((c, depth + 1, child_in_web_doc, frame_ordinal));
                    }
                }
                Some(Err(error)) => dlog!("  get_children failed: {error:#}"),
                None => dlog!("  get_children timed out"),
            }
        }

        // Only nodes the caller can act on count against `max_elements`, so
        // a budget of N returns N elements rather than N visited containers.
        if showing
            && is_indexable_capabilities(
                &role,
                !actions.is_empty(),
                has_editable,
                has_value,
                selectable,
                has_component,
                enabled,
            )
        {
            budget -= 1;
        }
        visited.push(Visited {
            depth,
            role,
            name,
            description,
            value,
            checked,
            enabled,
            selected,
            marks,
            value_total_chars,
            selectable,
            showing,
            offscreen,
            collapsed_children,
            actions,
            has_editable,
            has_value,
            has_component,
            focused,
            in_web_doc,
            on_web_process_bus: is_web_process_bus(&oref.name),
            frame_ordinal,
            object_ref: ObjectRef {
                bus: oref.name.clone(),
                path: oref.path.clone(),
            },
            identity: object_identity
                .zip(frame_identity)
                .map(|(object, frame)| AtspiIdentity {
                    bus_name: object.name,
                    path: object.path,
                    frame_bus_name: frame.name,
                    frame_path: frame.path,
                }),
            acc,
        });
    }

    if truncation.is_none() && !huge_containers.is_empty() {
        for (count, at) in &huge_containers {
            dlog!(
                "container {:?} at node {at} has {count} children; not expanded",
                visited.get(*at).map(|v| v.role.as_str()).unwrap_or("?")
            );
        }
        truncation = Some("huge_container");
    }
    let truncation = truncation.map(|reason| Truncation {
        reason,
        visited: visited.len(),
        pending: stack.len(),
    });
    dlog!(
        "walked pid {pid}: {} node(s), truncation={:?}",
        visited.len(),
        truncation
    );
    Ok(Some(Collected {
        visited,
        scoped_frame,
        truncation,
        searched_roles: flat,
    }))
}

/// Roles whose Description / LABELLED_BY are worth a round-trip even when
/// the node exposes no Action / Value / EditableText interface.
fn is_control_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "button"
            | "push button"
            | "toggle button"
            | "combo box"
            | "spin button"
            | "slider"
            | "check box"
            | "radio button"
            | "entry"
            | "text"
            | "password text"
    )
}

/// Value controls the model tells apart by their label; when nothing names
/// one, the tree says so instead of repeating the value as its name.
fn is_value_control_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "spin button" | "slider" | "scroll bar" | "progress bar" | "combo box" | "entry" | "text"
    )
}

/// The name of the first `LABELLED_BY` target of `acc`, when it has one.
async fn labelled_by_name(
    conn: &AccessibilityConnection,
    acc: &AccessibleProxy<'_>,
) -> Option<String> {
    let relations = call(acc.get_relation_set()).await?.ok()?;
    for (kind, targets) in relations {
        if kind != atspi::RelationType::LabelledBy {
            continue;
        }
        for target in targets {
            let Some(raw) = RawObjectRef::from_atspi(&target) else {
                continue;
            };
            let Some(Ok(label)) = call(accessible_for(conn, &raw)).await else {
                continue;
            };
            if let Some(Ok(text)) = call(label.name()).await {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_owned());
                }
            }
        }
    }
    None
}

/// Prefix of the description note an unnamed value control carries; the
/// element builder turns it into `unlabelled: true`.
pub const UNLABELLED_NOTE_PREFIX: &str = "unlabelled";

fn ordinal_word(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The real AT-SPI parent of every visited node (its position in `visited`),
/// from the pre-order depth sequence: a node at depth `d` ends every open
/// subtree at depth >= `d`. Ancestry never spans two top-levels.
fn true_parents(visited: &[Visited<'_>]) -> Vec<Option<usize>> {
    let mut parents = Vec::with_capacity(visited.len());
    let mut chain: Vec<usize> = Vec::new();
    let mut current_frame: Option<usize> = None;
    for (pos, v) in visited.iter().enumerate() {
        if current_frame != Some(v.frame_ordinal) {
            current_frame = Some(v.frame_ordinal);
            chain.clear();
        }
        chain.truncate(v.depth);
        parents.push(chain.last().copied());
        chain.push(pos);
    }
    parents
}

/// For an unnamed value control at `pos`: the nearest preceding sibling
/// label in the same container (a GTK grid row "Hue:" [spin]) — scanning
/// stops at another control of the same role, so the second row never
/// borrows the first row's label.
fn preceding_sibling_label(
    visited: &[Visited<'_>],
    parents: &[Option<usize>],
    pos: usize,
) -> Option<String> {
    let parent = parents[pos]?;
    let role = visited[pos].role.to_ascii_lowercase();
    for q in (0..pos).rev() {
        if visited[q].depth < visited[pos].depth {
            break;
        }
        if parents[q] != Some(parent) {
            continue;
        }
        let q_role = visited[q].role.to_ascii_lowercase();
        if q_role == role {
            return None;
        }
        if q_role == "label" && !visited[q].name.trim().is_empty() {
            return Some(visited[q].name.trim().to_owned());
        }
    }
    None
}

/// "3rd of 4 spin buttons in this window": the control's place among the
/// showing same-role controls of its top-level, in walk (= visual) order —
/// what a screenshot shows, so the model can map the rows it sees.
fn sibling_ordinal(visited: &[Visited<'_>], _parents: &[Option<usize>], pos: usize) -> String {
    let role = visited[pos].role.to_ascii_lowercase();
    let same_role = |q: usize| {
        visited[q].frame_ordinal == visited[pos].frame_ordinal
            && visited[q].showing
            && visited[q].role.eq_ignore_ascii_case(&role)
    };
    let total = (0..visited.len()).filter(|q| same_role(*q)).count();
    let nth = (0..=pos).filter(|q| same_role(*q)).count();
    let plural = if total == 1 {
        role.clone()
    } else {
        format!("{role}s")
    };
    format!("{} of {total} {plural} in this window", ordinal_word(nth))
}

/// Render visited nodes into the markdown + node list `walk_tree` returns.
/// Format matches the historical pyatspi output exactly so downstream parsing
/// is unaffected.
///
/// `parent_at_depth` tracks the most recently emitted actionable index at
/// each depth, so descendants can look up their parent_element_index without
/// a second pass.
///
/// `only_frame` restricts what is *emitted* to one application top-level while
/// leaving the index space application-wide. Element indices are the contract
/// between a snapshot and every actuator that later takes one
/// (`perform_action`, `focus_element`, `set_value`, …), and those resolve an
/// index against the whole application. Renumbering per window would make a
/// window-scoped snapshot's indices name different elements at actuation time.
fn render(visited: &[Visited<'_>], only_frame: Option<usize>) -> (String, Vec<AtspiNode>) {
    let mut md = String::new();
    let mut nodes = Vec::new();
    let mut idx = 0usize;
    // The real AT-SPI parent of every node, so `parent_element_index` is the
    // nearest *indexed ancestor* — never a preceding cousin at a lower depth
    // (which parented GIMP's spin buttons to a push button).
    let parents = true_parents(visited);
    let mut index_of: Vec<Option<usize>> = vec![None; visited.len()];

    for (pos, v) in visited.iter().enumerate() {
        let emit = only_frame.is_none_or(|frame| frame == v.frame_ordinal);
        let indent = "  ".repeat(v.depth);
        let parent_element_index = {
            let mut cursor = parents[pos];
            let mut found = None;
            while let Some(p) = cursor {
                if let Some(i) = index_of[p] {
                    found = Some(i);
                    break;
                }
                cursor = parents[p];
            }
            found
        };

        if is_indexable(v) {
            index_of[pos] = Some(idx);
            if !emit {
                // Consume the index without emitting: indices stay aligned with
                // the application-wide walk the actuators perform.
                idx += 1;
                continue;
            }
            let role_lower = v.role.to_ascii_lowercase();
            // An unnamed value control: name it from the sibling label of its
            // row when there is one; otherwise say it is unlabelled and where
            // it sits among its same-role siblings, and never repeat its value
            // as its name.
            let mut name = v.name.clone();
            let mut unlabelled_note = None;
            if name.trim().is_empty() && is_value_control_role(&role_lower) {
                if !v.description.trim().is_empty() {
                    // GIMP's spin scales draw their label inside the widget
                    // and publish it as the description.
                    name = v.description.trim().to_owned();
                } else if let Some(label) = preceding_sibling_label(visited, &parents, pos) {
                    name = label;
                } else {
                    unlabelled_note = Some(format!(
                        "{UNLABELLED_NOTE_PREFIX} {}; {}",
                        role_lower,
                        sibling_ordinal(visited, &parents, pos)
                    ));
                }
            }
            let act_str = v.actions.join(",");
            let val_part = match &v.value {
                Some(val) if !val.is_empty() => value_part(val, v.value_total_chars),
                _ => String::new(),
            };
            let description_shown = (!v.description.is_empty() && v.description != name)
                .then(|| format!(" (description \"{}\")", one_line(&v.description)));
            let collapsed_note = v
                .collapsed_children
                .map(|n| format!(" (closed menu, {n} items; click it to open)"))
                .unwrap_or_default();
            let unlabelled_shown = unlabelled_note
                .as_ref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default();
            // After the actions block, so the value's end (`" [actions=[`)
            // stays where the value cutter looks for it.
            let marks_shown = if v.marks.is_empty() {
                String::new()
            } else {
                format!(" ({})", v.marks.join(", "))
            };
            md.push_str(&format!(
                "{indent}- [{idx}] {role} \"{name}\"{desc}{val_part} [actions=[{act_str}]]{marks_shown}{collapsed_note}{unlabelled_shown}
",
                role = v.role,
                name = one_line(&name),
                desc = description_shown.as_deref().unwrap_or(""),
            ));
            let mut description_parts: Vec<String> = Vec::new();
            if !v.description.is_empty() {
                description_parts.push(v.description.clone());
            }
            if let Some(n) = v.collapsed_children {
                description_parts.push(format!(
                    "closed menu with {n} items; not expanded: click it to open"
                ));
            }
            if let Some(note) = unlabelled_note {
                description_parts.push(note);
            }
            if v.offscreen {
                description_parts.push(OFFSCREEN_NOTE.to_owned());
            }
            nodes.push(AtspiNode {
                element_index: Some(idx),
                role: v.role.clone(),
                name: if name.trim().is_empty() {
                    None
                } else {
                    Some(name)
                },
                value: v.value.clone().filter(|s| !s.is_empty()),
                checked: v.checked,
                enabled: v.enabled,
                selected: v.selected,
                description: (!description_parts.is_empty()).then(|| description_parts.join("; ")),
                actions: v.actions.clone(),
                element_key: idx as u64,
                identity: v.identity.clone(),
                depth: v.depth,
                parent_element_index,
                in_web_content: v.in_web_doc,
                object_ref: Some(v.object_ref.clone()),
            });
            idx += 1;
        } else if emit && !v.name.is_empty() && v.showing {
            let mut marker: Vec<&str> =
                passive_marker(&v.role, !v.actions.is_empty(), v.has_component, v.enabled)
                    .into_iter()
                    .collect();
            marker.extend(v.marks.iter().copied());
            let marker = if marker.is_empty() {
                String::new()
            } else {
                format!(" ({})", marker.join(", "))
            };
            md.push_str(&format!(
                "{indent}- {role} = \"{name}\"{marker}
",
                role = v.role,
                name = one_line(&v.name),
            ));
        }
    }

    (md, nodes)
}

/// Why a visible, named control is listed without an index. A disabled
/// button is rendered as `- push button = "Restore" (disabled)` so a caller
/// learns it must enable it (select something) rather than assume the tree
/// dropped it.
fn passive_marker(
    role: &str,
    has_action: bool,
    has_component: bool,
    enabled: Option<bool>,
) -> Option<&'static str> {
    let control = has_action
        || (has_component
            && matches!(
                role.trim().to_ascii_lowercase().as_str(),
                "button"
                    | "push button"
                    | "toggle button"
                    | "menu item"
                    | "check box"
                    | "radio button"
            ));
    (control && enabled == Some(false)).then_some("disabled")
}

/// The states a tree row shows, in a fixed order: `checked` / `unchecked`
/// (check boxes and check menu items say both; radio buttons, toggle buttons
/// and other checkable controls only `checked`), `mixed`, `pressed`,
/// `expanded` (and `collapsed` for an expandable list, tree or table item),
/// and `selected` for an item of a list, tree, table or tab bar. Menus get no
/// `expanded`: their rows already say whether they are open. Without these a
/// check box read the same before and after a toggle, so a `since` diff could
/// not show it (the Test Pad's Grid box, 2026-11-11).
fn row_state_marks(role_lower: &str, state: &StateSet) -> Vec<&'static str> {
    let mut marks = Vec::new();
    let menu = role_lower.contains("menu");
    let check_role = role_lower.contains("check");
    let checkable = check_role
        || role_lower.contains("radio")
        || role_lower.contains("toggle")
        || state.contains(State::Checkable);
    if state.contains(State::Indeterminate) && checkable {
        marks.push("mixed");
    } else if state.contains(State::Checked) {
        marks.push("checked");
    } else if check_role {
        marks.push("unchecked");
    }
    if state.contains(State::Pressed) {
        marks.push("pressed");
    }
    let item = matches!(
        role_lower,
        "list item" | "tree item" | "table cell" | "cell" | "table row" | "row"
    );
    if !menu && state.contains(State::Expanded) {
        marks.push("expanded");
    } else if item && state.contains(State::Expandable) {
        marks.push("collapsed");
    }
    if (item || matches!(role_lower, "page tab" | "tab" | "icon"))
        && state.contains(State::Selected)
    {
        marks.push("selected");
    }
    marks
}

/// GTK, Qt, VCL and the browser bridges publish `Showing` for a widget that
/// is mapped on screen; a hidden widget keeps `Visible` at most. A state set
/// that carries neither is a hidden widget, not an old bridge.
fn is_showing_state(state: &StateSet) -> bool {
    state.contains(State::Showing)
}

/// Menu entries are the exception to the `Showing` gate: gail (GTK2, e.g.
/// GIMP 2.10) never publishes `Showing` on a menu item, open menu or not,
/// and an item in a closed menu is still reachable through its AT-SPI
/// `click` action (that is how a background click opens a GIMP dialog). A
/// `Visible` menu entry therefore stays indexed; a foreground click on one
/// that is not actually on screen takes the action route instead of the
/// pointer (see the click tool), so listing it is safe.
fn is_menu_entry_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "menu" | "menu item" | "check menu item" | "radio menu item" | "tear off menu item"
    )
}

fn counts_as_showing(role_lower: &str, state: &StateSet) -> bool {
    is_showing_state(state) || (is_menu_entry_role(role_lower) && state.contains(State::Visible))
}

/// `(listed, offscreen)` for a node with a known state set. A hidden widget
/// (neither showing nor `Visible`) is left out of the tree and the index. A
/// `Visible` widget that is not showing stays listed — an element action can
/// still reach a control scrolled out of view, as on every other platform —
/// and is marked off-screen.
fn listing_state(role_lower: &str, state: &StateSet) -> (bool, bool) {
    if counts_as_showing(role_lower, state) {
        (true, false)
    } else if state.contains(State::Visible) {
        (true, true)
    } else {
        (false, false)
    }
}

/// Description note for an indexed element that is not on screen.
const OFFSCREEN_NOTE: &str =
    "off-screen: scroll it into view before a pixel action; element actions still reach it";

/// How many leading children of a menubar menu the closed-menu peek reads
/// (gail puts a state-less tearoff item first).
const MENU_PEEK_CHILDREN: usize = 3;

/// Format an AT-SPI numeric value like the historical `str(currentValue)`
/// (e.g. `1.0`), so `value="..."` fields stay byte-compatible.
fn format_value(v: f64) -> String {
    format!("{v:?}")
}

/// Interpret the positive AT-SPI states that establish user operability.
///
/// GTK3 commonly publishes both `Enabled` and `Sensitive`. GTK4's native
/// exporter derives widget operability from its `disabled` accessibility state
/// and publishes `Sensitive` alone for an enabled widget. Either positive state
/// therefore establishes operability; an empty set still means disabled.
fn is_enabled_state(state: &StateSet) -> bool {
    state.contains(State::Enabled) || state.contains(State::Sensitive)
}

/// Whether a walked node is exposed as an indexed, usable element.
///
/// Historically this was "the node advertises AT-SPI Actions" (buttons, menu
/// items, links). That silently dropped Value-only widgets, editable text, and
/// selectable list rows. GTK list rows expose Component + Selectable state but
/// no Action even though a coordinate click on their bounds is operable. Keep
/// every such control in the shared index space so physical-input fallbacks can
/// address it without inventing pixels in the caller. Some GTK4 buttons expose
/// only Component plus their control role; include those only when the state set
/// positively verifies that they are enabled. Passive component-backed labels
/// and containers remain outside the index.
///
/// This predicate is the single source of truth for the element-index space and
/// MUST be applied identically in `render` and in every `action_nodes` filter
/// (`perform_action`, `set_value`, `get_element_bounds_for_window`, snapshot bounds);
/// any divergence would desync indices between the snapshot and the operations.
fn is_indexable(v: &Visited) -> bool {
    v.showing
        && is_indexable_capabilities(
            &v.role,
            !v.actions.is_empty(),
            v.has_editable,
            v.has_value,
            v.selectable,
            v.has_component,
            v.enabled,
        )
}

fn is_indexable_capabilities(
    role: &str,
    has_action: bool,
    has_editable: bool,
    has_value: bool,
    has_selectable_state: bool,
    has_component: bool,
    enabled: Option<bool>,
) -> bool {
    let normalized_role = role.trim().to_ascii_lowercase();
    let pixel_addressable_control = has_component
        && enabled == Some(true)
        && matches!(normalized_role.as_str(), "button" | "push button");
    !is_passive_role(&normalized_role)
        && (has_action
            || has_editable
            || has_value
            || has_selectable_state
            || pixel_addressable_control)
        && enabled == Some(true)
}

// ── Public (sync) entry points ───────────────────────────────────────────────

fn first_application_walk(pid: u32) -> bool {
    static PIDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(pid)
}

/// One accessibility snapshot, plus whether it was provably narrowed to the
/// caller's window.
pub struct WalkedTree {
    pub markdown: String,
    pub nodes: Vec<AtspiNode>,
    pub bounds: Vec<(usize, i32, i32, u32, u32)>,
    /// True when a non-zero `xid` was resolved to exactly one application
    /// top-level and the snapshot contains only that window's nodes. False
    /// means the snapshot spans every window the application publishes.
    pub window_scoped: bool,
    /// Set when the walk stopped before exhausting the tree.
    pub truncation: Option<Truncation>,
    /// The nodes are a role search's matches ([`RoleSearch`]) below the
    /// window's top-level, found through AT-SPI Collection instead of a walk.
    pub searched_roles: bool,
    /// Accessibles visited, listed or not (the starting top-level included).
    pub visited: usize,
    /// False when the bounds phase ran out of time; some indexed elements
    /// then carry no frame even though they exist.
    pub bounds_complete: bool,
    /// Wall time of the walk + bounds phases.
    pub elapsed: Duration,
}

/// Walk the AT-SPI tree with caller-supplied node + depth caps.
/// `max_elements = None` keeps the historical 5 000-node default; `max_depth
/// = None` keeps the historical unbounded depth. Issue #22865.
pub fn walk_tree_bounded(
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
) -> Result<Option<WalkedTree>> {
    walk_tree_bounded_with_timeout(pid, xid, max_elements, max_depth, None, None, OP_TIMEOUT)
}

/// [`walk_tree_bounded`] within `timeout`. With `role_search`, a window whose
/// toolkit supports it is searched for those roles instead of walked
/// (`WalkedTree::searched_roles` says whether that happened).
/// `text_read_chars` is how much of a text field's content to read (`None`:
/// the usual [`TEXT_READ_CHARS`]).
pub(super) fn walk_tree_bounded_with_timeout(
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
    role_search: Option<&RoleSearch>,
    text_read_chars: Option<usize>,
    timeout: Duration,
) -> Result<Option<WalkedTree>> {
    runtime().block_on(async {
        // Tree traversal and bounds collection form one snapshot. Keep one
        // deadline for both phases so a dead AT-SPI peer cannot outlive the
        // operation timeout while resolving geometry. The deadline is installed
        // as the task-local operation deadline so every per-node call clamps
        // to it and the walk returns *partial* results instead of being
        // dropped wholesale by the outer timeout.
        let walk_started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        // Backstop only: the walk observes `deadline` itself and returns its
        // partial result; this guards the few un-wrapped awaits.
        let backstop = deadline + Duration::from_millis(500);
        let walk = OP_DEADLINE.scope(deadline, async {
            let conn = shared_connection().await?;
            let options = WalkOptions {
                role_search,
                text_read_chars,
                ..WalkOptions::default()
            };
            collect_visited_bounded_opts(conn, pid, xid, max_elements, max_depth, options).await
        });
        let walked = match before_snapshot_deadline(backstop, walk).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) if error.to_string() == "app_lookup_timeout" => {
                dlog!("walk_tree: application lookup timed out for pid {pid}");
                return Ok(Some(WalkedTree {
                    markdown: String::new(),
                    nodes: Vec::new(),
                    bounds: Vec::new(),
                    window_scoped: false,
                    truncation: Some(Truncation {
                        reason: "app_lookup_timeout",
                        visited: 0,
                        pending: 0,
                    }),
                    searched_roles: false,
                    visited: 0,
                    bounds_complete: false,
                    elapsed: walk_started.elapsed(),
                }));
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                dlog!("walk_tree timed out for pid {pid}");
                return Ok(Some(WalkedTree {
                    markdown: String::new(),
                    nodes: Vec::new(),
                    bounds: Vec::new(),
                    window_scoped: false,
                    truncation: Some(Truncation {
                        reason: "timeout",
                        visited: 0,
                        pending: 0,
                    }),
                    searched_roles: false,
                    visited: 0,
                    bounds_complete: false,
                    elapsed: walk_started.elapsed(),
                }));
            }
        };
        let Some(Collected {
            visited,
            scoped_frame,
            truncation,
            searched_roles,
        }) = walked
        else {
            return Ok(None);
        };
        let walk_elapsed = walk_started.elapsed();
        let visited_count = visited.len();
        let (markdown, nodes) = render(&visited, scoped_frame);
        let bounds_started = std::time::Instant::now();
        // Bounds resolution shares the deadline, but a walk that consumed the
        // whole budget must still yield *some* frames (element-index clicks
        // resolve against them without a re-walk), so grant the bounds phase a
        // small grace of up to half the budget. The tool documents this.
        let bounds_deadline = deadline
            .max(tokio::time::Instant::now() + (timeout / 2).min(Duration::from_millis(1500)));
        let bounds_backstop = bounds_deadline + Duration::from_millis(500);
        let (bounds, bounds_complete) = match before_snapshot_deadline(
            bounds_backstop,
            OP_DEADLINE.scope(
                bounds_deadline,
                element_bounds_for_visited(&visited, pid, xid, scoped_frame),
            ),
        )
        .await
        {
            Ok((bounds, complete)) => (bounds, complete),
            Err(_) => {
                dlog!("element bounds timed out for pid {pid}");
                (Vec::new(), false)
            }
        };
        dlog!(
            "snapshot phases pid {pid}: walk_ms={} bounds_ms={} visited={} bounds={} complete={}",
            walk_elapsed.as_millis(),
            bounds_started.elapsed().as_millis(),
            visited.len(),
            bounds.len(),
            bounds_complete
        );
        // Bounds are keyed by the application-wide element index, so drop the
        // entries for windows this snapshot no longer shows.
        let bounds = if scoped_frame.is_some() {
            let emitted: std::collections::HashSet<usize> =
                nodes.iter().filter_map(|node| node.element_index).collect();
            bounds
                .into_iter()
                .filter(|(index, ..)| emitted.contains(index))
                .collect()
        } else {
            bounds
        };
        Ok(Some(WalkedTree {
            markdown,
            nodes,
            bounds,
            window_scoped: scoped_frame.is_some(),
            truncation,
            searched_roles,
            visited: visited_count,
            bounds_complete,
            elapsed: walk_started.elapsed(),
        }))
    })
}

/// Enumerate top-level windows from the AT-SPI registry — the window-listing
/// fallback for Wayland compositors that DON'T implement
/// `zwlr_foreign_toplevel_management` (GNOME Mutter, KDE KWin). Native Wayland
/// apps have no X11 XID and Mutter/KWin expose no foreign-toplevel list, so
/// `wayland::list_windows` comes back empty there and the whole element flow
/// (get_window_state -> click by element_index) is unreachable — even though the
/// AT-SPI tree itself is keyed by PID and works fine (see `walk_tree_bounded`,
/// whose walk ignores the xid). This bridges that gap: it returns one
/// [`WindowInfo`] per application top-level frame, with a SYNTHETIC but stable
/// `xid`. Downstream `get_window_state` / `click` walk the tree by PID and never
/// dereference the xid against X11, so the synthetic value only needs to be
/// non-zero and to round-trip back from the caller.
pub fn list_windows(filter_pid: Option<u32>) -> Vec<crate::x11::WindowInfo> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || list_windows_blocking(filter_pid))
            .join()
            .unwrap_or_default();
    }
    list_windows_blocking(filter_pid)
}

/// Stacking position (`_NET_CLIENT_LIST_STACKING`, bottom-to-top) for an
/// AT-SPI frame, joined to the X11 toplevels of the same pid by title (the
/// AT-SPI xid is synthetic). `None` when X11 does not know the window.
fn z_index_from_stacking(
    stacking: &[crate::x11::WindowInfo],
    pid: u32,
    title: &str,
) -> Option<usize> {
    let same_pid: Vec<&crate::x11::WindowInfo> =
        stacking.iter().filter(|w| w.pid == Some(pid)).collect();
    same_pid
        .iter()
        .find(|w| w.title == title)
        .copied()
        .or_else(|| (same_pid.len() == 1).then(|| same_pid[0]))
        .and_then(|w| w.z_index)
}

fn list_windows_blocking(filter_pid: Option<u32>) -> Vec<crate::x11::WindowInfo> {
    use crate::x11::WindowInfo;
    // X11 knows the stacking order the AT-SPI registry does not; on an X
    // session the frames get their `z_index` from it so callers can rank
    // an app's dialog above the main window it covers.
    let stacking: Vec<WindowInfo> = if crate::wayland::is_wayland() {
        Vec::new()
    } else {
        crate::x11::list_windows(filter_pid)
    };
    runtime().block_on(async {
        let work = async {
            let conn = shared_connection().await?;
            let zconn = conn.connection();
            let root = match call(conn.root_accessible_on_registry()).await {
                Some(Ok(r)) => r,
                _ => return Ok(Vec::new()),
            };
            let dbus = atspi::zbus::fdo::DBusProxy::new(zconn)
                .await
                .map_err(|e| anyhow!("DBus proxy unavailable: {e}"))?;
            let apps = match call(root.get_children()).await {
                Some(Ok(a)) => a,
                _ => return Ok(Vec::new()),
            };
            let mut out: Vec<WindowInfo> = Vec::new();
            for app_ref in apps {
                // Skip apps that can't answer the pid query (modal-grabbed) and
                // apps that don't match the filter.
                let cpid = match call(pid_of(&dbus, &app_ref)).await {
                    Some(Some(p)) => p,
                    _ => continue,
                };
                if let Some(want) = filter_pid {
                    if cpid != want {
                        continue;
                    }
                }
                let app_ref = match RawObjectRef::from_atspi(&app_ref) {
                    Some(app_ref) => app_ref,
                    None => continue,
                };
                let app = match call(accessible_for(conn, &app_ref)).await {
                    Some(Ok(a)) => a,
                    _ => continue,
                };
                let app_name = call(app.name())
                    .await
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let frames = match call(app.get_children()).await {
                    Some(Ok(c)) => c,
                    _ => Vec::new(),
                };
                let mut emitted = 0usize;
                for (i, frame_ref) in frames.iter().enumerate() {
                    let frame_ref = match RawObjectRef::from_atspi(frame_ref) {
                        Some(frame_ref) => frame_ref,
                        None => continue,
                    };
                    let frame = match call(accessible_for(conn, &frame_ref)).await {
                        Some(Ok(f)) => f,
                        _ => continue,
                    };
                    let role = call(frame.get_role_name())
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                    if !is_window_role(&role) {
                        continue;
                    }
                    let title = call(frame.name())
                        .await
                        .and_then(|r| r.ok())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| app_name.clone());
                    let geometry = match call(frame.proxies()).await {
                        Some(Ok(proxies)) => match call(proxies.component()).await {
                            Some(Ok(component)) => call(component.get_extents(CoordType::Screen))
                                .await
                                .and_then(|result| result.ok()),
                            _ => None,
                        },
                        _ => None,
                    };
                    let (observed_x, observed_y, width, height) = geometry
                        .filter(|(_, _, width, height)| *width > 0 && *height > 0)
                        .map(|(x, y, width, height)| {
                            (x, y, width.max(0) as u32, height.max(0) as u32)
                        })
                        .unwrap_or((0, 0, 0, 0));
                    // On native Wayland, AT-SPI frame extents can be a stale
                    // toolkit default even when the compositor has already
                    // placed the window elsewhere. Prefer compositor metadata
                    // by pid/title so list_windows and later element geometry
                    // share the same screen origin.
                    let (x, y) = prefer_authoritative_wayland_origin(
                        authoritative_wayland_origin(cpid, 0, Some(&title)),
                        Some((observed_x, observed_y)),
                    )
                    .unwrap_or((observed_x, observed_y));
                    // Stable, non-zero, unique per (pid, frame ordinal).
                    let xid = (((cpid as u64) << 16) | (i as u64)).max(1);
                    let z_index = z_index_from_stacking(&stacking, cpid, &title);
                    out.push(WindowInfo {
                        xid,
                        pid: Some(cpid),
                        app_name: app_name.clone(),
                        title,
                        is_on_screen: width > 0 && height > 0,
                        z_index,
                        x,
                        y,
                        width,
                        height,
                    });
                    emitted += 1;
                }
                // App with no enumerable top-level frame still gets one handle so
                // the by-pid AT-SPI element flow stays reachable.
                if emitted == 0 {
                    out.push(WindowInfo {
                        xid: (cpid as u64).max(1),
                        pid: Some(cpid),
                        app_name: app_name.clone(),
                        title: app_name,
                        is_on_screen: true,
                        z_index: None,
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                }
            }
            Ok::<_, anyhow::Error>(out)
        };
        match tokio::time::timeout(OP_TIMEOUT, work).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                dlog!("atspi list_windows failed: {e}");
                Vec::new()
            }
            Err(_) => {
                dlog!("atspi list_windows timed out");
                Vec::new()
            }
        }
    })
}

/// Pick the editable node to write into, by priority:
///   1. the focused editable (if the toolkit exposes focus),
///   2. an editable inside web/document content — for a browser this is the
///      page's field, not the address bar (which sorts first in the tree but is
///      chrome),
///   3. the first editable anywhere (covers single-field apps like a GTK dialog
///      entry, or a GTK4 GtkEntry).
///
/// Choose the editable to write into. `complete` says whether `visited` is the
/// whole tree: on a *partial* walk the "first editable anywhere" fallback is
/// withheld, because the field the user means (the focused one) may simply not
/// have been reached yet and writing into an arbitrary earlier field (a name
/// box, a search entry) would be a silent misdelivery.
fn pick_editable<'v, 'a>(visited: &'v [Visited<'a>], complete: bool) -> Option<&'v Visited<'a>> {
    visited
        .iter()
        .find(|v| v.has_editable && v.focused)
        .or_else(|| visited.iter().find(|v| v.has_editable && v.in_web_doc))
        .or_else(|| {
            complete
                .then(|| visited.iter().find(|v| v.has_editable))
                .flatten()
        })
}

/// Try to write `text` into the best editable node in `visited` via AT-SPI
/// EditableText. Returns `Ok(true)` if the write landed,
/// `Ok(false)` if no editable was found / the EditableText write was rejected.
async fn write_into_editable(visited: &[Visited<'_>], complete: bool, text: &str) -> Result<bool> {
    let target = match pick_editable(visited, complete) {
        Some(t) => t,
        None => return Ok(false),
    };
    write_into_editable_target(target, text).await
}

async fn write_into_editable_target(target: &Visited<'_>, text: &str) -> Result<bool> {
    dlog!(
        "insert target: role={:?} in_web_doc={} focused={} has_component={}",
        target.role,
        target.in_web_doc,
        target.focused,
        target.has_component
    );
    write_into_editable_acc(&target.acc, target.has_component, text).await
}

/// EditableText write against a bare accessible (from a cached snapshot ref or
/// the focus-event log). `has_component` gates the GrabFocus retry.
async fn write_into_editable_acc(
    acc: &AccessibleProxy<'_>,
    has_component: bool,
    text: &str,
) -> Result<bool> {
    let target_has_component = has_component;
    let proxies = acc
        .proxies()
        .await
        .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;

    // A focus-free EditableText write is the strongest background route. Try it
    // before GrabFocus: WebKitGTK can invalidate the original proxy when focus
    // changes, and writing through that stale object then returns false.
    if write_through_editable_proxies(&proxies, text).await? {
        return Ok(true);
    }

    // Try to grab focus on the widget via AT-SPI Component.GrabFocus.
    // This should give the widget internal keyboard focus without activating
    // the window, allowing GTK4 (and similar toolkits) to expose EditableText
    // on an unfocused window's focused widget.
    if target_has_component {
        if let Ok(comp) = proxies.component().await {
            match call(comp.grab_focus()).await {
                Some(Ok(true)) => dlog!("GrabFocus succeeded"),
                Some(Ok(false)) => dlog!("GrabFocus returned false"),
                Some(Err(e)) => dlog!("GrabFocus failed: {}", e),
                None => dlog!("GrabFocus timed out"),
            }
        } else {
            dlog!("Component interface unavailable despite has_component=true");
        }
    } else {
        dlog!("Target has no Component interface, skipping GrabFocus");
    }

    write_through_editable_proxies(&proxies, text).await
}

async fn write_through_editable_proxies(
    proxies: &atspi::proxy::proxy_ext::Proxies<'_>,
    text: &str,
) -> Result<bool> {
    let et = proxies
        .editable_text()
        .await
        .map_err(|e| anyhow!("EditableText unavailable: {e}"))?;

    let off = match proxies.text().await {
        Ok(tp) => tp.caret_offset().await.unwrap_or(0),
        Err(_) => 0,
    };
    let len = text.chars().count() as i32;

    if et.insert_text(off, text, len).await.unwrap_or(false) {
        return Ok(true);
    }
    if et.set_text_contents(text).await.unwrap_or(false) {
        return Ok(true);
    }
    Ok(false)
}

/// Write into the best editable of window `xid` (0: any window of the app)
/// without falling through to synthetic X11 input.
pub fn type_into_editable(pid: u32, xid: u64, text: &str) -> Result<()> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            // Focused editable known from the event log: write there directly.
            if let Some(true) = write_into_focused_editable(conn, pid, xid, text).await {
                return Ok(());
            }
            let Collected {
                visited,
                truncation,
                ..
            } = collect_visited_bounded(conn, pid, xid, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            if write_into_editable(&visited, truncation.is_none(), text).await? {
                Ok(())
            } else {
                Err(anyhow!("no writable AT-SPI element found for pid {pid}"))
            }
        },
        || Err(anyhow!("AT-SPI editable lookup timed out for pid {pid}")),
    )
}

/// Write into the exact indexed editable exposed by the caller's snapshot of
/// window `xid`, re-walking that window (the snapshot's index space).
pub fn type_into_editable_at(pid: u32, xid: u64, idx: usize, text: &str) -> Result<()> {
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let visited = collect_window_visited(conn, pid, xid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = visited
                .iter()
                .filter(|node| is_indexable(node))
                .nth(idx)
                .ok_or_else(|| anyhow!("element {idx} not found (total: {})", visited.len()))?;
            if write_into_editable_target(target, text).await? {
                Ok(())
            } else {
                // GrabFocus can rebuild WebKitGTK's accessibility object. Walk
                // the same index space again and retry only that exact element;
                // never fall through to a different focused/first editable.
                let refreshed = collect_window_visited(conn, pid, xid)
                    .await?
                    .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
                let refreshed_target = refreshed
                    .iter()
                    .filter(|node| is_indexable(node))
                    .nth(idx)
                    .ok_or_else(|| {
                        anyhow!("element {idx} disappeared after AT-SPI focus refresh")
                    })?;
                if write_into_editable_target(refreshed_target, text).await? {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "element {idx} is not writable through AT-SPI EditableText"
                    ))
                }
            }
        },
        || {
            Err(anyhow!(
                "AT-SPI editable write timed out for element {idx} in pid {pid}"
            ))
        },
    )
}

/// Insert `text` into the focused editable of window `xid`, else the best
/// editable a walk of that window finds (`xid == 0`: any window of the app).
pub fn insert_text(pid: u32, xid: u64, text: &str) -> Result<bool> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            if let Some(true) = write_into_focused_editable(conn, pid, xid, text).await {
                return Ok(true);
            }
            let Collected {
                visited,
                truncation,
                ..
            } = match collect_visited_bounded(conn, pid, xid, None, None).await? {
                Some(v) => v,
                None => return Ok(false),
            };
            let complete = truncation.is_none();

            dlog!(
                "insert_text: {} node(s), {} editable, {} entry/text-role",
                visited.len(),
                visited.iter().filter(|v| v.has_editable).count(),
                visited
                    .iter()
                    .filter(|v| v.role.contains("entry") || v.role.contains("text"))
                    .count(),
            );

            // Primary attempt: write into an editable exposed by the current tree.
            // GrabFocus (inside write_into_editable) gives the widget internal
            // keyboard focus without activating the window, so toolkits that expose
            // EditableText on an unfocused window (Qt6, and GTK4 with GTK_A11Y=atspi)
            // accept the write here.
            if write_into_editable(&visited, complete, text).await? {
                return Ok(true);
            }
            if !complete {
                // A partial walk cannot prove there is no better field further
                // down the tree; the click+type guess below would be a silent
                // misdelivery. Let the caller refuse honestly instead.
                dlog!("insert_text: walk incomplete; skipping the entry-guess fallback");
                return Ok(false);
            }

            // GTK3 fallback: the toolkit exposes entry/text nodes in the tree (so
            // get_text reads work) but gates EditableText on focus/activation. Try
            // finding an entry/text role with Component bounds and use X11 click+type.
            dlog!("AT-SPI EditableText unavailable; checking for entry/text with Component for X11 fallback");

            let entry_candidate = visited.iter().find(|v| {
                let r = v.role.to_ascii_lowercase();
                (r.contains("entry") || r.contains("text")) && v.has_component
            });

            if let Some(entry) = entry_candidate {
                dlog!(
                "GTK3 fallback: found entry role={:?} with Component; attempting X11 click+type",
                entry.role
            );

                // Get the entry widget's screen bounds via Component.GetExtents.
                if let Ok(proxies) = entry.acc.proxies().await {
                    if let Ok(comp) = proxies.component().await {
                        if let Some(Ok((x, y, w, h))) =
                            call(comp.get_extents(CoordType::Screen)).await
                        {
                            // Click the center of the entry to establish widget focus (not window focus).
                            let cx = x + (w.max(0) / 2);
                            let cy = y + (h.max(0) / 2);
                            dlog!("GTK3 fallback: entry bounds ({x},{y} {w}x{h}), clicking center ({cx},{cy})");

                            // Get the window XID for this app so we can send X11 events to it.
                            let Some(xid) = entry_find_window_xid(pid).await else {
                                dlog!("GTK3 fallback: could not find window XID");
                                return Ok(false);
                            };

                            // Translate screen coords to window-local coords for XSendEvent.
                            let Some((wx, wy)) = screen_to_window_coords(xid, cx, cy) else {
                                dlog!("GTK3 fallback: screen-to-window coord translation failed");
                                return Ok(false);
                            };

                            dlog!("GTK3 fallback: window XID {xid}, local coords ({wx},{wy})");

                            // Click the entry to focus the widget (widget focus, not window focus).
                            if let Err(e) = crate::input::send_click(xid, wx, wy, 1, 1) {
                                dlog!("GTK3 fallback: click failed: {e}");
                                return Ok(false);
                            };

                            // Small delay for the click to register and the widget to update focus.
                            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

                            // Now type via X11 XSendEvent — the entry widget has internal focus
                            // so it should accept the keystrokes even though the window is unfocused.
                            if let Err(e) = crate::input::send_type_text(xid, text) {
                                dlog!("GTK3 fallback: send_type_text failed: {e}");
                                return Ok(false);
                            }

                            dlog!("GTK3 fallback: X11 click+type succeeded");
                            return Ok(true);
                        }
                    }
                }
            }

            Ok(false)
        },
        || {
            dlog!("insert_text timed out for pid {pid}; falling back to synthetic typing");
            Ok(false)
        },
    )
}

/// What holds the keyboard focus in window `xid` (0: any window of the app),
/// so `type_text` can target the thing the user just clicked rather than the
/// first editable anywhere. `Some(true)` is a focused editable widget (a text
/// entry or box), which AT-SPI EditableText insertion targets correctly.
/// `Some(false)` is a focused non-editable widget that still accepts typed
/// input (a spreadsheet cell, a terminal, a canvas); an AT-SPI editable search
/// would grab the wrong field there (e.g. gnumeric's name box), so the caller
/// should synth-type into the focused widget instead. `None` means nothing was
/// found, including a focus that is in another window of the app.
pub fn focused_is_editable(pid: u32, xid: u64) -> Result<Option<bool>> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            // O(1) answer from the focus-event log when available.
            if let Some((acc, oref)) = focused_accessible_via_events(conn, pid).await {
                if object_in_window(conn, pid, xid, &oref).await == Some(false) {
                    return Ok(None);
                }
                if let Some(Ok(ifaces)) = call(acc.get_interfaces()).await {
                    return Ok(Some(ifaces.contains(Interface::EditableText)));
                }
            }
            let visited = match collect_window_visited(conn, pid, xid).await? {
                Some(v) => v,
                None => return Ok(None),
            };
            Ok(visited.iter().find(|v| v.focused).map(|v| v.has_editable))
        },
        || Ok(None),
    )
}

/// Role and name of the accessible holding the widget focus in `pid`, from
/// the focus-event log only (no walk): cheap enough to run after every
/// pointer press, so a grid click can name the cell it landed on
/// (LibreOffice reports "D2"). `None` when the log has nothing current.
pub fn focused_control(pid: u32) -> Option<(String, String)> {
    bounded_for(
        Duration::from_millis(400),
        async {
            let conn = shared_connection().await?;
            let Some((acc, focused_ref)) = focused_accessible_via_events(conn, pid).await else {
                return Ok(None);
            };
            let (role, name) = tokio::join!(call(acc.get_role_name()), call(acc.name()));
            let (Some(Ok(role)), Some(Ok(name))) = (role, name) else {
                return Ok(None);
            };
            // LibreOffice keeps the focus on the sheet's `table`; the current
            // cell is its active descendant (event log), else its selected
            // child.
            if role.eq_ignore_ascii_case("table") {
                let active = active_descendant_map()
                    .lock()
                    .unwrap()
                    .get(&focused_ref.bus)
                    .cloned();
                if let Some(path) = active {
                    let raw = RawObjectRef {
                        name: focused_ref.bus.clone(),
                        path,
                    };
                    if let Some(Ok(cell)) = call(accessible_for(conn, &raw)).await {
                        let (cell_role, cell_name) =
                            tokio::join!(call(cell.get_role_name()), call(cell.name()));
                        if let (Some(Ok(cell_role)), Some(Ok(cell_name))) = (cell_role, cell_name) {
                            if !cell_name.trim().is_empty() {
                                return Ok(Some((cell_role, cell_name)));
                            }
                        }
                    }
                }
                if let Some(Ok(proxies)) = call(acc.proxies()).await {
                    if let Some(Ok(selection)) = call(proxies.selection()).await {
                        if let Some(Ok(n)) = call(selection.n_selected_children()).await {
                            if n > 0 {
                                if let Some(Ok(child)) = call(selection.get_selected_child(0)).await
                                {
                                    if let Some(raw) = RawObjectRef::from_atspi(&child) {
                                        if let Some(Ok(cell)) =
                                            call(accessible_for(conn, &raw)).await
                                        {
                                            let (cell_role, cell_name) = tokio::join!(
                                                call(cell.get_role_name()),
                                                call(cell.name())
                                            );
                                            if let (Some(Ok(cell_role)), Some(Ok(cell_name))) =
                                                (cell_role, cell_name)
                                            {
                                                return Ok(Some((cell_role, cell_name)));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some((role, name)))
        },
        || Ok(None),
    )
    .ok()
    .flatten()
}

/// Find the window XID for a PID by listing its X11 windows.
async fn entry_find_window_xid(pid: u32) -> Option<u64> {
    use crate::x11::list_windows;

    // List X11 windows for that PID and return the first one.
    let windows = list_windows(Some(pid));
    let xid = windows.first()?.xid;
    Some(xid)
}

/// Translate screen coordinates to window-local coordinates.
fn screen_to_window_coords(xid: u64, screen_x: i32, screen_y: i32) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;

    let (conn, _) = crate::x11::query_connection().ok()?;
    let window = xid as u32;

    // Get window geometry to find its screen position.
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;

    // Translate to root coordinates (screen coords of window's origin).
    let trans = conn
        .translate_coordinates(window, geom.root, 0, 0)
        .ok()?
        .reply()
        .ok()?;

    // Window-local = screen - window_origin.
    Some((screen_x - trans.dst_x as i32, screen_y - trans.dst_y as i32))
}

/// Activation verbs an AT-SPI action name may carry. Compared against the
/// segment after the last `.`, because GTK4 exposes namespaced action names
/// (`buffer.delete-line`, `clipboard.copy`) while GTK3/Qt expose bare ones
/// (`click`, `activate`).
const ACTIVATION_VERBS: &[&str] = &[
    "click",
    // Chromium exposes this on a static/text node whose clickable target is an
    // ancestor. Dropping it would take away a working path.
    "clickancestor",
    "activate",
    "press",
    "invoke",
    "toggle",
    "open",
    "jump",
    "dodefault",
];

/// Checkbox-only verbs exposed by Chromium's AT-SPI bridge. These are not
/// globally safe activation names: an unrelated widget may advertise a
/// namespaced action ending in `check`, so the role gate is mandatory.
const CHECKBOX_ACTIVATION_VERBS: &[&str] = &["check", "uncheck"];

/// Menu-only verbs. Qt gives a menu-bar item, and a menu entry that opens a
/// submenu, exactly one AT-SPI action: `Show Menu` (seen on Kate, KDE Plasma
/// 6.8.1, 2026-10-25). Role-gated like the checkbox verbs: a tool button or
/// combo box can advertise the same verb, and opening its drop-down is not
/// what a click on it does.
const MENU_ACTIVATION_VERBS: &[&str] = &["showmenu"];

fn normalized_action_verb(name: &str) -> String {
    name.rsplit('.')
        .next()
        .unwrap_or(name)
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Is this action name an activation — the AT-SPI analogue of a click?
///
/// Position is not meaning. A GTK4 text view advertises fifteen actions whose
/// first is `buffer.delete-line`, so actuating "action 0" there deletes a line
/// of the user's document instead of placing a caret.
fn is_activation_action(name: &str) -> bool {
    let verb = normalized_action_verb(name);
    ACTIVATION_VERBS.contains(&verb.as_str())
}

fn is_checkbox_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "check box" | "checkbox"
    )
}

fn is_menu_or_menu_item_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "menu item"
    )
}

/// Index of the action to actuate, or `None` when the element advertises no
/// activation. `None` must not fall back to index 0: firing an arbitrary
/// action is worse than reporting that there is nothing to fire.
fn activation_index(role: &str, actions: &[String]) -> Option<usize> {
    actions.iter().position(|action| {
        let role_gated = |verbs: &[&str]| verbs.contains(&normalized_action_verb(action).as_str());
        is_activation_action(action)
            || (is_checkbox_role(role) && role_gated(CHECKBOX_ACTIVATION_VERBS))
            || (is_menu_or_menu_item_role(role) && role_gated(MENU_ACTIVATION_VERBS))
    })
}

/// Container roles whose `activate` / `open` / `menu` action acts on the
/// container's *current selection* (Nautilus' `layered pane "Icon View"`
/// opens the selected file; a GTK `list` activates its selected row). A
/// bubbling at-point walk must never fire these: the user clicked an item,
/// not "open whatever is selected".
pub(crate) fn is_container_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "layered pane"
            | "list"
            | "list box"
            | "table"
            | "tree"
            | "tree table"
            | "icon view"
            | "panel"
            | "scroll pane"
            | "viewport"
    )
}

/// Item roles a click selects rather than activates when they expose no
/// Action of their own (Nautilus file `canvas` items, GTK list rows / cells).
/// A spreadsheet / grid cell. Selecting one through its table's `Selection`
/// interface marks it (LibreOffice Calc highlights the cell) but does not
/// move the cell cursor, so text typed afterwards still lands in the
/// previously active cell (A1 on a fresh sheet). With a real pointer
/// available, a click on a cell must be a real press.
fn is_cell_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "table cell" | "cell"
    )
}

fn is_selectable_item_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "canvas" | "list item" | "table cell" | "cell" | "icon" | "tree item" | "table row" | "row"
    )
}

/// The at-point activation rule. The deepest node under the point may fire
/// any activation it advertises unless it is a container (a click on a
/// container's empty area must not open its current selection); an ancestor
/// may only be fired when it is neither a container nor a `canvas` (a drawing
/// surface whose `activate` means "open the selection" once it has children).
fn at_point_activation_index(
    role: &str,
    actions: &[String],
    is_deepest_hit: bool,
) -> Option<usize> {
    if is_container_role(role) || crate::at_point_policy::is_top_level_shell_role(role) {
        return None;
    }
    if !is_deepest_hit && role.trim().eq_ignore_ascii_case("canvas") {
        return None;
    }
    activation_index(role, actions)
}

/// What an at-point accessibility click did, for the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtPointHit {
    /// AT-SPI action name fired (`activate`, `click`), or `select_child`
    /// when the item was selected through its container's Selection
    /// interface instead.
    pub action: String,
    pub role: String,
    pub name: String,
    /// D-Bus object path of the node acted on.
    pub path: String,
    /// Set on the Selection route: the item that is now selected.
    pub selected: Option<String>,
    /// Selection route only: `Selection.IsChildSelected` read back true.
    pub selection_verified: bool,
}

impl AtPointHit {
    pub fn fired(action: String, role: String, name: String, path: String) -> Self {
        Self {
            action,
            role,
            name,
            path,
            selected: None,
            selection_verified: false,
        }
    }

    /// `hit: push button "Restore" action=activate path=...` /
    /// `selected: canvas "file.txt" ...`.
    pub fn describe(&self) -> String {
        match &self.selected {
            Some(item) => format!(
                "selected: {} \"{}\" (Selection.SelectChild on its container{})",
                self.role,
                item,
                if self.selection_verified {
                    ", read back as selected"
                } else {
                    ", selection not read back"
                }
            ),
            None => format!(
                "hit: {} \"{}\" action={} path={}",
                self.role, self.name, self.action, self.path
            ),
        }
    }
}

/// Characters of a text field read into its tree value. A field holds its
/// whole document (KWrite's notes.txt is 24,492 characters), so the walk
/// reads only this much of it.
const TEXT_READ_CHARS: usize = 4096;

/// Characters of a text field a `full_output` read takes, which returns
/// values whole. AT-SPI's `GetText(0, CharacterCount)` returned all 24,492
/// characters of KWrite's notes.txt in one call in under a millisecond
/// (slot test, KWrite on KDE Frameworks 6, 2026-11-11), so the limit only
/// keeps a document of megabytes out of one reply; a row past it still
/// says `(N chars, the first M read)`.
pub const FULL_TEXT_READ_CHARS: usize = 250_000;

/// The ` value="..."` part of a row. Its character count is written here
/// whenever the shown text is not the whole value or the count of the shown
/// text would be wrong: a field longer than [`TEXT_READ_CHARS`] (` value=
/// "<first 4096>..." (24492 chars, the first 4096 read)`), and a long value
/// with line breaks, which the row escapes as two characters each. The value
/// cutter in pincer-core keeps a count it finds here instead of counting the
/// escaped text (it said 4146 for a 24,492-character document).
fn value_part(val: &str, total_chars: Option<usize>) -> String {
    let shown = one_line(val);
    let read = val.chars().count();
    match total_chars {
        Some(total) if total > read => {
            format!(" value=\"{shown}...\" ({total} chars, the first {read} read; full_output reads it whole)")
        }
        _ if matches!(shown, std::borrow::Cow::Owned(_))
            && shown.chars().count() > pincer_core::window_state_view::VALUE_PREVIEW_CHARS =>
        {
            format!(" value=\"{shown}\" ({read} chars)")
        }
        _ => format!(" value=\"{shown}\""),
    }
}

/// Text as it goes into one markdown row. `query` filters the tree line by
/// line, so a newline inside a name, description or value turned the text
/// after it into a stray row (Kate's document text, 2026-10-25). The
/// structured nodes keep the raw text.
fn one_line(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(['\n', '\r']) {
        text.replace('\r', "\\r").replace('\n', "\\n").into()
    } else {
        text.into()
    }
}

fn is_menu_role(role: &str) -> bool {
    role.trim().to_ascii_lowercase().contains("menu")
}

/// Whether a menu-path ancestor contributes its label to the lineage.
/// Qt hangs each menu-bar item's entries under a `popup menu` container,
/// which it names after the window title once open ("Kate"; checked with
/// AT-SPI on Kate, KDE Plasma 6.8.1, 2026-10-25). That name is no menu label,
/// so the container is transparent like an unlabelled one.
fn counts_in_menu_lineage(role: &str, name: &str) -> bool {
    is_menu_role(role) && !role.trim().eq_ignore_ascii_case("popup menu") && !name.trim().is_empty()
}

/// Find one exact visible menu lineage in a flattened pre-order AT-SPI walk.
/// Unlabelled menu containers are transparent; all labelled menu ancestors
/// must match the requested prefix, so duplicate labels elsewhere fail closed.
/// Labels compare as [`menu_label_key`] reads them ("Find..." is "Find…").
fn exact_menu_path_matches(visited: &[Visited<'_>], path: &[String]) -> Vec<usize> {
    let path: Vec<String> = path.iter().map(|segment| menu_label_key(segment)).collect();
    let mut parent_at_depth: Vec<Option<usize>> = Vec::new();
    let mut parents = vec![None; visited.len()];
    for (index, node) in visited.iter().enumerate() {
        parents[index] = if node.depth == 0 {
            None
        } else {
            (0..node.depth)
                .rev()
                .find_map(|depth| parent_at_depth.get(depth).copied().flatten())
        };
        while parent_at_depth.len() <= node.depth {
            parent_at_depth.push(None);
        }
        parent_at_depth[node.depth] = Some(index);
        parent_at_depth[node.depth + 1..].fill(None);
    }

    visited
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            if !is_menu_role(&node.role)
                || node.enabled == Some(false)
                || Some(&menu_label_key(&node.name)) != path.last()
            {
                return None;
            }
            let mut lineage = Vec::new();
            let mut cursor = Some(index);
            while let Some(current) = cursor {
                let ancestor = &visited[current];
                if counts_in_menu_lineage(&ancestor.role, &ancestor.name) {
                    lineage.push(menu_label_key(&ancestor.name));
                }
                cursor = parents[current];
            }
            lineage.reverse();
            lineage.dedup();
            (lineage == path).then_some(index)
        })
        .collect()
}

/// Resolve and invoke an application menu one live hierarchy level at a time,
/// scoped to the caller's native window. Each hop first looks only where the
/// segment can be ([`targeted_menu_hop`]: the window's menu bars, then the
/// menu the previous hop opened), which costs tens of AT-SPI calls. When that
/// finds no single answer, the hop walks the whole application instead
/// ([`walked_menu_hop`], thousands of calls), and that walk decides every
/// refusal. Either way each hop reads the live tree after the preceding menu
/// has materialized; no snapshot index is retained across mutations. When the
/// same menu path exists under several top-levels of the process
/// (LibreOffice publishes every document window's menubar in one AT-SPI
/// application), the hop under `xid`'s frame wins instead of the path being
/// ambiguous.
///
/// Once the final item has fired, or a later segment is refused, the menu
/// the first hop opened is closed when the toolkit left it open (see
/// [`close_opened_menu`]). A segment that is not found is refused with the
/// labels at its level ([`menu_level_labels`]), read before that menu closes.
pub fn invoke_menu_path_in(pid: u32, xid: u64, path: &[String]) -> Result<MenuInvoked> {
    bounded(
        async {
            let conn = shared_connection().await?;
            // The first hop (the menu-bar entry), its action proxy and the
            // action index that opened its menu.
            let mut opener: Option<MenuOpener<'_>> = None;
            let walked = invoke_menu_hops(conn, pid, xid, path, &mut opener).await;
            let cleanup = match opener {
                Some((entry, action, index)) => {
                    close_opened_menu(conn, pid, &entry, &action, index, &path[0]).await
                }
                None => MenuCleanup::NoneOpen,
            };
            match (walked, cleanup) {
                (Ok(acknowledged), cleanup) => Ok(MenuInvoked {
                    cleanup,
                    unacknowledged: !acknowledged,
                }),
                (Err(error), MenuCleanup::StillOpen { menu }) => Err(anyhow!(
                    "{error}. The {menu} menu Pincer opened is still open, so pid {pid} holds a \
                     keyboard grab until it closes: press_key escape on window {xid} closes it \
                     (with delivery_mode foreground if a background press is refused)"
                )),
                (Err(error), _) => Err(error),
            }
        },
        || Err(anyhow!("invoke_menu timed out for pid {pid}")),
    )
}

/// What invoke_menu did once every segment resolved and the final item's
/// action was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuInvoked {
    /// What became of the menu the first hop opened.
    pub cleanup: MenuCleanup,
    /// The final item's action got no reply within the per-call timeout.
    /// Qt runs the action inside the D-Bus call, so an item that opens a
    /// modal dialog answers only once the dialog closes: KWrite's
    /// File > Save As… held the call until the 25 s operation timeout and
    /// was refused as timed out while its dialog was on screen (slot test,
    /// 2026-11-11).
    pub unacknowledged: bool,
}

/// The menu-bar entry whose action opened the menu, its Action proxy and
/// that action's index.
type MenuOpener<'a> = (RawObjectRef, atspi::proxy::action::ActionProxy<'a>, i32);

/// Resolve and fire each segment of `path` in turn (see
/// [`invoke_menu_path_in`]). `opener` is set once the first hop has opened a
/// menu, so the caller closes it whether or not a later hop succeeds.
/// `Ok(false)`: the final item's action was sent but not answered in time
/// (see [`MenuInvoked::unacknowledged`]).
async fn invoke_menu_hops<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
    path: &[String],
    opener: &mut Option<MenuOpener<'a>>,
) -> Result<bool> {
    // The entry the previous hop activated: the next segment is looked for
    // inside it first.
    let mut anchor: Option<RawObjectRef> = None;
    for depth in 0..path.len() {
        let previous = depth.checked_sub(1).map(|index| path[index].as_str());
        let targeted =
            targeted_menu_hop(conn, pid, xid, anchor.as_ref(), &path[depth], previous).await;
        let hop = match targeted {
            Some(hop) => hop,
            None => {
                dlog!("invoke_menu: segment {depth} walks the application");
                match walked_menu_hop(conn, pid, xid, &path[..=depth]).await? {
                    Some(hop) => hop,
                    None => {
                        let entries = menu_level_labels(conn, pid, xid, anchor.as_ref(), previous)
                            .await
                            .unwrap_or_default();
                        // The walk leaves disabled entries out of its matches.
                        if entries.iter().any(|(label, enabled)| {
                            !enabled && menu_label_matches(label, &path[depth])
                        }) {
                            anyhow::bail!(
                                "menu path segment {depth} \"{}\" is disabled",
                                path[depth]
                            );
                        }
                        let labels: Vec<String> =
                            entries.into_iter().map(|(label, _)| label).collect();
                        anyhow::bail!(
                            "{}",
                            pincer_core::menu_label::missing_segment_message(depth, path, &labels)
                        );
                    }
                }
            }
        };
        let chosen = activation_index(&hop.role, &hop.actions)
            .ok_or_else(|| anyhow!("menu path segment {depth} has no safe activation action"))?;
        let proxies = hop
            .acc
            .proxies()
            .await
            .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?;
        let action = proxies
            .action()
            .await
            .map_err(|error| anyhow!("Action unavailable: {error}"))?;
        let last = depth + 1 == path.len();
        // The final item's action may run a modal dialog before it answers
        // (see `MenuInvoked::unacknowledged`), so its wait is the per-call
        // timeout; the hops before it only open menus and keep the
        // operation's.
        let reply = if last {
            call(action.do_action(chosen as i32)).await
        } else {
            Some(action.do_action(chosen as i32).await)
        };
        let accepted = match reply {
            Some(answer) => answer.map_err(|error| anyhow!("doAction failed: {error}"))?,
            None => {
                dlog!("invoke_menu: final doAction sent but not acknowledged in time");
                return Ok(false);
            }
        };
        if !accepted {
            anyhow::bail!("menu path segment {depth} rejected its native action");
        }
        if depth == 0 && !last {
            *opener = Some((hop.oref.clone(), action, chosen as i32));
        }
        anchor = Some(hop.oref);
        if !last {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }
    Ok(true)
}

/// What became of the menu `invoke_menu` opened on its way to the final item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCleanup {
    /// Nothing is left open: a one-segment path, or a toolkit that closes
    /// its menus when an item fires (GTK3: View > Zoom > Zoom In on the Test
    /// Pad, 2026-11-11).
    NoneOpen,
    /// The menu was still open after the final item fired, and Pincer closed
    /// it. `menu` is the first path segment.
    Closed { menu: String },
    /// The menu is still open: the toolkit is not one Pincer knows how to
    /// close a menu in without input, or the close did not take.
    StillOpen { menu: String },
}

/// How long the toolkit gets to close its own menus after the final item
/// fired, before Pincer looks.
const MENU_CLOSE_SETTLE: Duration = Duration::from_millis(60);
/// How long Pincer waits for a menu it closed to stop showing.
const MENU_CLOSE_WAIT: Duration = Duration::from_millis(600);

/// Close the menu that menu-bar entry `entry` opened, when it is still open
/// after the final item fired. Qt runs an item's accessible `Press` action as
/// `QAction::trigger()`, which leaves every menu on the way open, holding a
/// keyboard grab (KWrite Edit > Undo, 2026-11-11). Qt's `Show Menu` action on
/// a menu-bar entry whose menu is visible hides that menu instead of opening
/// it (`QAccessibleMenuItem::doAction` in qtbase
/// src/widgets/accessible/qaccessiblemenu.cpp, v6.12.0), and hiding the top
/// menu also closes the submenus open below it (KWrite Settings > Toolbars
/// Shown > Main Toolbar left two popups; one `Show Menu` on Settings unmapped
/// both, 2026-11-11). So the same action index that opened the menu closes
/// it, in the background, with no key event.
///
/// Only Qt gets that second action: on another toolkit the same action could
/// open the menu again, so an open menu there is reported as
/// [`MenuCleanup::StillOpen`]. The action is always called with its index
/// argument (Qt crashes on an `Action` call without it; see
/// qt-atspi-crash.md in the C tool bench).
async fn close_opened_menu(
    conn: &AccessibilityConnection,
    pid: u32,
    entry: &RawObjectRef,
    action: &atspi::proxy::action::ActionProxy<'_>,
    index: i32,
    label: &str,
) -> MenuCleanup {
    let menu = label.to_owned();
    tokio::time::sleep(MENU_CLOSE_SETTLE).await;
    if !entry_menu_showing(conn, entry).await {
        return MenuCleanup::NoneOpen;
    }
    let qt = match app_for_pid(conn, pid).await {
        Ok(Some(app)) => is_collection_toolkit_app(&app).await,
        _ => false,
    };
    if !qt {
        return MenuCleanup::StillOpen { menu };
    }
    if !matches!(call(action.do_action(index)).await, Some(Ok(true))) {
        return MenuCleanup::StillOpen { menu };
    }
    let deadline = tokio::time::Instant::now() + MENU_CLOSE_WAIT;
    loop {
        if !entry_menu_showing(conn, entry).await {
            return MenuCleanup::Closed { menu };
        }
        if tokio::time::Instant::now() >= deadline || deadline_passed() {
            return MenuCleanup::StillOpen { menu };
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Whether the menu below menu-bar entry `entry` is open: one of its
/// children is a menu or menu entry with `Showing` (Qt's `popup menu`, GTK's
/// and VCL's items). An unanswered read counts as closed.
async fn entry_menu_showing(conn: &AccessibilityConnection, entry: &RawObjectRef) -> bool {
    let Some(Ok(children)) = call(raw_children(conn.connection(), entry)).await else {
        return false;
    };
    for child in children.iter().take(MENU_LOOKUP_NODES) {
        let Some(Ok(acc)) = call(accessible_for(conn, child)).await else {
            continue;
        };
        let Some(Ok(role)) = call(acc.get_role_name()).await else {
            continue;
        };
        if !is_menu_role(&role) {
            continue;
        }
        if matches!(call(acc.get_state()).await, Some(Ok(state)) if is_showing_state(&state)) {
            return true;
        }
    }
    false
}

/// One resolved menu path segment, ready to activate.
struct MenuHop<'a> {
    acc: AccessibleProxy<'a>,
    oref: RawObjectRef,
    role: String,
    actions: Vec<String>,
}

/// Resolve the last segment of `path` from a walk of the whole application,
/// closed menus included: every match of the exact path, narrowed to the
/// caller's frame and deduplicated, must leave one enabled entry. `None`
/// when nothing matches.
async fn walked_menu_hop<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
    path: &[String],
) -> Result<Option<MenuHop<'a>>> {
    let depth = path.len() - 1;
    // The whole application: a GTK menu opens as a top-level of its own,
    // outside the caller's window.
    let options = WalkOptions {
        walk_closed_menus: true,
        other_windows: true,
        ..WalkOptions::default()
    };
    let collected = collect_visited_bounded_opts(conn, pid, xid, None, None, options)
        .await?
        .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
    let visited = collected.visited;
    let mut matches = exact_menu_path_matches(&visited, path);
    if matches.len() > 1 {
        if let Some(scope) = collected.scoped_frame {
            let scoped: Vec<usize> = matches
                .iter()
                .copied()
                .filter(|index| visited[*index].frame_ordinal == scope)
                .collect();
            if !scoped.is_empty() {
                matches = scoped;
            }
        }
    }
    if matches.len() > 1 {
        matches = dedupe_menu_matches(&visited, matches);
    }
    let target_index = match matches.as_slice() {
        [index] => *index,
        [] => return Ok(None),
        _ => anyhow::bail!("menu path segment {depth} is ambiguous"),
    };
    let target = &visited[target_index];
    if target.enabled == Some(false) {
        anyhow::bail!("menu path segment {depth} is disabled");
    }
    Ok(Some(MenuHop {
        acc: target.acc.clone(),
        oref: RawObjectRef {
            name: target.object_ref.bus.clone(),
            path: target.object_ref.path.clone(),
        },
        role: target.role.clone(),
        actions: target.actions.clone(),
    }))
}

/// The entries at one menu level, in menu order, as (label, enabled): below
/// the caller's window's menu bars when `anchor` is `None`, else below
/// `anchor`, the entry the previous hop opened. The level is read the way
/// [`menu_entries_named`] reads it for a segment (through transparent
/// containers, a repeat of `previous` included), so these are the labels a
/// segment at that level is compared with. `None` when a read fails. Qt
/// fills a menu's entries only once it is open, so this runs before the
/// menu the walk opened is closed.
async fn menu_level_labels(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    anchor: Option<&RawObjectRef>,
    previous: Option<&str>,
) -> Option<Vec<(String, bool)>> {
    let roots = match anchor {
        Some(anchor) => vec![anchor.clone()],
        None => window_menu_bars(conn, pid, xid).await?,
    };
    let zconn = conn.connection();
    let mut queue = std::collections::VecDeque::new();
    for root in &roots {
        let children = call(raw_children(zconn, root)).await?.ok()?;
        queue.extend(children.into_iter().map(|child| (child, 1usize)));
    }
    let mut labels = Vec::new();
    let mut read = 0usize;
    while let Some((oref, depth)) = queue.pop_front() {
        read += 1;
        if read > MENU_LOOKUP_NODES {
            break;
        }
        let Some(Ok(acc)) = call(accessible_for(conn, &oref)).await else {
            continue;
        };
        let (role, name) = tokio::join!(call(acc.get_role_name()), call(acc.name()));
        let (Some(Ok(role)), Some(Ok(name))) = (role, name) else {
            continue;
        };
        // An empty segment never matches, so every entry of this level
        // comes back as `Skip`.
        match menu_step(&role, &name, "", previous) {
            MenuStep::Skip | MenuStep::Match => {
                let enabled = !matches!(
                    call(acc.get_state()).await,
                    Some(Ok(state)) if !is_enabled_state(&state)
                );
                labels.push((name.trim().to_owned(), enabled));
            }
            MenuStep::LookInside if depth < MENU_LOOKUP_DEPTH => {
                if let Some(Ok(children)) = call(raw_children(zconn, &oref)).await {
                    queue.extend(children.into_iter().map(|child| (child, depth + 1)));
                }
            }
            MenuStep::LookInside => {}
        }
    }
    Some(labels)
}

/// How deep below a menu (or menu bar) the targeted lookup looks for the
/// next entry, through unlabelled containers: Qt hangs a menu's items under
/// a `popup menu`, one level down.
const MENU_LOOKUP_DEPTH: usize = 3;
/// Elements the targeted lookup reads for one segment before it gives up and
/// the hop walks the application.
const MENU_LOOKUP_NODES: usize = 400;
/// How far below the window the menu-bar search looks where Collection is
/// not used, and how many elements it reads.
const MENU_BAR_SEARCH_DEPTH: usize = 4;
const MENU_BAR_SEARCH_NODES: usize = 128;

/// Resolve one segment by reading only where it can be: the children of the
/// caller's window's menu bars for the first segment, the children of the
/// entry the previous hop activated (`anchor`) for the rest. `None` unless
/// exactly one enabled entry with a safe activation action matches, so the
/// caller walks the application for every case that walk would refuse or
/// narrow down (none or several matches, a disabled entry, a window without
/// a menu bar, an unanswered call).
async fn targeted_menu_hop<'a>(
    conn: &'a AccessibilityConnection,
    pid: u32,
    xid: u64,
    anchor: Option<&RawObjectRef>,
    segment: &str,
    previous: Option<&str>,
) -> Option<MenuHop<'a>> {
    let roots = match anchor {
        Some(anchor) => vec![anchor.clone()],
        None => window_menu_bars(conn, pid, xid).await?,
    };
    if roots.is_empty() {
        return None;
    }
    let mut found = menu_entries_named(conn, &roots, segment, previous).await?;
    if found.len() != 1 {
        dlog!(
            "invoke_menu: {} entries named {segment:?} where the lookup looked",
            found.len()
        );
        return None;
    }
    let (acc, oref, role) = found.pop()?;
    let proxies = call(acc.proxies()).await?.ok()?;
    let action = call(proxies.action()).await?.ok()?;
    let count = call(action.n_actions()).await?.ok()?;
    let mut actions = Vec::new();
    for index in 0..count {
        // Keep the AT-SPI index even when one name read fails.
        actions.push(
            call(action.get_name(index))
                .await
                .and_then(|result| result.ok())
                .unwrap_or_default(),
        );
    }
    activation_index(&role, &actions)?;
    Some(MenuHop {
        acc,
        oref,
        role,
        actions,
    })
}

/// What the targeted lookup does with one element below a menu.
#[derive(Debug, PartialEq, Eq)]
enum MenuStep {
    /// A menu entry with the segment's label.
    Match,
    /// No label of its own in the path (Qt's `popup menu`, an unnamed
    /// container, a separator), or the previous segment's label again: its
    /// children are entries of the same menu.
    LookInside,
    /// Another labelled entry: anything below it has that label in its path.
    Skip,
}

/// The rules of [`exact_menu_path_matches`] for one element: an entry
/// matches when its own label counts in the path and is the segment; an
/// element whose label does not count is transparent, and so is a repeat of
/// the previous segment's label (the walk collapses repeated labels).
fn menu_step(role: &str, name: &str, segment: &str, previous: Option<&str>) -> MenuStep {
    if !counts_in_menu_lineage(role, name) {
        return MenuStep::LookInside;
    }
    if menu_label_matches(name, segment) {
        MenuStep::Match
    } else if previous.is_some_and(|previous| menu_label_matches(name, previous)) {
        MenuStep::LookInside
    } else {
        MenuStep::Skip
    }
}

/// The enabled entries labelled `segment` below `roots` (breadth first,
/// looking through transparent elements up to [`MENU_LOOKUP_DEPTH`]), with
/// their roles. `None` when a read fails or the menu is larger than the
/// lookup reads, so the caller walks the application instead.
async fn menu_entries_named<'a>(
    conn: &'a AccessibilityConnection,
    roots: &[RawObjectRef],
    segment: &str,
    previous: Option<&str>,
) -> Option<Vec<(AccessibleProxy<'a>, RawObjectRef, String)>> {
    let zconn = conn.connection();
    let mut queue = std::collections::VecDeque::new();
    for root in roots {
        let children = call(raw_children(zconn, root)).await?.ok()?;
        queue.extend(children.into_iter().map(|child| (child, 1usize)));
    }
    let mut found = Vec::new();
    let mut read = 0usize;
    while let Some((oref, depth)) = queue.pop_front() {
        read += 1;
        if read > MENU_LOOKUP_NODES {
            dlog!("invoke_menu: menu lookup for {segment:?} passed {MENU_LOOKUP_NODES} elements");
            return None;
        }
        let acc = call(accessible_for(conn, &oref)).await?.ok()?;
        let (role, name) = tokio::join!(call(acc.get_role_name()), call(acc.name()));
        let (role, name) = (role?.ok()?, name?.ok()?);
        match menu_step(&role, &name, segment, previous) {
            MenuStep::Match => {
                // The walk leaves a disabled entry out of its matches.
                let enabled = call(acc.get_state())
                    .await
                    .and_then(|state| state.ok())
                    .map(|state| is_enabled_state(&state));
                if enabled != Some(false) {
                    found.push((acc, oref, role));
                }
            }
            MenuStep::LookInside if depth < MENU_LOOKUP_DEPTH => {
                match call(raw_child_count(zconn, &oref)).await {
                    Some(Ok(count)) if count <= 0 => continue,
                    Some(Ok(count)) if count as usize > MENU_LOOKUP_NODES => return None,
                    _ => {}
                }
                let children = call(raw_children(zconn, &oref)).await?.ok()?;
                queue.extend(children.into_iter().map(|child| (child, depth + 1)));
            }
            MenuStep::LookInside | MenuStep::Skip => {}
        }
    }
    Some(found)
}

/// The menu bars of the caller's window: one Collection search on Qt, a
/// shallow breadth-first look below the window elsewhere (stopping at the
/// depth of the first one found). `None` when the window's top-level cannot
/// be resolved.
async fn window_menu_bars(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
) -> Option<Vec<RawObjectRef>> {
    let ResolvedApp { app, children } = resolve_app_for_pid(conn, pid).await.ok()??;
    let children = match children {
        Some(children) => children,
        None => call(app.get_children()).await?.ok()?,
    };
    let seeds: Vec<RawObjectRef> = children
        .iter()
        .filter_map(RawObjectRef::from_atspi)
        .collect();
    let frame = seeds
        .get(resolve_window_frame(conn, pid, xid, &seeds).await?)?
        .clone();
    let zconn = conn.connection();
    if is_collection_toolkit_app(&app).await {
        return call(get_matches(zconn, &frame, &[atspi::Role::MenuBar as u32]))
            .await?
            .ok();
    }
    let mut queue = std::collections::VecDeque::from([(frame, 0usize)]);
    let mut bars = Vec::new();
    let mut found_at = None;
    let mut read = 0usize;
    while let Some((oref, depth)) = queue.pop_front() {
        if found_at.is_some_and(|found| depth > found) {
            break;
        }
        read += 1;
        if read > MENU_BAR_SEARCH_NODES {
            break;
        }
        let Some(Ok(acc)) = call(accessible_for(conn, &oref)).await else {
            continue;
        };
        let Some(Ok(role)) = call(acc.get_role_name()).await else {
            continue;
        };
        if role.trim().eq_ignore_ascii_case("menu bar") {
            bars.push(oref);
            found_at = Some(depth);
            continue;
        }
        if depth >= MENU_BAR_SEARCH_DEPTH {
            continue;
        }
        match call(raw_child_count(zconn, &oref)).await {
            Some(Ok(count)) if count <= 0 || count as usize > MENU_BAR_SEARCH_NODES => continue,
            _ => {}
        }
        if let Some(Ok(children)) = call(raw_children(zconn, &oref)).await {
            queue.extend(children.into_iter().map(|child| (child, depth + 1)));
        }
    }
    Some(bars)
}

/// Several nodes matched one menu path segment inside the scoped frame.
/// LibreOffice publishes its menubar twice per document frame (the VCL
/// menubar and its accessible mirror), so the same item shows up under two
/// parents: prefer the SHOWING instances, then collapse nodes that share one
/// D-Bus object path, and when what is left still carries one role and name
/// take the first (they are the same menu).
fn dedupe_menu_matches(visited: &[Visited<'_>], matches: Vec<usize>) -> Vec<usize> {
    let showing: Vec<usize> = matches
        .iter()
        .copied()
        .filter(|index| visited[*index].showing && !visited[*index].offscreen)
        .collect();
    let mut candidates = if showing.is_empty() { matches } else { showing };
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|index| {
        let node = &visited[*index];
        seen.insert((node.object_ref.bus.clone(), node.object_ref.path.clone()))
    });
    if candidates.len() > 1 {
        let first = &visited[candidates[0]];
        let same = candidates
            .iter()
            .all(|index| visited[*index].role == first.role && visited[*index].name == first.name);
        if same {
            candidates.truncate(1);
        }
    }
    candidates
}

/// Perform the activation action of element `idx` of window `xid`, resolved
/// against a fresh walk of that window (the index space of its snapshot).
pub fn perform_action(pid: u32, xid: u64, idx: usize) -> Result<(String, bool, bool)> {
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let visited = collect_window_visited(conn, pid, xid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes.get(idx).ok_or_else(|| {
                anyhow!("element {idx} not found (total: {})", action_nodes.len())
            })?;

            // Suspected no-op: actuating `do_action(0)` on a passive display role
            // (a `label`/`static`/`image` indexed only for its Value interface) or a
            // node that advertises no action at all is the AT-SPI analogue of macOS'
            // "element does not advertise this action" — the call returns success but
            // likely changes nothing. Reuses the same passive-role detector
            // `select_click_target` leans on for the coordinate paths. The caller
            // turns this into `effect: "suspected_noop"` + an escalation hint.
            let suspected_noop = target.actions.is_empty() || is_passive_role(&target.role);

            // Which action to actuate is decided by NAME, not by position. A
            // GTK4 text view advertises `buffer.delete-line` first, so firing
            // "action 0" there deletes a line of the user's document while
            // reporting an ordinary click. An element that advertises no
            // activation at all is a no-op the caller must escalate past —
            // not an invitation to fire whatever happens to be first.
            let chosen = activation_index(&target.role, &target.actions).ok_or_else(|| {
                anyhow!("element {idx} does not advertise a safe activation action")
            })?;

            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let action = target.actions.get(chosen).cloned().unwrap_or_default();
            // `doAction` answers a bool: `false` is the toolkit declining the
            // request (LibreOffice VCL menus answer it for a closed menu), not
            // a delivered click, so it is reported as a suspected no-op.
            // `unacknowledged` is distinct from `rejected`: the D-Bus call
            // never got a reply in time (the toolkit may still be processing
            // it — e.g. a nested dialog main loop), so we know neither
            // acceptance nor effect. This must never read as a plain
            // dispatched/confirmed success; the caller reports it as
            // `unverifiable` with an explicit "no acknowledgement received"
            // note, and — unlike a genuine `suspected_noop` — does not by
            // itself imply the toolkit declined anything.
            let mut unacknowledged = false;
            let rejected = match call(ap.do_action(chosen as i32)).await {
                Some(Ok(accepted)) => !accepted,
                Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
                None => {
                    dlog!("doAction dispatched but not acknowledged in time");
                    unacknowledged = true;
                    false
                }
            };
            let suspected_noop = suspected_noop || rejected;
            // AT-SPI's doAction acknowledgement can precede the renderer's
            // queued DOM mutation. Give WebKit/Chromium one short event-loop
            // turn before returning success so a caller's immediate external
            // state read observes the action it was told was delivered.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((action, suspected_noop, unacknowledged))
        },
        || {
            Err(anyhow!(
                "perform_action timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

// ── Cached-reference fast paths ──────────────────────────────────────────────
//
// Every per-index entry point above re-walks the application to turn an
// `element_index` back into an accessible. On large trees (LibreOffice, GIMP,
// Nautilus) that walk is the multi-second stall the agent feels on every
// click. A `get_window_state` snapshot already knows each indexed node's D-Bus
// identity ([`AtspiNode::object_ref`]); these entry points act on that identity
// directly and let the caller fall back to the index walk only when the object
// is gone.

/// Budget for a single cached-object action: a handful of round-trips to one
/// live object, never a walk.
const REF_ACTION_BUDGET: Duration = Duration::from_secs(3);

fn raw_ref(object_ref: &ObjectRef) -> RawObjectRef {
    RawObjectRef {
        name: object_ref.bus.clone(),
        path: object_ref.path.clone(),
    }
}

/// Current action names of an Action proxy, index-aligned with `do_action`.
async fn action_names(ap: &atspi::proxy::action::ActionProxy<'_>) -> Vec<String> {
    let n = call(ap.n_actions()).await.and_then(|r| r.ok()).unwrap_or(0);
    let mut names = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        names.push(
            call(ap.get_name(i))
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default(),
        );
    }
    names
}

/// The snapshot-cached object no longer answers on the bus (or never did):
/// the observation is stale and must not be retargeted by index.
#[derive(Debug)]
pub struct CachedElementGone(pub String);
impl std::fmt::Display for CachedElementGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CachedElementGone {}

/// Open a cached accessible and prove it is still alive (one role read).
async fn live_accessible<'a>(
    conn: &'a AccessibilityConnection,
    object_ref: &ObjectRef,
) -> Result<(AccessibleProxy<'a>, String)> {
    let acc = match call(accessible_for(conn, &raw_ref(object_ref))).await {
        Some(Ok(acc)) => acc,
        Some(Err(error)) => {
            return Err(CachedElementGone(format!("cached element unreachable: {error}")).into())
        }
        None => return Err(anyhow!("cached element did not answer in time")),
    };
    let role = match call(acc.get_role_name()).await {
        Some(Ok(role)) => role,
        Some(Err(error)) => {
            return Err(CachedElementGone(format!("cached element is gone: {error}")).into())
        }
        None => return Err(anyhow!("cached element did not answer in time")),
    };
    Ok((acc, role))
}

/// [`perform_action`] on a snapshot-cached element identity. Same contract:
/// `Ok((action_name, suspected_noop))`. Errors when the object no longer
/// exists so the caller can fall back to resolving the index afresh.
pub fn perform_action_ref(object_ref: &ObjectRef) -> Result<(String, bool, bool)> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, role) = live_accessible(conn, object_ref).await?;
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let ap = proxies
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let actions = action_names(&ap).await;
            let suspected_noop = actions.is_empty() || is_passive_role(&role);
            let chosen = activation_index(&role, &actions)
                .ok_or_else(|| anyhow!("element does not advertise a safe activation action"))?;
            // See `perform_action`'s `unacknowledged`: an unanswered doAction
            // is unknown, not dispatched-implies-success, and must not be
            // folded into `rejected`/`suspected_noop` (which would claim the
            // toolkit declined it) nor silently reported as a plain success.
            let mut unacknowledged = false;
            let rejected = match call(ap.do_action(chosen as i32)).await {
                Some(Ok(accepted)) => !accepted,
                Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
                // The request was delivered; a GTK item whose action opens a
                // dialog (nested main loop) only replies once that dialog
                // closes. Treat the timeout as dispatched-but-unconfirmed:
                // falling back to another route would fire the item twice.
                None => {
                    dlog!("doAction dispatched but not acknowledged in time");
                    unacknowledged = true;
                    false
                }
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((
                actions.get(chosen).cloned().unwrap_or_default(),
                suspected_noop || rejected,
                unacknowledged,
            ))
        },
        || Err(anyhow!("perform_action (cached element) timed out")),
    )
}

/// Budget for re-reading one cached element before a pixel click fires it.
const CACHED_HIT_CHECK_BUDGET: Duration = Duration::from_millis(400);

/// What the toolkit says now about a snapshot-cached element that a
/// background pixel click is about to fire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CachedHitLive {
    /// `Showing` (or, when the state set could not be read, assumed).
    pub showing: bool,
    /// Screen extents, when the toolkit's answer is trustworthy.
    pub extents: Option<(i32, i32, i32, i32)>,
    /// The element's parent is a `popup menu` or `menu`: it is drawn in a
    /// popup window, never in the window under it.
    pub in_popup_menu: bool,
    pub name: String,
}

/// The cached-hit rule. A snapshot remembers where an element was; Qt keeps
/// a closed menu's item extents but clears its `Showing`, so a click on plain
/// editor text fired "Reload All Projects" from a snapshot read while the
/// Projects menu was open (Kate, 2026-11-03). Fire a cached element only
/// when it is showing now, still covers the point, and lives in the kind of
/// window the click targets: a popup menu's item only for a click on that
/// popup, and nothing else there.
pub(crate) fn cached_hit_fires(
    live: &CachedHitLive,
    sx: i32,
    sy: i32,
    target_is_popup: bool,
) -> bool {
    if !live.showing {
        return false;
    }
    if let Some((x, y, w, h)) = live.extents {
        if w <= 0 || h <= 0 || sx < x || sy < y || sx >= x + w || sy >= y + h {
            return false;
        }
    }
    live.in_popup_menu == target_is_popup
}

/// Re-read a snapshot-cached element for [`cached_hit_fires`]: its state,
/// screen extents, parent role and name. `None` when it is gone or did not
/// answer in time; the caller then leaves the point to the live hit-test.
pub fn cached_hit_live(object_ref: &ObjectRef) -> Option<CachedHitLive> {
    let display = (!crate::wayland::is_wayland())
        .then(x11_display_size)
        .flatten();
    bounded_for(
        CACHED_HIT_CHECK_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _role) = live_accessible(conn, object_ref).await?;
            let showing = match call(acc.get_state()).await {
                Some(Ok(state)) => state.contains(State::Showing),
                Some(Err(_)) => true,
                None => return Err(anyhow!("state read timed out")),
            };
            let extents = match call(acc.proxies()).await {
                Some(Ok(proxies)) => match call(proxies.component()).await {
                    Some(Ok(component)) => call(component.get_extents(CoordType::Screen))
                        .await
                        .and_then(Result::ok)
                        .filter(|raw| screen_extents_trusted(*raw, display)),
                    _ => None,
                },
                _ => None,
            };
            let parent_role = match call(acc.parent()).await {
                Some(Ok(parent)) => match RawObjectRef::from_atspi(&parent) {
                    Some(parent) => match call(accessible_for(conn, &parent)).await {
                        Some(Ok(parent)) => call(parent.get_role_name())
                            .await
                            .and_then(Result::ok)
                            .unwrap_or_default(),
                        _ => String::new(),
                    },
                    None => String::new(),
                },
                _ => String::new(),
            };
            let name = call(acc.name())
                .await
                .and_then(Result::ok)
                .unwrap_or_default();
            Ok(Some(CachedHitLive {
                showing,
                extents,
                in_popup_menu: matches!(
                    parent_role.trim().to_ascii_lowercase().as_str(),
                    "popup menu" | "menu"
                ),
                name,
            }))
        },
        || Ok(None),
    )
    .ok()
    .flatten()
}

/// `(current, minimum, maximum)` of each object's `Value` interface, `None`
/// where it has none or did not answer; one bounded pass, no walk.
pub fn value_ranges(refs: &[ObjectRef]) -> Vec<Option<(f64, f64, f64)>> {
    let refs = refs.to_vec();
    let count = refs.len();
    bounded_for(
        CACHED_HIT_CHECK_BUDGET,
        async move {
            let conn = shared_connection().await?;
            let mut out = Vec::with_capacity(refs.len());
            for object_ref in &refs {
                let range = async {
                    let acc = call(accessible_for(conn, &raw_ref(object_ref)))
                        .await?
                        .ok()?;
                    let proxies = call(acc.proxies()).await?.ok()?;
                    let value = call(proxies.value()).await?.ok()?;
                    let current = call(value.current_value()).await?.ok()?;
                    let minimum = call(value.minimum_value()).await?.ok()?;
                    let maximum = call(value.maximum_value()).await?.ok()?;
                    Some((current, minimum, maximum))
                }
                .await;
                out.push(range);
            }
            Ok(out)
        },
        || Ok(vec![None; count]),
    )
    .unwrap_or_else(|_| vec![None; count])
}

/// Screen bounds of a snapshot-cached element identity: one `GetExtents`
/// round-trip, no tree re-walk. `Err` when the object is gone or does not
/// answer within [`REF_ACTION_BUDGET`]; the caller decides whether to re-walk.
/// The X11 origin lookup runs on the caller's (blocking) thread before the
/// runtime is entered, so no synchronous X11 I/O happens under `block_on`.
pub fn element_bounds_ref(
    object_ref: &ObjectRef,
    pid: u32,
    xid: u64,
    in_web_content: bool,
) -> Result<(i32, i32, u32, u32)> {
    if in_web_content {
        return Err(anyhow!(
            "web-content bounds need the document origin; re-walk"
        ));
    }
    let offset = window_to_screen_offset(pid, xid, None);
    let coord = if offset.is_some() {
        CoordType::Window
    } else {
        CoordType::Screen
    };
    let display = (!crate::wayland::is_wayland())
        .then(x11_display_size)
        .flatten();
    let origin_attested = native_wayland_origin_attested(pid, xid);
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let proxies = match call(acc.proxies()).await {
                Some(Ok(proxies)) => proxies,
                Some(Err(e)) => return Err(anyhow!("interface proxies unavailable: {e}")),
                None => return Err(anyhow!("cached element did not answer in time")),
            };
            let component = match call(proxies.component()).await {
                Some(Ok(component)) => component,
                Some(Err(e)) => return Err(anyhow!("Component unavailable: {e}")),
                None => return Err(anyhow!("cached element did not answer in time")),
            };
            if coord == CoordType::Window {
                if let Some(Ok(raw)) = call(component.get_extents(CoordType::Screen)).await {
                    if screen_answer_overrides_window(raw, display, origin_attested) {
                        return project_screen_extents(raw, (0, 0), None)
                            .ok_or_else(|| anyhow!("cached element reports no on-screen extents"));
                    }
                }
            }
            let extents = match call(component.get_extents(coord)).await {
                Some(Ok(extents)) => extents,
                Some(Err(e)) => return Err(anyhow!("getExtents failed: {e}")),
                None => return Err(anyhow!("getExtents did not answer in time")),
            };
            project_screen_extents(extents, offset.unwrap_or((0, 0)), None)
                .ok_or_else(|| anyhow!("cached element reports no on-screen extents"))
        },
        || Err(anyhow!("element_bounds (cached element) timed out")),
    )
}

/// Whether a snapshot-cached element is currently on screen: its own AT-SPI
/// state set carries `Showing` (a GTK menu item whose menu is closed, or a
/// widget on an unmapped notebook page, lacks it even though the object
/// still exists). `Ok(true)` when the toolkit exposes no state set.
pub fn element_showing_ref(object_ref: &ObjectRef) -> Result<bool> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            match call(acc.get_state()).await {
                Some(Ok(state)) => Ok(is_showing_state(&state)),
                Some(Err(_)) => Ok(true),
                None => Err(anyhow!("cached element did not answer in time")),
            }
        },
        || Err(anyhow!("element_showing (cached element) timed out")),
    )
}

/// [`focus_element`] on a snapshot-cached element identity.
pub fn focus_element_ref(object_ref: &ObjectRef) -> Result<bool> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let component = proxies
                .component()
                .await
                .map_err(|e| anyhow!("Component interface unavailable: {e}"))?;
            let accepted = match call(component.grab_focus()).await {
                Some(Ok(focused)) => focused,
                Some(Err(e)) => return Err(anyhow!("Component.GrabFocus failed: {e}")),
                None => return Err(anyhow!("Component.GrabFocus timed out")),
            };
            if !accepted {
                return Ok(false);
            }
            let settle_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            while tokio::time::Instant::now() < settle_deadline {
                match tokio::time::timeout(Duration::from_millis(100), acc.get_state()).await {
                    Ok(Ok(state)) if state.contains(State::Focused) => return Ok(true),
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            Ok(false)
        },
        || Err(anyhow!("focus_element (cached element) timed out")),
    )
}

/// [`type_into_editable_at`] on a snapshot-cached element identity.
pub fn type_into_editable_ref(object_ref: &ObjectRef, text: &str) -> Result<()> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Err(anyhow!("cached element interfaces unavailable")),
            };
            if !ifaces.contains(Interface::EditableText) {
                return Err(anyhow!("cached element is not editable"));
            }
            if write_into_editable_acc(&acc, ifaces.contains(Interface::Component), text).await? {
                Ok(())
            } else {
                Err(anyhow!("EditableText write was rejected"))
            }
        },
        || Err(anyhow!("type_into_editable (cached element) timed out")),
    )
}

/// Write `text` into the focused widget known from the focus-event log, if it
/// is editable. `Some(true)` = written, `Some(false)` = focused but not
/// editable (or write rejected), `None` = no usable focus record.
async fn write_into_focused_editable(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    text: &str,
) -> Option<bool> {
    let (acc, oref) = focused_accessible_via_events(conn, pid).await?;
    // The application's focus can be in another of its windows than the one
    // the caller addressed; typing there would land in the wrong window.
    if object_in_window(conn, pid, xid, &oref).await == Some(false) {
        dlog!(
            "focused object {} is in another window than {xid:#x}",
            oref.path
        );
        return Some(false);
    }
    let ifaces = call(acc.get_interfaces()).await?.ok()?;
    if !ifaces.contains(Interface::EditableText) {
        dlog!("focused object {} is not editable", oref.path);
        return Some(false);
    }
    match write_into_editable_acc(&acc, ifaces.contains(Interface::Component), text).await {
        Ok(written) => Some(written),
        Err(error) => {
            dlog!("focused editable write failed: {error:#}");
            Some(false)
        }
    }
}

/// Whether the accessible `object` belongs to window `xid`: its parent chain
/// reaches the top-level `xid` resolves to (`Some(true)`) or another
/// top-level of the application (`Some(false)`). `None` when it can't be
/// told: no window (`xid == 0`), a window no top-level corresponds to (a Qt
/// popup), or a chain that breaks off.
async fn object_in_window(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    object: &ObjectRef,
) -> Option<bool> {
    if xid == 0 {
        return None;
    }
    let app = app_for_pid(conn, pid).await.ok()??;
    let seeds: Vec<RawObjectRef> = call(app.get_children())
        .await?
        .ok()?
        .iter()
        .filter_map(RawObjectRef::from_atspi)
        .collect();
    let frame = seeds
        .get(resolve_window_frame(conn, pid, xid, &seeds).await?)?
        .clone();
    let mut current = raw_ref(object);
    for _ in 0..64 {
        if current == frame {
            return Some(true);
        }
        if seeds.contains(&current) {
            return Some(false);
        }
        let acc = call(accessible_for(conn, &current)).await?.ok()?;
        let parent = RawObjectRef::from_atspi(&call(acc.parent()).await?.ok()?)?;
        if parent == current || parent.path.ends_with("/null") {
            return None;
        }
        current = parent;
    }
    None
}

// ── Point hit-testing without a walk ─────────────────────────────────────────

/// One node of a `GetAccessibleAtPoint` descent.
struct HitNode<'a> {
    acc: AccessibleProxy<'a>,
    oref: RawObjectRef,
}

/// Descend from `start` through `Component.GetAccessibleAtPoint(Window)` until
/// the toolkit stops returning a deeper child. O(depth) round-trips instead of
/// a full tree walk — this is how AT-SPI clients are meant to hit-test.
async fn descend_at_point<'a>(
    conn: &'a AccessibilityConnection,
    start: RawObjectRef,
    x: i32,
    y: i32,
) -> Vec<HitNode<'a>> {
    let mut chain: Vec<HitNode<'a>> = Vec::new();
    let mut current = start;
    for _ in 0..64 {
        let Some(Ok(acc)) = call(accessible_for(conn, &current)).await else {
            break;
        };
        let next = match call(acc.proxies()).await {
            Some(Ok(proxies)) => match call(proxies.component()).await {
                Some(Ok(component)) => {
                    call(component.get_accessible_at_point(x, y, CoordType::Window)).await
                }
                _ => None,
            },
            _ => None,
        };
        chain.push(HitNode {
            acc,
            oref: current.clone(),
        });
        let Some(Ok(next)) = next else { break };
        let Some(next) = RawObjectRef::from_atspi(&next) else {
            break;
        };
        if next.path.ends_with("/null") || next == current {
            break;
        }
        current = next;
    }
    chain
}

impl PartialEq for RawObjectRef {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.path == other.path
    }
}

/// Hit-test `(win_x, win_y)` (window-local) inside `pid`'s application via
/// the toolkit's own `GetAccessibleAtPoint` and actuate the deepest node on
/// the chain that advertises a safe activation, preferring a real actuator
/// over a passive label. `xid` (when non-zero and resolvable) restricts the
/// descent to that window's frame. `Ok(None)` = nothing actionable there.
pub fn perform_action_at_point_in(
    pid: u32,
    xid: u64,
    win_x: i32,
    win_y: i32,
    skip_focus_roles: bool,
) -> Result<Option<AtPointHit>> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            let Some(app) = app_for_pid(conn, pid).await? else {
                return Ok(None);
            };
            let seeds: Vec<RawObjectRef> = match call(app.get_children()).await {
                Some(Ok(children)) => children
                    .into_iter()
                    .filter_map(|child| RawObjectRef::from_atspi(&child))
                    .collect(),
                _ => Vec::new(),
            };
            let scoped = if xid == 0 {
                None
            } else {
                resolve_window_frame(conn, pid, xid, &seeds).await
            };
            // The toolkit's `Window` coordinate frame is not always the X11
            // client window the caller's pixels are relative to: VCL
            // (LibreOffice) measures from the WM frame, title bar included,
            // so a client-local point hit-tests one or two rows too high.
            // Re-base the point on the frame's own screen origin.
            let (hit_x, hit_y) = match scoped {
                Some(ordinal) => {
                    let frame_origin = decoration_frame_origin(conn, &app, &seeds[ordinal]).await;
                    let client_origin = x11_window_origin(xid);
                    at_point_toolkit_coords((win_x, win_y), client_origin, frame_origin)
                }
                None => (win_x, win_y),
            };
            if (hit_x, hit_y) != (win_x, win_y) {
                dlog!(
                    "at-point ({win_x},{win_y}) re-based to toolkit window coords ({hit_x},{hit_y})"
                );
            }
            let ordered: Vec<RawObjectRef> = match scoped {
                Some(ordinal) => seeds.get(ordinal).cloned().into_iter().collect(),
                None => seeds,
            };
            for seed in ordered {
                let chain = descend_at_point(conn, seed, hit_x, hit_y).await;
                if chain.len() <= 1 {
                    // The frame itself, or nothing: this top-level does not
                    // cover the point (or cannot hit-test). Try the next one.
                    continue;
                }
                return actuate_chain(&chain, hit_x, hit_y, skip_focus_roles).await;
            }
            Ok(None)
        },
        || Ok(None),
    )
}

/// The frame origin to re-base an at-point query on: the top-level's screen
/// origin for VCL (LibreOffice), whose `Window` coordinates start at the WM
/// frame, and `None` for every other toolkit. GTK and Chromium measure
/// `Window` coordinates from their client window but report the top-level's
/// screen extents including server-side decorations (Openbox, XFCE), so
/// adding that inset would hit-test a title bar too low; under client-side
/// decorations (GNOME) the inset is zero anyway.
async fn decoration_frame_origin(
    conn: &AccessibilityConnection,
    app: &AccessibleProxy<'_>,
    frame: &RawObjectRef,
) -> Option<(i32, i32)> {
    if !toolkit_measures_from_frame(app).await {
        return None;
    }
    frame_screen_origin(conn, frame).await
}

async fn toolkit_measures_from_frame(app: &AccessibleProxy<'_>) -> bool {
    let Some(Ok(proxies)) = call(app.proxies()).await else {
        return false;
    };
    let Some(Ok(application)) = call(proxies.application()).await else {
        return false;
    };
    let Some(Ok(name)) = call(application.toolkit_name()).await else {
        return false;
    };
    is_frame_coordinate_toolkit(&name)
}

/// VCL (LibreOffice) measures `Window` coordinates from the WM frame, title
/// bar included; the other toolkits measure from the client window.
fn is_frame_coordinate_toolkit(toolkit_name: &str) -> bool {
    toolkit_name.trim().eq_ignore_ascii_case("vcl")
}

/// Screen origin of an application top-level as the toolkit reports it
/// (`Component.GetExtents(Screen)`), when that answer is trustworthy.
async fn frame_screen_origin(
    conn: &AccessibilityConnection,
    seed: &RawObjectRef,
) -> Option<(i32, i32)> {
    let acc = call(accessible_for(conn, seed)).await?.ok()?;
    let proxies = call(acc.proxies()).await?.ok()?;
    let component = call(proxies.component()).await?.ok()?;
    let raw = call(component.get_extents(CoordType::Screen)).await?.ok()?;
    let display = (!crate::wayland::is_wayland())
        .then(x11_display_size)
        .flatten();
    screen_extents_trusted(raw, display).then_some((raw.0, raw.1))
}

/// Largest offset between an X11 client origin and the toolkit's own frame
/// origin that still reads as decorations (a title bar, a border). Anything
/// larger is a mis-correlated frame, and the point is left alone.
const FRAME_DELTA_MAX_PX: i32 = 128;

/// Translate a client-window-local point into the toolkit's `Window`
/// coordinate frame for `GetAccessibleAtPoint`. The toolkit frame origin is
/// where the top-level accessible says it is on screen; the client origin is
/// the X11 window the caller's pixels are relative to. Their difference is
/// the decoration inset the toolkit folds into its frame (0 for GTK, the
/// title bar height for VCL). Unknown origins or an implausible delta leave
/// the point untouched.
pub(crate) fn at_point_toolkit_coords(
    (x, y): (i32, i32),
    client_origin: Option<(i32, i32)>,
    frame_origin: Option<(i32, i32)>,
) -> (i32, i32) {
    let (Some((cx, cy)), Some((fx, fy))) = (client_origin, frame_origin) else {
        return (x, y);
    };
    let (dx, dy) = (cx - fx, cy - fy);
    if dx.abs() > FRAME_DELTA_MAX_PX || dy.abs() > FRAME_DELTA_MAX_PX {
        return (x, y);
    }
    (x + dx, y + dy)
}

#[cfg(test)]
mod cached_hit_tests {
    use super::{cached_hit_fires, CachedHitLive};

    /// Kate's "Reload All Projects" after Escape closed the Projects menu:
    /// Qt cleared `Showing` but kept the item's extents (2026-11-03).
    fn closed_menu_item() -> CachedHitLive {
        CachedHitLive {
            showing: false,
            extents: Some((312, 240, 523, 36)),
            in_popup_menu: true,
            name: "Reload All Projects".into(),
        }
    }

    #[test]
    fn closed_menu_item_never_fires() {
        let item = closed_menu_item();
        assert!(!cached_hit_fires(&item, 573, 258, false));
        assert!(!cached_hit_fires(&item, 573, 258, true));
    }

    #[test]
    fn open_menu_item_fires_only_for_a_click_on_its_popup() {
        let item = CachedHitLive {
            showing: true,
            ..closed_menu_item()
        };
        assert!(cached_hit_fires(&item, 573, 258, true));
        // A click on the main window under the popup is not a click on the
        // menu (the real pointer press is the caller's to send).
        assert!(!cached_hit_fires(&item, 573, 258, false));
    }

    #[test]
    fn a_window_control_needs_to_still_cover_the_point() {
        let button = CachedHitLive {
            showing: true,
            extents: Some((100, 50, 80, 30)),
            in_popup_menu: false,
            name: "Save".into(),
        };
        assert!(cached_hit_fires(&button, 120, 60, false));
        assert!(!cached_hit_fires(&button, 300, 60, false));
        // Never from inside a popup window either.
        assert!(!cached_hit_fires(&button, 120, 60, true));
    }

    #[test]
    fn untrusted_extents_leave_the_showing_check() {
        let hidden = CachedHitLive {
            showing: false,
            extents: None,
            in_popup_menu: false,
            name: String::new(),
        };
        assert!(!cached_hit_fires(&hidden, 10, 10, false));
        let shown = CachedHitLive {
            showing: true,
            ..hidden
        };
        assert!(cached_hit_fires(&shown, 10, 10, false));
    }
}

#[cfg(test)]
mod at_point_coords_tests {
    use super::at_point_toolkit_coords;

    #[test]
    fn only_vcl_measures_from_the_wm_frame() {
        assert!(super::is_frame_coordinate_toolkit("VCL"));
        assert!(!super::is_frame_coordinate_toolkit("GTK"));
        assert!(!super::is_frame_coordinate_toolkit("GAIL"));
        assert!(!super::is_frame_coordinate_toolkit("Chromium"));
    }

    #[test]
    fn vcl_title_bar_inset_is_added() {
        // Calc: X11 client at (70,64), VCL frame origin (70,27): +37 px.
        assert_eq!(
            at_point_toolkit_coords((315, 218), Some((70, 64)), Some((70, 27))),
            (315, 255)
        );
    }

    #[test]
    fn gtk_frame_equals_client_so_nothing_changes() {
        assert_eq!(
            at_point_toolkit_coords((315, 218), Some((70, 64)), Some((70, 64))),
            (315, 218)
        );
    }

    #[test]
    fn unknown_or_implausible_origins_leave_the_point() {
        assert_eq!(
            at_point_toolkit_coords((10, 20), None, Some((1, 2))),
            (10, 20)
        );
        assert_eq!(
            at_point_toolkit_coords((10, 20), Some((1, 2)), None),
            (10, 20)
        );
        assert_eq!(
            at_point_toolkit_coords((10, 20), Some((900, 64)), Some((70, 64))),
            (10, 20)
        );
    }
}

/// Fire the right thing on a `GetAccessibleAtPoint` chain (frame first,
/// deepest hit last). The deepest node with a safe activation wins; a
/// container ancestor is never fired for a click on one of its items (see
/// [`at_point_activation_index`]). An item that advertises no Action is
/// selected through its container's `Selection` interface, the way a plain
/// click selects a file in a file manager; when that is not possible either,
/// `Ok(None)` leaves the point to the caller's real pointer press.
async fn actuate_chain(
    chain: &[HitNode<'_>],
    win_x: i32,
    win_y: i32,
    skip_focus_roles: bool,
) -> Result<Option<AtPointHit>> {
    let deepest = chain.len() - 1;
    let mut deepest_role = String::new();
    let mut deepest_name = String::new();
    let mut deepest_has_action = false;
    let mut passive_fallback: Option<(usize, String, String, Vec<String>)> = None;
    for (depth, node) in chain.iter().enumerate().rev() {
        let Some(Ok(role)) = call(node.acc.get_role_name()).await else {
            continue;
        };
        let Some(Ok(ifaces)) = call(node.acc.get_interfaces()).await else {
            continue;
        };
        let has_action = ifaces.contains(Interface::Action);
        if depth == deepest {
            deepest_role = role.clone();
            deepest_name = call(node.acc.name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            deepest_has_action = has_action;
        }
        if depth != deepest && !deepest_has_action && is_selectable_item_role(&role) {
            // The point is on a label / icon inside a file or list item: a
            // plain click selects that item.
            if let Some(hit) =
                select_item_in_chain(chain, depth, &role, &deepest_name, skip_focus_roles).await
            {
                return Ok(Some(hit));
            }
            dlog!(
                "hit-test ({win_x},{win_y}) -> depth {depth} {role:?} is a selectable item that cannot be selected through Selection; leaving it to a real click"
            );
            return Ok(None);
        }
        if !has_action {
            continue;
        }
        let Some(Ok(proxies)) = call(node.acc.proxies()).await else {
            continue;
        };
        let Some(Ok(ap)) = call(proxies.action()).await else {
            continue;
        };
        let actions = action_names(&ap).await;
        if depth == deepest && is_selectable_item_role(&role) {
            // A file / list item under a plain click is SELECTED; its own
            // `open` / `activate` is the double-click (Nautilus canvas items
            // advertise `open`, which launched the file). Select it through
            // the container; only fall back to the item's action when the
            // container cannot select.
            if let Some(hit) =
                select_item_in_chain(chain, deepest, &role, &deepest_name, skip_focus_roles).await
            {
                return Ok(Some(hit));
            }
            if actions.iter().any(|a| normalized_action_verb(a) == "open") {
                dlog!(
                    "hit-test ({win_x},{win_y}) -> {role:?} {deepest_name:?} only offers `open`; leaving the selection to a real click"
                );
                return Ok(None);
            }
        }
        let Some(chosen) = at_point_activation_index(&role, &actions, depth == deepest) else {
            if depth != deepest
                && !deepest_has_action
                && activation_index(&role, &actions).is_some()
            {
                // The item under the point has no Action and this ancestor's
                // activation is a container's "open the selection": select
                // the item instead, else leave the point to a real pointer
                // press, which is what selects it.
                dlog!(
                    "hit-test ({win_x},{win_y}) -> depth {depth} role={role:?} is a container over {deepest_role:?}: not firing {:?}",
                    activation_index(&role, &actions).and_then(|i| actions.get(i))
                );
                return Ok(select_item_in_chain(
                    chain,
                    deepest,
                    &deepest_role,
                    &deepest_name,
                    skip_focus_roles,
                )
                .await);
            }
            continue;
        };
        let name = call(node.acc.name())
            .await
            .and_then(|r| r.ok())
            .unwrap_or_default();
        if is_passive_role(&role) {
            if passive_fallback.is_none() {
                passive_fallback = Some((depth, role.clone(), name, actions.clone()));
            }
            continue;
        }
        if skip_focus_roles && is_focus_taking_role(&role) {
            // The caller has a real pointer: an entry / spin button /
            // cell gets the focus from a real press, not from
            // `doAction`. Fall through to that route.
            dlog!(
                "hit-test ({win_x},{win_y}) -> depth {depth} role={role:?}: focus-taking, leaving it to a real click"
            );
            return Ok(None);
        }
        dlog!(
            "hit-test ({win_x},{win_y}) -> depth {depth} role={role:?} name={name:?} path={} action={:?}",
            node.oref.path,
            actions.get(chosen)
        );
        let hit = AtPointHit::fired(
            actions.get(chosen).cloned().unwrap_or_default(),
            role.clone(),
            name,
            node.oref.path.clone(),
        );
        match call(ap.do_action(chosen as i32)).await {
            Some(Ok(true)) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Ok(Some(hit));
            }
            // Declined (a closed VCL menu answers false): nothing fired, so
            // the caller falls through to the real pointer press.
            Some(Ok(false)) => {
                dlog!("hit-test doAction declined; falling through to the pointer route");
                return Ok(None);
            }
            Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
            // Dispatched; the reply waits on a nested main loop.
            None => return Ok(Some(hit)),
        }
    }
    if !deepest_has_action && is_selectable_item_role(&deepest_role) {
        if let Some(hit) = select_item_in_chain(
            chain,
            deepest,
            &deepest_role,
            &deepest_name,
            skip_focus_roles,
        )
        .await
        {
            return Ok(Some(hit));
        }
    }
    if let Some((depth, role, name, actions)) = passive_fallback {
        let node = &chain[depth];
        let Some(chosen) = activation_index(&role, &actions) else {
            return Ok(None);
        };
        let Some(Ok(proxies)) = call(node.acc.proxies()).await else {
            return Ok(None);
        };
        let Some(Ok(ap)) = call(proxies.action()).await else {
            return Ok(None);
        };
        if let Some(Ok(true)) = call(ap.do_action(chosen as i32)).await {
            return Ok(Some(AtPointHit::fired(
                actions.get(chosen).cloned().unwrap_or_default(),
                role,
                name,
                node.oref.path.clone(),
            )));
        }
    }
    Ok(None)
}

/// Select the item under the point through its parent's `Selection`
/// interface: scanning the hit chain upward from the deepest node, the first
/// node with a selectable item role whose parent selects (Nautilus: `icon`
/// label -> `canvas` item -> `layered pane` with Selection) is cleared-and-
/// selected (`ClearSelection` + `SelectChild(index_in_parent)`) and read back
/// with `IsChildSelected`. `None` when nothing on the chain selects, the
/// toolkit refused, or the item is a grid cell and a real pointer is
/// available (`real_pointer_available`) — the caller then falls back to a
/// real pointer press.
async fn select_item_in_chain(
    chain: &[HitNode<'_>],
    from_depth: usize,
    _role_hint: &str,
    _name_hint: &str,
    real_pointer_available: bool,
) -> Option<AtPointHit> {
    for item_depth in (1..=from_depth.min(chain.len() - 1)).rev() {
        let item = &chain[item_depth];
        let parent = &chain[item_depth - 1];
        let Some(Ok(item_role)) = call(item.acc.get_role_name()).await else {
            continue;
        };
        if !is_selectable_item_role(&item_role) {
            continue;
        }
        if real_pointer_available && is_cell_role(&item_role) {
            // `Selection.SelectChild` marks a Calc cell without moving the
            // cell cursor: a following type_text would land in the old
            // active cell. The caller's real press both selects and
            // focuses it.
            dlog!(
                "{item_role:?} under the point is a grid cell; leaving it to the real pointer press instead of Selection.SelectChild"
            );
            return None;
        }
        let Some(Ok(ifaces)) = call(parent.acc.get_interfaces()).await else {
            continue;
        };
        if !ifaces.contains(Interface::Selection) {
            continue;
        }
        let item_name = call(item.acc.name())
            .await
            .and_then(|r| r.ok())
            .unwrap_or_default();
        let index = call(item.acc.get_index_in_parent()).await?.ok()?;
        if index < 0 {
            return None;
        }
        let proxies = call(parent.acc.proxies()).await?.ok()?;
        let selection = call(proxies.selection()).await?.ok()?;
        // Best effort: some containers refuse ClearSelection while still
        // honouring SelectChild (which then replaces a single selection).
        let _ = call(selection.clear_selection()).await;
        let accepted = call(selection.select_child(index)).await?.ok()?;
        if !accepted {
            dlog!(
                "Selection.SelectChild({index}) refused by {}",
                parent.oref.path
            );
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let verified = matches!(
            call(selection.is_child_selected(index)).await,
            Some(Ok(true))
        );
        dlog!(
            "selected child {index} ({item_role:?} {item_name:?}) of {} verified={verified}",
            parent.oref.path
        );
        return Some(AtPointHit {
            action: "select_child".to_owned(),
            role: item_role,
            name: item_name.clone(),
            path: item.oref.path.clone(),
            selected: Some(item_name),
            selection_verified: verified,
        });
    }
    None
}

/// A mutation was attempted; callers must not replay through another route.
#[derive(Debug)]
pub struct ScrollProgress {
    pub acknowledged: u32,
    pub complete: bool,
    pub detail: Option<String>,
}

fn finish_scroll(result: Result<()>, attempted: bool, acknowledged: u32) -> Result<ScrollProgress> {
    match result {
        Ok(()) => Ok(ScrollProgress {
            acknowledged,
            complete: true,
            detail: None,
        }),
        Err(error) if attempted => Ok(ScrollProgress {
            acknowledged,
            complete: false,
            detail: Some(error.to_string()),
        }),
        Err(error) => Err(error),
    }
}

async fn scroll_sequence<D, DF, S, SF>(
    amount: usize,
    mut baseline: Option<Vec<(i64, i64)>>,
    attempted: &std::cell::Cell<bool>,
    acknowledged: &std::cell::Cell<u32>,
    mut dispatch: D,
    mut settle: S,
) -> Result<()>
where
    D: FnMut() -> DF,
    DF: std::future::Future<Output = Result<bool>>,
    S: FnMut(Vec<(i64, i64)>) -> SF,
    SF: std::future::Future<Output = Result<Vec<(i64, i64)>>>,
{
    for step in 0..amount.max(1) {
        dlog!(
            "scroll sequence dispatch step={} baseline={baseline:?}",
            step + 1
        );
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        attempted.set(true);
        if !tokio::time::timeout_at(deadline, dispatch())
            .await
            .map_err(|_| anyhow!("scroll action timed out"))??
        {
            return Err(anyhow!("scroll action returned false"));
        }
        acknowledged.set(acknowledged.get() + 1);
        dlog!("scroll sequence acknowledged step={}", step + 1);
        if step + 1 < amount {
            if let Some(previous) = baseline.take() {
                baseline = Some(
                    tokio::time::timeout_at(deadline, settle(previous))
                        .await
                        .map_err(|_| anyhow!("scroll movement did not settle before deadline"))??,
                );
            }
        }
    }
    Ok(())
}

fn descendant_indices(depths: impl Iterator<Item = usize>, target: usize) -> Vec<usize> {
    let depths: Vec<_> = depths.collect();
    ((target + 1)..depths.len())
        .take_while(|&index| depths[index] > depths[target])
        .take(64)
        .collect()
}

fn relative_position(
    target: (i32, i32, i32, i32),
    probe: (i32, i32, i32, i32),
) -> Option<(i64, i64)> {
    let valid =
        |(x, y, w, h): (i32, i32, i32, i32)| x != i32::MIN && y != i32::MIN && w > 0 && h > 0;
    (valid(target) && valid(probe)).then(|| {
        (
            i64::from(probe.0) - i64::from(target.0),
            i64::from(probe.1) - i64::from(target.1),
        )
    })
}

struct PageMotion {
    baseline: Vec<(i64, i64)>,
    previous: Vec<(i64, i64)>,
    stable: usize,
}

impl PageMotion {
    fn new(baseline: Vec<(i64, i64)>) -> Self {
        Self {
            previous: baseline.clone(),
            baseline,
            stable: 0,
        }
    }

    fn observe(&mut self, sample: &[(i64, i64)], direction: &str) -> bool {
        if sample.len() != self.baseline.len() || sample.is_empty() {
            return false;
        }
        let axis = |point: (i64, i64)| {
            if matches!(direction, "left" | "right") {
                point.0
            } else {
                point.1
            }
        };
        let moved = sample.iter().zip(&self.baseline).any(|(&now, &before)| {
            if matches!(direction, "up" | "left") {
                axis(now) > axis(before)
            } else {
                axis(now) < axis(before)
            }
        });
        self.stable = if moved && sample == self.previous {
            self.stable + 1
        } else {
            0
        };
        self.previous = sample.to_vec();
        self.stable >= 2
    }
}

#[cfg(test)]
mod page_scroll_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn probes_are_bounded_descendants_in_preorder() {
        assert_eq!(
            descendant_indices([0, 1, 2, 3, 2, 1, 2].into_iter(), 1),
            vec![2, 3, 4]
        );
        assert!(descendant_indices([0, 1, 1].into_iter(), 1).is_empty());
        assert_eq!(
            descendant_indices(std::iter::once(0).chain(std::iter::repeat_n(1, 100)), 0).len(),
            64
        );
    }

    #[test]
    fn relative_geometry_rejects_unrealized_and_window_motion() {
        assert_eq!(
            relative_position((10, 20, 200, 100), (30, 60, 10, 10)),
            Some((20, 40))
        );
        assert_eq!(
            relative_position((110, 220, 200, 100), (130, 260, 10, 10)),
            Some((20, 40))
        );
        assert!(relative_position((0, 0, 200, 100), (i32::MIN, 0, 10, 10)).is_none());
        assert!(relative_position((0, 0, 0, 100), (0, 0, 10, 10)).is_none());
        let mut motion = PageMotion::new(vec![(20, 40)]);
        for _ in 0..10 {
            assert!(!motion.observe(&[(20, 40)], "down"));
        }
    }

    #[test]
    fn directional_motion_requires_consecutive_stable_samples() {
        for (direction, moved) in [
            ("down", (20, 10)),
            ("up", (20, 70)),
            ("left", (50, 40)),
            ("right", (-10, 40)),
        ] {
            let mut motion = PageMotion::new(vec![(20, 40)]);
            assert!(!motion.observe(&[moved], direction));
            assert!(!motion.observe(&[moved], direction));
            assert!(motion.observe(&[moved], direction));
        }
        let mut motion = PageMotion::new(vec![(20, 40)]);
        for _ in 0..4 {
            assert!(!motion.observe(&[(20, 70)], "down"));
        }
        assert!(!motion.observe(&[], "down"));
    }

    #[tokio::test(start_paused = true)]
    async fn asynchronous_page_ack_waits_for_movement_and_stability() {
        let events = RefCell::new(Vec::new());
        let attempted = Cell::new(false);
        let acknowledged = Cell::new(0);
        scroll_sequence(
            2,
            Some(vec![(0, 100)]),
            &attempted,
            &acknowledged,
            || async {
                events.borrow_mut().push("ack");
                Ok(true)
            },
            |baseline| async {
                let mut motion = PageMotion::new(baseline);
                for (event, y) in [
                    ("unchanged", 100),
                    ("move", 0),
                    ("stable", 0),
                    ("stable", 0),
                ] {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    events.borrow_mut().push(event);
                    if motion.observe(&[(0, y)], "down") {
                        return Ok(vec![(0, y)]);
                    }
                }
                unreachable!()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec!["ack", "unchanged", "move", "stable", "stable", "ack"]
        );
        assert_eq!(acknowledged.get(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_or_timed_out_attempts_never_become_fallback() {
        for successful_before in [0, 1] {
            for timeout in [false, true] {
                let attempted = Cell::new(false);
                let acknowledged = Cell::new(0);
                let result = scroll_sequence(
                    2,
                    None,
                    &attempted,
                    &acknowledged,
                    || async {
                        if acknowledged.get() < successful_before {
                            return Ok(true);
                        }
                        if timeout {
                            std::future::pending::<()>().await;
                        }
                        Ok(false)
                    },
                    |baseline| async { Ok(baseline) },
                )
                .await;
                let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
                assert!(!outcome.complete);
                assert_eq!(outcome.acknowledged, successful_before);
            }
        }
        assert!(finish_scroll(Err(anyhow!("unsupported before dispatch")), false, 0).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_or_stalled_probe_stops_after_first_ack() {
        for timeout in [false, true] {
            let attempted = Cell::new(false);
            let acknowledged = Cell::new(0);
            let result = scroll_sequence(
                2,
                Some(vec![(0, 100)]),
                &attempted,
                &acknowledged,
                || async { Ok(true) },
                |_| async {
                    if timeout {
                        std::future::pending::<()>().await;
                    }
                    Err(anyhow!("probe disappeared"))
                },
            )
            .await;
            let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
            assert!(!outcome.complete);
            assert_eq!(outcome.acknowledged, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overall_timeout_retains_progress() {
        let attempted = Cell::new(false);
        let acknowledged = Cell::new(0);
        let result = tokio::time::timeout(
            Duration::from_millis(1),
            scroll_sequence(
                2,
                Some(vec![(0, 100)]),
                &attempted,
                &acknowledged,
                || async { Ok(true) },
                |_| std::future::pending::<Result<Vec<(i64, i64)>>>(),
            ),
        )
        .await
        .map_err(|_| anyhow!("overall timeout"))
        .and_then(|r| r);
        let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
        assert_eq!(outcome.acknowledged, 1);
        assert!(!outcome.complete);
    }
}

async fn sample_page_probes(
    target: &atspi::proxy::component::ComponentProxy<'_>,
    probes: &[atspi::proxy::component::ComponentProxy<'_>],
    coord: CoordType,
) -> Result<Vec<(i64, i64)>> {
    let before = call(target.get_extents(coord))
        .await
        .and_then(|r| r.ok())
        .ok_or_else(|| anyhow!("scroll target geometry unavailable"))?;
    let mut sample = Vec::new();
    for probe in probes {
        let bounds = call(probe.get_extents(coord))
            .await
            .and_then(|r| r.ok())
            .ok_or_else(|| anyhow!("scroll probe disappeared"))?;
        sample.push(
            relative_position(before, bounds)
                .ok_or_else(|| anyhow!("scroll probe geometry invalid"))?,
        );
    }
    let after = call(target.get_extents(coord)).await.and_then(|r| r.ok());
    if after != Some(before) {
        return Err(anyhow!("scroll target geometry changed during observation"));
    }
    Ok(sample)
}

/// Invoke the original target's semantic actions, pacing multi-page requests
/// with descendant motion. `Err` means no mutation was attempted; an incomplete
/// `ScrollProgress` preserves uncertainty after an attempted mutation.
///
/// `idx` is an element of the caller's snapshot of window `xid`, resolved
/// against a fresh walk of that window.
pub fn scroll_element(
    pid: u32,
    xid: u64,
    idx: usize,
    direction: &str,
    amount: usize,
    by: pincer_contract::ScrollBy,
) -> Result<ScrollProgress> {
    let attempted = std::cell::Cell::new(false);
    let acknowledged = std::cell::Cell::new(0);
    let result = bounded(
        async {
            let conn = shared_connection().await?;
            let visited = collect_window_visited(conn, pid, xid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = visited
                .iter()
                .filter(|v| is_indexable(v))
                .nth(idx)
                .ok_or_else(|| anyhow!("element {idx} not found (total: {})", visited.len()))?;
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let wanted = match direction {
                "up" => ["scrollup", "scrollbackward"],
                "left" => ["scrollleft", "scrollbackward"],
                "right" => ["scrollright", "scrollforward"],
                _ => ["scrolldown", "scrollforward"],
            };
            let mut selected = None;
            let mut action_proxy = None;
            if let Ok(action) = proxies.action().await {
                let count = call(action.n_actions())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(0);
                for action_index in 0..count {
                    if let Some(Ok(name)) = call(action.get_name(action_index)).await {
                        let normalized: String = name
                            .chars()
                            .filter(|ch| ch.is_ascii_alphanumeric())
                            .flat_map(|ch| ch.to_lowercase())
                            .collect();
                        if wanted.iter().any(|candidate| *candidate == normalized) {
                            selected = Some(action_index);
                            break;
                        }
                    }
                }
                action_proxy = Some(action);
            }

            if let (Some(action), Some(action_index)) = (action_proxy, selected) {
                let mut observation = None;
                if by == pincer_contract::ScrollBy::Page && amount > 1 {
                    let component = proxies.component().await?;
                    let target_index = visited
                        .iter()
                        .position(|node| std::ptr::eq(node, target))
                        .unwrap();
                    let mut probes = Vec::new();
                    // Fix probe identities before dispatch; never replace vanished descendants.
                    for index in
                        descendant_indices(visited.iter().map(|node| node.depth), target_index)
                    {
                        if !visited[index].has_component {
                            continue;
                        }
                        if let Some(Ok(proxies)) = call(visited[index].acc.proxies()).await {
                            if let Some(Ok(probe)) = call(proxies.component()).await {
                                probes.push(probe);
                            }
                        }
                    }
                    for coord in [CoordType::Window, CoordType::Screen] {
                        let mut usable = Vec::new();
                        for probe in &probes {
                            if sample_page_probes(&component, std::slice::from_ref(probe), coord)
                                .await
                                .is_ok()
                            {
                                usable.push(probe.clone());
                                if usable.len() == 8 {
                                    break;
                                }
                            }
                        }
                        if !usable.is_empty() {
                            let baseline = sample_page_probes(&component, &usable, coord).await?;
                            observation = Some((component.clone(), usable, coord, baseline));
                            break;
                        }
                    }
                    if observation.is_none() {
                        return Err(anyhow!("no usable descendant scroll probes"));
                    }
                }
                scroll_sequence(
                    amount,
                    observation.as_ref().map(|o| o.3.clone()),
                    &attempted,
                    &acknowledged,
                    || async { action.do_action(action_index).await.map_err(Into::into) },
                    |baseline| async {
                        let (component, probes, coord, _) = observation
                            .as_ref()
                            .expect("page probes selected before dispatch");
                        let mut motion = PageMotion::new(baseline);
                        let started = tokio::time::Instant::now();
                        loop {
                            let sample = sample_page_probes(component, probes, *coord).await?;
                            let settled = motion.observe(&sample, direction);
                            dlog!(
                                "scroll sequence sample elapsed_ms={} positions={sample:?} settled={settled}",
                                started.elapsed().as_millis()
                            );
                            if settled {
                                return Ok(sample);
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    },
                )
                .await?;
                return Ok(());
            }

            if target.has_value {
                let value = proxies
                    .value()
                    .await
                    .map_err(|e| anyhow!("Value interface unavailable: {e}"))?;
                let current = call(value.current_value())
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value lookup timed out"))?;
                let minimum = call(value.minimum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let maximum = call(value.maximum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let increment = call(value.minimum_increment())
                    .await
                    .and_then(|result| result.ok())
                    .filter(|increment| *increment > 0.0)
                    .unwrap_or(1.0);
                let sign = if matches!(direction, "up" | "left") {
                    -1.0
                } else {
                    1.0
                };
                let next =
                    (current + sign * increment * amount.max(1) as f64).clamp(minimum, maximum);
                attempted.set(true);
                call(value.set_current_value(next))
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value update timed out"))?;
                acknowledged.set(1);
                return Ok(());
            }

            Err(anyhow!(
                "element {idx} exposes neither directional scroll actions nor Value"
            ))
        },
        || Err(anyhow!("scroll_element timed out for pid {pid}")),
    );
    finish_scroll(result, attempted.get(), acknowledged.get())
}

/// Give an indexed element keyboard focus through AT-SPI Component.GrabFocus
/// without activating or raising its toplevel window.
///
/// `GrabFocus` acknowledges the request before Chromium/Electron necessarily
/// updates its renderer-owned focused control. Sending key events immediately
/// after the acknowledgement can therefore split one string between the old
/// and new controls. Wait for the target's Focused state to become observable;
/// an acknowledgement without read-back is not sufficient for global input.
///
/// `idx` is an index of the caller's snapshot of window `xid`, so the walk
/// that resolves it is that window's.
pub fn focus_element(pid: u32, xid: u64, idx: usize) -> Result<bool> {
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let visited = collect_window_visited(conn, pid, xid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = visited
                .iter()
                .filter(|v| is_indexable(v))
                .nth(idx)
                .ok_or_else(|| anyhow!("element {idx} not found (total: {})", visited.len()))?;
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let component = proxies
                .component()
                .await
                .map_err(|e| anyhow!("Component interface unavailable: {e}"))?;
            let accepted = match call(component.grab_focus()).await {
                Some(Ok(focused)) => focused,
                Some(Err(e)) => {
                    return Err(anyhow!("Component.GrabFocus failed for element {idx}: {e}"))
                }
                None => return Err(anyhow!("Component.GrabFocus timed out for element {idx}")),
            };
            if !accepted {
                return Ok(false);
            }

            let settle_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(500);
            while tokio::time::Instant::now() < settle_deadline {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    target.acc.get_state(),
                )
                .await
                {
                    Ok(Ok(state)) if state.contains(State::Focused) => return Ok(true),
                    _ => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
            Ok(false)
        },
        || Err(anyhow!("focus_element timed out for pid {pid}")),
    )
}

/// Resolve a window-local pixel `(win_x, win_y)` to the deepest actionable
/// AT-SPI element covering it and perform its primary action.
///
/// This is the no-focus-steal way to land a *pixel* click on toolkits that drop
/// synthetic X11 pointer events. GTK3/4 take input via XInput2, so neither the
/// background `XSendEvent` path (synthetic, `send_event=True` — toolkits ignore
/// it) nor XTEST (its core events don't reach an XI2-only client; on a headless
/// Xvfb it also can't move a real device) actually clicks a GTK button. AT-SPI
/// `doAction` does, without activating or raising the window — the same path the
/// `element_index` click already uses, here driven by coordinates instead.
///
/// Hit-testing uses `Component.GetExtents(CoordType::Window)` so the caller's
/// window-local coordinates are compared directly against window-local widget
/// bounds — no screen-origin guessing. The smallest-area containing node wins so
/// a click lands on the button, not its enclosing panel. Returns `Ok(Some(action))`
/// when an element was actuated, `Ok(None)` when no actionable element covers the
/// point (the caller then falls back to the synthetic X11 path).
pub fn perform_action_at_point(
    pid: u32,
    xid: u64,
    win_x: i32,
    win_y: i32,
    skip_focus_roles: bool,
) -> Result<Option<AtPointHit>> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            // Only window `xid`'s elements: a window-local point compared with
            // every window's frames can hit another window's control that sits
            // at the same window coordinates (the main window under a dialog).
            let visited = match collect_window_visited(conn, pid, xid).await? {
                Some(v) => v,
                None => return Ok(None),
            };
            let web_document_origin = web_document_origin_for_visited(&visited, pid, 0, None)
                .await
                .unwrap_or((0, 0));

            // Collect actionable nodes whose window-local bounds contain the point,
            // then let `select_click_target` pick the innermost *real actuator* —
            // preferring a button over its slightly-smaller inner label (GTK4 nests
            // one inside every button; an area-only pick lands on the inert label
            // and `do_action` silently no-ops). Pre-order keeps containers ahead of
            // children, but the area/role split is what actually disambiguates.
            let mut frames: Vec<(usize, i32, i32, u32, u32, bool)> = Vec::new();
            for (i, v) in visited.iter().enumerate() {
                // Hidden nodes keep stale extents (a closed Qt menu's items),
                // so only what is on screen can be under the point.
                if v.actions.is_empty()
                    || !v.showing
                    || v.offscreen
                    || !v.has_component
                    || is_container_role(&v.role)
                    || crate::at_point_policy::is_top_level_shell_role(&v.role)
                {
                    continue;
                }
                let Some(Ok(proxies)) = call(v.acc.proxies()).await else {
                    continue;
                };
                let Some(Ok(comp)) = call(proxies.component()).await else {
                    continue;
                };
                let Some(Ok((x, y, w, h))) = call(comp.get_extents(CoordType::Window)).await else {
                    continue;
                };
                if w <= 0 || h <= 0 {
                    continue;
                }
                let (document_x, document_y) = if v.in_web_doc {
                    web_document_origin
                } else {
                    (0, 0)
                };
                frames.push((
                    i,
                    x + document_x,
                    y + document_y,
                    w as u32,
                    h as u32,
                    is_passive_role(&v.role),
                ));
            }

            let Some(idx) = select_click_target(&frames, win_x, win_y) else {
                return Ok(None);
            };
            let target = &visited[idx];
            if skip_focus_roles && is_focus_taking_role(&target.role) {
                return Ok(None);
            }
            if is_selectable_item_role(&target.role) {
                // A plain click on a file / list item selects it; its own
                // `open` / `activate` is the double-click. Leave it to the
                // caller's real pointer press.
                dlog!(
                    "full-walk hit {:?} {:?} is a selectable item; not firing",
                    target.role,
                    target.name
                );
                return Ok(None);
            }
            let Some(chosen) = activation_index(&target.role, &target.actions) else {
                return Ok(None);
            };
            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            match call(ap.do_action(chosen as i32)).await {
                Some(Ok(true)) => {}
                Some(Ok(false)) => {
                    dlog!("doAction declined at point; falling through to the pointer route");
                    return Ok(None);
                }
                Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
                None => dlog!("doAction dispatched but not acknowledged in time"),
            }
            Ok(Some(AtPointHit::fired(
                target.actions.get(chosen).cloned().unwrap_or_default(),
                target.role.clone(),
                target.name.clone(),
                target.object_ref.path.clone(),
            )))
        },
        || Ok(None),
    )
}

/// Vision/pixel click that actually lands — the Wayland answer (and a robust
/// GTK4 path generally). Maps a *screen* pixel to the smallest `element_index`
/// whose reconstructed screen frame covers it, then fires that element's
/// primary action via [`perform_action`].
///
/// Why not [`perform_action_at_point`]: that one hit-tests raw
/// `CoordType::Window` extents over an ad-hoc node set and `do_action`s the node
/// it resolves directly. On GTK4 that can land on an inner, non-actuating node
/// (a label inside the button) → a silent no-op that still returns `Some`
/// ("false success"). And on native Wayland there is no virtual-pointer click to
/// fall back to (Mutter drops synthetic pointer events). This routine instead
/// uses the SAME screen-frame reconstruction that `get_window_state` exposes to
/// the agent, via the GNOME Shell helper on Wayland and `_GTK_FRAME_EXTENTS` on
/// X11, and actuates by `element_index`, the click path already verified
/// working. So "click at pixel (x,y)" becomes "click the element the agent sees
/// there", with no pointer injection and no reliance on `CoordType::Screen`
/// (which GTK4 reports as (0,0)).
///
/// `screen_x`/`screen_y` are full-display screen pixels (what the vision
/// screenshot and `get_window_state` frames are in). Returns `Ok(Some(action))`
/// when AT-SPI accepts the action, `Ok(None)` when there is no target or the
/// action is rejected so the caller can fall back to its native injection path.
pub fn perform_action_at_screen_point(
    pid: u32,
    xid: u64,
    screen_x: i32,
    screen_y: i32,
) -> Result<Option<String>> {
    bounded_for(
        INPUT_QUERY_BUDGET,
        async {
            let conn = shared_connection().await?;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = match collect_visited_bounded(conn, pid, xid, None, None).await? {
                Some(walked) => walked,
                None => return Ok(None),
            };

            // Share snapshot correlation and geometry, including renderer-frame
            // rebasing and embedded WebProcess insets. Bounds retain application-
            // wide indices even when only the target window's nodes are emitted.
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let frames: Vec<_> = element_bounds_for_visited(&visited, pid, xid, scoped_frame)
                .await
                .0
                .into_iter()
                .filter(|&(idx, ..)| {
                    !crate::at_point_policy::is_top_level_shell_role(&action_nodes[idx].role)
                })
                .map(|(idx, x, y, w, h)| {
                    (idx, x, y, w, h, is_passive_role(&action_nodes[idx].role))
                })
                .collect();

            let Some(idx) = select_click_target(&frames, screen_x, screen_y) else {
                return Ok(None);
            };
            let target = action_nodes[idx];
            let Some(chosen) = activation_index(&target.role, &target.actions) else {
                return Ok(None);
            };
            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            match call(ap.do_action(chosen as i32)).await {
                Some(Ok(true)) => {}
                // Declined by the toolkit (a closed VCL menu): no accessible
                // hit, so the caller falls through to the real pointer press.
                Some(Ok(false)) => {
                    dlog!("doAction declined at point; falling through to the pointer route");
                    return Ok(None);
                }
                Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
                None => dlog!("doAction dispatched but not acknowledged in time"),
            }
            Ok(target.actions.get(chosen).cloned())
        },
        || Ok(None),
    )
}

/// AT-SPI roles whose `activate`/`press` action does not move the widget
/// focus into them (Chromium and GTK entries, spin buttons, sliders, table
/// cells), so a following keystroke would land elsewhere; a real pointer
/// click at the point is what a user does.
pub fn is_focus_taking_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "table cell"
            | "spin button"
            | "text"
            | "entry"
            | "password text"
            | "slider"
            | "combo box"
            | "editbar"
            | "search box"
            | "textbox"
    )
}

/// AT-SPI role names a toolkit gives a tooltip popup (`tool tip` is the
/// canonical spelling; `tooltip` appears in some bridges).
pub(crate) fn is_tooltip_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "tool tip" | "tooltip"
    )
}

/// Roles that draw text/graphics but don't *do* anything when actuated. GTK4
/// nests a `label` inside every `button` with a near-identical (slightly
/// smaller) frame, so an area-only hit-test lands on the inert label, where
/// `do_action` is a silent no-op (the "false success"). Treat these as
/// last-resort click targets.
pub(crate) fn is_passive_role(role: &str) -> bool {
    matches!(
        role,
        "label" | "static" | "static text" | "separator" | "filler" | "image" | "icon"
    )
}

/// Pick the `element_index` to actuate for a click at `(px, py)`. `frames` are
/// `(element_index, x, y, w, h, is_passive_label)` in the same coordinate space
/// as the point. Prefers the smallest covering *real actuator*; only falls back
/// to a passive label if nothing else covers the point. Smallest-area within a
/// class wins so the click lands on the button, not its enclosing panel.
/// Right/bottom edges are exclusive (`px < x + w`). `None` if nothing covers it.
pub(crate) fn select_click_target(
    frames: &[(usize, i32, i32, u32, u32, bool)],
    px: i32,
    py: i32,
) -> Option<usize> {
    let mut best_active: Option<(i64, usize)> = None;
    let mut best_passive: Option<(i64, usize)> = None;
    for &(idx, x, y, w, h, passive) in frames {
        let (w, h) = (w as i32, h as i32);
        if px >= x && px < x + w && py >= y && py < y + h {
            let area = (w as i64) * (h as i64);
            let slot = if passive {
                &mut best_passive
            } else {
                &mut best_active
            };
            if slot.map(|(a, _)| area < a).unwrap_or(true) {
                *slot = Some((area, idx));
            }
        }
    }
    best_active.or(best_passive).map(|(_, idx)| idx)
}

/// Error-message prefix when an element offers neither `EditableText` nor
/// `Value`: the tool layer branches to the keyboard fallback on it.
pub const NO_VALUE_ROUTE: &str = "no_value_route";

pub fn is_no_value_route(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(NO_VALUE_ROUTE)
}

/// A spin button / spin scale only parses its entry text on activate (Return
/// or focus-out); after an EditableText write, fire the element's `activate`
/// action when it exposes one so the numeric value commits. Best effort.
async fn commit_editable_write(proxies: &atspi::proxy::proxy_ext::Proxies<'_>) {
    let Some(Ok(ap)) = call(proxies.action()).await else {
        return;
    };
    let names = action_names(&ap).await;
    if let Some(index) = names
        .iter()
        .position(|name| normalized_action_verb(name) == "activate")
    {
        let _ = call(ap.do_action(index as i32)).await;
    }
}

/// The focus-free write shared by the index and cached-ref entry points.
/// SetValue never calls Component.GrabFocus: GTK may activate and raise the
/// toplevel in response, violating the background contract. Toolkits that
/// expose EditableText only while focused get an honest `NO_VALUE_ROUTE`.
async fn set_value_on(
    acc: &AccessibleProxy<'_>,
    has_value: bool,
    value: &str,
    label: &str,
) -> Result<()> {
    let proxies = acc
        .proxies()
        .await
        .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
    if let Ok(et) = proxies.editable_text().await {
        // Replace whole contents (parity with the Windows/macOS set_value,
        // which overwrite rather than insert at the caret).
        if et.set_text_contents(value).await.unwrap_or(false) {
            commit_editable_write(&proxies).await;
            return Ok(());
        }
        // Some toolkits reject SetTextContents but accept an insert at the
        // caret offset; clear-then-insert as a fallback.
        let off = match proxies.text().await {
            Ok(tp) => tp.caret_offset().await.unwrap_or(0),
            Err(_) => 0,
        };
        let len = value.chars().count() as i32;
        if et.insert_text(off, value, len).await.unwrap_or(false) {
            commit_editable_write(&proxies).await;
            return Ok(());
        }
    }
    if has_value {
        let v: f64 = value
            .parse()
            .map_err(|_| anyhow!("value '{value}' is not numeric for a Value element"))?;
        proxies
            .value()
            .await
            .map_err(|e| anyhow!("Value unavailable: {e}"))?
            .set_current_value(v)
            .await
            .map_err(|e| anyhow!("setCurrentValue failed: {e}"))?;
        return Ok(());
    }
    Err(anyhow!(
        "{NO_VALUE_ROUTE}: {label} exposes neither EditableText nor Value"
    ))
}

/// Set the value of element `idx` of window `xid`, resolved against a fresh
/// walk of that window (the index space of its snapshot).
pub fn set_value(pid: u32, xid: u64, idx: usize, value: &str) -> Result<()> {
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let visited = collect_window_visited(conn, pid, xid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes.get(idx).ok_or_else(|| {
                anyhow!("element {idx} not found (total: {})", action_nodes.len())
            })?;
            set_value_on(
                &target.acc,
                target.has_value,
                value,
                &format!("element {idx}"),
            )
            .await
        },
        || {
            Err(anyhow!(
                "set_value timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

/// [`set_value`] on a snapshot-cached element identity (the exact object the
/// caller's `element_index` named, not the same ordinal in a fresh walk).
pub fn set_value_ref(object_ref: &ObjectRef, value: &str) -> Result<()> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Err(anyhow!("cached element interfaces unavailable")),
            };
            set_value_on(
                &acc,
                ifaces.contains(Interface::Value),
                value,
                "cached element",
            )
            .await
        },
        || Err(anyhow!("set_value (cached element) timed out")),
    )
}

/// Error prefix: a selection container (combo box, list) has no option
/// with the requested text.
pub const NO_SUCH_OPTION: &str = "no_such_option";

pub fn is_no_such_option(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(NO_SUCH_OPTION)
}

/// Most options a [`NoSuchOption`] error names.
pub const OPTIONS_LISTED: usize = 20;

/// `set_value` named an option the combo box or list does not have. Its
/// text starts with [`NO_SUCH_OPTION`] and names the options there are, at
/// most [`OPTIONS_LISTED`] of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoSuchOption {
    pub value: String,
    pub options: Vec<String>,
}

impl NoSuchOption {
    /// The options the error names, in order, and how many more there are.
    pub fn listed(&self) -> (&[String], usize) {
        let shown = self.options.len().min(OPTIONS_LISTED);
        (&self.options[..shown], self.options.len() - shown)
    }
}

impl std::fmt::Display for NoSuchOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (shown, more) = self.listed();
        write!(
            f,
            "{NO_SUCH_OPTION}: no option {:?}; options: {shown:?}",
            self.value
        )?;
        if more > 0 {
            write!(f, " and {more} more")?;
        }
        Ok(())
    }
}

impl std::error::Error for NoSuchOption {}

/// Options scanned under one combo box or list.
const OPTION_SCAN_CAP: usize = 200;

fn is_option_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu item" | "check menu item" | "radio menu item" | "list item" | "option"
    )
}

/// A combo box's options live in a child menu (GTK) or list (Qt).
fn is_option_container_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "popup menu" | "list" | "list box" | "window"
    )
}

/// Index of the option `value` names: an exact match first, then one that
/// differs only in case or surrounding space.
pub fn match_option(names: &[String], value: &str) -> Option<usize> {
    names.iter().position(|name| name == value).or_else(|| {
        let wanted = value.trim().to_lowercase();
        names
            .iter()
            .position(|name| name.trim().to_lowercase() == wanted)
    })
}

/// Outcome of picking an option of a selection container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionPick {
    /// The option was selected or activated.
    Picked { name: String },
    /// No option has that text; nothing was changed.
    NoMatch { options: Vec<String> },
    /// The element lists no options (or cannot select them).
    Unavailable,
}

struct OptionNode {
    oref: RawObjectRef,
    name: String,
    /// Position among its container's children: the model row of a GTK
    /// combo box, which its `Selection.SelectChild` takes.
    index: i32,
}

/// The options under `oref`: item children, or the items of its popup menu
/// or list child (one level down), in order.
async fn list_options(conn: &AccessibilityConnection, oref: &RawObjectRef) -> Vec<OptionNode> {
    let zconn = conn.connection();
    let mut options = Vec::new();
    let Some(Ok(children)) = call(raw_children(zconn, oref)).await else {
        return options;
    };
    for (position, child) in children.into_iter().enumerate() {
        if options.len() >= OPTION_SCAN_CAP {
            break;
        }
        let Some(Ok(acc)) = call(accessible_for(conn, &child)).await else {
            continue;
        };
        let role = call(acc.get_role_name())
            .await
            .and_then(|r| r.ok())
            .unwrap_or_default();
        if is_option_role(&role) {
            let name = call(acc.name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            options.push(OptionNode {
                oref: child,
                name,
                index: position as i32,
            });
            continue;
        }
        if !is_option_container_role(&role) {
            continue;
        }
        let Some(Ok(items)) = call(raw_children(zconn, &child)).await else {
            continue;
        };
        for (index, item) in items.into_iter().enumerate() {
            if options.len() >= OPTION_SCAN_CAP {
                break;
            }
            let Some(Ok(item_acc)) = call(accessible_for(conn, &item)).await else {
                continue;
            };
            let item_role = call(item_acc.get_role_name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            if !is_option_role(&item_role) {
                continue;
            }
            let name = call(item_acc.name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            options.push(OptionNode {
                oref: item,
                name,
                index: index as i32,
            });
        }
    }
    options
}

/// Pick the option `value` names on a snapshot-cached selection container
/// (a GTK combo box) through its own `Selection.SelectChild`: the toolkit
/// sets the active row and emits its `changed` signal, focus-free and
/// without opening the popup.
pub fn select_option_ref(object_ref: &ObjectRef, value: &str) -> Result<OptionPick> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Ok(OptionPick::Unavailable),
            };
            if !ifaces.contains(Interface::Selection) {
                return Ok(OptionPick::Unavailable);
            }
            let options = list_options(conn, &raw_ref(object_ref)).await;
            if options.is_empty() {
                return Ok(OptionPick::Unavailable);
            }
            let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
            let Some(found) = match_option(&names, value) else {
                return Ok(OptionPick::NoMatch { options: names });
            };
            let option = &options[found];
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let selection = proxies
                .selection()
                .await
                .map_err(|e| anyhow!("Selection unavailable: {e}"))?;
            match call(selection.select_child(option.index)).await {
                Some(Ok(true)) => Ok(OptionPick::Picked {
                    name: option.name.clone(),
                }),
                Some(Ok(false)) => Err(anyhow!(
                    "Selection.SelectChild({}) was refused for option {:?}",
                    option.index,
                    option.name
                )),
                Some(Err(e)) => Err(anyhow!("Selection.SelectChild failed: {e}")),
                None => Err(anyhow!("Selection.SelectChild did not answer in time")),
            }
        },
        || Err(anyhow!("select option (cached element) timed out")),
    )
}

/// Activate the option `value` names in the open popup of a snapshot-cached
/// combo box (its items exist and act only while it is open): the option's
/// own click/activate action selects it and closes the popup.
pub fn activate_option_ref(object_ref: &ObjectRef, value: &str) -> Result<OptionPick> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            live_accessible(conn, object_ref).await?;
            let options = list_options(conn, &raw_ref(object_ref)).await;
            if options.is_empty() {
                return Ok(OptionPick::Unavailable);
            }
            let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
            let Some(found) = match_option(&names, value) else {
                return Ok(OptionPick::NoMatch { options: names });
            };
            let option = &options[found];
            let acc = match call(accessible_for(conn, &option.oref)).await {
                Some(Ok(acc)) => acc,
                _ => return Err(anyhow!("option {:?} is gone", option.name)),
            };
            let role = call(acc.get_role_name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let ap = proxies
                .action()
                .await
                .map_err(|e| anyhow!("option {:?} has no Action: {e}", option.name))?;
            let actions = action_names(&ap).await;
            let chosen = activation_index(&role, &actions).ok_or_else(|| {
                anyhow!("option {:?} advertises no activation action", option.name)
            })?;
            match call(ap.do_action(chosen as i32)).await {
                Some(Ok(true)) => Ok(OptionPick::Picked {
                    name: option.name.clone(),
                }),
                Some(Ok(false)) => Err(anyhow!("option {:?} refused its action", option.name)),
                Some(Err(e)) => Err(anyhow!("option action failed: {e}")),
                None => Err(anyhow!("option action did not answer in time")),
            }
        },
        || Err(anyhow!("activate option (cached element) timed out")),
    )
}

/// Whether a snapshot-cached combo box's popup is open: any of its options
/// is SHOWING.
pub fn options_showing_ref(object_ref: &ObjectRef) -> bool {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            for option in list_options(conn, &raw_ref(object_ref)).await {
                if let Some(Ok(acc)) = call(accessible_for(conn, &option.oref)).await {
                    if let Some(Ok(state)) = call(acc.get_state()).await {
                        if is_showing_state(&state) {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        },
        || Ok(false),
    )
    .unwrap_or(false)
}

/// How [`match_option`] matched `value` to the option `name`: its text
/// exactly, or only after ignoring case and surrounding space.
pub fn option_match_kind(name: &str, value: &str) -> &'static str {
    if name == value {
        "exact"
    } else {
        "case-insensitive"
    }
}

/// The option names of a snapshot-cached combo box or list, in order: item
/// children, or the items of its menu or list child. Qt lists a combo box's
/// rows while it is closed too; GTK and VCL build them only while it is open,
/// so empty says nothing about the options.
pub fn option_names_ref(object_ref: &ObjectRef) -> Vec<String> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            Ok(list_options(conn, &raw_ref(object_ref))
                .await
                .into_iter()
                .map(|option| option.name)
                .collect())
        },
        || Ok(Vec::new()),
    )
    .unwrap_or_default()
}

/// The live accessible name of a snapshot-cached element. Qt names a
/// non-editable combo box by its current text on Linux, even when the app set
/// an accessible name (checked on Qt 6.12 with `setAccessibleName`,
/// 2026-11-09), and exposes no Value, Text or Selection on it, so its name is
/// the only read-back of the option it shows.
pub fn name_ref(object_ref: &ObjectRef) -> Result<String> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            match call(acc.name()).await {
                Some(Ok(name)) => Ok(name),
                Some(Err(e)) => Err(anyhow!("name unavailable: {e}")),
                None => Err(anyhow!("cached element did not answer in time")),
            }
        },
        || Err(anyhow!("name (cached element) timed out")),
    )
}

/// Whether `pid`'s application is one whose popups [`qt_popup_items`] can
/// read: its toolkit is Qt ([`is_collection_toolkit_app`]).
pub fn is_qt_application(pid: u32) -> bool {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            match app_for_pid(conn, pid).await? {
                Some(app) => Ok(is_collection_toolkit_app(&app).await),
                None => Ok(false),
            }
        },
        || Ok(false),
    )
    .unwrap_or(false)
}

/// One row of an open Qt popup (a combo box's option), with what a real
/// press on it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupItem {
    pub name: String,
    pub role: String,
    /// Screen `(x, y, width, height)` as the toolkit reports it (a Qt combo
    /// row can be wider than its popup); `None` when it answered none.
    pub rect: Option<(i32, i32, i32, i32)>,
    pub object_ref: ObjectRef,
}

/// The rows of an open Qt popup and the accessible that owns them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PopupItems {
    /// Parent of the container the rows hang under: the combo box whose
    /// list the popup shows (`combo box > list > list item`).
    pub owner: Option<ObjectRef>,
    pub items: Vec<PopupItem>,
}

/// The rows of the open popup `popup_xid` of Qt application `pid`, in order:
/// the option rows of the container that fills the popup, found the way a
/// read of the popup finds it ([`qt_popup_item_container`]). `Ok(None)` when
/// no showing container fills the popup (Qt marks a combo's list showing a
/// moment after the popup maps) or the application is not Qt.
pub fn qt_popup_items(pid: u32, popup_xid: u64) -> Result<Option<PopupItems>> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let Some(ResolvedApp { app, children }) = resolve_app_for_pid(conn, pid).await? else {
                return Ok(None);
            };
            let seeds: Vec<RawObjectRef> = match children {
                Some(children) => children
                    .iter()
                    .filter_map(RawObjectRef::from_atspi)
                    .collect(),
                None => match call(app.get_children()).await {
                    Some(Ok(children)) => children
                        .iter()
                        .filter_map(RawObjectRef::from_atspi)
                        .collect(),
                    _ => return Ok(None),
                },
            };
            let Some((_, container)) = qt_popup_item_container(conn, &app, &seeds, popup_xid).await
            else {
                return Ok(None);
            };
            let zconn = conn.connection();
            let owner = match call(accessible_for(conn, &container)).await {
                Some(Ok(acc)) => match call(acc.parent()).await {
                    Some(Ok(parent)) => RawObjectRef::from_atspi(&parent).map(|parent| ObjectRef {
                        bus: parent.name,
                        path: parent.path,
                    }),
                    _ => None,
                },
                _ => None,
            };
            let Some(Ok(rows)) = call(raw_children(zconn, &container)).await else {
                return Ok(None);
            };
            let mut items = Vec::new();
            for row in rows.into_iter().take(OPTION_SCAN_CAP) {
                let Some(Ok(acc)) = call(accessible_for(conn, &row)).await else {
                    continue;
                };
                let role = call(acc.get_role_name())
                    .await
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                if !is_option_role(&role) {
                    continue;
                }
                let name = call(acc.name())
                    .await
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let rect = match call(raw_component(zconn, &row)).await {
                    Some(Ok(component)) => call(component.get_extents(CoordType::Screen))
                        .await
                        .and_then(|r| r.ok()),
                    _ => None,
                };
                items.push(PopupItem {
                    name,
                    role,
                    rect,
                    object_ref: ObjectRef {
                        bus: row.name,
                        path: row.path,
                    },
                });
            }
            Ok(Some(PopupItems { owner, items }))
        },
        || Ok(None),
    )
}

/// Whether a cached element is a password field by its numeric AT-SPI role
/// (`PasswordText`). Qt answers `GetRoleName` with "text" for a QLineEdit in
/// Password echo mode while `GetRole` says 40, password text (seen on the Qt
/// probe), so the role name cached by the walk cannot tell.
pub fn is_password_ref(object_ref: &ObjectRef) -> bool {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            Ok(matches!(
                call(acc.get_role()).await,
                Some(Ok(atspi::Role::PasswordText))
            ))
        },
        || Ok(false),
    )
    .unwrap_or(false)
}

/// Name of the selected child of a `Selection` element (a combo box's
/// active option), for the `set_value` read-back.
async fn selected_child_name(
    conn: &AccessibilityConnection,
    proxies: &atspi::proxy::proxy_ext::Proxies<'_>,
) -> Option<String> {
    let selection = call(proxies.selection()).await?.ok()?;
    if call(selection.n_selected_children()).await?.ok()? < 1 {
        return None;
    }
    let child = call(selection.get_selected_child(0)).await?.ok()?;
    let raw = RawObjectRef::from_atspi(&child)?;
    let acc = call(accessible_for(conn, &raw)).await?.ok()?;
    call(acc.name()).await?.ok()
}

/// The text of the element at `object_ref` from `before` characters ahead of
/// its caret to `after` characters past it. An insert through EditableText
/// leaves the caret after the inserted text (KWrite: 14 characters typed at
/// 1:1 left the caret at 1:15, 2026-11-11), so this reads what was typed in
/// a document of any length, where [`read_value_ref`] stops at 4,096
/// characters. Returns the text and whether the field goes on before and
/// after it; `None` when the element has no Text interface or no caret.
pub fn read_text_around_caret(
    object_ref: &ObjectRef,
    before: i32,
    after: i32,
) -> Result<Option<(String, bool, bool)>> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let Some(Ok(proxies)) = call(acc.proxies()).await else {
                return Ok(None);
            };
            let Some(Ok(text)) = call(proxies.text()).await else {
                return Ok(None);
            };
            let Some(Ok(caret)) = call(text.caret_offset()).await else {
                return Ok(None);
            };
            let Some(Ok(count)) = call(text.character_count()).await else {
                return Ok(None);
            };
            if caret < 0 || count <= 0 {
                return Ok(None);
            }
            let start = caret.saturating_sub(before).max(0);
            let end = caret.saturating_add(after).min(count);
            match call(text.get_text(start, end)).await {
                Some(Ok(around)) => Ok(Some((around, start > 0, end < count))),
                _ => Ok(None),
            }
        },
        || Ok(None),
    )
}

/// Current value of a cached element for read-back after a write: the
/// `Value` interface's number, else the `Text` content. `None` when the
/// element exposes neither (or does not answer in time).
pub fn read_value_ref(object_ref: &ObjectRef) -> Result<Option<String>> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Ok(None),
            };
            let proxies = match call(acc.proxies()).await {
                Some(Ok(proxies)) => proxies,
                _ => return Ok(None),
            };
            if ifaces.contains(Interface::Value) {
                if let Some(Ok(vp)) = call(proxies.value()).await {
                    if let Some(Ok(v)) = call(vp.current_value()).await {
                        return Ok(Some(format_value(v)));
                    }
                }
            }
            if ifaces.contains(Interface::Text) {
                if let Some(Ok(tp)) = call(proxies.text()).await {
                    let count = call(tp.character_count())
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or(0);
                    if let Some(Ok(t)) =
                        call(tp.get_text(0, count.min(TEXT_READ_CHARS as i32))).await
                    {
                        return Ok(Some(t));
                    }
                }
            }
            if ifaces.contains(Interface::Selection) {
                return Ok(selected_child_name(conn, &proxies).await);
            }
            Ok(None)
        },
        || Err(anyhow!("read_value (cached element) timed out")),
    )
}

/// Screen bounds a walk resolved are usable for a pointer action only when
/// they have a real size and intersect the display. `display` is `None` when
/// the display size is unknown (Wayland); the size check still applies.
pub(crate) fn walk_bounds_usable(
    bounds: Option<(i32, i32, u32, u32)>,
    display: Option<(u32, u32)>,
) -> Option<(i32, i32, u32, u32)> {
    let (x, y, w, h) = bounds?;
    if w == 0 || h == 0 {
        return None;
    }
    if let Some((dw, dh)) = display {
        let right = i64::from(x) + i64::from(w);
        let bottom = i64::from(y) + i64::from(h);
        if x >= dw as i32 || y >= dh as i32 || right <= 0 || bottom <= 0 {
            return None;
        }
    }
    Some((x, y, w, h))
}

/// Resolve element `idx` against a fresh walk and read its screen bounds.
/// `xid != 0` walks the caller's window FIRST, in the same visiting order (and
/// therefore the same index space) `get_window_state` used for the snapshot
/// the index came from; an application-wide pre-order would number a
/// multi-window app (GIMP's docks, LibreOffice dialogs) differently.
fn element_bounds_by_walk(pid: u32, xid: u64, idx: usize) -> Result<(i32, i32, u32, u32)> {
    // Synchronous X11 lookups stay on the caller's blocking thread, outside
    // the AT-SPI runtime.
    let offset = window_to_screen_offset(pid, xid, None);
    let display = (!crate::wayland::is_wayland())
        .then(x11_display_size)
        .flatten();
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let visited = collect_visited_bounded(conn, pid, xid, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?
                .visited;
            let web_document_origin = web_document_origin_for_visited(&visited, pid, xid, None)
                .await
                .unwrap_or((0, 0));
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes
                .get(idx)
                .ok_or_else(|| anyhow!("element {idx} not found"))?;
            if !target.has_component {
                return Err(anyhow!("element {idx} exposes no Component interface"));
            }
            let comp = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .component()
                .await
                .map_err(|e| anyhow!("Component unavailable: {e}"))?;
            // Prefer WINDOW coords + a deterministic screen offset — fixes GTK4,
            // whose CoordType::Screen collapses every element to (0,0). Fall back to
            // Screen on Wayland / when no X11 window resolves (offset is None).
            match offset {
                Some((ox, oy)) => {
                    let raw = comp
                        .get_extents(CoordType::Window)
                        .await
                        .map_err(|e| anyhow!("getExtents failed: {e}"))?;
                    let document_origin = target.in_web_doc.then_some(web_document_origin);
                    // An unrealized widget (a menu item whose menu is closed)
                    // answers (0,0,0,0) in Window coordinates; projected onto
                    // the window origin that would be a "valid" centre at the
                    // window's top-left corner. Refuse it like the cached
                    // path does.
                    walk_bounds_usable(
                        project_screen_extents(raw, (ox, oy), document_origin),
                        display,
                    )
                    .ok_or_else(|| {
                        anyhow!(
                            "element {idx} ({}) reports no usable on-screen extents                              (raw window extents {raw:?}); it may be inside a closed menu                              or an unmapped page",
                            target.role
                        )
                    })
                }
                None => {
                    let raw = comp
                        .get_extents(CoordType::Screen)
                        .await
                        .map_err(|e| anyhow!("getExtents failed: {e}"))?;
                    walk_bounds_usable(project_screen_extents(raw, (0, 0), None), display)
                        .ok_or_else(|| {
                            anyhow!(
                                "element {idx} ({}) reports no usable on-screen extents                                  (raw screen extents {raw:?})",
                                target.role
                            )
                        })
                }
            }
        },
        || {
            Err(anyhow!(
                "get_element_bounds timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

pub fn get_element_bounds_for_window(
    pid: u32,
    xid: u64,
    idx: usize,
) -> Result<(i32, i32, u32, u32)> {
    if !crate::wayland::is_wayland() || !crate::wayland::hyprland::is_session() {
        return element_bounds_by_walk(pid, xid, idx);
    }
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async {
            let conn = shared_connection().await?;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(conn, pid, xid, None, None)
                .await?
                .context("no AT-SPI application")?;
            let scope =
                scoped_frame.context("Hyprland accessibility window identity is unproven")?;
            let target = visited
                .iter()
                .filter(|v| is_indexable(v))
                .nth(idx)
                .context("element no longer exists")?;
            if target.frame_ordinal != scope {
                return Err(anyhow!("element does not belong to the requested window"));
            }
            element_bounds_for_visited(&visited, pid, xid, Some(scope))
                .await
                .0
                .into_iter()
                .find(|(index, ..)| *index == idx)
                .map(|(_, x, y, w, h)| (x, y, w, h))
                .context("element has no compositor-attested screen bounds")
        },
        || Err(anyhow!("Hyprland element bounds timed out")),
    )
}

/// Size of the X11 display, for sanity-checking screen extents.
fn x11_display_size() -> Option<(u32, u32)> {
    use x11rb::connection::Connection as _;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None).ok()?;
    let screen = &conn.setup().roots[screen_num];
    Some((
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
    ))
}

/// Whether a `CoordType::Screen` answer can be used as is. GTK 4.6 (Ubuntu
/// 22.04) reports correct Screen extents while its `CoordType::Window`
/// extents are relative to the *outer* X11 window (shadow included), so the
/// `_GTK_FRAME_EXTENTS` reconstruction lands (left, top) px too far; newer
/// GTK4 instead collapses Screen extents to (0,0). Trust a Screen answer when
/// it is neither that sentinel nor off the display.
pub(crate) fn screen_extents_trusted(
    raw: (i32, i32, i32, i32),
    display: Option<(u32, u32)>,
) -> bool {
    let (x, y, w, h) = raw;
    if !crate::snapshot_queries::plausible_raw_extents(raw) || (x == 0 && y == 0) {
        return false;
    }
    match display {
        Some((dw, dh)) => x < dw as i32 && y < dh as i32 && x + w > 0 && y + h > 0,
        None => true,
    }
}

/// Whether a `CoordType::Screen` answer may override the Window-relative
/// reconstruction. GTK3 on native Wayland reports window-local values as
/// `Screen` (no display bounds exist to reject them), so a compositor-attested
/// native client's origin plus `Window` extents must win. XWayland clients
/// report true screen extents and are never attested.
fn screen_answer_overrides_window(
    raw: (i32, i32, i32, i32),
    display: Option<(u32, u32)>,
    native_origin_attested: bool,
) -> bool {
    !native_origin_attested && screen_extents_trusted(raw, display)
}

/// True only when the Hyprland compositor identifies this exact client
/// (nonzero address `xid` AND `pid`) as native Wayland, not XWayland. Missing,
/// mismatched, or unreported clients fail closed, so `Screen` extents keep
/// winning, which is correct for XWayland GTK and LibreOffice. Performs
/// blocking compositor IPC: async callers must use `bounded_blocking`.
fn native_wayland_origin_attested(pid: u32, xid: u64) -> bool {
    xid != 0
        && crate::wayland::is_wayland()
        && crate::wayland::hyprland::is_session()
        && crate::wayland::hyprland::native_client_attested(xid, pid)
}

/// Real on-screen origin (root-relative top-left) of an X11 window, or `None`
/// if it can't be resolved. Mirrors `list_windows`' geometry path.
pub(crate) fn x11_window_origin(xid: u64) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;

    let (conn, _) = crate::x11::query_connection().ok()?;
    let window = xid as u32;
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;
    let trans = conn
        .translate_coordinates(window, geom.root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((trans.dst_x as i32, trans.dst_y as i32))
}

fn x11_top_level_geometry(xid: u64) -> Option<(i32, i32, i32, i32)> {
    use x11rb::protocol::xproto::*;

    let (conn, _) = crate::x11::query_connection().ok()?;
    let mut window = xid as u32;
    loop {
        let tree = conn.query_tree(window).ok()?.reply().ok()?;
        if tree.parent == tree.root || tree.parent == x11rb::NONE {
            break;
        }
        window = tree.parent;
    }
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;
    Some((
        i32::from(geom.x),
        i32::from(geom.y),
        i32::from(geom.width),
        i32::from(geom.height),
    ))
}

/// Read the GTK4 client-side-decoration shadow inset from the X11
/// `_GTK_FRAME_EXTENTS` property (`CARDINAL[4]` = left, right, top, bottom).
///
/// A GTK4 window is an outer X11 window whose *visible content* starts `left`
/// px in and `top` px down — the rest is the invisible CSD shadow. AT-SPI
/// `CoordType::Window` coordinates are relative to that content origin, so
/// reconstructing true screen coords needs this inset added to the X11 window
/// origin. Returns `None` (treated as no inset, i.e. `(0,0)`) for non-GTK /
/// server-side-decorated windows that don't set the property — which is also
/// how we tell GTK4-CSD apart from everyone else.
fn gtk_frame_extents(xid: u64) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;

    let (conn, _) = crate::x11::query_connection().ok()?;
    // only_if_exists=true → atom is 0 when no client ever set the property.
    let atom = conn
        .intern_atom(true, b"_GTK_FRAME_EXTENTS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if atom == 0 {
        return None;
    }
    let reply = conn
        .get_property(false, xid as u32, atom, AtomEnum::CARDINAL, 0, 4)
        .ok()?
        .reply()
        .ok()?;
    let vals: Vec<u32> = reply.value32()?.collect();
    parse_gtk_frame_extents(&vals)
}

/// Parse a `_GTK_FRAME_EXTENTS` `CARDINAL[4]` (`[left, right, top, bottom]`) into
/// the `(left, top)` shadow inset. `None` when fewer than 4 values (property
/// absent or malformed). Split out from [`gtk_frame_extents`] so the index
/// mapping (left = `[0]`, top = `[2]`, *not* `[1]`/`[3]`) is unit-tested without
/// an X server.
fn parse_gtk_frame_extents(vals: &[u32]) -> Option<(i32, i32)> {
    if vals.len() < 4 {
        return None;
    }
    Some((vals[0] as i32, vals[2] as i32))
}

/// Additive screen-coordinate offset that turns an element's
/// `CoordType::Window` extents into true screen coordinates:
/// `screen = x11_window_origin + _GTK_FRAME_EXTENTS.(left,top) + window_xy`.
///
/// AT-SPI `CoordType::Window` coords are relative to the toolkit's *content*
/// toplevel. The content's screen position is the X11 window's root-relative
/// origin plus the GTK4 CSD shadow inset (`_GTK_FRAME_EXTENTS`). This is
/// deterministic and replaces the old frame-(0,0)-detection heuristic; it fixes
/// GTK4, whose `CoordType::Screen` collapses *every* element to (0,0) so a
/// constant offset could never separate them (GNOME/gtk a11y rework, issues
/// #1564 / #1739) — `CoordType::Window` returns the distinct per-widget offsets
/// instead.
///
/// **Gated on `_GTK_FRAME_EXTENTS` presence**: only GTK toolkits set that
/// property (for CSD), and only GTK's Screen extents are unreliable. Non-GTK
/// toolkits (Qt, etc.) have no such property *and* report correct Screen
/// extents, so we return `None` for them — callers keep the unchanged Screen
/// path and the WINDOW reconstruction can never regress a toolkit that was
/// already correct. Also returns `None` on native Wayland (clients may not
/// query screen origins, by design) or when no X11 window resolves.
fn window_to_screen_offset(pid: u32, xid: u64, title: Option<&str>) -> Option<(i32, i32)> {
    if crate::wayland::is_wayland() {
        if crate::wayland::hyprland::is_session() {
            // Never promote toolkit-local coordinates or a stale PID origin
            // to screen coordinates when native target geometry is missing.
            let window = if xid != 0 {
                crate::wayland::hyprland::window_for_address(xid)
            } else {
                crate::wayland::hyprland::window_for_pid(pid)
            }?;
            return (window.pid == pid).then_some((window.x, window.y));
        }
        // Native Wayland: clients can't query a window's screen origin, and
        // AT-SPI CoordType::Screen collapses to (0,0) on Mutter. The bundled
        // `org.pincer.WinRects` GNOME Shell extension supplies the window's screen
        // origin (`meta_window.get_frame_rect()`); combined with the per-widget
        // CoordType::Window coords (which GTK4 reports correctly on Wayland too)
        // this reconstructs real screen coords — the GNOME analogue of the X11
        // `_GTK_FRAME_EXTENTS` path below. `None` (no extension) keeps the
        // legacy Screen path (still (0,0), but no worse than before).
        let authoritative = authoritative_wayland_origin(pid, xid, title);
        // AT-SPI discovery can only guess a native Wayland origin when the
        // compositor exposes no geometry. Keep that observation as the final
        // fallback so stale default placement cannot override Sway IPC or a
        // shell helper's authoritative frame.
        return prefer_authoritative_wayland_origin(
            authoritative,
            crate::wayland::observed_window_origin(pid),
        );
    }
    // Resolve a usable window xid. `xid == 0` means "no hint" (get_element_bounds
    // has no window context); fall back to this pid's first window — the same
    // convention resolve_element_local_coords uses. Guard the 0 case explicitly:
    // x11_window_origin(0) would resolve the *root* window to (0,0), not None.
    let win_xid = if xid != 0 {
        xid
    } else {
        crate::x11::list_windows(Some(pid)).first().map(|w| w.xid)?
    };
    // `?` here is the GTK gate: no _GTK_FRAME_EXTENTS → non-GTK toolkit → keep
    // the legacy Screen path (which those toolkits report correctly).
    let (fl, ft) = gtk_frame_extents(win_xid)?;
    let (ox, oy) = x11_window_origin(win_xid)?;
    Some((ox + fl, oy + ft))
}

/// Resolve a native Wayland window origin from compositor-owned metadata.
/// These sources are authoritative over AT-SPI's observed frame location,
/// which can remain at a toolkit default after Sway places the real window.
fn authoritative_wayland_origin(pid: u32, xid: u64, title: Option<&str>) -> Option<(i32, i32)> {
    if !crate::wayland::is_wayland() {
        return None;
    }
    crate::wayland::inject_accessibility_offset(pid)
        .or_else(|| crate::wayland::sway_ipc::window_origin_for_pid(pid))
        .or_else(|| {
            (xid != 0)
                .then(|| crate::wayland::sway_ipc::window_for_id(xid))
                .flatten()
                .map(|window| (window.x, window.y))
        })
        .or_else(|| {
            (xid != 0)
                .then(|| {
                    crate::wayland::sway_ipc::list_windows().and_then(|_| {
                        crate::wayland::window_geometry(xid)
                            .map(|(window_x, window_y, _, _)| (window_x, window_y))
                    })
                })
                .flatten()
        })
        .or_else(|| crate::wayland::shell_helper::window_origin_for_pid(pid))
        .or_else(|| title.and_then(crate::wayland::sway_ipc::window_origin_for_title))
}

fn prefer_authoritative_wayland_origin(
    authoritative: Option<(i32, i32)>,
    observed: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    authoritative.or(observed)
}

fn combine_wayland_content_offsets(
    compositor: Option<(i32, i32)>,
    document: Option<(i32, i32)>,
    document_is_separate: bool,
) -> Option<(i32, i32)> {
    if !document_is_separate {
        // Chromium descendants' CoordType::Window extents are already rooted
        // below the document accessible. Adding that document's own (x,y)
        // double-counts its renderer inset. WebKitGTK exports page content on
        // a distinct WebProcess bus, so only that bridge needs the extra hop.
        return compositor;
    }
    match (compositor, document) {
        (Some((cx, cy)), Some((dx, dy))) => Some((cx + dx, cy + dy)),
        (Some(offset), None) | (None, Some(offset)) => Some(offset),
        (None, None) => None,
    }
}

fn hyprland_document_top_inset(
    target: (u64, u32),
    window: Option<(u64, u32, u32, u32)>,
    document: (i32, i32, i32, i32),
) -> Option<i32> {
    let (address, pid, window_width, window_height) = window?;
    let (x, y, width, height) = document;
    if target.0 == 0 || target != (address, pid) || x != 0 || y < 0 || width <= 0 || height <= 0 {
        return None;
    }
    let top = i64::from(window_height) - i64::from(height);
    // A full-width embedded document can omit GTK's CSD header from its
    // WINDOW origin. Reject partial panes and gaps larger than a title bar.
    if (i64::from(window_width) - i64::from(width)).abs() > 4 || !(1..=128).contains(&top) {
        return None;
    }
    Some(y.max(top as i32))
}

fn select_web_document<T>(
    candidates: impl Iterator<Item = (T, usize, bool)>,
    require_web_process_bus: bool,
) -> Option<T> {
    candidates
        .filter(|(_, _, on_web_process_bus)| !require_web_process_bus || *on_web_process_bus)
        .min_by_key(|(_, depth, _)| *depth)
        .map(|(document, _, _)| document)
}

/// Offset of embedded web content inside a captured Wayland toplevel.
/// Compositor decorations and toolkit document offsets are independent and
/// therefore additive: choosing one or the other leaves WebKit controls one
/// title bar away from the pixels shown to the caller.
async fn web_document_origin_for_visited(
    visited: &[Visited<'_>],
    pid: u32,
    xid: u64,
    scoped_frame: Option<usize>,
) -> Option<(i32, i32)> {
    if !crate::wayland::is_wayland() {
        return None;
    }
    let sway_window = crate::wayland::sway_ipc::window_for_pid(pid);
    let compositor = sway_window
        .as_ref()
        .map(|window| (window.content_x, window.content_y));
    let document_is_separate = visited.iter().any(|node| {
        scoped_frame.is_none_or(|scope| node.frame_ordinal == scope) && node.on_web_process_bus
    });
    // Only a correlated AX frame proves these document dimensions belong to
    // the named client; an application-wide walk could select a sibling.
    let hyprland_window = if document_is_separate
        && scoped_frame.is_some()
        && xid != 0
        && crate::wayland::hyprland::is_session()
    {
        crate::wayland::hyprland::accessibility_window(xid, pid)
            .map(|window| (window.address, window.pid, window.width, window.height))
    } else {
        None
    };
    // Hyprland's inferred inset must use dimensions from the WebProcess whose
    // descendants receive that inset, not a shallower application-bus document.
    let document = select_web_document(
        visited
            .iter()
            .filter(|node| scoped_frame.is_none_or(|scope| node.frame_ordinal == scope))
            .filter(|node| node.has_component)
            .filter(|node| is_document_role(&node.role) || node.in_web_doc)
            .map(|node| (node, node.depth, node.on_web_process_bus)),
        hyprland_window.is_some(),
    );
    let document = if let Some(document) = document {
        match call(document.acc.proxies()).await {
            Some(Ok(proxies)) => match call(proxies.component()).await {
                Some(Ok(component)) => match call(component.get_extents(CoordType::Window)).await {
                    Some(Ok((x, y, width, height))) if x >= 0 && y >= 0 => {
                        dlog!(
                            "Wayland web document extents: target={xid:#x} pid={pid} document=({x},{y},{width},{height}) hyprland={hyprland_window:?}"
                        );
                        let inferred_top = match (compositor, sway_window.as_ref()) {
                            (Some((_, 0)), Some(window))
                                if width > 0
                                    && height > 0
                                    && (i64::from(window.width) - i64::from(width)).abs() <= 4
                                    && i64::from(window.height) > i64::from(height) =>
                            {
                                (i64::from(window.height) - i64::from(height))
                                    .min(i64::from(i32::MAX)) as i32
                            }
                            _ => 0,
                        };
                        let top = hyprland_document_top_inset(
                            (xid, pid),
                            hyprland_window,
                            (x, y, width, height),
                        )
                        .unwrap_or(y.max(inferred_top));
                        Some((x, top))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let combined = combine_wayland_content_offsets(compositor, document, document_is_separate);
    dlog!(
        "Wayland web content offset: compositor={compositor:?} document={document:?} separate_process={document_is_separate} combined={combined:?}"
    );
    combined
}

fn screen_extent_rebase(
    x11_origin: (i32, i32),
    accessible_frame_extents: (i32, i32, i32, i32),
    top_level: Option<(i32, i32, i32, i32)>,
) -> Option<(i32, i32)> {
    if !crate::snapshot_queries::plausible_raw_extents(accessible_frame_extents) {
        return None;
    }
    let (frame_x, frame_y, _, _) = accessible_frame_extents;
    // Chromium's broken "Screen" provider is rooted at the renderer-local
    // origin. A legitimate screen provider may differ from the X11 client
    // origin by title-bar/CSD extents; rebasing that small decoration delta
    // would move otherwise-correct GTK coordinates off their controls.
    if top_level == Some(accessible_frame_extents) {
        return None;
    }
    if frame_x.abs() <= 2 && frame_y.abs() <= 2 {
        Some((x11_origin.0 - frame_x, x11_origin.1 - frame_y))
    } else {
        None
    }
}

fn rebase_renderer_window_offset(
    mut offset: (i32, i32),
    frame_origin: Option<(i32, i32)>,
) -> (i32, i32) {
    if let Some((frame_x, frame_y)) = frame_origin {
        // Chromium may expose a negative renderer-local frame origin. Rebase
        // that shape, but keep positive content insets: its descendants are
        // already relative to the content origin and subtracting the inset
        // moves first-row controls above the captured Wayland window.
        if frame_x < 0 {
            offset.0 = offset.0.saturating_sub(frame_x);
        }
        if frame_y < 0 {
            offset.1 = offset.1.saturating_sub(frame_y);
        }
    }
    offset
}

fn project_screen_extents(
    (x, y, w, h): (i32, i32, i32, i32),
    (offset_x, offset_y): (i32, i32),
    document_origin: Option<(i32, i32)>,
) -> Option<(i32, i32, u32, u32)> {
    // Reject unrealized-widget sentinels before applying any offsets.
    if !crate::snapshot_queries::plausible_raw_extents((x, y, w, h)) {
        return None;
    }
    let (document_x, document_y) = document_origin.unwrap_or((0, 0));
    Some((
        x + offset_x + document_x,
        y + offset_y + document_y,
        w as u32,
        h as u32,
    ))
}

fn scoped_component_nodes<'a, T>(
    nodes: &'a [T],
    scoped_frame: Option<usize>,
    component: impl Fn(&T) -> (usize, bool) + 'a,
) -> impl Iterator<Item = (usize, &'a T)> + 'a {
    // Filter after enumeration: public element indices belong to the whole app.
    nodes.iter().enumerate().filter(move |(_, node)| {
        let (frame, has_component) = component(node);
        has_component && scoped_frame.is_none_or(|scope| scope == frame)
    })
}

/// Screen-coordinate bounds for the exact visited sequence rendered into the
/// current snapshot. Nodes without a usable Component interface, or whose
/// extents query fails/times out, are omitted rather than borrowing another
/// live traversal's ordinal.
///
/// GTK4 caveat: GTK4's AT-SPI bridge returns `GetExtents(Screen)` as `(0,0)`
/// for every element (issue #1564 / the #1739 a11y rework), so a screen query
/// is useless. Instead we query `CoordType::Window` (which GTK4 *does* report
/// correctly, per-widget) and add a deterministic screen offset — the X11
/// window origin plus the GTK4 CSD shadow inset from `_GTK_FRAME_EXTENTS` (see
/// [`window_to_screen_offset`]). For GTK3/Qt the inset is absent, so the
/// offset is just the X11 origin and the result matches the old screen path.
///
/// Returns `(element_index, x, y, width, height)` tuples.
async fn element_bounds_for_visited(
    visited: &[Visited<'_>],
    pid: u32,
    xid: u64,
    scoped_frame: Option<usize>,
) -> (Vec<(usize, i32, i32, u32, u32)>, bool) {
    // Query WINDOW-relative extents and add a deterministic screen offset
    // (X11 window origin + GTK4 CSD inset). This fixes GTK4 — whose
    // CoordType::Screen reports every element at (0,0) — by using the
    // distinct per-widget WINDOW coords instead. On Wayland / when no X11
    // window resolves, `offset` is None and we keep the legacy Screen path
    // so non-X11 behaviour is unchanged.
    let window_title = visited.iter().find_map(|node| {
        (scoped_frame.is_none_or(|scope| node.frame_ordinal == scope) && is_window_role(&node.role))
            .then_some(node.name.as_str())
    });
    // Wayland's `window_to_screen_offset` path never touches X11 (it reads
    // compositor IPC / AT-SPI state instead), so only the X11 branch risks an
    // un-timed blocking round-trip; still route both through `bounded_blocking`
    // so this single call site can't stall the walk past its budget (#42).
    let window_title_owned = window_title.map(str::to_owned);
    let offset =
        bounded_blocking(move || window_to_screen_offset(pid, xid, window_title_owned.as_deref()))
            .await
            .flatten();
    if crate::wayland::is_wayland()
        && crate::wayland::hyprland::is_session()
        && (offset.is_none() || scoped_frame.is_none())
    {
        return (Vec::new(), true);
    }
    let coord = if offset.is_some() {
        CoordType::Window
    } else {
        CoordType::Screen
    };
    let display = if crate::wayland::is_wayland() {
        None
    } else {
        bounded_blocking(x11_display_size).await.flatten()
    };
    // Chromium on X11 labels its component extents as Screen while
    // returning coordinates relative to the renderer frame. Rebase
    // those values by comparing the top-level accessible frame with
    // the actual X11 window origin. Correct screen-coordinate providers
    // produce a zero delta; Chromium's local (0,0) frame produces the
    // required window-origin delta. GTK's explicit Window-coordinate
    // path above remains authoritative when available.
    let screen_rebase = if offset.is_none() && !crate::wayland::is_wayland() && xid != 0 {
        let x11_origin = bounded_blocking(move || x11_window_origin(xid))
            .await
            .flatten();
        let top_level = bounded_blocking(move || x11_top_level_geometry(xid))
            .await
            .flatten();
        let frame = visited.iter().find(|node| {
            scoped_frame.is_none_or(|scope| node.frame_ordinal == scope)
                && node.has_component
                && is_window_role(&node.role)
        });
        if let (Some(origin), Some(frame)) = (x11_origin, frame) {
            let accessible_frame = match call(frame.acc.proxies()).await {
                Some(Ok(proxies)) => match call(proxies.component()).await {
                    Some(Ok(component)) => {
                        match call(component.get_extents(CoordType::Screen)).await {
                            Some(Ok(extents)) => Some(extents),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            };
            accessible_frame
                .and_then(|frame_extents| screen_extent_rebase(origin, frame_extents, top_level))
        } else {
            None
        }
    } else {
        None
    };
    // Renderer bridges can expose Window coordinates relative to an internal
    // frame whose origin is not (0,0) (Chromium commonly reports a negative
    // title-bar offset). Normalize that frame to the compositor window origin
    // before adding the screen offset. Native GTK reports (0,0), so this is a
    // no-op there.
    let window_frame_origin = if offset.is_some() {
        let frame = visited.iter().find(|node| {
            scoped_frame.is_none_or(|scope| node.frame_ordinal == scope)
                && node.has_component
                && is_window_role(&node.role)
        });
        if let Some(frame) = frame {
            match call(frame.acc.proxies()).await {
                Some(Ok(proxies)) => match call(proxies.component()).await {
                    Some(Ok(component)) => {
                        match call(component.get_extents(CoordType::Window)).await {
                            Some(Ok((x, y, _, _))) => Some((x, y)),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    // Add compositor decorations and the embedded document origin for web
    // descendants only. Electron commonly contributes zero for both; WebKitGTK
    // under Sway needs the sum.
    let web_document_origin = if offset.is_some() {
        web_document_origin_for_visited(visited, pid, xid, scoped_frame).await
    } else {
        None
    };
    let (offset_x, offset_y) = rebase_renderer_window_offset(
        offset.or(screen_rebase).unwrap_or((0, 0)),
        window_frame_origin,
    );
    if let Some((ox, oy)) = offset {
        dlog!("element bounds: WINDOW coords + screen offset ({ox},{oy})");
    } else if let Some((ox, oy)) = screen_rebase {
        dlog!("element bounds: SCREEN coords + X11 frame rebase ({ox},{oy})");
    }

    let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
    // Hard wall-clock budget for the whole collection: on pathological
    // trees individual D-Bus calls each burn up to CALL_TIMEOUT (geany's
    // unrealized nodes did exactly that). Return whatever was collected
    // in time, but do not impose an index-based node cap: a cap silently
    // stripped frames from valid controls later in renderer trees and
    // made PX targeting depend on DOM order.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20).min(remaining_budget());
    // Bounds are independent read-only queries. Overlap a bounded number of
    // calls instead of serializing thousands of unrealized menu components.
    // Preserve original indices, all extents checks, and per-call timeouts.
    let origin_attested = bounded_blocking(move || native_wayland_origin_attested(pid, xid))
        .await
        .unwrap_or(false);
    let queries = scoped_component_nodes(&action_nodes, scoped_frame, |node| {
        (node.frame_ordinal, node.has_component)
    })
    .map(|(idx, node)| async move {
        let proxies = call(node.acc.proxies()).await?.ok()?;
        let comp = call(proxies.component()).await?.ok()?;
        // A trustworthy Screen answer wins over the Window reconstruction
        // (see `screen_extents_trusted`); web content keeps its document
        // origin path.
        if coord == CoordType::Window && !node.in_web_doc {
            if let Some(Ok(raw)) = call(comp.get_extents(CoordType::Screen)).await {
                if screen_answer_overrides_window(raw, display, origin_attested) {
                    return project_screen_extents(raw, (0, 0), None).map(|bounds| (idx, bounds));
                }
            }
        }
        if let Some(Ok((x, y, w, h))) = call(comp.get_extents(coord)).await {
            let document_origin = if node.in_web_doc {
                web_document_origin
            } else {
                None
            };
            return project_screen_extents((x, y, w, h), (offset_x, offset_y), document_origin)
                .map(|bounds| (idx, bounds));
        }
        None
    });
    let collected = crate::snapshot_queries::collect_indexed(queries, deadline).await;
    let complete = tokio::time::Instant::now() < deadline;
    if !complete {
        dlog!(
            "snapshot bounds: time budget exhausted; returning {} bound(s)",
            collected.len()
        );
    }
    (
        collected
            .into_iter()
            .map(|(idx, (x, y, w, h))| (idx, x, y, w, h))
            .collect(),
        complete,
    )
}

#[cfg(test)]
mod walk_bounds_tests {
    use super::walk_bounds_usable;

    #[test]
    fn degenerate_window_origin_is_rejected() {
        // A closed menu item projects (0,0,0,0) onto the window origin.
        assert_eq!(
            walk_bounds_usable(Some((40, 60, 0, 0)), Some((1920, 1080))),
            None
        );
        assert_eq!(walk_bounds_usable(None, Some((1920, 1080))), None);
    }

    #[test]
    fn off_screen_bounds_are_rejected() {
        assert_eq!(
            walk_bounds_usable(Some((2000, 10, 50, 20)), Some((1920, 1080))),
            None
        );
        assert_eq!(
            walk_bounds_usable(Some((-80, 10, 50, 20)), Some((1920, 1080))),
            None
        );
    }

    #[test]
    fn real_bounds_pass_through() {
        assert_eq!(
            walk_bounds_usable(Some((100, 200, 80, 24)), Some((1920, 1080))),
            Some((100, 200, 80, 24))
        );
        assert_eq!(
            walk_bounds_usable(Some((100, 200, 80, 24)), None),
            Some((100, 200, 80, 24))
        );
    }
}

#[cfg(test)]
mod screen_extents_tests {
    use super::screen_extents_trusted;

    #[test]
    fn trusts_plausible_on_screen_extents() {
        assert!(screen_extents_trusted(
            (70, 110, 848, 433),
            Some((1920, 1080))
        ));
        assert!(screen_extents_trusted(
            (76, 1045, 43, 32),
            Some((1920, 1080))
        ));
    }

    #[test]
    fn rejects_sentinels_and_off_screen_answers() {
        // GTK4 (newer a11y) collapses every element to the origin.
        assert!(!screen_extents_trusted((0, 0, 100, 20), Some((1920, 1080))));
        // Unrealized widgets report INT_MIN.
        assert!(!screen_extents_trusted(
            (i32::MIN, i32::MIN, 1, 1),
            Some((1920, 1080))
        ));
        // Entirely off the display.
        assert!(!screen_extents_trusted(
            (2000, 10, 40, 20),
            Some((1920, 1080))
        ));
        assert!(!screen_extents_trusted(
            (-500, 10, 40, 20),
            Some((1920, 1080))
        ));
    }
}

#[cfg(test)]
mod screen_override_tests {
    use super::{project_screen_extents, screen_answer_overrides_window};

    #[test]
    fn attested_origin_rejects_window_local_screen_answer() {
        // GTK3 on Hyprland reports window-local (12,211) as Screen; with no
        // display bounds it would otherwise pass `screen_extents_trusted`.
        let local = (12, 211, 917, 34);
        assert!(screen_answer_overrides_window(local, None, false));
        assert!(!screen_answer_overrides_window(local, None, true));
        // Window extents plus the attested nonzero origin give screen space.
        assert_eq!(
            project_screen_extents(local, (955, 349), None),
            Some((967, 560, 917, 34))
        );
    }

    #[test]
    fn xwayland_or_unattested_screen_answer_wins() {
        // XWayland GTK/LibreOffice report true screen extents; without an
        // exact native-client attestation they must not be discarded. A zero
        // address never attests, without consulting the compositor.
        assert!(!super::native_wayland_origin_attested(1, 0));
        assert!(super::screen_answer_overrides_window(
            (967, 560, 917, 34),
            None,
            super::native_wayland_origin_attested(1, 0)
        ));
    }

    #[test]
    fn native_attested_window_local_screen_answer_loses_to_window_extents() {
        assert!(!super::screen_answer_overrides_window(
            (10, 20, 800, 600),
            None,
            true
        ));
    }

    #[test]
    fn x11_screen_answer_still_wins_with_display_bounds() {
        let screen = (70, 110, 848, 433);
        assert!(screen_answer_overrides_window(
            screen,
            Some((1920, 1080)),
            false
        ));
    }

    #[test]
    fn invalid_screen_answers_never_override() {
        for attested in [false, true] {
            assert!(!screen_answer_overrides_window(
                (i32::MIN, i32::MIN, 1, 1),
                None,
                attested
            ));
            assert!(!screen_answer_overrides_window(
                (0, 0, 100, 20),
                None,
                attested
            ));
        }
        assert_eq!(
            project_screen_extents((i32::MIN, i32::MIN, 1, 1), (955, 349), None),
            None
        );
    }

    #[test]
    fn web_document_origin_is_added_after_window_offset() {
        assert_eq!(
            project_screen_extents((10, 20, 30, 40), (955, 349), Some((0, 80))),
            Some((965, 449, 30, 40))
        );
    }
}

#[cfg(test)]
mod stacking_z_index_tests {
    use super::z_index_from_stacking;
    use crate::x11::WindowInfo;

    fn window(pid: u32, title: &str, z: usize) -> WindowInfo {
        WindowInfo {
            xid: 100 + z as u64,
            pid: Some(pid),
            app_name: "app".into(),
            title: title.into(),
            is_on_screen: true,
            z_index: Some(z),
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }
    }

    #[test]
    fn frames_take_the_stacking_position_of_the_same_pid_and_title() {
        let stacking = vec![
            window(7, "GNU Image Manipulation Program", 2),
            window(9, "Other", 3),
            window(7, "Brightness-Contrast", 4),
        ];
        assert_eq!(
            z_index_from_stacking(&stacking, 7, "Brightness-Contrast"),
            Some(4)
        );
        assert_eq!(
            z_index_from_stacking(&stacking, 7, "GNU Image Manipulation Program"),
            Some(2)
        );
        // Unknown title with several candidates: no guess.
        assert_eq!(z_index_from_stacking(&stacking, 7, "Untitled"), None);
        // A pid with exactly one X toplevel maps regardless of the title.
        assert_eq!(z_index_from_stacking(&stacking, 9, "anything"), Some(3));
        assert_eq!(z_index_from_stacking(&stacking, 11, "anything"), None);
        assert_eq!(z_index_from_stacking(&[], 7, "anything"), None);
    }
}

#[cfg(test)]
mod window_walk_tests {
    use super::*;

    #[test]
    fn a_closed_window_has_no_index_space_left() {
        use crate::x11::WindowPresence;
        // A destroyed dialog or a closed popup: an index walk of it is
        // refused instead of numbering another window's elements.
        assert!(window_index_space_is_gone(Some(WindowPresence::Gone)));
        assert!(window_index_space_is_gone(Some(
            WindowPresence::ClosedPopup
        )));
        // A window that still exists (a minimized one is unmapped) keeps it.
        assert!(!window_index_space_is_gone(Some(WindowPresence::Exists)));
        // No answer from the server: unchanged.
        assert!(!window_index_space_is_gone(None));
    }

    #[test]
    fn a_missing_option_lists_twenty_options_and_counts_the_rest() {
        let options: Vec<String> = (1..=23).map(|n| format!("Row {n}")).collect();
        let missing = NoSuchOption {
            value: "Row 99".into(),
            options,
        };
        let (shown, more) = missing.listed();
        assert_eq!(shown.len(), OPTIONS_LISTED);
        assert_eq!(more, 3);
        let text = missing.to_string();
        assert!(text.starts_with(NO_SUCH_OPTION), "{text}");
        assert!(text.ends_with("\"Row 20\"] and 3 more"), "{text}");
    }

    #[test]
    fn a_resolved_window_is_walked_alone_unless_the_caller_wants_every_window() {
        let seeds = ["main", "import", "warning"];
        // The message box (ordinal 2): only its own subtree.
        assert_eq!(
            walk_seeds(&seeds, Some(2), None, None, false),
            vec![("warning", 0, 2)]
        );
        // invoke_menu: the window first, then the others in their own order.
        assert_eq!(
            walk_seeds(&seeds, Some(2), None, None, true),
            vec![("warning", 0, 2), ("main", 0, 0), ("import", 0, 1)]
        );
        // Unresolved: the whole application, as before.
        assert_eq!(
            walk_seeds(&seeds, None, None, None, false),
            vec![("main", 0, 0), ("import", 0, 1), ("warning", 0, 2)]
        );
    }

    #[test]
    fn a_qt_popup_walks_only_its_item_container() {
        let seeds = ["main", "import"];
        assert_eq!(
            walk_seeds(&seeds, Some(0), Some("combo list"), None, false),
            vec![("combo list", 0, 0)]
        );
    }

    #[test]
    fn a_role_search_visits_the_top_level_then_its_matches_one_level_below() {
        let seeds = ["main", "import"];
        let matches = Some(vec!["Save", "Cancel"]);
        let expected = vec![("import", 0, 1), ("Save", 1, 1), ("Cancel", 1, 1)];
        assert_eq!(
            walk_seeds(&seeds, Some(1), None, matches.clone(), false),
            expected
        );
        assert_eq!(walk_seeds(&seeds, Some(1), None, matches, true), expected);
    }

    fn roles(query: &str) -> Vec<atspi::Role> {
        role_search_for(query)
            .map(|search| {
                search
                    .roles
                    .into_iter()
                    .map(|role| atspi::Role::try_from(role).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_query_of_role_words_names_roles_and_anything_else_does_not() {
        let buttons = roles("button");
        assert!(buttons.contains(&atspi::Role::Button));
        assert!(buttons.contains(&atspi::Role::ToggleButton));
        assert!(buttons.contains(&atspi::Role::RadioButton));
        assert!(!buttons.contains(&atspi::Role::Label));
        assert_eq!(roles("combo"), vec![atspi::Role::ComboBox]);
        assert_eq!(roles("  Combo   BOX "), vec![atspi::Role::ComboBox]);
        // Qt's own role names count too.
        assert_eq!(
            roles("push button"),
            vec![atspi::Role::Button, atspi::Role::PushButtonMenu]
        );
        assert!(roles("cell").contains(&atspi::Role::TableCell));
        assert_eq!(roles("alert message"), vec![atspi::Role::Alert]);
        // Names, fragments of words and empty queries filter a walk instead.
        assert!(role_search_for("Save").is_none());
        assert!(role_search_for("butt").is_none());
        assert!(role_search_for("e").is_none());
        assert!(role_search_for("  ").is_none());
        assert!(role_search_for("button \"OK\"").is_none());
    }

    #[test]
    fn match_rule_sets_have_libatspis_wire_form() {
        // dbus-monitor of libatspi's GetMatches for push button (43) and for
        // combo box (11): word 1 = 2048 and word 0 = 2048, five words.
        let push_button = atspi::Role::Button as u32;
        let combo_box = atspi::Role::ComboBox as u32;
        assert_eq!((push_button, combo_box), (43, 11));
        assert_eq!(role_bitset(&[push_button]), vec![0, 2048, 0, 0, 0]);
        assert_eq!(role_bitset(&[combo_box]), vec![2048, 0, 0, 0, 0]);
        assert_eq!(role_bitset(&[129]), vec![0, 0, 0, 0, 2]);
        // ATSPI_STATE_SHOWING is 25 and ATSPI_STATE_VISIBLE 30.
        assert_eq!(
            state_bitset(StateSet::new(State::Showing)),
            vec![1 << 25, 0]
        );
        assert_eq!(
            state_bitset(StateSet::new(State::Showing | State::Visible)),
            vec![(1 << 25) | (1 << 30), 0]
        );
    }

    #[test]
    fn the_popup_container_is_the_one_that_fills_the_popup() {
        // Qt 6.12 on the probe: the combo popup was 126x290 at 785,340 and
        // its list 124x288 at 786,341; the combo box itself sits at the top.
        let popup = (785, 340, 126, 290);
        let combo_box = (785, 340, 126, 37);
        let list = (786, 341, 124, 288);
        let table = (700, 300, 900, 600);
        assert_eq!(
            pick_popup_container(&[combo_box, table, list], popup),
            Some(2)
        );
        // An exact match beats a near one.
        assert_eq!(pick_popup_container(&[list, popup], popup), Some(1));
        // Nothing fills it: no container, so the read walks the application.
        assert_eq!(pick_popup_container(&[combo_box, table], popup), None);
        assert_eq!(pick_popup_container(&[], popup), None);
    }
}

#[cfg(test)]
mod frame_correlation_tests {
    use super::{
        correlate_frame_to_window, is_window_role, native_twins, FRAME_MATCH_TOLERANCE_PX,
    };
    use crate::x11::WindowInfo;

    fn window(x: i32, y: i32, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            xid: 4242,
            pid: Some(99),
            app_name: "Google-chrome".to_owned(),
            title: "Pincer - Google Chrome".to_owned(),
            is_on_screen: true,
            z_index: Some(3),
            x,
            y,
            width,
            height,
        }
    }

    /// The configuration that made the existing-profile route unreachable: one
    /// browser process publishing three windows.
    #[test]
    fn picks_the_frame_matching_the_named_window_among_siblings() {
        let candidates = [
            (0usize, (144, 51, 1244, 953)),
            (1, (438, 80, 1050, 953)),
            (2, (550, 225, 500, 584)),
        ];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 0),
            Some(1)
        );
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(550, 225, 500, 584), 0),
            Some(2)
        );
    }

    /// Server-side decorations shift a frame's reported origin; a small offset
    /// must still resolve rather than fall back to an application-wide walk.
    #[test]
    fn tolerates_decoration_offsets() {
        let candidates = [(0usize, (440, 108, 1050, 925)), (1, (144, 51, 1244, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 0),
            Some(0)
        );
    }

    /// Two windows of the same geometry cannot be told apart this way, and the
    /// caller needs a refusal rather than a coin flip.
    #[test]
    fn refuses_when_two_frames_are_equally_plausible() {
        // Two real windows stacked exactly: each is the other's native twin.
        let candidates = [(0usize, (438, 80, 1050, 953)), (1, (438, 80, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 1),
            None
        );
        // Close but not identical frames are two windows, twins or not.
        let near = [(0usize, (438, 80, 1050, 953)), (1, (440, 84, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&near, &window(438, 80, 1050, 953), 0),
            None
        );
    }

    /// Qt publishes a QMessageBox as two "alert message" accessibles with the
    /// same extents (the probe's "Probe warning", 452x113 at 1046,597, next to
    /// the main frame and the Import dialog). With one native window of that
    /// geometry they are the same window: the first one is taken.
    #[test]
    fn a_window_published_twice_resolves_to_its_first_frame() {
        let candidates = [
            (0usize, (680, 297, 1200, 800)),
            (1, (1016, 545, 520, 260)),
            (2, (1046, 597, 452, 113)),
            (3, (1046, 597, 452, 113)),
        ];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(1046, 597, 452, 113), 0),
            Some(2)
        );
        let message_box = window(1046, 597, 452, 113);
        let import = WindowInfo {
            xid: 4243,
            ..window(1016, 545, 520, 260)
        };
        let twin = WindowInfo {
            xid: 4244,
            ..window(1046, 597, 452, 113)
        };
        assert_eq!(
            native_twins(&message_box, &[message_box.clone(), import.clone()]),
            0
        );
        assert_eq!(
            native_twins(&message_box, &[message_box.clone(), import, twin]),
            1
        );
    }

    #[test]
    fn qt_message_boxes_are_window_roles() {
        for role in [
            "frame",
            "window",
            "dialog",
            "alert",
            "alert message",
            "Alert Message",
            "file chooser",
        ] {
            assert!(is_window_role(role), "{role}");
        }
        for role in ["menu", "popup menu", "panel", "push button"] {
            assert!(!is_window_role(role), "{role}");
        }
    }

    #[test]
    fn refuses_when_no_frame_is_close_enough() {
        let candidates = [(0usize, (0, 0, 200, 200))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 0),
            None
        );
    }

    #[test]
    fn refuses_when_the_application_publishes_no_frame_extents() {
        assert_eq!(
            correlate_frame_to_window(&[], &window(438, 80, 1050, 953), 0),
            None
        );
    }

    /// Zero-area extents are what a frame reports before it has been mapped;
    /// they must never be treated as a match for a real window.
    #[test]
    fn ignores_frames_without_usable_extents() {
        let candidates = [(0usize, (0, 0, 0, 0)), (1, (438, 80, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 0),
            Some(1)
        );
    }

    /// Being the only candidate is not evidence of correspondence. (The walk
    /// does short-circuit a genuinely single-top-level application before it
    /// reaches this function — see `resolve_window_frame`.)
    #[test]
    fn a_sole_candidate_still_has_to_be_close_enough() {
        let far_away = i32::try_from(FRAME_MATCH_TOLERANCE_PX).unwrap() + 500;
        let candidates = [(0usize, (far_away, far_away, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953), 0),
            None
        );
    }
}

#[cfg(test)]
mod coord_tests {
    use super::parse_gtk_frame_extents;
    use super::OP_DEADLINE;
    use super::{
        activation_index, before_snapshot_deadline, bounded_blocking,
        combine_wayland_content_offsets, counts_as_showing, counts_in_menu_lineage,
        hyprland_document_top_inset, is_activation_action, is_enabled_state,
        is_indexable_capabilities, is_passive_role, is_showing_state, is_web_process_bus,
        listing_state, menu_step, one_line, passive_marker, prefer_authoritative_wayland_origin,
        project_screen_extents, rebase_renderer_window_offset, row_state_marks,
        scoped_component_nodes, screen_extent_rebase, select_click_target, select_web_document,
        value_part, ApplicationSelection, MenuStep,
    };
    use atspi::{State, StateSet};
    use std::time::Duration;

    #[test]
    fn duplicate_pid_prefers_populated_application_after_empty_registration() {
        let target_pid = 4242;
        let candidates = [
            (Some(9000), "other-process", true),
            (Some(target_pid), "empty-root", false),
            (Some(target_pid), "live-tree", true),
        ];
        let mut selection = ApplicationSelection::new(target_pid);
        for (pid, app, has_children) in candidates {
            if selection.matches_pid(pid) {
                selection.consider_matching(app, has_children);
            }
        }

        assert_eq!(selection.into_selected(), Ok(Some("live-tree")));
    }

    #[test]
    fn childless_exact_pid_application_remains_the_fallback() {
        let mut selection = ApplicationSelection::new(4242);
        selection.consider_matching("first-empty", false);
        selection.consider_matching("second-empty", false);

        assert_eq!(selection.into_selected(), Ok(Some("first-empty")));
    }

    #[test]
    fn multiple_populated_exact_pid_applications_are_ambiguous() {
        let mut selection = ApplicationSelection::new(4242);
        selection.consider_matching("first-live-tree", true);
        selection.consider_matching("second-live-tree", true);

        assert_eq!(selection.into_selected(), Err(2));
    }

    #[tokio::test(start_paused = true)]
    async fn one_absolute_deadline_spans_traversal_and_bounds() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);

        before_snapshot_deadline(deadline, tokio::time::sleep(Duration::from_millis(60)))
            .await
            .expect("traversal should fit the shared budget");
        before_snapshot_deadline(deadline, tokio::time::sleep(Duration::from_millis(60)))
            .await
            .expect_err("bounds must receive only the traversal's remaining budget");

        assert_eq!(tokio::time::Instant::now(), deadline);
    }

    // Regression coverage for #42: a synchronous, un-timed blocking call
    // (the shape of `window_to_screen_offset`/`x11_window_origin`'s raw X11
    // round-trips) executed directly inside the bounds phase must not be
    // able to hold the walk hostage past the operation's deadline, even
    // though the blocking work itself cannot be cancelled once started.
    #[tokio::test]
    async fn bounded_blocking_returns_none_once_the_operation_deadline_passes() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        let started = std::time::Instant::now();

        let result = OP_DEADLINE
            .scope(deadline, async {
                // Simulates a wedged X11 socket read: a plain blocking sleep
                // with no cooperative await points of its own, far longer
                // than the operation's deadline.
                bounded_blocking(|| {
                    std::thread::sleep(Duration::from_secs(2));
                    "would-be X11 reply"
                })
                .await
            })
            .await;

        assert_eq!(
            result, None,
            "bounded_blocking must give up once the deadline passes rather than \
             waiting for the blocking closure to finish"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the caller must not be held up by the still-running blocking thread; \
             took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn bounded_blocking_returns_none_immediately_once_budget_is_exhausted() {
        // A deadline already in the past means `remaining_budget()` is zero,
        // so `bounded_blocking` must refuse to even spawn the closure —
        // exercising the fast-bail branch the per-node `call()` helper shares.
        let deadline = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(1)).await;

        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in_closure = ran.clone();
        let result = OP_DEADLINE
            .scope(deadline, async {
                bounded_blocking(move || {
                    ran_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
                    42
                })
                .await
            })
            .await;

        assert_eq!(result, None);
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "an already-exhausted budget must skip the closure entirely"
        );
    }

    #[test]
    fn operable_nodes_are_addressable() {
        assert!(is_indexable_capabilities(
            "entry",
            false,
            true,
            false,
            false,
            true,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "button",
            true,
            false,
            false,
            false,
            false,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "slider",
            false,
            false,
            true,
            false,
            true,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "list item",
            false,
            false,
            false,
            true,
            true,
            Some(true)
        ));
        assert!(!is_indexable_capabilities(
            "label",
            false,
            false,
            false,
            false,
            true,
            Some(true)
        ));
        assert!(!is_indexable_capabilities(
            "button",
            true,
            false,
            false,
            false,
            true,
            Some(false)
        ));
        assert!(!is_indexable_capabilities(
            "button", true, false, false, false, true, None
        ));
        assert!(!is_indexable_capabilities(
            "label",
            true,
            false,
            false,
            false,
            true,
            Some(true)
        ));
    }

    #[test]
    fn hidden_and_disabled_controls_are_not_indexed_but_are_named() {
        // `Showing` is the on-screen state.
        assert!(!is_showing_state(&StateSet::new(
            State::Enabled | State::Visible
        )));
        assert!(is_showing_state(&StateSet::new(
            State::Enabled | State::Visible | State::Showing
        )));
        // A Visible-but-not-Showing control (scrolled out of its viewport, a
        // hover-only eject button) stays listed, marked off-screen; a hidden
        // one (not Visible) is left out.
        assert_eq!(
            listing_state(
                "push button",
                &StateSet::new(State::Enabled | State::Visible)
            ),
            (true, true)
        );
        assert_eq!(
            listing_state("push button", &StateSet::new(State::Enabled)),
            (false, false)
        );
        assert_eq!(
            listing_state(
                "push button",
                &StateSet::new(State::Enabled | State::Visible | State::Showing)
            ),
            (true, false)
        );
        // ...but a GTK2 menu item is never `Showing`, and stays indexed while `Visible`.
        assert!(!counts_as_showing(
            "push button",
            &StateSet::new(State::Enabled | State::Visible)
        ));
        assert!(counts_as_showing(
            "menu item",
            &StateSet::new(State::Enabled | State::Visible)
        ));
        assert!(counts_as_showing(
            "check menu item",
            &StateSet::new(State::Visible)
        ));
        assert!(!counts_as_showing(
            "menu item",
            &StateSet::new(State::Enabled)
        ));
        // A visible but insensitive button keeps a marker in the markdown.
        assert_eq!(
            passive_marker("push button", true, true, Some(false)),
            Some("disabled")
        );
        assert_eq!(
            passive_marker("push button", false, true, Some(false)),
            Some("disabled")
        );
        assert_eq!(passive_marker("push button", true, true, Some(true)), None);
        assert_eq!(passive_marker("label", false, true, Some(false)), None);
    }

    #[test]
    fn rows_show_the_states_a_toggle_changes() {
        let marks = |role: &str, states: &[State]| {
            let mut set = StateSet::empty();
            for state in states {
                set.insert(*state);
            }
            row_state_marks(role, &set)
        };
        // A check box says both ways, so a toggle changes its row.
        assert_eq!(marks("check box", &[State::Enabled]), vec!["unchecked"]);
        assert_eq!(
            marks("check box", &[State::Enabled, State::Checked]),
            vec!["checked"]
        );
        assert_eq!(
            marks("check box", &[State::Checkable, State::Indeterminate]),
            vec!["mixed"]
        );
        assert_eq!(
            marks("check menu item", &[State::Checkable, State::Checked]),
            vec!["checked"]
        );
        // Radio and toggle buttons say only the positive state.
        assert!(marks("radio button", &[State::Checkable]).is_empty());
        assert_eq!(
            marks("toggle button", &[State::Checkable, State::Pressed]),
            vec!["pressed"]
        );
        // Items: expanded / collapsed and selected.
        assert_eq!(
            marks("tree item", &[State::Expandable, State::Selected]),
            vec!["collapsed", "selected"]
        );
        assert_eq!(
            marks("tree item", &[State::Expandable, State::Expanded]),
            vec!["expanded"]
        );
        assert_eq!(marks("page tab", &[State::Selected]), vec!["selected"]);
        assert_eq!(
            marks("combo box", &[State::Expandable, State::Expanded]),
            vec!["expanded"]
        );
        // Plain controls and menus stay unmarked.
        assert!(marks("push button", &[State::Enabled, State::Focusable]).is_empty());
        assert!(marks("menu item", &[State::Expanded, State::Selected]).is_empty());
    }

    #[test]
    fn a_long_text_value_carries_its_real_length() {
        // KWrite's notes.txt: 24,492 characters, of which the walk reads
        // the first 4,096; the row used to say 4146 (escaped line breaks).
        let read = "line\n".repeat(819) + "x";
        assert_eq!(read.chars().count(), 4096);
        let part = value_part(&read, Some(24_492));
        assert!(
            part.ends_with("x...\" (24492 chars, the first 4096 read; full_output reads it whole)"),
            "{part}"
        );
        // Whole, but with line breaks the row escapes: the real count.
        let whole = "ab\n".repeat(100);
        assert!(
            value_part(&whole, None).ends_with("\" (300 chars)"),
            "{}",
            value_part(&whole, None)
        );
        // Short or single-line values carry no count.
        assert_eq!(value_part("a\nb", None), " value=\"a\\nb\"");
        assert_eq!(
            value_part(&"y".repeat(300), None),
            format!(" value=\"{}\"", "y".repeat(300))
        );
    }

    #[test]
    fn gtk_state_sets_establish_operability_from_enabled_or_sensitive() {
        assert!(is_enabled_state(&StateSet::new(State::Enabled)));
        assert!(is_enabled_state(&StateSet::new(State::Sensitive)));
        assert!(is_enabled_state(&StateSet::new(
            State::Enabled | State::Sensitive
        )));
        assert!(!is_enabled_state(&StateSet::empty()));
    }

    #[test]
    fn enabled_component_backed_buttons_are_pixel_addressable() {
        for role in ["button", "push button", " Button "] {
            assert!(is_indexable_capabilities(
                role,
                false,
                false,
                false,
                false,
                true,
                Some(true)
            ));
        }
    }

    #[test]
    fn component_role_fallback_rejects_unverified_or_passive_nodes() {
        for enabled in [None, Some(false)] {
            assert!(!is_indexable_capabilities(
                "button", false, false, false, false, true, enabled
            ));
        }
        assert!(!is_indexable_capabilities(
            "button",
            false,
            false,
            false,
            false,
            false,
            Some(true)
        ));
        for role in ["label", "application", "panel", "frame", "window"] {
            assert!(!is_indexable_capabilities(
                role,
                false,
                false,
                false,
                false,
                true,
                Some(true)
            ));
        }
    }

    #[test]
    fn screen_extents_are_rebased_from_accessible_frame_to_x11_origin() {
        let frame = Some((600, 50, 800, 600));
        assert_eq!(
            screen_extent_rebase((604, 80), (0, 0, 792, 566), frame),
            Some((604, 80))
        );
        assert_eq!(
            screen_extent_rebase((604, 100), (604, 80, 792, 566), frame),
            None
        );
        assert_eq!(
            screen_extent_rebase((604, 80), (604, 80, 792, 566), frame),
            None
        );
    }

    #[test]
    fn sentinel_frame_extents_are_not_a_renderer_origin() {
        assert_eq!(
            screen_extent_rebase((155, 149), (-1, -1, -1, -1), None),
            None
        );
        assert_eq!(screen_extent_rebase((155, 149), (0, 0, 0, 0), None), None);
    }

    #[test]
    fn a_renderer_local_frame_at_the_corner_keeps_its_rebase() {
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 640, 480), Some((0, 0, 650, 514))),
            Some((5, 29))
        );
        assert_eq!(
            screen_extent_rebase((5, 56), (0, 0, 640, 480), Some((0, 27, 650, 514))),
            Some((5, 56))
        );
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 640, 480), None),
            Some((5, 29))
        );
    }

    #[test]
    fn a_gtk_frame_matching_its_window_manager_frame_at_the_corner_is_not_rebased() {
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 650, 514), Some((0, 0, 650, 514))),
            None
        );
    }

    #[test]
    fn renderer_window_offset_only_rebases_negative_frame_origins() {
        assert_eq!(
            rebase_renderer_window_offset((100, 50), Some((-8, -29))),
            (108, 79)
        );
        assert_eq!(
            rebase_renderer_window_offset((100, 50), Some((0, 29))),
            (100, 50)
        );
    }

    #[test]
    fn point_hit_combines_negative_renderer_origin_and_web_process_inset() {
        let offset = rebase_renderer_window_offset((100, 50), Some((-8, -29)));
        let document = combine_wayland_content_offsets(None, Some((0, 47)), true);
        let (x, y, w, h) = project_screen_extents((450, 270, 40, 20), offset, document).unwrap();
        assert_eq!((x, y, w, h), (558, 396, 40, 20));
        // Keep the application-wide index and prefer the button over its label.
        let frames = [(82, x, y, w, h, false), (83, x + 1, y + 1, 38, 18, true)];
        assert_eq!(select_click_target(&frames, 569, 404), Some(82));
        assert_eq!(select_click_target(&frames, 569, 375), None);
        assert_eq!(
            project_screen_extents((450, 270, 40, 20), offset, None),
            Some((558, 349, 40, 20))
        );
    }

    #[test]
    fn point_hit_projection_rejects_unrealized_extents_before_offsets() {
        // The -16390 rows fall below the -16384 sentinel floor only before
        // the offsets are applied, so they fail if the check moves after them.
        for raw in [
            (i32::MIN, 0, 40, 20),
            (0, i32::MIN, 40, 20),
            (-16390, 0, 40, 20),
            (0, -16390, 40, 20),
            (0, 0, 1, 1),
        ] {
            assert_eq!(project_screen_extents(raw, (108, 79), Some((0, 47))), None);
        }
    }

    #[test]
    fn scoped_bounds_keep_application_indices_and_original_target_nodes() {
        let nodes = [
            (0, true, "sibling button"),
            (1, false, "target without geometry"),
            (1, true, "target button"),
            (0, true, "sibling entry"),
            (1, true, "target entry"),
        ];
        let scoped: Vec<_> = scoped_component_nodes(&nodes, Some(1), |node| (node.0, node.1))
            .map(|(index, node)| (index, node.2))
            .collect();
        assert_eq!(scoped, [(2, "target button"), (4, "target entry")]);
        assert_eq!(nodes[scoped[0].0].2, "target button");
        let all: Vec<_> = scoped_component_nodes(&nodes, None, |node| (node.0, node.1))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(all, [0, 2, 3, 4]);
        assert_eq!(
            scoped_component_nodes(&nodes, Some(2), |node| (node.0, node.1)).count(),
            0
        );
    }

    #[test]
    fn compositor_origin_wins_over_stale_accessibility_observation() {
        assert_eq!(
            prefer_authoritative_wayland_origin(Some((0, 0)), Some((120, 120))),
            Some((0, 0))
        );
        assert_eq!(
            prefer_authoritative_wayland_origin(None, Some((120, 120))),
            Some((120, 120))
        );
    }

    #[test]
    fn wayland_compositor_and_document_offsets_are_additive() {
        assert_eq!(
            combine_wayland_content_offsets(Some((0, 47)), Some((0, 0)), true),
            Some((0, 47))
        );
        assert_eq!(
            combine_wayland_content_offsets(Some((2, 20)), Some((0, 47)), true),
            Some((2, 67))
        );
        assert_eq!(
            combine_wayland_content_offsets(None, Some((0, 47)), true),
            Some((0, 47))
        );
        assert_eq!(combine_wayland_content_offsets(None, None, true), None);
    }

    #[test]
    fn hyprland_document_selection_requires_its_own_web_process_bus() {
        let mixed_bus_documents = [("application", 1, false), ("web-process", 3, true)];
        assert_eq!(
            select_web_document(mixed_bus_documents.into_iter(), true),
            Some("web-process")
        );
        // Sway retains its existing shallowest-document selection.
        assert_eq!(
            select_web_document(mixed_bus_documents.into_iter(), false),
            Some("application")
        );
        assert_eq!(
            select_web_document([("application", 1, false)].into_iter(), true),
            None
        );
    }

    #[test]
    fn hyprland_document_infers_csd_without_double_counting() {
        let target = (0x1234_5678_9abc, 42);
        let window = Some((target.0, target.1, 940, 700));
        for y in [0, 20, 47] {
            assert_eq!(
                hyprland_document_top_inset(target, window, (0, y, 940, 653)),
                Some(47)
            );
        }
        assert_eq!(
            combine_wayland_content_offsets(None, Some((0, 47)), false),
            None
        );
    }

    #[test]
    fn hyprland_document_rejects_invalid_or_partial_dimensions() {
        let target = (0x1234_5678_9abc, 42);
        let window = Some((target.0, target.1, 940, 700));
        for document in [
            (0, 0, 0, 653),
            (0, 0, 940, 0),
            (0, 0, -1, 653),
            (0, 0, 940, -1),
            (0, 0, 935, 653),
            (0, 0, 945, 653),
            (0, 0, 940, 700),
            (0, 0, 940, 701),
            (0, 0, 940, 500),
            (1, 0, 940, 653),
            (0, -1, 940, 653),
        ] {
            assert_eq!(hyprland_document_top_inset(target, window, document), None);
        }
    }

    #[test]
    fn hyprland_document_requires_full_address_and_pid() {
        let target = (0x1234_5678_9abc, 42);
        let document = (0, 0, 940, 653);
        for window in [
            None,
            Some((0x5678_9abc, 42, 940, 700)),
            Some((0x1234_5678_9abd, 42, 940, 700)),
            Some((target.0, 43, 940, 700)),
        ] {
            assert_eq!(hyprland_document_top_inset(target, window, document), None);
        }
        assert_eq!(
            hyprland_document_top_inset((0, 42), Some((0, 42, 940, 700)), document),
            None
        );
    }

    #[test]
    fn chromium_window_extents_do_not_double_count_document_origin() {
        assert_eq!(
            combine_wayland_content_offsets(Some((2, 20)), Some((22, 55)), false),
            Some((2, 20))
        );
        assert_eq!(
            combine_wayland_content_offsets(None, Some((22, 55)), false),
            None
        );
    }

    #[test]
    fn webkit_web_process_bus_marks_roleless_document_subtrees() {
        assert!(is_web_process_bus("org.webkitgtk.WebProcess.1234"));
        assert!(is_web_process_bus("org.example.WebProcess"));
        assert!(!is_web_process_bus(":1.42"));
    }

    #[test]
    fn click_target_prefers_button_over_its_inner_label() {
        // The exact live GTK4 gnome-calculator case this fixes: button "7"
        // (idx 6, role 'button', 82,331 64x44) wraps a slightly smaller inner
        // label (idx 7, role 'label', 82,331 56x40). A click at the shared
        // center must actuate the BUTTON (idx 6) — area alone would pick the
        // smaller inert label (idx 7) → silent no-op "false success".
        let frames = vec![
            (6usize, 82, 331, 64, 44, false), // button "7"
            (7usize, 82, 331, 56, 40, true),  // inner label "7"
        ];
        assert_eq!(select_click_target(&frames, 114, 353), Some(6));
    }

    #[test]
    fn click_target_smallest_active_over_enclosing_panel() {
        let frames = vec![
            (0usize, 0, 0, 400, 600, false),   // panel
            (3usize, 80, 320, 64, 44, false),  // button "7"
            (5usize, 150, 320, 64, 44, false), // button "8"
        ];
        assert_eq!(select_click_target(&frames, 100, 340), Some(3));
        assert_eq!(select_click_target(&frames, 180, 340), Some(5));
    }

    #[test]
    fn click_target_falls_back_to_label_when_no_actuator_covers() {
        // A lone clickable label (no button covers the point) is still a valid
        // last-resort target — don't drop the click entirely.
        let frames = vec![(9usize, 10, 10, 30, 20, true)];
        assert_eq!(select_click_target(&frames, 20, 15), Some(9));
    }

    #[test]
    fn click_target_edges_exclusive_and_misses_return_none() {
        let frames = vec![(7usize, 10, 10, 20, 20, false)];
        assert_eq!(select_click_target(&frames, 10, 10), Some(7)); // top-left inclusive
        assert_eq!(select_click_target(&frames, 29, 29), Some(7)); // inside
        assert_eq!(select_click_target(&frames, 30, 20), None); // right edge exclusive
        assert_eq!(select_click_target(&frames, 20, 30), None); // bottom edge exclusive
        assert_eq!(select_click_target(&frames, 5, 5), None); // outside
        assert_eq!(select_click_target(&[], 0, 0), None); // no frames
    }

    #[test]
    fn passive_roles_classified() {
        assert!(is_passive_role("label"));
        assert!(is_passive_role("static text"));
        assert!(!is_passive_role("button"));
        assert!(!is_passive_role("push button"));
        assert!(!is_passive_role("text box")); // editable display is a real target
    }

    #[test]
    fn frame_extents_maps_left_and_top_not_right_or_bottom() {
        // _GTK_FRAME_EXTENTS = [left, right, top, bottom]; we need (left, top).
        assert_eq!(parse_gtk_frame_extents(&[61, 61, 55, 67]), Some((61, 55)));
        // Asymmetric values prove we don't accidentally read right([1])/bottom([3]).
        assert_eq!(parse_gtk_frame_extents(&[10, 20, 30, 40]), Some((10, 30)));
        // Maximized GTK4 window: zero inset, but property present.
        assert_eq!(parse_gtk_frame_extents(&[0, 0, 0, 0]), Some((0, 0)));
    }

    #[test]
    fn frame_extents_absent_or_short_is_none() {
        assert_eq!(parse_gtk_frame_extents(&[]), None);
        assert_eq!(parse_gtk_frame_extents(&[61, 61]), None);
        assert_eq!(parse_gtk_frame_extents(&[61, 61, 55]), None);
    }

    #[test]
    fn plain_activation_names_are_recognised() {
        for name in [
            "click",
            "activate",
            "press",
            "Toggle",
            "do default",
            "do-default",
        ] {
            assert!(is_activation_action(name), "{name} should activate");
        }
    }

    #[test]
    fn gtk4_namespaced_editing_actions_are_not_activations() {
        // Live capture from gnome-text-editor's GTK4 text view. `do_action(0)`
        // here deletes a line of the user's document.
        for name in [
            "buffer.delete-line",
            "buffer.select-line",
            "clipboard.copy",
            "clipboard.cut",
            "selection.delete",
            "text.clear",
            "menu.popup",
        ] {
            assert!(!is_activation_action(name), "{name} must not activate");
        }
    }

    #[test]
    fn a_text_view_action_list_has_no_activation_index() {
        let text_view: Vec<String> = [
            "buffer.delete-line",
            "buffer.select-line",
            "misc.insert-emoji",
            "clipboard.copy",
            "selection.select-all",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(activation_index("text", &text_view), None);
    }

    #[test]
    fn chromium_action_names_stay_activatable() {
        // Live capture from Chromium on GNOME Wayland. These elements were
        // actuable before this change and must remain so; only
        // `showContextMenu` is not an activation.
        assert!(is_activation_action("activate")); // entry
        assert!(is_activation_action("press")); // button
        assert!(is_activation_action("clickAncestor")); // static in web content
        assert!(!is_activation_action("showContextMenu"));
        let entry = vec!["activate".to_owned(), "showContextMenu".to_owned()];
        let statisch = vec!["clickAncestor".to_owned(), "showContextMenu".to_owned()];
        assert_eq!(activation_index("entry", &entry), Some(0));
        assert_eq!(activation_index("static", &statisch), Some(0));
    }

    #[test]
    fn chromium_checkbox_verbs_are_role_gated() {
        let check = vec!["check".to_owned(), "showContextMenu".to_owned()];
        let uncheck = vec!["uncheck".to_owned(), "showContextMenu".to_owned()];

        assert_eq!(activation_index("check box", &check), Some(0));
        assert_eq!(activation_index("checkbox", &uncheck), Some(0));
        assert_eq!(activation_index("text", &check), None);
        assert_eq!(activation_index("entry", &uncheck), None);
        assert!(!is_activation_action("check"));
        assert!(!is_activation_action("uncheck"));
    }

    // Regression for 2026-10-25: with `query`, the tail of Kate's multi-line
    // document text showed up as its own row (`li" [actions=[]]`).
    #[test]
    fn multi_line_text_stays_on_one_markdown_row() {
        assert_eq!(one_line("first\nsecond\r\nli"), "first\\nsecond\\r\\nli");
        assert!(matches!(
            one_line("plain"),
            std::borrow::Cow::Borrowed("plain")
        ));

        let markdown = format!(
            "- [0] frame \"Kate\" [actions=[]]\n  - [1] text \"\" value=\"{}\" [actions=[]]\n",
            one_line("hello\nsecond line\nli")
        );
        let filtered = super::super::filter_tree(&markdown, "li");
        assert!(
            filtered
                .lines()
                .all(|line| line.trim_start().starts_with("- [")),
            "{filtered}"
        );
        assert_eq!(filtered.lines().count(), 2, "{filtered}");
    }

    // Regression for 2026-10-25: Kate's open Selection menu read as
    // `Selection > Kate > Select All`, so no Qt menu path could match.
    #[test]
    fn qt_popup_menu_containers_are_transparent_in_menu_paths() {
        assert!(counts_in_menu_lineage("menu item", "Selection"));
        assert!(counts_in_menu_lineage("menu", "File"));
        assert!(!counts_in_menu_lineage("popup menu", "Kate"));
        assert!(!counts_in_menu_lineage("menu", "  "));
        assert!(!counts_in_menu_lineage("push button", "Selection"));
    }

    #[test]
    fn menu_lookup_steps_follow_the_walks_path_rules() {
        // Qt: menu bar items, a menu's popup container and its entries.
        assert_eq!(
            menu_step("menu item", "View", "View", None),
            MenuStep::Match
        );
        assert_eq!(menu_step("menu item", "Edit", "View", None), MenuStep::Skip);
        assert_eq!(
            menu_step("popup menu", "KWrite", "Borders", Some("View")),
            MenuStep::LookInside
        );
        assert_eq!(
            menu_step(
                "check menu item",
                " Show Line Numbers ",
                "Show Line Numbers",
                Some("Borders")
            ),
            MenuStep::Match
        );
        assert_eq!(
            menu_step("separator", "", "Enlarge Font", Some("View")),
            MenuStep::LookInside
        );
        // An unnamed menu is transparent, and so is one that repeats the
        // previous segment's label (the walk collapses the repeat).
        assert_eq!(
            menu_step("menu", "", "Open", Some("File")),
            MenuStep::LookInside
        );
        assert_eq!(
            menu_step("menu", "File", "Open", Some("File")),
            MenuStep::LookInside
        );
        // Only a menu role matches; anything else is looked through.
        assert_eq!(
            menu_step("push button", "View", "View", None),
            MenuStep::LookInside
        );
    }

    // Regression for 2026-10-25: invoke_menu(["Edit", "Select All"]) on Kate
    // failed because Qt's menu-bar items expose only `Show Menu`.
    #[test]
    fn qt_show_menu_is_role_gated_to_menus() {
        let qt_menu_bar_item = vec!["Show Menu".to_owned()];
        assert_eq!(activation_index("menu item", &qt_menu_bar_item), Some(0));
        assert_eq!(activation_index("menu", &qt_menu_bar_item), Some(0));
        assert_eq!(
            activation_index("Menu Item", &["showMenu".to_owned()]),
            Some(0)
        );
        assert_eq!(activation_index("push button", &qt_menu_bar_item), None);
        assert_eq!(activation_index("combo box", &qt_menu_bar_item), None);
        assert!(!is_activation_action("Show Menu"));
        // A context menu is never a menu activation, whatever the role.
        let context = vec!["showContextMenu".to_owned()];
        assert_eq!(activation_index("menu item", &context), None);
    }

    #[test]
    fn a_button_activates_on_its_click_action() {
        let button = vec!["click".to_owned()];
        assert_eq!(activation_index("button", &button), Some(0));
        // Position is not meaning: the activation may sit anywhere.
        let mixed = vec!["clipboard.copy".to_owned(), "activate".to_owned()];
        assert_eq!(activation_index("button", &mixed), Some(1));
        // Failed action-name lookups are retained as empty placeholders so
        // the selected vector position is still the original AT-SPI index.
        let sparse = vec![
            String::new(),
            "buffer.delete-line".to_owned(),
            "activate".to_owned(),
        ];
        assert_eq!(activation_index("button", &sparse), Some(2));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn application_attributes_wake_once_per_pid() {
        assert!(first_application_walk(u32::MAX));
        assert!(!first_application_walk(u32::MAX));
        assert!(first_application_walk(u32::MAX - 1));
        assert!(!first_application_walk(u32::MAX - 1));
    }

    #[test]
    fn focus_log_tracks_latest_focused_object_per_bus_and_clears_on_blur() {
        let bus = ":9.test-focus-log";
        note_focus_event(bus, "/a", true);
        assert_eq!(
            focus_map().lock().unwrap().get(bus).map(String::as_str),
            Some("/a")
        );
        note_focus_event(bus, "/b", true);
        assert_eq!(
            focus_map().lock().unwrap().get(bus).map(String::as_str),
            Some("/b")
        );
        // A blur for a stale object must not erase the newer focus.
        note_focus_event(bus, "/a", false);
        assert_eq!(
            focus_map().lock().unwrap().get(bus).map(String::as_str),
            Some("/b")
        );
        note_focus_event(bus, "/b", false);
        assert!(focus_map().lock().unwrap().get(bus).is_none());
    }

    #[test]
    fn per_call_timeout_clamps_to_operation_deadline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // No deadline in scope: the historical per-call timeout applies.
            assert_eq!(remaining_budget(), CALL_TIMEOUT);
            assert!(!deadline_passed());
            let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            OP_DEADLINE
                .scope(deadline, async {
                    assert!(remaining_budget() <= Duration::from_millis(200));
                    // A call that would outlive the remaining budget is cut at
                    // the deadline, not at CALL_TIMEOUT.
                    let started = std::time::Instant::now();
                    let got = call(tokio::time::sleep(Duration::from_secs(10))).await;
                    assert!(got.is_none());
                    assert!(started.elapsed() < Duration::from_secs(2));
                    assert!(deadline_passed());
                    // Once the deadline has passed, calls short-circuit.
                    let started = std::time::Instant::now();
                    assert!(call(tokio::time::sleep(Duration::from_secs(10)))
                        .await
                        .is_none());
                    assert!(started.elapsed() < Duration::from_millis(50));
                })
                .await;
        });
    }

    #[test]
    fn bounded_for_returns_partial_via_on_timeout_when_work_ignores_deadline() {
        let started = std::time::Instant::now();
        let result: Result<&'static str> = bounded_for(
            Duration::from_millis(100),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("finished")
            },
            || Ok("partial"),
        );
        assert_eq!(result.unwrap(), "partial");
        // deadline + 500 ms backstop, well under the old 25 s.
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

#[cfg(test)]
mod at_point_rules_tests {
    use super::*;

    fn acts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn container_activation_never_bubbles() {
        // Nautilus: canvas item (no Action) under `layered pane "Icon View"`
        // [activate, menu]: the container is an ancestor -> not fired.
        assert_eq!(
            at_point_activation_index("layered pane", &acts(&["activate", "menu"]), false),
            None
        );
        for role in [
            "list",
            "table",
            "tree",
            "tree table",
            "icon view",
            "panel",
            "list box",
        ] {
            assert_eq!(
                at_point_activation_index(role, &acts(&["activate"]), false),
                None
            );
            // A click on the container's own empty area must not open its
            // current selection either.
            assert_eq!(
                at_point_activation_index(role, &acts(&["activate"]), true),
                None
            );
        }
        // A canvas is a surface: as an ancestor its activate is the
        // container's; as the deepest hit it is the widget itself.
        assert_eq!(
            at_point_activation_index("canvas", &acts(&["activate"]), false),
            None
        );
        assert_eq!(
            at_point_activation_index("canvas", &acts(&["activate"]), true),
            Some(0)
        );
    }

    #[test]
    fn top_level_frame_default_action_is_never_a_pixel_click() {
        // Chrome 151 on X11: the hit test over page content resolves only to
        // `frame "… - Google Chrome"` [doDefault]. Firing it changes nothing,
        // so it must not be reported as the click, as ancestor or deepest hit.
        for role in ["frame", "window", "application", "desktop frame"] {
            for deepest in [false, true] {
                assert_eq!(
                    at_point_activation_index(role, &acts(&["doDefault"]), deepest),
                    None,
                    "{role} deepest={deepest}"
                );
            }
        }
        // The page content itself remains actionable once exposed.
        assert_eq!(
            at_point_activation_index("push button", &acts(&["doDefault"]), true),
            Some(0)
        );
    }

    #[test]
    fn real_actuators_still_fire_from_any_depth() {
        assert_eq!(
            at_point_activation_index("push button", &acts(&["click"]), false),
            Some(0)
        );
        assert_eq!(
            at_point_activation_index("menu item", &acts(&["click"]), true),
            Some(0)
        );
        assert_eq!(
            at_point_activation_index("check box", &acts(&["check"]), true),
            Some(0)
        );
        // Non-activation verbs never fire, container or not.
        assert_eq!(
            at_point_activation_index("push button", &acts(&["buffer.delete-line"]), true),
            None
        );
    }

    #[test]
    fn cell_roles_take_a_real_press() {
        for role in ["table cell", "cell", "Table Cell"] {
            assert!(is_cell_role(role), "{role}");
            assert!(is_selectable_item_role(role), "{role}");
        }
        for role in [
            "canvas",
            "list item",
            "icon",
            "tree item",
            "row",
            "table row",
        ] {
            assert!(!is_cell_role(role), "{role}");
        }
    }

    #[test]
    fn selectable_item_roles() {
        for role in [
            "canvas",
            "list item",
            "table cell",
            "tree item",
            "icon",
            "row",
        ] {
            assert!(is_selectable_item_role(role), "{role}");
        }
        for role in ["push button", "layered pane", "label", "menu item"] {
            assert!(!is_selectable_item_role(role), "{role}");
        }
    }

    #[test]
    fn hit_description_names_what_was_fired() {
        let hit = AtPointHit::fired(
            "activate".into(),
            "push button".into(),
            "Restore".into(),
            "/org/a11y/atspi/accessible/42".into(),
        );
        assert_eq!(
            hit.describe(),
            "hit: push button \"Restore\" action=activate path=/org/a11y/atspi/accessible/42"
        );
        let selected = AtPointHit {
            action: "select_child".into(),
            role: "canvas".into(),
            name: "file.txt".into(),
            path: "/x".into(),
            selected: Some("file.txt".into()),
            selection_verified: true,
        };
        assert!(selected
            .describe()
            .starts_with("selected: canvas \"file.txt\""));
        assert!(selected.describe().contains("read back as selected"));
    }
}
