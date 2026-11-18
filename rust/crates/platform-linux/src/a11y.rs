//! Switch on Chromium / Electron accessibility for the whole desktop session.
//!
//! Chromium — and therefore every Electron, CEF, and Chrome-based app — ships
//! its accessibility tree disabled and only builds it once it believes an
//! assistive technology is listening. It decides that by watching the
//! freedesktop accessibility status published on the session bus: the
//! `org.a11y.Bus` service exposes an `org.a11y.Status` interface whose
//! `ScreenReaderEnabled` property is the signal. Until that property is true a
//! Chromium window registers either nothing or an empty application on the
//! AT-SPI registry, so [`crate::atspi`] walks an empty tree and
//! `get_window_state` reports the app as having no elements — it is invisible
//! to the driver. Electron apps shipped as AppImages behave identically; they
//! embed the same Chromium.
//!
//! A real screen reader turns the Chromium signal on. Doing that ourselves is
//! unsafe on GNOME and COSMIC: their session services treat the signal as a user
//! request and launch Orca. Those desktops therefore get only the generic
//! `IsEnabled` signal by default. Cinnamon cannot safely receive even that
//! reduced signal: its settings daemon derives toolkit accessibility from the
//! screen-reader setting and can enter a high-frequency settings write loop when
//! they disagree. Cinnamon therefore receives no advertisement by default.
//! Other desktops retain the Chromium signal for compatibility, and a caller
//! can choose either policy explicitly with `PINCER_A11Y_ADVERTISE_MODE`.
//!
//! Everything here is best-effort. A session without an accessibility bus (some
//! headless or minimal setups) just yields an error we log and ignore; enabling
//! accessibility must never be able to fail daemon startup.
//!
//! # The persisted setting
//!
//! The AT-SPI bus launcher (at-spi-bus-launcher) writes the accessibility
//! status into the user's settings (dconf): `org.gnome.desktop.interface
//! toolkit-accessibility`, and up to at-spi2-core 2.60 also
//! `org.gnome.desktop.a11y.applications screen-reader-enabled`. It does so
//! when a client sets `IsEnabled` (or `ScreenReaderEnabled`), and from 2.62
//! on also when the first AT-SPI event listener registers: 2.62 derives
//! `IsEnabled` from registered listeners, writes `true` to the setting when
//! the first one registers, and writes nothing when the last one leaves. So
//! Pincer's own listener turned the setting on in every advertise mode, and
//! it stayed on after Pincer exited (checked in a sandbox with
//! at-spi2-core 2.62.1; the launcher source of 2.60.0 and 2.62.0 agrees).
//!
//! Pincer now reads the stored values right before its own listener
//! registers and before it advertises, and puts back what those actions
//! changed. A launcher that
//! no longer reads the setting back (2.62 on, recognized by its missing
//! `ScreenReaderEnabled` property) gets the value back at once, so the live
//! `IsEnabled` it serves stays as it is. An older launcher follows the
//! setting, so putting it back would switch accessibility off under Pincer:
//! there the values go back when the last runtime that advertised shuts
//! down ([`release_session_accessibility`]), which the signal path runs too.

use std::collections::HashMap;
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use atspi::zbus;

/// Well-known session-bus name of the freedesktop accessibility-bus launcher,
/// which also carries the session's accessibility status.
const ACCESSIBILITY_BUS_SERVICE: &str = "org.a11y.Bus";
/// Object on [`ACCESSIBILITY_BUS_SERVICE`] exposing `org.a11y.Status`.
const ACCESSIBILITY_BUS_OBJECT: &str = "/org/a11y/bus";
/// Interface holding the session's accessibility-enabled / screen-reader flags.
const ACCESSIBILITY_STATUS_INTERFACE: &str = "org.a11y.Status";
/// Property Chromium watches to decide whether to build its AT-SPI tree.
const SCREEN_READER_ENABLED_PROPERTY: &str = "ScreenReaderEnabled";
/// Companion property GTK/Qt watch to load their AT-SPI bridges.
const ACCESSIBILITY_IS_ENABLED_PROPERTY: &str = "IsEnabled";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdvertiseMode {
    All,
    IsEnabledOnly,
    None,
}

