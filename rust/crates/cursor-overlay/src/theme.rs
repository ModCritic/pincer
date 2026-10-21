//! Canonical cursor-theme semantics and `cua.default` runtime parameters.
//!
//! The default artwork is authored as a checked-in dotLottie archive and
//! compiled into the bounded artifact embedded by [`crate::theme_artifact`].
//! This module owns transport-neutral state, session colors, and the shared
//! runtime float transform. It does not parse Lottie at runtime.

use serde::{Deserialize, Serialize};

pub use cua_driver_contract::{
    CursorAction, CursorDelivery as DeliveryModifier, CursorPlayback as PlaybackKind,
    CursorReducedMotion as ReducedMotion, CursorTarget as TargetModifier,
};

pub const DEFAULT_THEME_ID: &str = "cua.default";
pub const DEFAULT_THEME_VERSION: &str = "3.0.0";
pub const THEME_PROFILE: &str = "cua-driver-actions-v2";
pub const CANVAS_SIZE: f32 = 128.0;
pub const DISPLAY_SIZE: f32 = 42.0;
const FLOAT_DURATION_SECS: f32 = 4.0;

/// Pincer's main cursor color (#C95E48). It is also the tint key: the
/// default theme paints the cursor body in it, and the runtime replaces it
/// with the session's own color.
pub const DEFAULT_CURSOR_FILL: [u8; 4] = [201, 94, 72, 255];

/// Main colors for the second and later cursors on this desktop: the default
/// turned redder or yellower in turn, further each time. The yellower ones
/// keep its lightness; past pure red the redder ones get deeper instead of
/// pinker.
const SESSION_CURSOR_FILLS: &[[u8; 4]] = &[
    [201, 73, 72, 255],  // #C94948
    [201, 118, 72, 255], // #C97648
    [186, 53, 44, 255],  // #BA352C
    [201, 137, 72, 255], // #C98948
    [161, 38, 42, 255],  // #A1262A
    [201, 154, 72, 255], // #C99A48
    [133, 39, 35, 255],  // #852723
    [201, 169, 72, 255], // #C9A948
];

/// Main color for the cursor holding `slot` (see [`session_slot`]): slot 0
/// keeps the default, and later slots cycle through the variants.
pub fn slot_fill_rgba(slot: usize) -> [u8; 4] {
    match slot.checked_sub(1) {
        None => DEFAULT_CURSOR_FILL,
        Some(index) => SESSION_CURSOR_FILLS[index % SESSION_CURSOR_FILLS.len()],
    }
}

/// Return the main color of one session's cursor. The first cursor drawn on
/// this desktop keeps the default color and later ones get variants, in
/// the order they claimed a slot.
pub fn session_fill_rgba(session_id: &str) -> [u8; 4] {
    slot_fill_rgba(session_slot(session_id))
}

pub fn session_fill_hex(session_id: &str) -> String {
    let [r, g, b, _] = session_fill_rgba(session_id);
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// One cursor's place in the order of cursors on this desktop. `_lock` holds
/// an exclusive lock on the slot's file for as long as the cursor exists.
struct SessionSlot {
    index: usize,
    _lock: Option<std::fs::File>,
}

static SESSION_SLOTS: std::sync::Mutex<Option<std::collections::HashMap<String, SessionSlot>>> =
    std::sync::Mutex::new(None);

/// Slots looked for before giving up on the lock files.
const MAX_SESSION_SLOTS: usize = 64;

/// The slot of `session_id`'s cursor, claimed on first use. Claims are shared
/// by every Pincer process of this user: each holds a lock on the lowest free
/// numbered file in [`session_slot_dir`], so separate agents get separate
/// colors. Without that directory the order is kept within this process.
pub fn session_slot(session_id: &str) -> usize {
    let mut guard = SESSION_SLOTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let slots = guard.get_or_insert_with(Default::default);
    if let Some(slot) = slots.get(session_id) {
        return slot.index;
    }
    let held: Vec<usize> = slots.values().map(|slot| slot.index).collect();
    let slot = claim_session_slot(session_slot_dir().as_deref(), &held);
    let index = slot.index;
    slots.insert(session_id.to_owned(), slot);
    index
}

/// Give up `session_id`'s slot, so the next new cursor can take it.
pub fn release_session_slot(session_id: &str) {
    let mut guard = SESSION_SLOTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(slots) = guard.as_mut() {
        slots.remove(session_id);
    }
}

/// Where the slot lock files live: under `$XDG_RUNTIME_DIR` when it is set,
/// else the user's temporary directory. Unit tests keep slots in memory.
fn session_slot_dir() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    match std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) {
        Some(runtime) if runtime.is_absolute() => Some(runtime.join("pincer").join("cursor-slots")),
        _ => Some(std::env::temp_dir().join("pincer-cursor-slots")),
    }
}

