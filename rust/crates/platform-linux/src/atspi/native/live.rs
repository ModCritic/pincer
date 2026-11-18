//! The process's shared AT-SPI connection, the focus tracker that reads its
//! events, and what happens when the accessibility bus or its registry goes
//! away.
//!
//! One connection serves every AT-SPI call. It registers Pincer's event
//! listeners with the registry (WebKitGTK publishes its web content only
//! while a listener is registered) and feeds the focus log the typing paths
//! read. It used to be opened once for the process lifetime: after the
//! accessibility bus or at-spi2-registryd restarted, every call kept the
//! dead connection, or one whose listeners the new registry never heard of,
//! the focus log kept bus names of the old bus, and health_report, which
//! opened a connection of its own, still said the registry answered.
//!
//! Now each connection is a numbered generation. Its tracker marks it dead
//! when the bus closes the connection (the event stream ends) or when the
//! registry's well-known name gets a new owner. The next AT-SPI call, and a
//! retry the tracker schedules, opens a new connection, registers the
//! listeners again, starts a new tracker and clears the focus log. The focus
//! log also forgets an application when its bus name goes away, and holds
//! at most [`FOCUS_LOG_BUSES`] applications.

use super::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Well-known name of at-spi2-registryd on the accessibility bus.
pub(crate) const REGISTRY_BUS_NAME: &str = "org.a11y.atspi.Registry";

/// Applications the focus log remembers at most; the oldest-inserted entry
/// goes first when a new application would exceed it.
pub(crate) const FOCUS_LOG_BUSES: usize = 256;

/// Wait before the tracker reopens a dead connection, so a registry that is
/// still starting has its name before the listeners are registered again.
const RECONNECT_DELAY: Duration = Duration::from_millis(300);

/// One opened connection and its state.
struct Generation<C> {
    conn: Arc<C>,
    id: u64,
    dead: bool,
    /// Tells this generation's tracker to stop once a newer one replaced it.
    stop: Arc<tokio::sync::Notify>,
}

/// The current shared connection, numbered so a tracker of a replaced
/// connection can never mark the new one dead.
pub(crate) struct ConnectionSlot<C> {
    current: Option<Generation<C>>,
    last_id: u64,
}

impl<C> ConnectionSlot<C> {
    pub(crate) const fn new() -> Self {
        Self {
            current: None,
            last_id: 0,
        }
    }

    /// The current connection and its generation, unless it was found dead.
    pub(crate) fn live(&self) -> Option<(Arc<C>, u64)> {
        self.current
            .as_ref()
            .filter(|current| !current.dead)
            .map(|current| (current.conn.clone(), current.id))
    }

    /// Whether this process ever opened the shared connection (a one-shot
    /// diagnostic never does).
    pub(crate) fn ever_connected(&self) -> bool {
        self.current.is_some()
    }

    /// The current generation, dead or not.
    pub(crate) fn generation(&self) -> Option<u64> {
        self.current.as_ref().map(|current| current.id)
    }

    /// Make a freshly opened connection current. Returns its generation and
    /// the signal its tracker stops on; the replaced generation's tracker is
    /// told to stop.
    pub(crate) fn install(&mut self, conn: Arc<C>) -> (u64, Arc<tokio::sync::Notify>) {
        self.last_id += 1;
        let stop = Arc::new(tokio::sync::Notify::new());
        if let Some(replaced) = self.current.replace(Generation {
            conn,
            id: self.last_id,
            dead: false,
            stop: stop.clone(),
        }) {
            replaced.stop.notify_one();
        }
        (self.last_id, stop)
    }

    /// Mark `generation` dead when it is still the current one, so the next
    /// call opens a new connection. Returns whether it was current and alive.
    pub(crate) fn mark_dead(&mut self, generation: u64) -> bool {
        match self.current.as_mut() {
            Some(current) if current.id == generation && !current.dead => {
                current.dead = true;
                true
            }
            _ => false,
        }
    }
}