/// Advertise an assistive technology to the session exactly once per daemon
/// process, so Chromium/Electron (including Electron AppImages), GTK, and Qt
/// expose their accessibility trees to [`crate::atspi`]. Idempotent and
/// best-effort: repeated calls do nothing, and any failure is logged and
/// swallowed so it can never block startup.
pub fn ensure_chromium_accessibility_enabled() {
    static ADVERTISED: Once = Once::new();
    guard().lock().unwrap().holds += 1;
    ADVERTISED.call_once(|| {
        let mode = advertise_mode_from(
            std::env::var_os("PINCER_DISABLE_A11Y_ADVERTISE").is_some(),
            std::env::var("PINCER_A11Y_ADVERTISE_MODE").ok().as_deref(),
            current_desktop().as_deref(),
        );
        if mode == AdvertiseMode::None {
            tracing::debug!(
                "accessibility advertisement disabled; leaving session status untouched"
            );
            return;
        }
        if let Err(error) = advertise_accessibility_to_session(mode) {
            tracing::debug!(
                "skipped advertising accessibility to the session \
                 (Chromium/Electron trees may stay empty): {error:#}"
            );
        }
    });
}

fn advertise_accessibility_to_session(mode: AdvertiseMode) -> anyhow::Result<()> {
    // The daemon's tokio runtime is already driving this thread when the tool
    // registry is built, and `block_on` panics if called from within a runtime.
    // Run the one-shot bus work on a dedicated OS thread that owns a small
    // runtime of its own, then join it — no nesting, torn down once it returns.
    std::thread::Builder::new()
        .name("pincer-a11y-advertise".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(advertise_accessibility(mode))
        })
        .context("spawning the accessibility-advertise thread")?
        .join()
        .map_err(|_| anyhow!("accessibility-advertise thread panicked"))?
}

async fn advertise_accessibility(mode: AdvertiseMode) -> anyhow::Result<()> {
    let session_bus = zbus::Connection::session().await?;
    let status = zbus::Proxy::new(
        &session_bus,
        ACCESSIBILITY_BUS_SERVICE,
        ACCESSIBILITY_BUS_OBJECT,
        ACCESSIBILITY_STATUS_INTERFACE,
    )
    .await?;
    let screen_reader = read_flag(&status, SCREEN_READER_ENABLED_PROPERTY).await;
    let is_enabled = read_flag(&status, ACCESSIBILITY_IS_ENABLED_PROPERTY).await;
    let writes = advertisement_writes(mode, screen_reader, is_enabled);
    if writes.is_empty() {
        return Ok(());
    }
    let before = read_stored_all();
    let mut wrote = false;
    for property in writes {
        // A launcher without the property (at-spi2-core 2.62 has no
        // ScreenReaderEnabled) refuses it; that must not cost the
        // IsEnabled write after it.
        match status.set_property(property, true).await {
            Ok(()) => wrote = true,
            Err(error) => tracing::debug!("could not set {property}: {error}"),
        }
    }
    if wrote {
        // The launcher writes the setting before it answers the Set.
        settle_advertisement(before, LauncherKind::from_screen_reader(screen_reader));
    }
    Ok(())
}

/// The status properties to switch on for `mode`, given what they read now
/// (`None`: the launcher has no such property, or it could not be read).
/// Only a flag that is off is written: a screen reader the user runs stays
/// authoritative, and no redundant PropertiesChanged is emitted.
fn advertisement_writes(
    mode: AdvertiseMode,
    screen_reader: Option<bool>,
    is_enabled: Option<bool>,
) -> Vec<&'static str> {
    let mut writes = Vec::new();
    if mode == AdvertiseMode::None {
        return writes;
    }
    if mode == AdvertiseMode::All && screen_reader == Some(false) {
        writes.push(SCREEN_READER_ENABLED_PROPERTY);
    }
    if is_enabled != Some(true) {
        writes.push(ACCESSIBILITY_IS_ENABLED_PROPERTY);
    }
    writes
}