fn claim_session_slot(dir: Option<&std::path::Path>, held: &[usize]) -> SessionSlot {
    if let Some(dir) = dir.filter(|dir| std::fs::create_dir_all(dir).is_ok()) {
        for index in 0..MAX_SESSION_SLOTS {
            if held.contains(&index) {
                continue;
            }
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("{index}.lock")))
            else {
                break;
            };
            match file.try_lock() {
                Ok(()) => {
                    return SessionSlot {
                        index,
                        _lock: Some(file),
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(std::fs::TryLockError::Error(_)) => break,
            }
        }
    }
    let index = (0..).find(|index| !held.contains(index)).unwrap_or(0);
    SessionSlot { index, _lock: None }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CursorVisualState {
    pub requested_action: CursorAction,
    pub resolved_action: CursorAction,
    pub delivery: Option<DeliveryModifier>,
    pub target: Option<TargetModifier>,
    pub elapsed_secs: f64,
    /// Grace period remaining after a short tool call ends, so observe/key
    /// cues remain visible for at least one rendered frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending_secs: Option<f64>,
    pub reduced_motion: ReducedMotion,
    pub preempted_count: u64,
}

impl Default for CursorVisualState {
    fn default() -> Self {
        Self {
            requested_action: CursorAction::Idle,
            resolved_action: CursorAction::Idle,
            delivery: None,
            target: None,
            elapsed_secs: 0.0,
            ending_secs: None,
            reduced_motion: ReducedMotion::Auto,
            preempted_count: 0,
        }
    }
}

impl CursorVisualState {
    pub fn begin(
        &mut self,
        action: CursorAction,
        delivery: Option<DeliveryModifier>,
        target: Option<TargetModifier>,
    ) {
        if self.resolved_action != CursorAction::Idle && self.resolved_action != action {
            self.preempted_count = self.preempted_count.saturating_add(1);
        }
        self.requested_action = action;
        self.resolved_action = action;
        self.delivery = delivery;
        self.target = target;
        self.elapsed_secs = 0.0;
        self.ending_secs = None;
    }

    pub fn end(&mut self, action: CursorAction) {
        if self.resolved_action == action
            && matches!(action.playback(), PlaybackKind::Held | PlaybackKind::Loop)
        {
            self.ending_secs = Some(0.4);
        }
    }

    pub fn to_idle(&mut self) {
        self.requested_action = CursorAction::Idle;
        self.resolved_action = CursorAction::Idle;
        self.delivery = None;
        self.target = None;
        self.elapsed_secs = 0.0;
        self.ending_secs = None;
    }

    pub fn tick(&mut self, dt: f64) {
        let dt = dt.max(0.0);
        self.elapsed_secs = (self.elapsed_secs + dt).min(86_400.0);
        if let Some(remaining) = self.ending_secs {
            if remaining <= dt {
                self.to_idle();
                return;
            }
            self.ending_secs = Some(remaining - dt);
        }
        let action = self.resolved_action;
        if action.playback() == PlaybackKind::OneShot && self.elapsed_secs >= action.duration_secs()
        {
            self.to_idle();
        }
    }

    pub fn phase(&self) -> &'static str {
        match self.resolved_action.playback() {
            PlaybackKind::Resting | PlaybackKind::Loop => "loop",
            PlaybackKind::Held => "sustain",
            PlaybackKind::OneShot => "one_shot",
        }
    }

    pub fn frame(&self) -> u32 {
        let duration = self.resolved_action.duration_secs().max(1.0 / 30.0);
        let elapsed = match self.resolved_action.playback() {
            PlaybackKind::Resting | PlaybackKind::Loop | PlaybackKind::Held => {
                self.elapsed_secs.rem_euclid(duration)
            }
            PlaybackKind::OneShot => self.elapsed_secs.min(duration),
        };
        (elapsed * 30.0).floor() as u32
    }
}

pub(crate) fn shared_float_motion(visual: &CursorVisualState) -> (f32, f32, f32) {
    if visual.reduced_motion == ReducedMotion::On {
        return (0.0, 0.0, 0.0);
    }

    let progress =
        (visual.elapsed_secs as f32).rem_euclid(FLOAT_DURATION_SECS) / FLOAT_DURATION_SECS;
    let angle = progress * std::f32::consts::TAU;
    (
        angle.sin() * 5.0,
        6.0 * angle.cos() - 5.0,
        2.5_f32.to_radians() * angle.cos(),
    )
}

