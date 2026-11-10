import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import Pango from 'gi://Pango';
import PangoCairo from 'gi://PangoCairo';
import Cairo from 'cairo';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

Gio._promisify(Shell.Screenshot.prototype, 'screenshot');
Gio._promisify(Shell.Screenshot, 'composite_to_stream');

const IFACE = `<node><interface name="org.pincer.WinRects">
<method name="GetVersion"><arg type="u" direction="out" name="version"/></method>
<method name="GetRects"><arg type="s" direction="out" name="json"/></method>
<method name="Capture"><arg type="s" direction="out" name="png_base64"/></method>
<method name="CaptureWindow"><arg type="u" direction="in" name="id"/><arg type="u" direction="in" name="pid"/><arg type="s" direction="out" name="png_base64"/></method>
<method name="CaptureWindowPreview"><arg type="u" direction="in" name="id"/><arg type="u" direction="in" name="pid"/><arg type="u" direction="in" name="max_dimension"/><arg type="s" direction="out" name="png_base64"/><arg type="u" direction="out" name="width"/><arg type="u" direction="out" name="height"/></method>
<method name="Activate"><arg type="u" direction="in" name="id"/><arg type="b" direction="out" name="activated"/></method>
<method name="MoveCursor"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="ClickPulse"><arg type="i" direction="in" name="x"/><arg type="i" direction="in" name="y"/></method>
<method name="SetCursorColor"><arg type="s" direction="in" name="fill_color"/></method>
<method name="SetCursorState"><arg type="s" direction="in" name="action"/><arg type="s" direction="in" name="delivery"/><arg type="s" direction="in" name="target"/><arg type="b" direction="in" name="active"/></method>
<method name="SetSessionLabel"><arg type="s" direction="in" name="label"/></method>
<method name="ShowMinimalChip"><arg type="as" direction="in" name="keys"/><arg type="b" direction="in" name="mac"/><arg type="b" direction="in" name="foreground"/></method>
<method name="HideCursor"></method>
</interface></node>`;

// GNOME cannot host the Rust renderer, so this Shell actor draws the agent
// cursor itself: the pincer.default theme that the Rust overlay draws on X11
// and elsewhere, ported by hand from
// rust/crates/cursor-overlay/assets/build_default_theme.py. The dart, its
// outline and drop shadow, each action's marks and their keyframes all come
// from that script, so it and this port must change together.
const CANVAS_SIZE = 128;
const DISPLAY_SIZE = 42;
const SCALE = DISPLAY_SIZE / CANVAS_SIZE;
const THEME_FPS = 30;
// Every layer of the theme scales and turns about the canvas center.
const LAYER_ANCHOR = CANVAS_SIZE / 2;
// The outline's tip, drawn on the pointer's point.
const HOTSPOT = [42, 28];
// MAIN, the body's fill, is the tint key: the session color replaces it.
const DEFAULT_CURSOR_FILL = [201, 94, 72];
const THEME_ACCENT = [226, 129, 80];
const THEME_OUTLINE = [43, 0, 0];
const OUTLINE_WIDTH = 5;
// tiny-skia's miter limit, which keeps the outline's tip and notch pointed.
const OUTLINE_MITER_LIMIT = 4;
const SHADOW_OFFSET = [5, 6];
const SHADOW_STEPS = 8;
const FLOAT_DURATION = 4.0;
// The motion effects of the default motion style, signature_arc, as
// pincer-cursor-motion's effects.rs shapes them: a speed glow while the
// cursor glides, and a ripple and a squish on a click, in the session color
// lifted 45% toward white.
const EFFECT_LIFT = 0.45;
const POINTER_ANCHOR_OFFSET = 16;
const RIPPLE_SECONDS = 0.52;
const CLICK_FX_SECONDS = 0.55;
const SQUISH = 0.12;
// The cursor's actor leaves room around the pointer for the speed glow. The
// ripple has an actor of its own at the click point, since the cursor can
// still be gliding there; the ring reaches 53 px out.
const ACTOR_SIZE = 160;
const ACTOR_CENTER = ACTOR_SIZE / 2;
const RIPPLE_ACTOR_SIZE = 112;
const BADGE_Y_OFFSET = 29;
const BADGE_HOLD_SECONDS = 2.0;
const BADGE_FADE_SECONDS = 0.4;
// Badge geometry and colors follow cursor-overlay's session_badge.rs.
const BADGE_HEIGHT = 28;
const BADGE_MAX_WIDTH = 188;
const BADGE_CHIP_SIZE = 18;
const BADGE_CHIP_GAP = 4;
const BADGE_CHIP_GROUP_GAP = 7;
// St's CSS takes a quoted family only first in the list, so the names are bare.
const BADGE_FONT = 'font-family: Inter, Adwaita Sans, sans-serif; font-size: 11.5px';
// The word pill: the session color with white text and a darker rim.
const PILL_FILL_ALPHA = 245 / 255;
const PILL_RIM_SHADE = 0.35;
const PILL_RIM_ALPHA = 220 / 255;
// Filled chips on the pill sit in a darker shade of the session color.
const PILL_CHIP_SHADE = 0.4;
// Key combos: one light cap per key over a session-colored bevel, in a dark
// tray. Cap, bevel and padding fill exactly the badge height.
const KEYCAP_FONT_FAMILY = 'Inter, Adwaita Sans, sans-serif';
const KEYCAP_FONT_SIZE = 10.5;
const KEYCAP_HEIGHT = 18;
const KEYCAP_BEVEL = 2;
const KEYCAP_RADIUS = 4.5;
const KEYCAP_TEXT_PADDING = 5;
const KEYCAP_JOIN_WIDTH = 10;
const KEYCAP_MAC_GAP = 3;
const KEYCAP_TRAY_PADDING = (BADGE_HEIGHT - KEYCAP_HEIGHT - KEYCAP_BEVEL) / 2;
const KEYCAP_TRAY_RADIUS = 8;
const KEYCAP_TRAY = [22, 22, 26, 215];
const KEYCAP_FACE = [246, 246, 248];
const KEYCAP_TEXT = [30, 30, 36];
const KEYCAP_JOIN_TEXT = [196, 196, 204];
const KEYCAP_MAX_KEYS = 8;
const KEYCAP_MAX_KEY_CHARS = 32;
// The soft black shadow under the tray, and room for it around the tray.
const SHADOW_ALPHA = 10;
const SHADOW_MARGIN = 10;
// The minimal badge's chip (render_state.rs MINIMAL_CHIP_*): after its
// action ends and while the cursor is at rest, a short delay, a quick fade
// in, a hold, and a fade out.
const MINIMAL_CHIP_DELAY_SECONDS = 0.12;
const MINIMAL_CHIP_FADE_IN_SECONDS = 0.08;
const MINIMAL_CHIP_HOLD_SECONDS = 0.5;
const MINIMAL_CHIP_FADE_OUT_SECONDS = 0.3;
const MINIMAL_CHIP_SECONDS =
  MINIMAL_CHIP_DELAY_SECONDS +
  MINIMAL_CHIP_FADE_IN_SECONDS +
  MINIMAL_CHIP_HOLD_SECONDS +
  MINIMAL_CHIP_FADE_OUT_SECONDS;
// An unarmed chip plays anyway after this long, in case its action's end
// never arrives.
const MINIMAL_CHIP_ARM_TIMEOUT_SECONDS = 10;
const FOREGROUND_CHIP_LABEL = 'foreground';
// MoveCursor glides for this long; the minimal chip waits for rest.
const CURSOR_GLIDE_MS = 480;
const FRAME_INTERVAL_MS = 33;
const ACTIONS = new Set([
  'idle',
  'observe',
  'click',
  'drag',
  'scroll',
  'text',
  'key',
  'navigate',
  'app',
  'transfer',
  'record',
  'system',
]);
const ONE_SHOT_ACTIONS = new Set(['click', 'key', 'navigate', 'app', 'system']);
const ACTION_DURATIONS = {
  idle: 4.0,
  click: 0.67,
  observe: 1.6,
  drag: 1.6,
  scroll: 1.6,
  text: 1.6,
  key: 1.6,
  navigate: 1.6,
  app: 1.6,
  transfer: 1.6,
  record: 1.6,
  system: 1.6,
};

function nowSeconds() {
  return GLib.get_monotonic_time() / 1_000_000;
}

function mixChannel(base, accent, weight) {
  return Math.round(base * (1 - weight) + accent * weight);
}

/** `color` moved `amount` of the way toward black. */
function shaded(color, amount) {
  return color.map((channel) => mixChannel(channel, 0, amount));
}

/**
 * The word pill, as paint_session_badge draws it: the session color with a
 * 1 px rim of that color darkened 35% and a soft black shadow. St draws the
 * border inside the box, so 9 px of padding puts content 10 px from the edge.
 */