fn advertise_mode_from(
    disabled: bool,
    configured: Option<&str>,
    desktop: Option<&str>,
) -> AdvertiseMode {
    if disabled {
        return AdvertiseMode::None;
    }
    match configured
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("all") => AdvertiseMode::All,
        Some("is_enabled_only") => AdvertiseMode::IsEnabledOnly,
        Some("none") => AdvertiseMode::None,
        Some(other) => {
            tracing::warn!(
                mode = other,
                "unknown PINCER_A11Y_ADVERTISE_MODE; using desktop default"
            );
            desktop_default_mode(desktop)
        }
        None => desktop_default_mode(desktop),
    }
}

fn desktop_default_mode(desktop: Option<&str>) -> AdvertiseMode {
    let has_desktop = |candidate: &str| {
        desktop.is_some_and(|desktop| {
            desktop
                .split([':', ';'])
                .map(str::trim)
                .any(|part| part.eq_ignore_ascii_case(candidate))
        })
    };

    // Cinnamon mirrors its GNOME and Cinnamon accessibility schemas and
    // recomputes toolkit accessibility from the assistive-technology toggles.
    // Advertising only IsEnabled can therefore livelock the settings daemons,
    // while advertising ScreenReaderEnabled launches Orca. Fail closed.
    if has_desktop("cinnamon") || has_desktop("x-cinnamon") {
        AdvertiseMode::None
    } else if has_desktop("gnome") || has_desktop("cosmic") {
        AdvertiseMode::IsEnabledOnly
    } else {
        AdvertiseMode::All
    }
}

/// The gsettings switch the AT-SPI bus launcher reports as `IsEnabled`.
const GSETTINGS_ACCESSIBILITY: &str =
    "`gsettings set org.gnome.desktop.interface toolkit-accessibility true`";

/// Whether `desktop` (`XDG_CURRENT_DESKTOP`, a colon-separated list) names
/// KDE Plasma.
fn is_kde(desktop: Option<&str>) -> bool {
    desktop.is_some_and(|desktop| {
        desktop
            .split([':', ';'])
            .any(|part| part.trim().eq_ignore_ascii_case("kde"))
    })
}

/// The session's desktop, as [`ensure_chromium_accessibility_enabled`]
/// reads it.
pub fn current_desktop() -> Option<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .or_else(|_| std::env::var("XDG_SESSION_DESKTOP"))
        .or_else(|_| std::env::var("DESKTOP_SESSION"))
        .ok()
}

/// How to switch an application's accessibility bridge on, for a window
/// whose AT-SPI tree came back empty, on the desktop `desktop`
/// (`XDG_CURRENT_DESKTOP`).
///
/// GNOME-family desktops keep the switch in gsettings, and the AT-SPI bus
/// launcher reports that key as `IsEnabled` (at-spi2-core 2.62 reads
/// `org.gnome.desktop.interface toolkit-accessibility`). KDE's own settings
/// have no such switch (its Accessibility page offers only a screen reader,
/// which needs Orca), and none is needed there. Checked in a KWin X11 slot
/// with at-spi2-core 2.62: the bus reports `IsEnabled` while Pincer runs
/// (its AT-SPI event listener is enough, even with
/// `PINCER_A11Y_ADVERTISE_MODE=none`), and KWrite (Qt 6) and Firefox 159
/// started before that published their trees once it did, without a
/// restart. Chromium 156 did not: with `IsEnabled` on it published an
/// application with no web document until it was started with
/// `--force-renderer-accessibility`, which launch_app adds.
pub fn bridge_off_advice(desktop: Option<&str>) -> String {
    if is_kde(desktop) {
        "the app's a11y bridge is off (on KDE nothing needs switching on: Qt and Firefox apps \
         turn theirs on once the AT-SPI bus reports IsEnabled, which it does while Pincer \
         runs; a Chromium or Electron app also needs --force-renderer-accessibility, so \
         restart it with launch_app, which adds it)"
            .to_owned()
    } else {
        format!("the a11y bridge is off (enable {GSETTINGS_ACCESSIBILITY})")
    }
}