static SLOT: Mutex<ConnectionSlot<AccessibilityConnection>> = Mutex::new(ConnectionSlot::new());

/// Held while a new connection is opened, so concurrent callers that find
/// none open only one.
static CONNECTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The shared connection, opened (or reopened after it died) on demand.
pub(crate) async fn shared_connection() -> Result<Arc<AccessibilityConnection>> {
    shared_connection_with_generation()
        .await
        .map(|(conn, _)| conn)
}

async fn shared_connection_with_generation() -> Result<(Arc<AccessibilityConnection>, u64)> {
    if let Some(live) = SLOT.lock().unwrap().live() {
        return Ok(live);
    }
    let _connecting = CONNECTING.lock().await;
    if let Some(live) = SLOT.lock().unwrap().live() {
        return Ok(live);
    }
    let conn = Arc::new(open_listening_connection().await?);
    let (generation, stop) = SLOT.lock().unwrap().install(conn.clone());
    focus_log().lock().unwrap().reset(generation);
    spawn_tracker(conn.clone(), generation, stop);
    dlog!("AT-SPI connection generation {generation} is listening");
    Ok((conn, generation))
}

/// Open a connection to the accessibility bus and register the listeners the
/// registry and the focus log need.
async fn open_listening_connection() -> Result<AccessibilityConnection> {
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
}

/// Why a tracker stopped reading its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackerEnd {
    /// The bus closed the connection (it exited or restarted).
    Closed,
    /// The registry's name has a new owner: the new registry knows nothing
    /// of this connection's listeners.
    RegistryReplaced,
    /// A newer connection replaced this one.
    Superseded,
}

/// What one `NameOwnerChanged` on the accessibility bus means here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnerChange {
    /// The registry's well-known name went to a new process.
    RegistryReplaced,
    /// An application's connection (a unique name) closed.
    BusGone(String),
    Other,
}

pub(crate) fn owner_change(name: &str, new_owner: Option<&str>) -> OwnerChange {
    match (name, new_owner) {
        (REGISTRY_BUS_NAME, Some(_)) => OwnerChange::RegistryReplaced,
        (name, None) if name.starts_with(':') => OwnerChange::BusGone(name.to_owned()),
        _ => OwnerChange::Other,
    }
}

/// Read generation `generation`'s events for as long as it is current:
/// focus and active-descendant events into the focus log, application exits
/// out of it. When the connection closes or the registry is replaced, mark
/// the generation dead and open a new one after [`RECONNECT_DELAY`].
fn spawn_tracker(
    conn: Arc<AccessibilityConnection>,
    generation: u64,
    stop: Arc<tokio::sync::Notify>,
) {
    runtime().spawn(async move {
        // `track` owns the connection and its streams and drops them when it
        // returns, so a dead connection is not held while reconnecting.
        let end = track(conn, generation, stop).await;
        dlog!("AT-SPI generation {generation} tracker ended: {end:?}");
        if end != TrackerEnd::Superseded && SLOT.lock().unwrap().mark_dead(generation) {
            tokio::time::sleep(RECONNECT_DELAY).await;
            if let Err(error) = shared_connection().await {
                dlog!("AT-SPI reconnect after {end:?} failed: {error}; the next call retries");
            }
        }
    });
}