function badgeStyle(fillColor) {
  const rim = shaded(fillColor, PILL_RIM_SHADE);
  return [
    `spacing: ${BADGE_CHIP_GROUP_GAP}px`,
    'padding: 0 9px',
    `background-color: rgba(${fillColor.join(', ')}, ${PILL_FILL_ALPHA.toFixed(3)})`,
    `border: 1px solid rgba(${rim.join(', ')}, ${PILL_RIM_ALPHA.toFixed(3)})`,
    `border-radius: ${BADGE_HEIGHT / 2}px`,
    'box-shadow: 0 2px 8px rgba(0, 0, 0, 0.2)',
    // St limits the content box; with the padding and border the pill is at
    // most BADGE_MAX_WIDTH wide, and the label ellipsizes.
    `max-width: ${BADGE_MAX_WIDTH - 20}px`,
  ].join('; ');
}

/**
 * Keep `actor` centered on its x position at whatever width St allocates,
 * which is only final once its label has its style and text.
 */
function centerOnX(actor) {
  actor.connect('notify::width', () => {
    actor.translation_x = -Math.round(actor.width / 2);
  });
}

function badgeLabelStyle() {
  return `${BADGE_FONT}; font-weight: normal; color: white;`;
}

/** Clean key labels from D-Bus: no control characters, bounded in count and length. */
function sanitizeKeys(keys) {
  if (!Array.isArray(keys)) return [];
  return keys
    .filter((key) => typeof key === 'string')
    .map((key) =>
      Array.from(key.replace(/[\u0000-\u001f\u007f-\u009f]/g, ''))
        .slice(0, KEYCAP_MAX_KEY_CHARS)
        .join('')
    )
    .filter((key) => key.length > 0)
    .slice(0, KEYCAP_MAX_KEYS);
}

function keycapFontDescription() {
  const description = Pango.FontDescription.from_string(KEYCAP_FONT_FAMILY);
  description.set_absolute_size(KEYCAP_FONT_SIZE * Pango.SCALE);
  return description;
}

/** Ink box of `text` in px, relative to the layout origin. */
function inkBounds(layout, text) {
  layout.set_text(text, -1);
  const [ink] = layout.get_extents();
  return {
    x: ink.x / Pango.SCALE,
    y: ink.y / Pango.SCALE,
    width: ink.width / Pango.SCALE,
    height: ink.height / Pango.SCALE,
  };
}

/**
 * Lay `keys` out as keycaps in a tray, as keycap_badge_layout does: one cap
 * per key, a "+" between caps unless `mac`, then the foreground chip. Null
 * when the tray would be wider than the badge allows, so the caller shows
 * the combo's text in the pill instead. Coordinates are relative to the
 * tray's top-left corner.
 */
function keycapLayout(keys, mac, foreground) {
  const surface = new Cairo.ImageSurface(Cairo.Format.ARGB32, 1, 1);
  const cr = new Cairo.Context(surface);
  try {
    const layout = PangoCairo.create_layout(cr);
    layout.set_font_description(keycapFontDescription());
    const joinWidth = mac ? KEYCAP_MAC_GAP : KEYCAP_JOIN_WIDTH;
    // Whole-pixel cap widths keep the small caps' edges crisp.
    const measured = keys.map((key) => {
      const ink = inkBounds(layout, key);
      return {
        key,
        ink,
        width: Math.ceil(Math.max(ink.width + KEYCAP_TEXT_PADDING * 2, KEYCAP_HEIGHT)),
      };
    });
    if (measured.length === 0) return null;
    const capsWidth =
      measured.reduce((sum, cap) => sum + cap.width, 0) + joinWidth * (measured.length - 1);
    const chipsWidth = foreground ? BADGE_CHIP_GROUP_GAP + BADGE_CHIP_SIZE : 0;
    const width = capsWidth + chipsWidth + KEYCAP_TRAY_PADDING * 2;
    if (width > BADGE_MAX_WIDTH) return null;

    const faceY = KEYCAP_TRAY_PADDING;
    const faceCenter = faceY + KEYCAP_HEIGHT / 2;
    // The "+" sits on the middle of the whole cap, bevel included.
    const joinCenter = faceY + (KEYCAP_HEIGHT + KEYCAP_BEVEL) / 2;
    const capital = inkBounds(layout, 'H');
    const plus = inkBounds(layout, '+');
    const caps = [];
    const joins = [];
    let x = KEYCAP_TRAY_PADDING;
    measured.forEach((cap, index) => {
      if (index > 0) {
        if (!mac) {
          joins.push({
            x: Math.round(x + (joinWidth - plus.width) / 2 - plus.x),
            y: Math.round(joinCenter - plus.height / 2 - plus.y),
          });
        }
        x += joinWidth;
      }
      // Keys with a letter or digit share one baseline; symbol keys such
      // as ⌘ or + center their own ink.
      const ink = /[\p{L}\p{N}]/u.test(cap.key) ? capital : cap.ink;
      caps.push({
        key: cap.key,
        x,
        width: cap.width,
        textX: Math.round(x + (cap.width - cap.ink.width) / 2 - cap.ink.x),
        textY: Math.round(faceCenter - ink.height / 2 - ink.y),
      });
      x += cap.width;
    });
    return {
      width,
      caps,
      joins,
      chipX: foreground ? x + BADGE_CHIP_GROUP_GAP : null,
    };
  } finally {
    cr.$dispose();
    surface.finish();
  }
}

/** The soft black shadow paint_diffuse_glow draws under a badge. */
function drawSoftShadow(cr, x, y, width, height, radius) {
  for (let step = 8; step >= 1; step--) {
    roundedRect(
      cr,
      x - step,
      y - step * 0.55 + 2,
      width + step * 2,
      height + step * 1.1,
      radius + step
    );
    cr.setSourceRGBA(0, 0, 0, Math.round(SHADOW_ALPHA * (0.35 + (8 - step) * 0.085)) / 255);
    cr.fill();
  }
}

/**
 * The part of a keycap's bevel that shows below its face, as keycap_bevel
 * traces it: the face's lower outline moved down by `depth`, closed along the
 * face's own lower outline raised by `overlap`.
 */
function keycapBevelPath(cr, x, y, width, height, radius, depth, overlap) {
  const k = 0.5522848;
  const r = Math.max(0, Math.min(radius, width / 2, height / 2));
  const bottom = y + height;
  const low = bottom + depth;
  const high = bottom - overlap;
  cr.moveTo(x, high - r);
  cr.lineTo(x, low - r);
  cr.curveTo(x, low - r + r * k, x + r - r * k, low, x + r, low);
  cr.lineTo(x + width - r, low);
  cr.curveTo(x + width - r + r * k, low, x + width, low - r + r * k, x + width, low - r);
  cr.lineTo(x + width, high - r);
  cr.curveTo(x + width, high - r + r * k, x + width - r + r * k, high, x + width - r, high);
  cr.lineTo(x + r, high);
  cr.curveTo(x + r - r * k, high, x, high - r + r * k, x, high - r);
  cr.closePath();
}

/**
 * Paint a keycap tray from keycapLayout at (`x`, `y`): the dark tray, a light
 * face per key over a bevel in the session color, the "+" joins, and the
 * foreground chip in a well of the session color itself.
 */
function drawKeycapTray(cr, x, y, keycaps, fillColor) {
  cr.save();
  cr.translate(x, y);
  drawSoftShadow(cr, 0, 0, keycaps.width, BADGE_HEIGHT, KEYCAP_TRAY_RADIUS);
  roundedRect(cr, 0, 0, keycaps.width, BADGE_HEIGHT, KEYCAP_TRAY_RADIUS);
  setFill(cr, KEYCAP_TRAY, KEYCAP_TRAY[3] / 255);
  cr.fill();
  const layout = PangoCairo.create_layout(cr);
  layout.set_font_description(keycapFontDescription());
  for (const cap of keycaps.caps) {
    keycapBevelPath(
      cr,
      cap.x,
      KEYCAP_TRAY_PADDING,
      cap.width,
      KEYCAP_HEIGHT,
      KEYCAP_RADIUS,
      KEYCAP_BEVEL,
      0.75
    );
    setFill(cr, fillColor);
    cr.fill();
    roundedRect(cr, cap.x, KEYCAP_TRAY_PADDING, cap.width, KEYCAP_HEIGHT, KEYCAP_RADIUS);
    setFill(cr, KEYCAP_FACE);
    cr.fill();
    layout.set_text(cap.key, -1);
    cr.moveTo(cap.textX, cap.textY);
    setFill(cr, KEYCAP_TEXT);
    PangoCairo.show_layout(cr, layout);
  }
  layout.set_text('+', -1);
  for (const join of keycaps.joins) {
    cr.moveTo(join.x, join.y);
    setFill(cr, KEYCAP_JOIN_TEXT);
    PangoCairo.show_layout(cr, layout);
  }
  if (keycaps.chipX !== null) {
    cr.translate(keycaps.chipX, (BADGE_HEIGHT - BADGE_CHIP_SIZE) / 2);
    // The dark tray shows the session color itself well.
    drawBadgeChip(cr, 'foreground', true, fillColor);
  }
  cr.restore();
}