/// How to get an accessibility bus when `org.a11y.Bus` has no owner on
/// the session bus, on the desktop `desktop`. GNOME and Unity autostart the
/// bus launcher only while the gsettings switch is on; elsewhere D-Bus
/// starts it on demand from at-spi2-core's `org.a11y.Bus` service file.
pub fn no_bus_advice(desktop: Option<&str>) -> String {
    if is_kde(desktop) {
        "start the AT-SPI bus: D-Bus starts it on demand from at-spi2-core's org.a11y.Bus \
         service, so check that at-spi2-core is installed, or run its at-spi-bus-launcher \
         (`/usr/lib/at-spi-bus-launcher` or `/usr/libexec/at-spi-bus-launcher`)"
            .to_owned()
    } else {
        format!(
            "enable accessibility ({GSETTINGS_ACCESSIBILITY}) and/or start the AT-SPI bus \
             (`/usr/libexec/at-spi-bus-launcher`)"
        )
    }
}

/// Read a boolean status property; `None` when the launcher has no such
/// property or does not answer.
async fn read_flag(status: &zbus::Proxy<'_>, property: &str) -> Option<bool> {
    tokio::time::timeout(STATUS_TIMEOUT, status.get_property::<bool>(property))
        .await
        .ok()?
        .ok()
}

// -- The persisted setting --------------------------------------------------

/// How long one status read or one settings command may take.
const STATUS_TIMEOUT: Duration = Duration::from_millis(800);
const SETTINGS_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the launcher gets to react to Pincer's listener (2.62 answered
/// within 30 ms in the sandbox).
const LISTENER_SETTLE: Duration = Duration::from_millis(1500);

/// A setting the AT-SPI bus launcher writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PersistedKey {
    /// `org.gnome.desktop.interface toolkit-accessibility` (`IsEnabled`).
    ToolkitAccessibility,
    /// `org.gnome.desktop.a11y.applications screen-reader-enabled`
    /// (`ScreenReaderEnabled`, up to at-spi2-core 2.60).
    ScreenReaderEnabled,
}

impl PersistedKey {
    const ALL: [Self; 2] = [Self::ToolkitAccessibility, Self::ScreenReaderEnabled];

    fn schema(self) -> &'static str {
        match self {
            Self::ToolkitAccessibility => "org.gnome.desktop.interface",
            Self::ScreenReaderEnabled => "org.gnome.desktop.a11y.applications",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ToolkitAccessibility => "toolkit-accessibility",
            Self::ScreenReaderEnabled => "screen-reader-enabled",
        }
    }

    fn dconf_path(self) -> String {
        format!("/{}/{}", self.schema().replace('.', "/"), self.key())
    }
}

/// A setting as the user's settings store holds it: `None` when the user
/// never set it (the schema default applies).
type Stored = Option<bool>;

/// What putting a setting back takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Restore {
    Nothing,
    /// Remove the user value again (it was unset before).
    Reset,
    Write(bool),
}

/// Put `prior` back over `now`.
fn restore_for(prior: Stored, now: Stored) -> Restore {
    match (prior, now) {
        (prior, now) if prior == now => Restore::Nothing,
        (None, _) => Restore::Reset,
        (Some(value), _) => Restore::Write(value),
    }
}