async fn track(
    conn: Arc<AccessibilityConnection>,
    generation: u64,
    stop: Arc<tokio::sync::Notify>,
) -> TrackerEnd {
    use futures_util::StreamExt;
    let mut owners = match zbus::fdo::DBusProxy::new(conn.connection()).await {
        Ok(proxy) => proxy.receive_name_owner_changed().await.ok(),
        Err(_) => None,
    };
    if owners.is_none() {
        dlog!("AT-SPI generation {generation}: no NameOwnerChanged subscription");
    }
    let events = conn.event_stream();
    futures_util::pin_mut!(events);
    loop {
        let next_owner = async {
            match owners.as_mut() {
                Some(owners) => owners.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stop.notified() => return TrackerEnd::Superseded,
            event = events.next() => match event {
                Some(Ok(event)) => note_event(generation, &event),
                // One message that did not parse; the stream goes on.
                Some(Err(_)) => {}
                None => return TrackerEnd::Closed,
            },
            change = next_owner => {
                let Some(change) = change else {
                    owners = None;
                    continue;
                };
                let Ok(args) = change.args() else { continue };
                let new_owner = args.new_owner().as_ref().map(|owner| owner.as_str());
                match owner_change(args.name().as_str(), new_owner) {
                    OwnerChange::RegistryReplaced => return TrackerEnd::RegistryReplaced,
                    OwnerChange::BusGone(bus) => focus_log().lock().unwrap().forget_bus(&bus),
                    OwnerChange::Other => {}
                }
            }
        }
    }
}

fn note_event(generation: u64, event: &atspi::Event) {
    match event {
        atspi::Event::Object(atspi::ObjectEvents::ActiveDescendantChanged(active)) => {
            if let Some(bus) = active.item.name_as_str() {
                focus_log().lock().unwrap().note_active_descendant(
                    generation,
                    bus,
                    active.descendant.path_as_str(),
                );
            }
        }
        atspi::Event::Object(atspi::ObjectEvents::StateChanged(changed))
            if changed.state == State::Focused =>
        {
            if let Some(bus) = changed.item.name_as_str() {
                focus_log().lock().unwrap().note_focus(
                    generation,
                    bus,
                    changed.item.path_as_str(),
                    changed.enabled,
                );
            }
        }
        _ => {}
    }
}

/// Focus events per application bus name, as the current connection's
/// tracker saw them.
#[derive(Default)]
pub(crate) struct FocusLog {
    /// The connection generation whose events are kept.
    generation: u64,
    /// Object path of the accessible that most recently reported
    /// `focused=true`, per application bus.
    focused: HashMap<String, String>,
    /// Object path of the active descendant most recently reported per bus
    /// (`object:active-descendant-changed`): the current cell of a grid.
    active_descendant: HashMap<String, String>,
    /// Buses in the order they were first noted, for the size bound.
    order: std::collections::VecDeque<String>,
}

impl FocusLog {
    /// Start over for a new connection: bus names of the old one mean
    /// nothing now, and events of older generations are ignored.
    pub(crate) fn reset(&mut self, generation: u64) {
        *self = Self {
            generation,
            ..Self::default()
        };
    }

    /// Record one `object:state-changed:focused` event.
    pub(crate) fn note_focus(&mut self, generation: u64, bus: &str, path: &str, enabled: bool) {
        if generation != self.generation {
            return;
        }
        if enabled {
            self.admit(bus);
            self.focused.insert(bus.to_owned(), path.to_owned());
        } else if self.focused.get(bus).is_some_and(|current| current == path) {
            self.focused.remove(bus);
        }
    }

    /// Record one `object:active-descendant-changed` event.
    pub(crate) fn note_active_descendant(&mut self, generation: u64, bus: &str, path: &str) {
        if generation != self.generation {
            return;
        }
        self.admit(bus);
        self.active_descendant
            .insert(bus.to_owned(), path.to_owned());
    }

    /// Forget an application whose bus connection closed.
    pub(crate) fn forget_bus(&mut self, bus: &str) {
        self.focused.remove(bus);
        self.active_descendant.remove(bus);
        self.order.retain(|known| known != bus);
    }

    pub(crate) fn focused(&self, bus: &str) -> Option<&str> {
        self.focused.get(bus).map(String::as_str)
    }

    pub(crate) fn active_descendant(&self, bus: &str) -> Option<&str> {
        self.active_descendant.get(bus).map(String::as_str)
    }

    /// The number of applications the log holds anything for.
    #[cfg(test)]
    pub(crate) fn buses(&self) -> usize {
        self.order.len()
    }

    /// Make room for `bus`: a bus already known stays where it is; a new one
    /// pushes out the oldest once the log is full.
    fn admit(&mut self, bus: &str) {
        if self.order.iter().any(|known| known == bus) {
            return;
        }
        while self.order.len() >= FOCUS_LOG_BUSES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.focused.remove(&oldest);
            self.active_descendant.remove(&oldest);
        }
        self.order.push_back(bus.to_owned());
    }
}