/** Opacity of the minimal chip `secs` into its timeline. */
function minimalChipAlpha(secs) {
  const fadeInEnd = MINIMAL_CHIP_DELAY_SECONDS + MINIMAL_CHIP_FADE_IN_SECONDS;
  const holdEnd = fadeInEnd + MINIMAL_CHIP_HOLD_SECONDS;
  if (secs < MINIMAL_CHIP_DELAY_SECONDS) return 0;
  if (secs < fadeInEnd)
    return easeInOut((secs - MINIMAL_CHIP_DELAY_SECONDS) / MINIMAL_CHIP_FADE_IN_SECONDS);
  if (secs < holdEnd) return 1;
  return 1 - easeInOut((secs - holdEnd) / MINIMAL_CHIP_FADE_OUT_SECONDS);
}

function easeInOut(value) {
  const t = Math.max(0, Math.min(1, value));
  return t * t * (3 - 2 * t);
}

function setFill(cr, color, alpha = 1) {
  cr.setSourceRGBA(color[0] / 255, color[1] / 255, color[2] / 255, alpha);
}

function roundedRect(cr, x, y, width, height, radius) {
  const r = Math.max(0, Math.min(radius, width / 2, height / 2));
  const k = 0.5522848;
  cr.moveTo(x + r, y);
  cr.lineTo(x + width - r, y);
  cr.curveTo(x + width - r + r * k, y, x + width, y + r - r * k, x + width, y + r);
  cr.lineTo(x + width, y + height - r);
  cr.curveTo(
    x + width,
    y + height - r + r * k,
    x + width - r + r * k,
    y + height,
    x + width - r,
    y + height
  );
  cr.lineTo(x + r, y + height);
  cr.curveTo(x + r - r * k, y + height, x, y + height - r + r * k, x, y + height - r);
  cr.lineTo(x, y + r);
  cr.curveTo(x, y + r - r * k, x + r - r * k, y, x + r, y);
  cr.closePath();
}

function roundTo2(value) {
  return Math.round(value * 100) / 100;
}

// ── The pincer.default theme ────────────────────────────────────────────
//
// Everything below is in the theme's 128-unit canvas, ported from
// build_default_theme.py. The Rust overlay plays a compiled copy of it
// (pincer.default.pincer-theme, built by cursor-theme-cli), sampled at whole
// frames of 30 fps, and so does this port.

// CURSOR_PATH: the dart, with its tip, its notch and two rounded back
// corners, as Lottie vertices with in and out tangents. Reprint it from the
// assets directory with
// python3 -c 'import build_default_theme as b; print(b.CURSOR_PATH["ks"]["k"])'
const CURSOR_PATH = {
  v: [[46.26, 32.26], [71.085, 98.652], [73.406, 98.704], [85.15, 71.15], [112.704, 59.406], [112.652, 57.085]],
  i: [[0, 0], [0, 0], [-0.443, 1.038], [0, 0], [0, 0], [1.057, 0.395]],
  o: [[0, 0], [0.395, 1.057], [0, 0], [0, 0], [1.038, -0.443], [0, 0]],
};

/** Trace a closed Lottie path as tiny-skia's append_geometry does. */
function traceCursorPath(cr) {
  const { v, i, o } = CURSOR_PATH;
  cr.moveTo(v[0][0], v[0][1]);
  for (let index = 0; index < v.length; index++) {
    const next = (index + 1) % v.length;
    const [x0, y0] = v[index];
    const [x1, y1] = v[next];
    const out = o[index];
    const incoming = i[next];
    if (out[0] || out[1] || incoming[0] || incoming[1])
      cr.curveTo(x0 + out[0], y0 + out[1], x1 + incoming[0], y1 + incoming[1], x1, y1);
    else cr.lineTo(x1, y1);
  }
  cr.closePath();
}

// Shapes of the action marks, as functions that add to the current path.
function polyline(...points) {
  return (cr) => {
    cr.moveTo(points[0][0], points[0][1]);
    for (const [x, y] of points.slice(1)) cr.lineTo(x, y);
  };
}

/** A two-vertex Lottie path: from `start` along `out`, into `end` along `incoming`. */
function curve(start, out, incoming, end) {
  return (cr) => {
    const [x0, y0] = start;
    cr.moveTo(x0, y0);
    const [x1, y1] = end;
    cr.curveTo(x0 + out[0], y0 + out[1], x1 + incoming[0], y1 + incoming[1], x1, y1);
  };
}

/** rectangle(): a square of `size` centered on (x, y), its corners rounded. */
function square(x, y, size, roundness) {
  return (cr) => roundedRect(cr, x - size / 2, y - size / 2, size, size, roundness);
}

/** ellipse(): a circle `size` across, traced as tiny-skia's append_ellipse does. */
function circle(x, y, size) {
  return (cr) => {
    const r = size / 2;
    const k = 0.5522848 * r;
    cr.moveTo(x + r, y);
    cr.curveTo(x + r, y + k, x + k, y + r, x, y + r);
    cr.curveTo(x - k, y + r, x - r, y + k, x - r, y);
    cr.curveTo(x - r, y - k, x - k, y - r, x, y - r);
    cr.curveTo(x + k, y - r, x + r, y - k, x + r, y);
    cr.closePath();
  };
}

/** A keyframe track, as animated() writes it: [frame, value] pairs. */
function track(keys, linear = false) {
  return { keys, linear };
}

/** Whether `value` is a track rather than a constant number or point. */
function isTrack(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

// The handles animated() gives every keyframe but the last. The compiler
// eases into the last one, which has no in-handle, with (1, 1).
const EASED_HANDLES = { out: [0.2, 0.75], in: [0.25, 1] };
const LINEAR_HANDLES = { out: [0.167, 0.167], in: [0.833, 0.833] };

function cubicCoordinate(t, first, second) {
  const u = 1 - t;
  return 3 * u * u * t * first + 3 * u * t * t * second + t * t * t;
}

/** cursor-theme-cli's eased_progress: solve the handles' x by bisection, then read y. */
function easedProgress(progress, [x1, y1], [x2, y2]) {
  const target = Math.max(0, Math.min(1, progress));
  let low = 0;
  let high = 1;
  for (let step = 0; step < 14; step++) {
    const middle = (low + high) / 2;
    if (cubicCoordinate(middle, x1, x2) < target) low = middle;
    else high = middle;
  }
  return Math.max(0, Math.min(1, cubicCoordinate((low + high) / 2, y1, y2)));
}

/** `value` at `frame`: a constant as it is, a track as the compiler samples it. */
function sample(value, frame) {
  if (!isTrack(value)) return value;
  const { keys, linear } = value;
  if (frame <= keys[0][0]) return keys[0][1];
  for (let index = 0; index + 1 < keys.length; index++) {
    const [start, from] = keys[index];
    const [end, to] = keys[index + 1];
    if (frame >= end) continue;
    const handles = linear ? LINEAR_HANDLES : EASED_HANDLES;
    const progress = easedProgress(
      end > start ? (frame - start) / (end - start) : 0,
      handles.out,
      index + 2 < keys.length ? handles.in : [1, 1]
    );
    return Array.isArray(from)
      ? from.map((component, axis) => component + (to[axis] - component) * progress)
      : from + (to - from) * progress;
  }
  return keys[keys.length - 1][1];
}

/** faster_fade: a mark's shadow fades with each keyframe's opacity squared. */
function fasterFade(opacity) {
  if (opacity === undefined) return undefined;
  if (!isTrack(opacity)) return roundTo2((opacity * opacity) / 100);
  return track(
    opacity.keys.map(([frame, value]) => [frame, roundTo2((value * value) / 100)]),
    opacity.linear
  );
}

/** A layer's transform at `frame` (shape_layer), moved by `offset` for its shadow. */
function applyLayerTransform(cr, layer, frame, offset = [0, 0]) {
  const [x, y] = sample(layer.position ?? [LAYER_ANCHOR, LAYER_ANCHOR], frame);
  const scale = sample(layer.scale ?? 100, frame) / 100;
  cr.translate(x + offset[0], y + offset[1]);
  cr.rotate((sample(layer.rotation ?? 0, frame) * Math.PI) / 180);
  cr.scale(scale, scale);
  cr.translate(-LAYER_ANCHOR, -LAYER_ANCHOR);
}

/**
 * soft_shadow: SHADOW_STEPS black strokes over the current path, each wider
 * by 2 * spread / SHADOW_STEPS, that build up to `peak` percent where they
 * all overlap. `width` is the stroke of the shape casting it (0 when filled).
 */
function strokeSoftShadow(cr, width, spread, peak, opacity) {
  const each = roundTo2(100 * (1 - (1 - peak / 100) ** (1 / SHADOW_STEPS))) / 100;
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  cr.setSourceRGBA(0, 0, 0, each * opacity);
  for (let step = 1; step <= SHADOW_STEPS; step++) {
    cr.setLineWidth(width + (2 * spread * step) / SHADOW_STEPS);
    cr.strokePreserve();
  }
  cr.newPath();
}

/**
 * cursor_layers: the dart's shadow, offset down and right, then its body in
 * the session color with the dark outline. `motion` holds the action's
 * keyframes for the dart (the click's press, the drag's tug).
 */
function drawCursorLayers(cr, motion, frame, fillColor) {
  cr.save();
  applyLayerTransform(cr, motion, frame, SHADOW_OFFSET);
  traceCursorPath(cr);
  cr.setSourceRGBA(0, 0, 0, 0.08);
  cr.fillPreserve();
  strokeSoftShadow(cr, 0, 8, 20, 1);
  cr.restore();

  cr.save();
  applyLayerTransform(cr, motion, frame);
  traceCursorPath(cr);
  setFill(cr, fillColor);
  cr.fillPreserve();
  cr.setLineWidth(OUTLINE_WIDTH);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.MITER);
  cr.setMiterLimit(OUTLINE_MITER_LIMIT);
  setFill(cr, THEME_OUTLINE);
  cr.stroke();
  cr.restore();
}