/// Whether the launcher reads the setting back into its live status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LauncherKind {
    /// at-spi2-core 2.60 and older: `ScreenReaderEnabled` exists, both
    /// settings are watched, and a listener changes nothing. Putting a
    /// setting back switches the live status with it.
    FollowsSettings,
    /// at-spi2-core 2.62 on: no `ScreenReaderEnabled`; `IsEnabled` follows
    /// registered listeners and only writes the setting. Putting it back
    /// leaves the live status alone.
    TracksListeners,
}

impl LauncherKind {
    fn from_screen_reader(screen_reader: Option<bool>) -> Self {
        if screen_reader.is_some() {
            Self::FollowsSettings
        } else {
            Self::TracksListeners
        }
    }
}

/// Parse `dconf read` output: empty when unset.
fn parse_dconf_value(output: &str) -> Option<Stored> {
    match output.trim() {
        "" => Some(None),
        "true" => Some(Some(true)),
        "false" => Some(Some(false)),
        _ => None,
    }
}

/// Run a settings command with a time limit; its trimmed stdout on success.
fn settings_command(program: &str, args: &[&str]) -> Option<String> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + SETTINGS_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                use std::io::Read as _;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The stored value of `key`: exact through `dconf`, else the effective
/// value through `gsettings`. `None` when neither can read it.
fn read_stored(key: PersistedKey) -> Option<Stored> {
    if let Some(out) = settings_command("dconf", &["read", &key.dconf_path()]) {
        return parse_dconf_value(&out);
    }
    let out = settings_command("gsettings", &["get", key.schema(), key.key()])?;
    match out.trim() {
        "true" => Some(Some(true)),
        "false" => Some(Some(false)),
        _ => None,
    }
}

fn read_stored_all() -> HashMap<PersistedKey, Stored> {
    PersistedKey::ALL
        .into_iter()
        .filter_map(|key| Some((key, read_stored(key)?)))
        .collect()
}

/// Apply `restore` to `key` with whichever tool read it.
fn apply_restore(key: PersistedKey, restore: Restore) -> bool {
    let path = key.dconf_path();
    let done = match restore {
        Restore::Nothing => return true,
        Restore::Reset => {
            settings_command("dconf", &["reset", &path]).is_some()
                || settings_command("gsettings", &["reset", key.schema(), key.key()]).is_some()
        }
        Restore::Write(value) => {
            let value = if value { "true" } else { "false" };
            settings_command("dconf", &["write", &path, value]).is_some()
                || settings_command("gsettings", &["set", key.schema(), key.key(), value]).is_some()
        }
    };
    if done {
        tracing::info!(
            "put {} {} back to its value before Pincer ({restore:?})",
            key.schema(),
            key.key()
        );
    } else {
        tracing::warn!(
            "could not put {} {} back ({restore:?}); it stays as the AT-SPI bus launcher wrote it",
            key.schema(),
            key.key()
        );
    }
    done
}

/// What Pincer owes the user's settings.
#[derive(Default)]
struct SettingsGuard {
    /// Runtimes that advertised; the owed values go back when the last one
    /// shuts down.
    holds: usize,
    /// For each setting Pincer's advertisement changed on a launcher that
    /// follows the settings: the value before, and the value it caused.
    owed: HashMap<PersistedKey, (Stored, Stored)>,
}

fn guard() -> &'static Mutex<SettingsGuard> {
    static GUARD: OnceLock<Mutex<SettingsGuard>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(SettingsGuard::default()))
}

/// After Pincer's own Set of a status property: put back at once what the
/// launcher wrote when it does not read the setting back, else owe it for
/// shutdown.
fn settle_advertisement(before: HashMap<PersistedKey, Stored>, launcher: LauncherKind) {
    for (key, prior) in before {
        let Some(now) = read_stored(key) else {
            continue;
        };
        if restore_for(prior, now) == Restore::Nothing {
            continue;
        }
        match launcher {
            LauncherKind::TracksListeners => {
                apply_restore(key, restore_for(prior, now));
            }
            LauncherKind::FollowsSettings => {
                guard()
                    .lock()
                    .unwrap()
                    .owed
                    .entry(key)
                    .or_insert((prior, now));
            }
        }
    }
}