/// Paint one frame of the embedded actions-v2 theme.
///
/// `anchor_x/y` is where the theme hotspot is drawn. `heading` and the
/// display scale pivot around that hotspot.
pub fn paint_default_theme(
    pm: &mut tiny_skia::Pixmap,
    visual: &CursorVisualState,
    anchor_x: f32,
    anchor_y: f32,
    heading: f32,
    backing_scale: f32,
    alpha: f32,
) {
    paint_default_theme_with_fill(
        pm,
        visual,
        anchor_x,
        anchor_y,
        heading,
        backing_scale,
        alpha,
        DEFAULT_CURSOR_FILL,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn paint_default_theme_with_fill(
    pm: &mut tiny_skia::Pixmap,
    visual: &CursorVisualState,
    anchor_x: f32,
    anchor_y: f32,
    heading: f32,
    backing_scale: f32,
    alpha: f32,
    fill_rgba: [u8; 4],
) {
    let theme = crate::theme_artifact::embedded_default_theme();
    crate::theme_artifact::paint_compiled_theme_with_tint(
        pm,
        &theme,
        visual,
        anchor_x,
        anchor_y,
        heading,
        backing_scale,
        alpha,
        Some(fill_rgba),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_uses_compact_42_point_footprint() {
        assert_eq!(DISPLAY_SIZE, 42.0);
    }

    #[test]
    fn first_slot_keeps_the_pincer_color_and_later_ones_alternate() {
        assert_eq!(slot_fill_rgba(0), DEFAULT_CURSOR_FILL);
        assert_eq!(DEFAULT_CURSOR_FILL, [0xC9, 0x5E, 0x48, 255]);
        let [_, base_green, _, _] = DEFAULT_CURSOR_FILL;
        // Odd slots are redder (less green), even slots yellower (more green).
        for slot in 1..=SESSION_CURSOR_FILLS.len() {
            let [red, green, _, _] = slot_fill_rgba(slot);
            if slot % 2 == 1 {
                assert!(green < base_green, "slot {slot} should be redder");
            } else {
                assert!(
                    green > base_green && red == 201,
                    "slot {slot} should be yellower"
                );
            }
        }
        assert_eq!(
            slot_fill_rgba(SESSION_CURSOR_FILLS.len() + 1),
            slot_fill_rgba(1)
        );
    }

    #[test]
    fn named_session_colors_are_stable_and_distinct() {
        assert_eq!(session_fill_rgba("agent-1"), session_fill_rgba("agent-1"));
        assert_ne!(session_fill_rgba("agent-1"), session_fill_rgba("agent-2"));
        release_session_slot("agent-1");
        release_session_slot("agent-2");
    }

    #[test]
    fn slot_files_order_cursors_across_owners_and_free_on_release() {
        let dir = std::env::temp_dir().join(format!("pincer-slot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Separate claims stand in for separate processes: each holds its own
        // lock on its file, so the next claim skips it.
        let first = claim_session_slot(Some(&dir), &[]);
        let second = claim_session_slot(Some(&dir), &[]);
        assert_eq!((first.index, second.index), (0, 1));
        drop(first);
        let third = claim_session_slot(Some(&dir), &[]);
        assert_eq!(third.index, 0, "a released slot is taken again");
        // Without a directory, slots are numbered within the process.
        assert_eq!(claim_session_slot(None, &[0, 1]).index, 2);
        drop((second, third));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_cursor_uses_parameterized_fill_black_ink_and_a_shadow() {
        let mut pixmap = tiny_skia::Pixmap::new(256, 256).unwrap();
        paint_default_theme_with_fill(
            &mut pixmap,
            &CursorVisualState::default(),
            128.0,
            128.0,
            std::f32::consts::FRAC_PI_4,
            2.0,
            1.0,
            [12, 34, 56, 255],
        );

        let pixels = pixmap.data().chunks_exact(4).collect::<Vec<_>>();
        assert!(pixels.iter().any(|pixel| *pixel == [12, 34, 56, 255]));
        assert!(pixels.iter().any(|pixel| *pixel == [0, 0, 0, 255]));
        // A light shadow: translucent black, never as dark as the outline.
        assert!(pixels
            .iter()
            .any(|pixel| pixel[..3] == [0, 0, 0] && pixel[3] > 8 && pixel[3] < 96));
    }

    #[test]
    fn shadow_falls_below_and_right_of_the_pointer() {
        let mut pixmap = tiny_skia::Pixmap::new(256, 256).unwrap();
        let visual = CursorVisualState {
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        paint_default_theme_with_fill(
            &mut pixmap,
            &visual,
            128.0,
            128.0,
            std::f32::consts::FRAC_PI_4,
            2.0,
            1.0,
            [12, 34, 56, 255],
        );

        let bounds = |predicate: &dyn Fn(&[u8]) -> bool| {
            let mut result = (u32::MAX, u32::MAX, 0, 0);
            for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
                if predicate(pixel) {
                    let x = index as u32 % pixmap.width();
                    let y = index as u32 / pixmap.width();
                    result.0 = result.0.min(x);
                    result.1 = result.1.min(y);
                    result.2 = result.2.max(x);
                    result.3 = result.3.max(y);
                }
            }
            result
        };
        let ink = bounds(&|pixel| pixel == [0, 0, 0, 255]);
        let shadow = bounds(&|pixel| pixel[..3] == [0, 0, 0] && pixel[3] > 4 && pixel[3] < 96);

        assert!(
            shadow.2 >= ink.2 + 3 && shadow.3 >= ink.3 + 3,
            "shadow should reach past the pointer's right and bottom: ink={ink:?}, shadow={shadow:?}"
        );
        assert!(
            shadow.0 + 3 >= ink.0 && shadow.1 + 3 >= ink.1,
            "shadow should not spread up or left: ink={ink:?}, shadow={shadow:?}"
        );
        // The outline's tip is the hotspot, drawn at (128, 128).
        assert!(
            ink.0.abs_diff(128) <= 1 && ink.1.abs_diff(128) <= 1,
            "tip at {ink:?}"
        );
    }

    #[test]
    fn action_theme_ignores_host_owned_modifiers() {
        let mut base = tiny_skia::Pixmap::new(128, 128).unwrap();
        let mut pixmap = tiny_skia::Pixmap::new(128, 128).unwrap();
        let base_visual = CursorVisualState {
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        let visual = CursorVisualState {
            delivery: Some(DeliveryModifier::Foreground),
            target: Some(TargetModifier::Pixel),
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        paint_default_theme_with_fill(
            &mut base,
            &base_visual,
            64.0,
            64.0,
            std::f32::consts::FRAC_PI_4,
            1.0,
            1.0,
            [12, 34, 56, 255],
        );
        paint_default_theme_with_fill(
            &mut pixmap,
            &visual,
            64.0,
            64.0,
            std::f32::consts::FRAC_PI_4,
            1.0,
            1.0,
            [12, 34, 56, 255],
        );

        assert_eq!(pixmap.data(), base.data());
    }

    #[test]
    fn every_action_inherits_the_same_floating_base_motion() {
        let idle = CursorVisualState {
            elapsed_secs: 0.75,
            ..CursorVisualState::default()
        };
        let expected = shared_float_motion(&idle);
        assert_ne!(expected, (0.0, 0.0, 0.0));

        for action in CursorAction::ALL {
            let mut visual = CursorVisualState::default();
            visual.begin(action, None, None);
            visual.elapsed_secs = 0.75;
            assert_eq!(
                shared_float_motion(&visual),
                expected,
                "{} did not inherit the shared floating motion",
                action.as_str()
            );
        }
    }

    #[test]
    fn reduced_motion_disables_shared_floating_motion() {
        let visual = CursorVisualState {
            elapsed_secs: 0.75,
            reduced_motion: ReducedMotion::On,
            ..CursorVisualState::default()
        };
        assert_eq!(shared_float_motion(&visual), (0.0, 0.0, 0.0));
    }

    #[test]
    fn one_shot_returns_to_idle() {
        let mut state = CursorVisualState::default();
        state.begin(CursorAction::Click, None, Some(TargetModifier::Pixel));
        state.tick(CursorAction::Click.duration_secs() + 0.01);
        assert_eq!(state.resolved_action, CursorAction::Idle);
        assert_eq!(state.target, None);
    }

    #[test]
    fn held_action_waits_for_matching_end() {
        let mut state = CursorVisualState::default();
        state.begin(CursorAction::Text, Some(DeliveryModifier::Foreground), None);
        state.tick(30.0);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.end(CursorAction::Click);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.end(CursorAction::Text);
        assert_eq!(state.resolved_action, CursorAction::Text);
        state.tick(0.41);
        assert_eq!(state.resolved_action, CursorAction::Idle);
    }

    #[test]
    fn every_action_renders_pixels_at_one_and_two_x() {
        for action in CursorAction::ALL {
            for scale in [1.0, 2.0] {
                let mut pm = tiny_skia::Pixmap::new(256, 256).unwrap();
                let mut visual = CursorVisualState::default();
                visual.begin(
                    action,
                    Some(DeliveryModifier::Background),
                    Some(TargetModifier::Ax),
                );
                visual.elapsed_secs = action.duration_secs() * 0.4;
                paint_default_theme(&mut pm, &visual, 128.0, 128.0, 0.0, scale, 1.0);
                assert!(
                    pm.data().chunks_exact(4).any(|pixel| pixel[3] != 0),
                    "{} did not render at {scale}x",
                    action.as_str()
                );
            }
        }
    }
}