/**
 * cue_layers: one action mark, its soft shadow first, then its shapes
 * stroked in the accent color (or filled, for the record dot). The accent is
 * the same for every session.
 */
function drawMark(cr, mark, frame) {
  const opacity = sample(mark.opacity ?? 100, frame) / 100;
  const shadowOpacity = sample(fasterFade(mark.opacity) ?? 100, frame) / 100;
  const trace = () => {
    for (const shape of mark.shapes) shape(cr);
  };
  if (shadowOpacity > 0) {
    cr.save();
    applyLayerTransform(cr, mark, frame, SHADOW_OFFSET);
    trace();
    if (mark.filled) {
      cr.setSourceRGBA(0, 0, 0, 0.08 * shadowOpacity);
      cr.fillPreserve();
    }
    strokeSoftShadow(cr, mark.filled ? 0 : mark.width, 4, 22, shadowOpacity);
    cr.restore();
  }
  if (opacity <= 0) return;
  cr.save();
  applyLayerTransform(cr, mark, frame);
  trace();
  setFill(cr, THEME_ACCENT, opacity);
  if (mark.filled) {
    cr.fill();
  } else {
    cr.setLineWidth(mark.width);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.setLineJoin(Cairo.LineJoin.ROUND);
    cr.stroke();
  }
  cr.restore();
}

const DRAG_MOVEMENT = track([
  [0, [64, 64]],
  [8, [64, 64]],
  [30, [71, 67]],
  [40, [71, 67]],
  [48, [64, 64]],
]);

/**
 * Each action's animation (action_<name> in build_default_theme.py): its
 * length in frames, the dart's keyframes, and its marks with their shapes,
 * stroke width and keyframes.
 */
const THEME_ACTIONS = {
  idle: { frames: 1, marks: [] },
  observe: {
    frames: 48,
    marks: [
      {
        shapes: [
          curve([38, 28], [-11, 1], [0, -11], [20, 49]),
          curve([42, 19], [-19, 0], [0, -18], [11, 51]),
        ],
        width: 4,
        position: [72, 54],
        scale: track([[0, 88], [24, 100], [48, 108]]),
        opacity: track([[0, 0], [7, 100], [34, 100], [44, 0], [48, 0]]),
      },
    ],
  },
  click: {
    frames: 20,
    cursor: { scale: track([[0, 100], [7, 93], [12, 103], [20, 100]]) },
    marks: [
      {
        shapes: [polyline([35, 20], [34, 11]), polyline([27, 25], [19, 19]), polyline([25, 34], [15, 34])],
        width: 4,
        position: [74, 67],
        scale: track([[0, 110], [8, 130], [20, 150]]),
        opacity: track([[0, 0], [4, 100], [11, 100], [20, 0]]),
      },
    ],
  },
  drag: {
    frames: 48,
    cursor: { position: DRAG_MOVEMENT },
    marks: [
      {
        shapes: [polyline([28, 38], [16, 35]), polyline([26, 48], [12, 45])],
        width: 4,
        position: DRAG_MOVEMENT,
        opacity: track([[0, 20], [15, 100], [36, 100], [48, 20]]),
      },
    ],
  },
  scroll: {
    frames: 48,
    marks: [
      {
        shapes: [polyline([23, 31], [31, 22], [39, 31]), polyline([23, 49], [31, 58], [39, 49])],
        width: 4,
        position: track([[0, [59, 68]], [24, [59, 60]], [48, [59, 68]]]),
        opacity: track([[0, 42], [24, 100], [48, 42]]),
      },
    ],
  },
  text: {
    frames: 48,
    marks: [
      {
        shapes: [polyline([31, 22], [31, 58]), polyline([24, 22], [38, 22]), polyline([24, 58], [38, 58])],
        width: 4,
        position: [60, 64],
        opacity: track([[0, 100], [16, 100], [17, 18], [30, 18], [31, 100], [48, 100]], true),
      },
    ],
  },
  key: {
    frames: 48,
    marks: [
      {
        shapes: [
          square(28, 39, 28, 6),
          polyline([23, 32], [23, 46]),
          polyline([24, 39], [33, 32]),
          polyline([25, 39], [34, 46]),
        ],
        width: 3.5,
        position: track([[0, [55, 64]], [8, [55, 67]], [17, [55, 63]], [26, [55, 64]], [48, [55, 64]]]),
        scale: track([[0, 100], [8, 92], [17, 103], [26, 100], [48, 100]]),
        opacity: track([[0, 0], [4, 100], [30, 100], [42, 0], [48, 0]]),
      },
    ],
  },
  navigate: {
    frames: 48,
    marks: [
      {
        shapes: [polyline([15, 29], [25, 40], [15, 51]), polyline([29, 29], [39, 40], [29, 51])],
        width: 4,
        position: track([[0, [54, 64]], [18, [59, 64]], [30, [63, 64]], [48, [63, 64]]]),
        opacity: track([[0, 0], [8, 100], [28, 100], [42, 0], [48, 0]]),
      },
    ],
  },
  app: {
    frames: 48,
    marks: [
      {
        shapes: [[18, 31], [34, 31], [18, 47], [34, 47]].map(([x, y]) => square(x, y, 10, 2)),
        width: 3.5,
        position: [59, 64],
        scale: track([[0, 20], [14, 112], [24, 100], [48, 100]]),
        opacity: track([[0, 0], [8, 100], [34, 100], [44, 0], [48, 0]]),
      },
    ],
  },
  transfer: {
    frames: 48,
    marks: [
      {
        shapes: [
          polyline([22, 50], [22, 20], [14, 28], [22, 20], [30, 28]),
          polyline([37, 28], [37, 58], [29, 50], [37, 58], [45, 50]),
        ],
        width: 4,
        position: track([[0, [55, 70]], [24, [55, 58]], [48, [55, 70]]]),
        opacity: track([[0, 38], [24, 100], [48, 38]]),
      },
    ],
  },
  record: {
    frames: 48,
    marks: [
      { shapes: [circle(29, 39, 34)], width: 4, position: [52, 64] },
      {
        shapes: [circle(29, 39, 10)],
        filled: true,
        position: [52, 64],
        scale: track([[0, 72], [24, 114], [48, 72]]),
        opacity: track([[0, 42], [24, 100], [48, 42]]),
      },
    ],
  },
  system: {
    frames: 48,
    marks: [
      {
        shapes: [
          circle(29, 39, 24),
          circle(29, 39, 8),
          polyline([29, 20], [29, 25]),
          polyline([29, 53], [29, 58]),
          polyline([10, 39], [15, 39]),
          polyline([43, 39], [48, 39]),
          polyline([16, 26], [20, 30]),
          polyline([38, 48], [42, 52]),
          polyline([16, 52], [20, 48]),
          polyline([38, 30], [42, 26]),
        ],
        width: 3.5,
        position: [50, 64],
        rotation: track([[0, -18], [22, 50], [34, 0], [48, 0]]),
        opacity: track([[0, 0], [6, 100], [34, 100], [44, 0], [48, 0]]),
      },
    ],
  },
};

/** The float every action shares (theme.rs shared_float_motion), in canvas units. */
function sharedFloatMotion(progress) {
  const angle = progress * Math.PI * 2;
  return {
    dx: Math.sin(angle) * 5,
    dy: Math.cos(angle) * 6 - 5,
    rotation: Math.cos(angle) * ((2.5 * Math.PI) / 180),
  };
}

/** effect_rgb: the session color lifted toward white. */
function effectColor(color) {
  return color.map((channel) => Math.round(channel + (255 - channel) * EFFECT_LIFT));
}

/**
 * effects::glow for a cursor moving at (vx, vy) px/s, relative to the
 * pointer: a radial glow around the cursor's anchor, POINTER_ANCHOR_OFFSET
 * behind the tip along its resting heading, pulled back against the motion.
 * Null when the cursor is too slow for one.
 */
function speedGlow(vx, vy) {
  const speed = Math.hypot(vx, vy);
  const alpha = Math.min(speed * 0.00014, 0.42);
  if (alpha <= 0.02) return null;
  const pullback = Math.min(speed * 0.009, 18);
  const anchor = POINTER_ANCHOR_OFFSET * Math.SQRT1_2;
  return {
    x: anchor - (vx / speed) * pullback,
    y: anchor - (vy / speed) * pullback,
    r: 30 * (1 + Math.min(speed * 0.00024, 0.44)),
    alpha,
  };
}