/// What a listener registration is compared against: taken by
/// [`before_listener_registers`] and handed to [`listener_registered`].
#[derive(Debug)]
pub(crate) struct BeforeListener {
    stored: Stored,
}

/// Whether a launcher in this state writes the setting when Pincer's
/// listener registers: only one that tracks listeners, and only when
/// `IsEnabled` is off (the write comes with the off-to-on change).
fn listener_will_write(is_enabled: Option<bool>, launcher: LauncherKind) -> bool {
    is_enabled == Some(false) && launcher == LauncherKind::TracksListeners
}

/// Read the status and the stored `toolkit-accessibility` right before the
/// shared AT-SPI connection registers its listeners. `None` when the
/// registration will not make the launcher write anything. Blocking.
pub(crate) fn before_listener_registers() -> Option<BeforeListener> {
    let (screen_reader, is_enabled) = read_status()?;
    if !listener_will_write(is_enabled, LauncherKind::from_screen_reader(screen_reader)) {
        return None;
    }
    Some(BeforeListener {
        stored: read_stored(PersistedKey::ToolkitAccessibility)?,
    })
}

/// After the listeners registered: wait for the launcher to switch
/// `IsEnabled` on (it writes the setting first), then put the setting back.
/// This launcher does not read it back, so the live status stays on while
/// Pincer listens. Blocking, bounded by [`LISTENER_SETTLE`].
pub(crate) fn listener_registered(before: BeforeListener) {
    let deadline = Instant::now() + LISTENER_SETTLE;
    loop {
        match read_status() {
            Some((_, Some(true))) => break,
            _ if Instant::now() >= deadline => return,
            _ => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let key = PersistedKey::ToolkitAccessibility;
    if let Some(now) = read_stored(key) {
        apply_restore(key, restore_for(before.stored, now));
    }
}

/// `ScreenReaderEnabled` and `IsEnabled` as the launcher reports them; `None`
/// without a session bus. Blocking: runs its own small runtime, so callers
/// must not be on an async worker.
fn read_status() -> Option<(Option<bool>, Option<bool>)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        let session_bus = tokio::time::timeout(STATUS_TIMEOUT, zbus::Connection::session())
            .await
            .ok()?
            .ok()?;
        let status = zbus::Proxy::new(
            &session_bus,
            ACCESSIBILITY_BUS_SERVICE,
            ACCESSIBILITY_BUS_OBJECT,
            ACCESSIBILITY_STATUS_INTERFACE,
        )
        .await
        .ok()?;
        Some((
            read_flag(&status, SCREEN_READER_ENABLED_PROPERTY).await,
            read_flag(&status, ACCESSIBILITY_IS_ENABLED_PROPERTY).await,
        ))
    })
}

/// Put back, once the last runtime that advertised shuts down, each setting
/// Pincer's advertisement changed on a launcher that follows the settings,
/// unless something else changed it again since. Called by the runtime's
/// shutdown, which a termination signal runs too. Blocking.
pub fn release_session_accessibility() {
    let owed = {
        let mut guard = guard().lock().unwrap();
        if guard.holds == 0 {
            return;
        }
        guard.holds -= 1;
        if guard.holds > 0 {
            return;
        }
        std::mem::take(&mut guard.owed)
    };
    for (key, (prior, caused)) in owed {
        if let Some(restore) = exit_restore(prior, caused, read_stored(key)) {
            apply_restore(key, restore);
        }
    }
}

/// What to do at exit for a setting Pincer changed from `prior` to
/// `caused`, given its value `now`: put it back only while it still holds
/// what Pincer caused.
fn exit_restore(prior: Stored, caused: Stored, now: Option<Stored>) -> Option<Restore> {
    let now = now?;
    (now == caused)
        .then(|| restore_for(prior, now))
        .filter(|restore| *restore != Restore::Nothing)
}