pub(crate) fn focus_log() -> &'static Mutex<FocusLog> {
    static LOG: OnceLock<Mutex<FocusLog>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(FocusLog::default()))
}

/// What the registry answered on the shared connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveRegistry {
    /// Applications the registry's root lists.
    pub applications: usize,
    /// The connection was dead (or found dead by this probe) and a new one
    /// answered.
    pub reconnected: bool,
}

/// Ask the registry, over the connection every AT-SPI call uses, how many
/// applications it lists. `None` when this process never opened that
/// connection (a one-shot diagnostic), so the caller probes over a
/// connection of its own. A failure marks the connection dead and asks once
/// more on a new one: the answer is what the next tool call would get.
pub(crate) fn probe_live_registry(
    budget: Duration,
) -> Option<std::result::Result<LiveRegistry, String>> {
    let before = {
        let slot = SLOT.lock().unwrap();
        if !slot.ever_connected() {
            return None;
        }
        slot.live().map(|(_, generation)| generation)
    };
    let attempt = budget / 2;
    Some(runtime().block_on(async move {
        let first = ask_registry(attempt).await;
        let (generation, why) = match first {
            Ok((applications, generation)) => {
                return Ok(LiveRegistry {
                    applications,
                    reconnected: before != Some(generation),
                });
            }
            Err(failure) => failure,
        };
        if let Some(generation) = generation {
            SLOT.lock().unwrap().mark_dead(generation);
        }
        match ask_registry(attempt).await {
            Ok((applications, _)) => Ok(LiveRegistry {
                applications,
                reconnected: true,
            }),
            Err((_, again)) if again == why => Err(why),
            Err((_, again)) => Err(format!("{why}; on a new connection: {again}")),
        }
    }))
}