function drawSpeedGlow(cr, glow, color) {
  const gradient = new Cairo.RadialGradient(glow.x, glow.y, 0, glow.x, glow.y, glow.r);
  const [r, g, b] = color.map((channel) => channel / 255);
  // tiny-skia takes the center's alpha as a truncated byte.
  gradient.addColorStopRGBA(0, r, g, b, Math.floor(glow.alpha * 255) / 255);
  gradient.addColorStopRGBA(1, r, g, b, 0);
  cr.newPath();
  cr.arc(glow.x, glow.y, glow.r, 0, Math.PI * 2);
  cr.setSource(gradient);
  cr.fill();
}

/** effects::ripple: the click's ring, `age` seconds after it, around the origin. */
function drawClickRipple(cr, age, color) {
  if (age >= RIPPLE_SECONDS) return;
  const k = age / RIPPLE_SECONDS;
  cr.newPath();
  cr.arc(0, 0, 8 + 44 * (1 - (1 - k) ** 3), 0, Math.PI * 2);
  cr.setLineWidth(4 * (1 - k) + 1);
  setFill(cr, color, Math.round(0.75 * (1 - k) * 255) / 255);
  cr.stroke();
}

/** effects::squish: how much the click shrinks the cursor, `age` seconds after it. */
function clickSquish(age) {
  const press = 0.09;
  if (age < press) return SQUISH * Math.min(age / 0.05, 1);
  const after = age - press;
  return (
    SQUISH *
    Math.max(0, Math.cos(Math.min(after / 0.22, 1) * Math.PI * 1.5)) *
    Math.max(0, 1 - after / 0.22)
  );
}

/**
 * One frame of the agent cursor, as render_state.rs paint_cursor draws it,
 * with the pointer's point at the origin: the speed glow, then the theme's
 * frame for `action` `elapsed` seconds in, squished about the hotspot and
 * floating. `effects` comes from _effectFrame. The ripple, which paint_cursor
 * draws last, is the ripple actor's.
 */
function drawAgentCursor(cr, action, elapsed, fillColor, effects) {
  if (effects.glow) drawSpeedGlow(cr, effects.glow, effectColor(fillColor));
  const animation = THEME_ACTIONS[action] ?? THEME_ACTIONS.idle;
  const frame = Math.floor(Math.max(0, elapsed) * THEME_FPS) % animation.frames;
  const scale = SCALE * (1 - effects.squish);
  const float = sharedFloatMotion((Math.max(0, elapsed) % FLOAT_DURATION) / FLOAT_DURATION);
  cr.save();
  cr.translate(float.dx * scale, float.dy * scale);
  cr.rotate(float.rotation);
  cr.scale(scale, scale);
  cr.translate(-HOTSPOT[0], -HOTSPOT[1]);
  drawCursorLayers(cr, animation.cursor ?? {}, frame, fillColor);
  for (const mark of animation.marks) drawMark(cr, mark, frame);
  cr.restore();
}

/**
 * One 18 px chip, as paint_badge_chip draws it. A filled chip sits in
 * `wellColor`, which the badge picks so its white glyph keeps contrast: the
 * session color darkened 40% on the pill, the session color on the tray.
 */
function drawBadgeChip(cr, glyph, filled, wellColor) {
  const ink = 238 / 255;
  roundedRect(cr, 0.5, 0.5, 17, 17, 5);
  if (filled) {
    setFill(cr, wellColor, 220 / 255);
  } else {
    cr.setSourceRGBA(1, 1, 1, 22 / 255);
  }
  cr.fillPreserve();
  cr.setSourceRGBA(1, 1, 1, ink * (filled ? 0.72 : 0.42));
  cr.setLineWidth(1);
  cr.stroke();
  cr.setSourceRGBA(1, 1, 1, ink);
  cr.setLineWidth(1.35);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);

  const stroke = () => cr.stroke();
  const line = (points) => {
    cr.moveTo(points[0][0], points[0][1]);
    for (const [x, y] of points.slice(1)) cr.lineTo(x, y);
    stroke();
  };
  switch (glyph) {
    case 'background':
      roundedRect(cr, 4, 4, 7, 7, 1.8);
      stroke();
      roundedRect(cr, 6.4, 6.4, 7, 7, 1.8);
      stroke();
      break;
    case 'foreground':
      roundedRect(cr, 4, 5, 10, 9, 1.8);
      stroke();
      line([
        [5, 7.6],
        [13, 7.6],
      ]);
      break;
    case 'ax':
      line([
        [9, 5],
        [9, 9],
        [5, 13],
      ]);
      line([
        [9, 9],
        [13, 13],
      ]);
      cr.setSourceRGBA(1, 1, 1, 245 / 255);
      for (const [x, y] of [
        [9, 5],
        [5, 13],
        [13, 13],
      ]) {
        cr.arc(x, y, 1.35, 0, Math.PI * 2);
        cr.fill();
      }
      break;
    case 'pixel':
      line([
        [4, 7],
        [4, 4],
        [7, 4],
      ]);
      line([
        [11, 4],
        [14, 4],
        [14, 7],
      ]);
      line([
        [14, 11],
        [14, 14],
        [11, 14],
      ]);
      line([
        [7, 14],
        [4, 14],
        [4, 11],
      ]);
      break;
    case 'browser':
      cr.arc(9, 9, 5, 0, Math.PI * 2);
      stroke();
      line([
        [4, 9],
        [14, 9],
      ]);
      line([
        [9, 4],
        [7, 9],
        [9, 14],
        [11, 9],
        [9, 4],
      ]);
      break;
    case 'desktop':
      roundedRect(cr, 4, 4, 10, 7.6, 1.3);
      stroke();
      line([
        [9, 11.6],
        [9, 14],
        [6, 14],
        [12, 14],
      ]);
      break;
    default:
      break;
  }
}