#[cfg(test)]
mod tests {
    use super::{advertise_mode_from, AdvertiseMode};
    use super::{
        advertisement_writes, exit_restore, listener_will_write, parse_dconf_value, restore_for,
        LauncherKind, PersistedKey, Restore, ACCESSIBILITY_IS_ENABLED_PROPERTY,
        SCREEN_READER_ENABLED_PROPERTY,
    };

    // at-spi2-core 2.62 has no ScreenReaderEnabled: mode all set it, got an
    // error, and returned before IsEnabled (seen with dbus-monitor in the
    // sandbox: one Set of ScreenReaderEnabled, none of IsEnabled).
    #[test]
    fn mode_all_still_sets_is_enabled_without_a_screen_reader_property() {
        assert_eq!(
            advertisement_writes(AdvertiseMode::All, None, Some(false)),
            vec![ACCESSIBILITY_IS_ENABLED_PROPERTY]
        );
        assert_eq!(
            advertisement_writes(AdvertiseMode::All, Some(false), Some(false)),
            vec![
                SCREEN_READER_ENABLED_PROPERTY,
                ACCESSIBILITY_IS_ENABLED_PROPERTY
            ]
        );
        // A running screen reader and an enabled status are left alone.
        assert!(advertisement_writes(AdvertiseMode::All, Some(true), Some(true)).is_empty());
        assert_eq!(
            advertisement_writes(AdvertiseMode::IsEnabledOnly, Some(false), Some(false)),
            vec![ACCESSIBILITY_IS_ENABLED_PROPERTY]
        );
        // Mode none writes nothing at all.
        assert!(advertisement_writes(AdvertiseMode::None, Some(false), Some(false)).is_empty());
    }

    #[test]
    fn a_setting_goes_back_to_unset_or_to_its_old_value() {
        assert_eq!(restore_for(None, Some(true)), Restore::Reset);
        assert_eq!(restore_for(Some(false), Some(true)), Restore::Write(false));
        assert_eq!(restore_for(Some(true), Some(true)), Restore::Nothing);
        assert_eq!(restore_for(None, None), Restore::Nothing);
    }

    #[test]
    fn dconf_output_tells_unset_from_false() {
        assert_eq!(parse_dconf_value(""), Some(None));
        assert_eq!(parse_dconf_value("true\n"), Some(Some(true)));
        assert_eq!(parse_dconf_value("false"), Some(Some(false)));
        assert_eq!(parse_dconf_value("'maybe'"), None);
        assert_eq!(
            PersistedKey::ToolkitAccessibility.dconf_path(),
            "/org/gnome/desktop/interface/toolkit-accessibility"
        );
        assert_eq!(
            PersistedKey::ScreenReaderEnabled.dconf_path(),
            "/org/gnome/desktop/a11y/applications/screen-reader-enabled"
        );
    }

    #[test]
    fn only_a_launcher_that_tracks_listeners_writes_for_a_listener() {
        // 2.62: off before, so the first listener switches it on and writes.
        assert!(listener_will_write(
            Some(false),
            LauncherKind::TracksListeners
        ));
        // Already on (another listener, or Pincer's own Set): no change, no write.
        assert!(!listener_will_write(
            Some(true),
            LauncherKind::TracksListeners
        ));
        // 2.60 and older ignore listeners.
        assert!(!listener_will_write(
            Some(false),
            LauncherKind::FollowsSettings
        ));
        assert!(!listener_will_write(None, LauncherKind::TracksListeners));
        assert_eq!(
            LauncherKind::from_screen_reader(Some(false)),
            LauncherKind::FollowsSettings
        );
        assert_eq!(
            LauncherKind::from_screen_reader(None),
            LauncherKind::TracksListeners
        );
    }