/// The registry root's application count on the shared connection, with the
/// generation that answered. A failure carries the generation it happened on
/// (`None` when no connection could be opened).
async fn ask_registry(
    budget: Duration,
) -> std::result::Result<(usize, u64), (Option<u64>, String)> {
    tokio::time::timeout(budget, async {
        let (conn, generation) = shared_connection_with_generation().await.map_err(|error| {
            (
                None,
                format!("could not connect to the accessibility bus: {error}"),
            )
        })?;
        let root = conn.root_accessible_on_registry().await.map_err(|error| {
            (
                Some(generation),
                format!("its root is unavailable: {error}"),
            )
        })?;
        let apps = root.get_children().await.map_err(|error| {
            (
                Some(generation),
                format!("it did not list its applications: {error}"),
            )
        })?;
        Ok((apps.len(), generation))
    })
    .await
    .unwrap_or_else(|_| {
        Err((
            SLOT.lock().unwrap().generation(),
            format!("it did not answer within {} ms", budget.as_millis()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_connection_is_replaced_and_its_tracker_cannot_kill_the_new_one() {
        let mut slot: ConnectionSlot<&'static str> = ConnectionSlot::new();
        assert!(!slot.ever_connected());
        assert!(slot.live().is_none());
        let (first, first_stop) = slot.install(Arc::new("first"));
        assert_eq!(
            slot.live().map(|(conn, id)| (*conn, id)),
            Some(("first", first))
        );
        // The bus closed the first connection: the next call must not get it.
        assert!(slot.mark_dead(first));
        assert!(slot.live().is_none());
        assert!(slot.ever_connected());
        assert_eq!(slot.generation(), Some(first));
        // Marking it twice is a no-op (one reconnect, not two).
        assert!(!slot.mark_dead(first));
        let (second, _) = slot.install(Arc::new("second"));
        assert_ne!(first, second);
        // The replaced generation's tracker was told to stop...
        let stopped = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(100), first_stop.notified())
                    .await
                    .is_ok()
            });
        assert!(stopped, "the replaced tracker was not told to stop");
        // ...and when it ends late it cannot mark the new connection dead.
        assert!(!slot.mark_dead(first));
        assert_eq!(
            slot.live().map(|(conn, id)| (*conn, id)),
            Some(("second", second))
        );
    }

    #[test]
    fn a_new_registry_owner_means_reconnect_and_a_closed_app_is_forgotten() {
        assert_eq!(
            owner_change(REGISTRY_BUS_NAME, Some(":1.40")),
            OwnerChange::RegistryReplaced
        );
        // The registry exiting alone: wait for the new one.
        assert_eq!(owner_change(REGISTRY_BUS_NAME, None), OwnerChange::Other);
        assert_eq!(
            owner_change(":1.17", None),
            OwnerChange::BusGone(":1.17".into())
        );
        assert_eq!(owner_change(":1.17", Some(":1.17")), OwnerChange::Other);
        assert_eq!(owner_change("org.kde.kwrite", None), OwnerChange::Other);
    }

    #[test]
    fn focus_log_tracks_latest_focused_object_per_bus_and_clears_on_blur() {
        let mut log = FocusLog::default();
        log.reset(1);
        let bus = ":9.test-focus-log";
        log.note_focus(1, bus, "/a", true);
        assert_eq!(log.focused(bus), Some("/a"));
        log.note_focus(1, bus, "/b", true);
        assert_eq!(log.focused(bus), Some("/b"));
        // A blur for a stale object must not erase the newer focus.
        log.note_focus(1, bus, "/a", false);
        assert_eq!(log.focused(bus), Some("/b"));
        log.note_focus(1, bus, "/b", false);
        assert!(log.focused(bus).is_none());
    }

    #[test]
    fn focus_log_forgets_closed_apps_and_stays_bounded() {
        let mut log = FocusLog::default();
        log.reset(1);
        log.note_focus(1, ":1.5", "/focus", true);
        log.note_active_descendant(1, ":1.5", "/cell");
        log.forget_bus(":1.5");
        assert!(log.focused(":1.5").is_none());
        assert!(log.active_descendant(":1.5").is_none());
        assert_eq!(log.buses(), 0);
        for n in 0..FOCUS_LOG_BUSES * 4 {
            let bus = format!(":1.{n}");
            log.note_focus(1, &bus, "/focus", true);
            log.note_active_descendant(1, &bus, "/cell");
        }
        assert_eq!(log.buses(), FOCUS_LOG_BUSES);
        assert_eq!(log.focused.len(), FOCUS_LOG_BUSES);
        assert_eq!(log.active_descendant.len(), FOCUS_LOG_BUSES);
        // The newest applications are the ones kept.
        let newest = format!(":1.{}", FOCUS_LOG_BUSES * 4 - 1);
        assert_eq!(log.focused(&newest), Some("/focus"));
        assert!(log.focused(":1.0").is_none());
    }

    #[test]
    fn a_new_connection_clears_the_focus_log_and_ignores_the_old_tracker() {
        let mut log = FocusLog::default();
        log.reset(1);
        log.note_focus(1, ":1.5", "/focus", true);
        log.note_active_descendant(1, ":1.5", "/cell");
        log.reset(2);
        assert!(log.focused(":1.5").is_none());
        assert!(log.active_descendant(":1.5").is_none());
        // A late event read by generation 1's tracker is not kept.
        log.note_focus(1, ":1.6", "/late", true);
        log.note_active_descendant(1, ":1.6", "/late");
        assert!(log.focused(":1.6").is_none());
        assert!(log.active_descendant(":1.6").is_none());
        log.note_focus(2, ":1.6", "/now", true);
        assert_eq!(log.focused(":1.6"), Some("/now"));
    }
}
