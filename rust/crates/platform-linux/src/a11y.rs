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

use std::sync::Once;

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

    // Don't clobber a screen reader the user is already running: only write when
    // a flag is currently false, so an active Orca session stays authoritative
    // and we avoid emitting a redundant PropertiesChanged.
    if mode == AdvertiseMode::All && !is_flag_set(&status, SCREEN_READER_ENABLED_PROPERTY).await {
        status
            .set_property(SCREEN_READER_ENABLED_PROPERTY, true)
            .await?;
    }
    if !is_flag_set(&status, ACCESSIBILITY_IS_ENABLED_PROPERTY).await {
        status
            .set_property(ACCESSIBILITY_IS_ENABLED_PROPERTY, true)
            .await?;
    }
    Ok(())
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

/// Read a boolean status property, treating an unreadable property as unset so
/// the caller falls through to writing it.
async fn is_flag_set(status: &zbus::Proxy<'_>, property: &str) -> bool {
    status.get_property::<bool>(property).await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{advertise_mode_from, AdvertiseMode};

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
            assert!(advice.contains("--force-renderer-accessibility"), "{advice}");
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
                advice.contains("gsettings set org.gnome.desktop.interface toolkit-accessibility true"),
                "{desktop:?}: {advice}"
            );
            assert!(super::no_bus_advice(desktop).contains("gsettings"), "{desktop:?}");
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