    #[test]
    fn exit_puts_back_only_what_still_holds_pincers_value() {
        assert_eq!(
            exit_restore(None, Some(true), Some(Some(true))),
            Some(Restore::Reset)
        );
        assert_eq!(
            exit_restore(Some(false), Some(true), Some(Some(true))),
            Some(Restore::Write(false))
        );
        // Changed again since (by the user): left alone.
        assert_eq!(exit_restore(None, Some(true), Some(Some(false))), None);
        // Unreadable now: left alone.
        assert_eq!(exit_restore(None, Some(true), None), None);
    }

    #[test]
    fn gnome_default_does_not_claim_a_screen_reader() {
        assert_eq!(
            advertise_mode_from(false, None, Some("ubuntu:GNOME")),
            AdvertiseMode::IsEnabledOnly
        );
    }

    #[test]
    fn cosmic_default_does_not_claim_a_screen_reader() {
        assert_eq!(
            advertise_mode_from(false, None, Some("COSMIC")),
            AdvertiseMode::IsEnabledOnly
        );
        assert_eq!(
            advertise_mode_from(false, None, Some("pop:COSMIC")),
            AdvertiseMode::IsEnabledOnly
        );
    }

    #[test]
    fn cinnamon_default_leaves_accessibility_status_untouched() {
        for desktop in ["Cinnamon", "X-Cinnamon", "LinuxMint:X-Cinnamon"] {
            assert_eq!(
                advertise_mode_from(false, None, Some(desktop)),
                AdvertiseMode::None,
                "desktop={desktop}"
            );
        }
    }

    #[test]
    fn cinnamon_safety_wins_in_composite_desktop_names() {
        assert_eq!(
            advertise_mode_from(false, None, Some("GNOME:X-Cinnamon")),
            AdvertiseMode::None
        );
    }

    #[test]
    fn non_gnome_default_preserves_chromium_compatibility() {
        assert_eq!(
            advertise_mode_from(false, None, Some("KDE")),
            AdvertiseMode::All
        );
    }

    #[test]
    fn explicit_mode_overrides_desktop_default() {
        assert_eq!(
            advertise_mode_from(false, Some("all"), Some("GNOME")),
            AdvertiseMode::All
        );
        assert_eq!(
            advertise_mode_from(false, Some("is_enabled_only"), Some("KDE")),
            AdvertiseMode::IsEnabledOnly
        );
        assert_eq!(
            advertise_mode_from(false, Some("none"), Some("KDE")),
            AdvertiseMode::None
        );
    }

    /// The empty-tree hint told KDE users to run a GNOME gsettings command.
    #[test]
    fn kde_gets_kde_advice_for_an_empty_tree() {
        for desktop in ["KDE", "kde", "ubuntu:KDE"] {
            let advice = super::bridge_off_advice(Some(desktop));
            assert!(!advice.contains("gsettings"), "{desktop}: {advice}");
            assert!(advice.contains("IsEnabled"), "{advice}");
            assert!(
                advice.contains("--force-renderer-accessibility"),
                "{advice}"
            );
            assert!(advice.contains("restart it with launch_app"), "{advice}");
            let no_bus = super::no_bus_advice(Some(desktop));
            assert!(!no_bus.contains("gsettings"), "{desktop}: {no_bus}");
            assert!(no_bus.contains("at-spi2-core"), "{no_bus}");
        }
    }

    #[test]
    fn other_desktops_keep_the_gsettings_switch() {
        for desktop in [Some("GNOME"), Some("ubuntu:GNOME"), Some("XFCE"), None] {
            let advice = super::bridge_off_advice(desktop);
            assert!(
                advice.contains(
                    "gsettings set org.gnome.desktop.interface toolkit-accessibility true"
                ),
                "{desktop:?}: {advice}"
            );
            assert!(
                super::no_bus_advice(desktop).contains("gsettings"),
                "{desktop:?}"
            );
        }
    }

    #[test]
    fn legacy_disable_wins_over_explicit_mode() {
        assert_eq!(
            advertise_mode_from(true, Some("all"), Some("KDE")),
            AdvertiseMode::None
        );
    }
}