export default class WinRectsExtension extends Extension {
  enable() {
    this._captureClones = new Map();
    this._impl = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
    this._impl.export(Gio.DBus.session, '/org/pincer/WinRects');
    this._nameId = Gio.bus_own_name(
      Gio.BusType.SESSION,
      'org.pincer.WinRects',
      Gio.BusNameOwnerFlags.REPLACE,
      null,
      null,
      null
    );
    this._action = 'idle';
    this._delivery = '';
    this._target = '';
    this._actionStarted = nowSeconds();
    this._endingAt = null;
    // The first session's color until Pincer sends one.
    this._fillColor = [...DEFAULT_CURSOR_FILL];
    this._fillColorCss = '#c95e48';
    // The glide MoveCursor started and the last ClickPulse, for the effects.
    this._glide = null;
    this._clickAt = null;
    this._cursor = new St.DrawingArea({
      width: ACTOR_SIZE,
      height: ACTOR_SIZE,
      visible: false,
      reactive: false,
      can_focus: false,
    });
    this._cursor.connect('repaint', (area) => {
      const cr = area.get_context();
      const now = nowSeconds();
      cr.translate(ACTOR_CENTER, ACTOR_CENTER);
      drawAgentCursor(
        cr,
        this._action,
        now - this._actionStarted,
        this._fillColor,
        this._effectFrame(now)
      );
      cr.$dispose();
    });
    this._cursor.set_pivot_point(0.5, 0.5);
    // Shell can dispose these actors on its own, at shutdown before
    // disable() runs: forget them and stop the frame timer then.
    this._cursor.connect('destroy', () => {
      this._cursor = null;
      this._stopTicking();
    });
    Main.layoutManager.addTopChrome(this._cursor);
    // Over the cursor, under the badge, as paint_cursor layers them.
    this._ripple = new St.DrawingArea({
      width: RIPPLE_ACTOR_SIZE,
      height: RIPPLE_ACTOR_SIZE,
      visible: false,
      reactive: false,
      can_focus: false,
    });
    this._ripple.connect('repaint', (area) => {
      const cr = area.get_context();
      const age = this._clickAt === null ? -1 : nowSeconds() - this._clickAt;
      if (age >= 0) {
        cr.translate(RIPPLE_ACTOR_SIZE / 2, RIPPLE_ACTOR_SIZE / 2);
        drawClickRipple(cr, age, effectColor(this._fillColor));
      }
      cr.$dispose();
    });
    this._ripple.connect('destroy', () => {
      this._ripple = null;
    });
    Main.layoutManager.addTopChrome(this._ripple);
    this._badgeLabel = new St.Label({
      text: '',
      visible: false,
      y_align: Clutter.ActorAlign.CENTER,
      style: badgeLabelStyle(),
    });
    this._deliveryChip = new St.DrawingArea({
      width: BADGE_CHIP_SIZE,
      height: BADGE_CHIP_SIZE,
      visible: false,
      y_align: Clutter.ActorAlign.CENTER,
    });
    this._deliveryChip.connect('repaint', (area) => {
      const cr = area.get_context();
      drawBadgeChip(cr, this._delivery, true, shaded(this._fillColor, PILL_CHIP_SHADE));
      cr.$dispose();
    });
    this._targetChip = new St.DrawingArea({
      width: BADGE_CHIP_SIZE,
      height: BADGE_CHIP_SIZE,
      visible: false,
      y_align: Clutter.ActorAlign.CENTER,
    });
    this._targetChip.connect('repaint', (area) => {
      const cr = area.get_context();
      drawBadgeChip(cr, this._target, false, this._fillColor);
      cr.$dispose();
    });
    // Chips sit 4 px apart and 7 px after the label.
    this._badgeChips = new St.BoxLayout({
      visible: false,
      y_align: Clutter.ActorAlign.CENTER,
      style: `spacing: ${BADGE_CHIP_GAP}px`,
    });
    this._badgeChips.add_child(this._deliveryChip);
    this._badgeChips.add_child(this._targetChip);
    this._badge = new St.BoxLayout({
      visible: false,
      reactive: false,
      can_focus: false,
      height: BADGE_HEIGHT,
      style: badgeStyle(this._fillColor),
    });
    this._badge.add_child(this._badgeLabel);
    this._badge.add_child(this._badgeChips);
    centerOnX(this._badge);
    this._badge.connect('destroy', () => {
      this._badge = null;
      this._badgeLabel = null;
      this._badgeChips = null;
      this._deliveryChip = null;
      this._targetChip = null;
    });
    Main.layoutManager.addTopChrome(this._badge);

    // The minimal badge's chip briefly takes the badge over: a pill with
    // "foreground" (or a combo too wide for keycaps) and the foreground
    // chip, or a key combo as keycaps in a dark tray.
    this._chipLabel = new St.Label({
      text: '',
      y_align: Clutter.ActorAlign.CENTER,
      style: badgeLabelStyle(),
    });
    this._chipForeground = new St.DrawingArea({
      width: BADGE_CHIP_SIZE,
      height: BADGE_CHIP_SIZE,
      visible: false,
      y_align: Clutter.ActorAlign.CENTER,
    });
    this._chipForeground.connect('repaint', (area) => {
      const cr = area.get_context();
      drawBadgeChip(cr, 'foreground', true, shaded(this._fillColor, PILL_CHIP_SHADE));
      cr.$dispose();
    });
    this._chipPill = new St.BoxLayout({
      visible: false,
      reactive: false,
      can_focus: false,
      height: BADGE_HEIGHT,
      style: badgeStyle(this._fillColor),
    });
    this._chipPill.add_child(this._chipLabel);
    this._chipPill.add_child(this._chipForeground);
    centerOnX(this._chipPill);
    this._chipPill.connect('destroy', () => {
      this._chipPill = null;
      this._chipLabel = null;
      this._chipForeground = null;
    });
    Main.layoutManager.addTopChrome(this._chipPill);
    this._keycaps = new St.DrawingArea({
      width: 1,
      height: 1,
      visible: false,
      reactive: false,
      can_focus: false,
    });
    this._keycaps.connect('repaint', (area) => {
      const cr = area.get_context();
      if (this._keycapLayout)
        drawKeycapTray(cr, SHADOW_MARGIN, SHADOW_MARGIN, this._keycapLayout, this._fillColor);
      cr.$dispose();
    });
    this._keycaps.connect('destroy', () => {
      this._keycaps = null;
    });
    Main.layoutManager.addTopChrome(this._keycaps);
    this._keycapLayout = null;
    this._minimalChip = null;

    this._cursorX = null;
    this._cursorY = null;
    this._badgeRevealedAt = null;
    this._modifierFadeAt = null;
    this._glideEndsAt = 0;
    this._frameId = 0;
    this._lastFrame = null;
  }
  disable() {
    this._stopTicking();
    for (const entry of this._captureClones.values()) {
      GLib.source_remove(entry.timeout);
      entry.clone.destroy();
    }
    this._captureClones.clear();
    // Each destroy handler forgets its own references.
    this._cursor?.destroy();
    this._ripple?.destroy();
    this._badge?.destroy();
    this._chipPill?.destroy();
    this._keycaps?.destroy();
    this._minimalChip = null;
    this._keycapLayout = null;
    this._glide = null;
    this._clickAt = null;
    if (this._impl) {
      this._impl.unexport();
      this._impl = null;
    }
    if (this._nameId) {
      Gio.bus_unown_name(this._nameId);
      this._nameId = 0;
    }
  }
  /** Run the frame timer while the cursor is on screen, and only then. */
  _startTicking() {
    if (this._frameId || !this._cursor?.visible) return;
    this._lastFrame = nowSeconds();
    this._frameId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, FRAME_INTERVAL_MS, () => {
      if (this._frame()) return GLib.SOURCE_CONTINUE;
      this._frameId = 0;
      return GLib.SOURCE_REMOVE;
    });
  }
  _stopTicking() {
    if (!this._frameId) return;
    GLib.source_remove(this._frameId);
    this._frameId = 0;
  }
  /** Where the current glide has the pointer at `now`, as Clutter's ease-out cubic. */
  _glidePoint(now) {
    const glide = this._glide;
    if (!glide) return null;
    const t = Math.max(0, Math.min(1, (now - glide.start) / glide.seconds));
    const eased = 1 - (1 - t) ** 3;
    return [
      glide.fromX + (glide.toX - glide.fromX) * eased,
      glide.fromY + (glide.toY - glide.fromY) * eased,
    ];
  }
  /** Whether the glide MoveCursor started is still under way at `now`. */
  _gliding(now) {
    return this._glide !== null && now - this._glide.start < this._glide.seconds;
  }
  /**
   * The cursor's motion effects at `now`, as RenderStateCore.effect_frame
   * gives them for the default motion style: the speed glow while a glide
   * runs, and the squish for CLICK_FX_SECONDS after a ClickPulse.
   */
  _effectFrame(now) {
    let glow = null;
    const glide = this._glide;
    const t = glide ? (now - glide.start) / glide.seconds : 1;
    if (t >= 0 && t < 1) {
      // The ease-out cubic's speed, as a share of the distance per second.
      const rate = (3 * (1 - t) ** 2) / glide.seconds;
      glow = speedGlow((glide.toX - glide.fromX) * rate, (glide.toY - glide.fromY) * rate);
    }
    const age = this._clickAt === null ? -1 : now - this._clickAt;
    return { glow, squish: age >= 0 && age < CLICK_FX_SECONDS ? clickSquish(age) : 0 };
  }
  /** Show the ripple while it plays, and repaint it. */
  _frameRipple(now) {
    if (!this._ripple) return;
    const age = this._clickAt === null ? -1 : now - this._clickAt;
    this._ripple.visible = age >= 0 && age < RIPPLE_SECONDS;
    if (this._ripple.visible) this._ripple.queue_repaint();
  }
  /** One frame. Returns false once the cursor is gone or hidden. */
  _frame() {
    if (!this._cursor?.visible) return false;
    const now = nowSeconds();
    const elapsed = Math.min(1, Math.max(0, now - (this._lastFrame ?? now)));
    this._lastFrame = now;
    if (this._endingAt !== null && now >= this._endingAt) this._setCursorState('idle', '', '');
    if (
      ONE_SHOT_ACTIONS.has(this._action) &&
      now - this._actionStarted >= ACTION_DURATIONS[this._action]
    )
      this._setCursorState('idle', '', '');
    this._cursor.queue_repaint();
    this._frameRipple(now);
    let labelAlpha = this._badgeRevealedAt === null ? 0 : 1;
    if (this._badgeRevealedAt !== null) {
      const badgeElapsed = now - this._badgeRevealedAt;
      if (
        badgeElapsed > BADGE_HOLD_SECONDS &&
        badgeElapsed < BADGE_HOLD_SECONDS + BADGE_FADE_SECONDS
      ) {
        const fade = (badgeElapsed - BADGE_HOLD_SECONDS) / BADGE_FADE_SECONDS;
        labelAlpha = 1 - easeInOut(fade);
      } else if (badgeElapsed >= BADGE_HOLD_SECONDS + BADGE_FADE_SECONDS) {
        labelAlpha = 0;
        this._badgeRevealedAt = null;
      }
    }
    let chipAlpha = this._delivery || this._target ? 1 : 0;
    if (this._modifierFadeAt !== null) {
      const fade = (now - this._modifierFadeAt) / BADGE_FADE_SECONDS;
      chipAlpha = 1 - easeInOut(fade);
      if (fade >= 1) {
        chipAlpha = 0;
        this._modifierFadeAt = null;
        this._delivery = '';
        this._target = '';
        this._updateModifierChips();
      }
    }
    if (this._badgeLabel) {
      this._badgeLabel.opacity = Math.round(255 * labelAlpha);
      if (labelAlpha <= 0.001 && this._badgeLabel.visible) {
        this._badgeLabel.hide();
        if (this._cursorX !== null && this._cursorY !== null)
          this._positionBadge(this._cursorX, this._cursorY, 0);
      }
    }
    if (this._badgeChips) this._badgeChips.opacity = Math.round(255 * chipAlpha);
    // While the minimal chip shows it takes the badge over.
    const takenOver = this._frameMinimalChip(now, elapsed) > 0.001;
    if (this._badge) {
      if (!takenOver && (labelAlpha > 0.001 || chipAlpha > 0.001)) {
        this._badge.opacity = 255;
        this._badge.show();
      } else {
        this._badge.hide();
      }
    }
    return true;
  }
  /**
   * Advance the minimal chip, as tick_minimal_badge does: it waits for its
   * action to end (or the arm timeout), then plays while the cursor is at
   * rest. Returns its opacity this frame.
   */
  _frameMinimalChip(now, elapsed) {
    const chip = this._minimalChip;
    if (!chip) return 0;
    const moving = now < this._glideEndsAt;
    if (!chip.armed) {
      // The frame that arms the chip does not play it.
      chip.waiting += elapsed;
      chip.armed = chip.waiting >= MINIMAL_CHIP_ARM_TIMEOUT_SECONDS;
    } else if (!moving) {
      chip.secs += elapsed;
      if (chip.secs >= MINIMAL_CHIP_SECONDS) {
        this._clearMinimalChip();
        return 0;
      }
    }
    const alpha = chip.armed && !moving ? minimalChipAlpha(chip.secs) : 0;
    const actor = this._keycapLayout ? this._keycaps : this._chipPill;
    if (actor) {
      actor.opacity = Math.round(255 * alpha);
      actor.visible = alpha > 0.001;
    }
    return alpha;
  }
  _clearMinimalChip() {
    this._minimalChip = null;
    this._keycapLayout = null;
    this._keycaps?.hide();
    this._chipPill?.hide();
  }
  /** Lay the minimal chip out as keycaps, or as a pill when it has no keys or they do not fit. */
  _layoutMinimalChip() {
    const chip = this._minimalChip;
    if (!chip) return;
    this._keycapLayout =
      chip.keys.length > 0 ? keycapLayout(chip.keys, chip.mac, chip.foreground) : null;
    if (this._keycapLayout && this._keycaps) {
      this._keycaps.set_size(
        this._keycapLayout.width + SHADOW_MARGIN * 2,
        BADGE_HEIGHT + SHADOW_MARGIN * 2
      );
      this._keycaps.queue_repaint();
      this._chipPill?.hide();
    } else if (this._chipLabel && this._chipForeground) {
      const text =
        chip.keys.length > 0 ? chip.keys.join(chip.mac ? '' : '+') : FOREGROUND_CHIP_LABEL;
      this._chipLabel.set_text(text);
      this._chipForeground.visible = chip.foreground;
      this._chipForeground.queue_repaint();
      this._keycaps?.hide();
    }
    if (this._cursorX !== null && this._cursorY !== null)
      this._positionMinimalChip(this._cursorX, this._cursorY);
  }
  _positionMinimalChip(x, y) {
    const badgeY = Math.round(y + BADGE_Y_OFFSET);
    if (this._keycapLayout && this._keycaps) {
      this._keycaps.set_position(
        Math.round(x - this._keycapLayout.width / 2) - SHADOW_MARGIN,
        badgeY - SHADOW_MARGIN
      );
    } else if (this._minimalChip && this._chipPill) {
      this._chipPill.set_position(Math.round(x), badgeY);
    }
  }
  GetVersion() {
    return 11;
  }
  GetRects() {
    const actors = global.get_window_actors();
    const actorByWindow = new Map();
    for (const actor of actors) {
      if (actor.meta_window) actorByWindow.set(actor.meta_window, actor);
    }
    const windows = global.display.sort_windows_by_stacking([...actorByWindow.keys()]);
    const focusedWindow = global.display.focus_window;
    const out = [];
    for (let stacking = 0; stacking < windows.length; stacking++) {
      const w = windows[stacking];
      const actor = actorByWindow.get(w);
      const r = w.get_frame_rect();
      let buffer = r;
      try {
        buffer = w.get_buffer_rect();
      } catch (_error) {
        // Older Shell releases may not expose the buffer rectangle.
      }
      const minimized = Boolean(w.minimized);
      out.push({
        id: w.get_stable_sequence(),
        pid: w.get_pid(),
        title: w.get_title() ?? '',
        x: r.x,
        y: r.y,
        w: r.width,
        h: r.height,
        buffer_x: buffer.x,
        buffer_y: buffer.y,
        focused: focusedWindow === w,
        minimized,
        visible: Boolean(actor?.visible) && !minimized,
        stacking,
      });
    }
    return JSON.stringify(out);
  }
  CaptureWindowAsync([id, pid], invocation) {
    return this._captureWindow(id, pid, 0, invocation);
  }
  CaptureWindowPreviewAsync([id, pid, maxDimension], invocation) {
    if (maxDimension < 1 || maxDimension > 4096) {
      invocation.return_dbus_error('org.pincer.WinRects.InvalidSize', 'Invalid preview size');
      return;
    }
    return this._captureWindow(id, pid, maxDimension, invocation);
  }
  async _captureWindow(id, pid, maxDimension, invocation) {
    let stream;
    try {
      if (Main.sessionMode.isLocked || Main.sessionMode.isGreeter)
        throw new Error('Desktop is unavailable');
      const actor = global.get_window_actors().find(
        candidate => candidate.meta_window?.get_stable_sequence() === id);
      const window = actor?.meta_window;
      if (!window || (pid !== 0 && window.get_pid() !== pid) || window.minimized)
        throw new Error('Target window is unavailable');
      // A mapped clone keeps Mutter from suspending a fully covered source.
      // It paints no pixels and expires when capture requests stop.
      let entry = this._captureClones.get(id);
      if (entry) GLib.source_remove(entry.timeout);
      else {
        const clone = new Clutter.Clone({source: actor, opacity: 0, width: 1, height: 1, reactive: false});
        global.stage.add_child(clone);
        entry = {clone, timeout: 0};
        this._captureClones.set(id, entry);
      }
      entry.timeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 4000, () => {
        entry.clone.destroy();
        this._captureClones.delete(id);
        return GLib.SOURCE_REMOVE;
      });
      // Snapshot this actor, never the stage. Covered windows retain their own
      // content; neither activation nor a desktop crop is needed.
      const frame = window.get_frame_rect();
      const buffer = window.get_buffer_rect();
      const scale = actor.get_resource_scale();
      const content = actor.paint_to_content(null);
      const texture = content?.get_texture();
      if (!texture || !Number.isFinite(scale) || scale <= 0)
        throw new Error('Window content is unavailable');
      const x = Math.round((frame.x - buffer.x) * scale);
      const y = Math.round((frame.y - buffer.y) * scale);
      const width = Math.round(frame.width * scale);
      const height = Math.round(frame.height * scale);
      if (x < 0 || y < 0 || width <= 0 || height <= 0 ||
          x + width > texture.get_width() || y + height > texture.get_height())
        throw new Error('Window content does not match its frame');
      stream = Gio.MemoryOutputStream.new_resizable();
      let captureTexture = texture;
      let captureX = x, captureY = y, captureWidth = width, captureHeight = height;
      if (maxDimension > 0 && Math.max(width, height) > maxDimension) {
        const ratio = maxDimension / Math.max(width, height);
        captureWidth = Math.max(1, Math.round(width * ratio));
        captureHeight = Math.max(1, Math.round(height * ratio));
        const context = global.stage.get_context().get_backend().get_cogl_context();
        captureTexture = Cogl.Texture2D.new_with_size(context, captureWidth, captureHeight);
        const framebuffer = Cogl.Offscreen.new_with_texture(captureTexture);
        framebuffer.allocate();
        framebuffer.set_viewport(0, 0, captureWidth, captureHeight);
        framebuffer.orthographic(0, 0, captureWidth, captureHeight, -1, 1);
        const pipeline = Cogl.Pipeline.new(context);
        pipeline.set_layer_texture(0, texture);
        framebuffer.draw_textured_rectangle(pipeline, 0, 0, captureWidth, captureHeight,
          x / texture.get_width(), y / texture.get_height(),
          (x + width) / texture.get_width(), (y + height) / texture.get_height());
        framebuffer.finish();
        captureX = 0;
        captureY = 0;
      }
      await Shell.Screenshot.composite_to_stream(
        captureTexture, captureX, captureY, captureWidth, captureHeight, 1, null, 0, 0, 1, stream);
      stream.close(null);
      if (Main.sessionMode.isLocked || Main.sessionMode.isGreeter)
        throw new Error('Desktop became unavailable');
      if (stream.get_data_size() > 32 * 1024 * 1024)
        throw new Error('Window capture is too large');
      const encoded = GLib.base64_encode(stream.steal_as_bytes().get_data());
      invocation.return_value(maxDimension > 0
        ? new GLib.Variant('(suu)', [encoded, width, height])
        : new GLib.Variant('(s)', [encoded]));
    } catch (error) {
      invocation.return_dbus_error('org.pincer.WinRects.CaptureFailed', String(error));
    } finally {
      if (stream && !stream.is_closed()) stream.close(null);
    }
  }
  async CaptureAsync(_params, invocation) {
    try {
      const shooter = new Shell.Screenshot();
      const stream = Gio.MemoryOutputStream.new_resizable();
      const [width, height] = global.display.get_size();
      if (width <= 0 || height <= 0) throw new Error(`Invalid display size ${width}x${height}`);
      // GNOME 50's stage-content capture copies the real cursor sprite
      // after painting the stage. Remote/headless pointer seats can
      // expose a 0x0 sprite, and that copy can crash Shell. The stream
      // API has an explicit include_cursor flag, so keep cursor capture
      // disabled; pincer draws its own agent cursor.
      await shooter.screenshot(false, stream);
      stream.close(null);
      const encoded = GLib.base64_encode(stream.steal_as_bytes().get_data());
      invocation.return_value(new GLib.Variant('(s)', [encoded]));
    } catch (error) {
      invocation.return_dbus_error('org.pincer.WinRects.CaptureFailed', String(error));
    }
  }
  ActivateAsync([id], invocation) {
    const target = global
      .get_window_actors()
      .map((actor) => actor.meta_window)
      .find((window) => window?.get_stable_sequence() === id);
    if (!target) {
      invocation.return_value(new GLib.Variant('(b)', [false]));
      return;
    }
    target.activate(global.get_current_time());
    GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
      invocation.return_value(new GLib.Variant('(b)', [global.display.focus_window === target]));
      return GLib.SOURCE_REMOVE;
    });
  }
  MoveCursor(x, y) {
    if (!this._cursor) return;
    const revealBadge = !this._cursor.visible;
    const now = nowSeconds();
    // Only a real move is a glide the minimal chip waits out.
    if (x !== this._cursorX || y !== this._cursorY)
      this._glideEndsAt = now + CURSOR_GLIDE_MS / 1000;
    // The ease below starts over from wherever the cursor is, and the speed
    // glow follows it. Shell scales every ease's duration, and makes it 0
    // while animations are off (adjustAnimationTime), so the glow does too.
    const settings = St.Settings.get();
    const seconds = settings.enable_animations
      ? (settings.slow_down_factor * CURSOR_GLIDE_MS) / 1000
      : 0;
    const [fromX, fromY] = this._glidePoint(now) ?? [
      this._cursor.x + ACTOR_CENTER,
      this._cursor.y + ACTOR_CENTER,
    ];
    this._glide = seconds > 0 ? { fromX, fromY, toX: x, toY: y, start: now, seconds } : null;
    this._cursorX = x;
    this._cursorY = y;
    this._cursor.show();
    this._cursor.ease({
      x: x - ACTOR_CENTER,
      y: y - ACTOR_CENTER,
      duration: CURSOR_GLIDE_MS,
      mode: Clutter.AnimationMode.EASE_OUT_CUBIC,
    });
    this._positionBadge(x, y, CURSOR_GLIDE_MS);
    this._positionMinimalChip(x, y);
    if (revealBadge) this._revealBadge();
    this._startTicking();
  }
  ClickPulse(x, y) {
    if (!this._cursor) return;
    const revealBadge = !this._cursor.visible;
    this._cursorX = x;
    this._cursorY = y;
    this._cursor.set_position(x - ACTOR_CENTER, y - ACTOR_CENTER);
    this._glideEndsAt = 0;
    // set_position leaves a running ease on its way, so a glide still under
    // way keeps its glow, as a move settling on the click point does on X11.
    const now = nowSeconds();
    if (!this._gliding(now)) this._glide = null;
    // The ripple stays where the click landed.
    this._clickAt = now;
    this._ripple?.set_position(x - RIPPLE_ACTOR_SIZE / 2, y - RIPPLE_ACTOR_SIZE / 2);
    this._frameRipple(now);
    this._positionBadge(x, y, 0);
    this._positionMinimalChip(x, y);
    if (['idle', 'navigate', 'click'].includes(this._action))
      this._setCursorState('click', this._delivery, this._target);
    this._cursor.show();
    this._cursor.queue_repaint();
    if (revealBadge) this._revealBadge();
    this._startTicking();
  }
  SetCursorColor(fillColor) {
    const match = /^#([0-9a-fA-F]{6})$/.exec(fillColor);
    if (!match) return;
    const rgb = Number.parseInt(match[1], 16);
    const next = [(rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff];
    // Pincer sends the color before every command; repaint only on a change.
    if (next.every((channel, index) => channel === this._fillColor[index])) return;
    this._fillColor = next;
    this._fillColorCss = `#${match[1].toLowerCase()}`;
    this._badge?.set_style(badgeStyle(this._fillColor));
    this._chipPill?.set_style(badgeStyle(this._fillColor));
    this._deliveryChip?.queue_repaint();
    this._targetChip?.queue_repaint();
    this._chipForeground?.queue_repaint();
    this._keycaps?.queue_repaint();
    if (this._cursor) this._cursor.queue_repaint();
  }
  SetCursorState(action, delivery, target, active) {
    if (!ACTIONS.has(action)) return;
    if (active) {
      // A new action replaces any minimal chip still playing; its own chip,
      // if it earns one, follows through ShowMinimalChip.
      this._clearMinimalChip();
      this._setCursorState(action, delivery, target);
      return;
    }
    // The action is over: its minimal chip plays from now.
    if (this._minimalChip && !this._minimalChip.armed) {
      this._minimalChip.armed = true;
      this._minimalChip.secs = 0;
    }
    if (this._action === action && !ONE_SHOT_ACTIONS.has(action))
      this._endingAt = nowSeconds() + 0.4;
  }
  /**
   * API 11. The minimal badge's brief chip, as the Rust overlay's
   * MinimalChip: `keys` as keycaps (none when empty), and the foreground
   * chip when `foreground`; with no keys the pill reads "foreground". A call
   * while a chip is pending joins it: keys replace, foreground adds. The
   * chip plays once SetCursorState reports its action inactive, and a new
   * active SetCursorState drops it.
   */
  ShowMinimalChip(keys, mac, foreground) {
    if (!this._cursor?.visible) return;
    const parts = sanitizeKeys(keys);
    if (parts.length === 0 && !foreground) return;
    if (!this._minimalChip) {
      this._minimalChip = {
        keys: [],
        mac: false,
        foreground: false,
        armed: false,
        secs: 0,
        waiting: 0,
      };
    }
    const chip = this._minimalChip;
    if (parts.length > 0) {
      chip.keys = parts;
      chip.mac = Boolean(mac);
    }
    chip.foreground = chip.foreground || Boolean(foreground);
    chip.secs = 0;
    this._layoutMinimalChip();
    this._startTicking();
  }
  SetSessionLabel(label) {
    if (!this._badgeLabel || !this._badge) return;
    this._badgeLabel.set_text(label);
    if (label.length === 0 || !this._cursor?.visible) {
      this._badgeLabel.hide();
      this._badgeRevealedAt = null;
      if (!this._delivery && !this._target) this._badge.hide();
      return;
    }
    if (this._cursorX !== null && this._cursorY !== null) {
      this._positionBadge(this._cursorX, this._cursorY, 0);
      this._revealBadge();
    }
  }
  _positionBadge(x, y, duration) {
    if (!this._badge) return;
    // centerOnX shifts the badge left by half its allocated width.
    const badgeX = Math.round(x);
    const badgeY = Math.round(y + BADGE_Y_OFFSET);
    if (duration > 0) {
      this._badge.ease({
        x: badgeX,
        y: badgeY,
        duration,
        mode: Clutter.AnimationMode.EASE_OUT_CUBIC,
      });
    } else {
      this._badge.set_position(badgeX, badgeY);
    }
  }
  _revealBadge() {
    if (!this._badge || !this._badgeLabel || this._badgeLabel.get_text().length === 0) return;
    this._badgeLabel.show();
    if (this._cursorX !== null && this._cursorY !== null)
      this._positionBadge(this._cursorX, this._cursorY, 0);
    this._badgeRevealedAt = nowSeconds();
    this._badge.opacity = 255;
    this._badge.show();
  }
  _setCursorState(action, delivery, target) {
    this._action = ACTIONS.has(action) ? action : 'idle';
    const nextDelivery = delivery ?? '';
    const nextTarget = target ?? '';
    if (nextDelivery || nextTarget) {
      this._delivery = nextDelivery;
      this._target = nextTarget;
      this._modifierFadeAt = null;
    } else if ((this._delivery || this._target) && this._modifierFadeAt === null) {
      this._modifierFadeAt = nowSeconds();
    }
    this._updateModifierChips();
    this._actionStarted = nowSeconds();
    this._endingAt = null;
    if (this._cursor) this._cursor.queue_repaint();
    if (this._cursorX !== null && this._cursorY !== null)
      this._positionBadge(this._cursorX, this._cursorY, 0);
  }
  _updateModifierChips() {
    if (this._deliveryChip) {
      this._deliveryChip.visible = this._delivery.length > 0;
      this._deliveryChip.queue_repaint();
    }
    if (this._targetChip) {
      this._targetChip.visible = this._target.length > 0;
      this._targetChip.queue_repaint();
    }
    // A hidden chip row takes no room, so a lone label stays centered.
    if (this._badgeChips)
      this._badgeChips.visible = this._delivery.length > 0 || this._target.length > 0;
  }
  HideCursor() {
    this._stopTicking();
    if (this._cursor) this._cursor.hide();
    this._ripple?.hide();
    this._glide = null;
    this._clickAt = null;
    if (this._badge) this._badge.hide();
    this._clearMinimalChip();
    this._badgeRevealedAt = null;
    this._modifierFadeAt = null;
    this._delivery = '';
    this._target = '';
    this._updateModifierChips();
  }
}
