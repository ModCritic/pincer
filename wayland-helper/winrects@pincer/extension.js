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

// GNOME cannot host the Rust renderer directly, so this Shell actor mirrors
// the embedded pincer.default vector theme and semantic state vocabulary.
const CANVAS_SIZE = 128;
const DISPLAY_SIZE = 42;
const ACTOR_SIZE = 112;
const ACTOR_CENTER = ACTOR_SIZE / 2;
const SCALE = DISPLAY_SIZE / CANVAS_SIZE;
const FLOAT_DURATION = 4.0;
const GLOW_SURFACE_SCALE = 3;
const GLOW_PADDING = 24;
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

function triangleWave(value) {
  const t = ((value % 1) + 1) % 1;
  return t < 0.5 ? t * 2 : (1 - t) * 2;
}

function setInk(cr, alpha = 1) {
  cr.setSourceRGBA(1, 1, 1, alpha);
}

function setPaper(cr, alpha = 1) {
  cr.setSourceRGBA(1, 1, 1, alpha);
}

function setFill(cr, color, alpha = 1) {
  cr.setSourceRGBA(color[0] / 255, color[1] / 255, color[2] / 255, alpha);
}

function glowPath(cr, width, alpha, glowColor) {
  if (!glowColor) return;
  const layers = 12;
  const outerExpansion = 13;
  const innerExpansion = 2.5;
  const maxOpacity = 0.17;
  let accumulatedOpacity = 0;
  for (let layer = 0; layer < layers; layer++) {
    const progress = (layer + 1) / layers;
    const expansion = outerExpansion + (innerExpansion - outerExpansion) * progress;
    const targetOpacity = maxOpacity * Math.max(0, Math.min(1, alpha)) * Math.pow(progress, 1.6);
    const layerOpacity =
      (targetOpacity - accumulatedOpacity) / Math.max(0.001, 1 - accumulatedOpacity);
    accumulatedOpacity = targetOpacity;
    cr.setLineWidth(width + expansion);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.setLineJoin(Cairo.LineJoin.ROUND);
    setFill(cr, glowColor, layerOpacity);
    cr.strokePreserve();
  }
}

function strokePath(cr, width, alpha = 1, glowColor = null) {
  glowPath(cr, width, alpha, glowColor);
  cr.setLineWidth(glowColor ? width + 1.5 : width);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setInk(cr, alpha);
  if (!glowColor) {
    cr.stroke();
    return;
  }
  cr.strokePreserve();
  cr.setLineWidth(Math.max(1.5, width - 1));
  setFill(cr, glowColor, alpha);
  cr.stroke();
}

function fillPath(cr, glowWidth, alpha = 1, glowColor = null) {
  glowPath(cr, glowWidth, alpha, glowColor);
  cr.setLineWidth(3);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setInk(cr, alpha);
  cr.strokePreserve();
  setFill(cr, glowColor, alpha);
  cr.fill();
}

function linePath(cr, points, width = 4, alpha = 1, glowColor = null) {
  if (points.length === 0) return;
  cr.moveTo(points[0][0], points[0][1]);
  for (let i = 1; i < points.length; i++) cr.lineTo(points[i][0], points[i][1]);
  strokePath(cr, width, alpha, glowColor);
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

function traceCursorBody(cr) {
  cr.moveTo(55, 30);
  cr.curveTo(48, 28, 42, 33, 43, 41);
  cr.lineTo(64, 98);
  cr.curveTo(67, 106, 73, 106, 77, 99);
  cr.lineTo(86, 79);
  cr.curveTo(88, 75, 91, 72, 95, 70);
  cr.lineTo(108, 63);
  cr.curveTo(115, 59, 114, 53, 107, 50);
  cr.closePath();
}

function drawCursorGlowShape(cr, fillColor) {
  const layers = 36;
  const outerWidth = 44;
  const innerWidth = 7;
  const maxOpacity = 0.34;
  let accumulatedOpacity = 0;

  for (let layer = 0; layer < layers; layer++) {
    const progress = (layer + 1) / layers;
    const width = outerWidth + (innerWidth - outerWidth) * progress;
    const targetOpacity = maxOpacity * Math.pow(progress, 1.65);
    const layerOpacity =
      (targetOpacity - accumulatedOpacity) / Math.max(0.001, 1 - accumulatedOpacity);
    accumulatedOpacity = targetOpacity;
    if (layerOpacity <= 0) continue;

    traceCursorBody(cr);
    cr.setLineWidth(width);
    cr.setLineCap(Cairo.LineCap.ROUND);
    cr.setLineJoin(Cairo.LineJoin.ROUND);
    setFill(cr, fillColor, layerOpacity);
    cr.stroke();
  }
}

function createGlowSurface(fillColor) {
  const surface = new Cairo.ImageSurface(
    Cairo.Format.ARGB32,
    (CANVAS_SIZE + GLOW_PADDING * 2) * GLOW_SURFACE_SCALE,
    (CANVAS_SIZE + GLOW_PADDING * 2) * GLOW_SURFACE_SCALE
  );
  const cr = new Cairo.Context(surface);
  cr.scale(GLOW_SURFACE_SCALE, GLOW_SURFACE_SCALE);
  cr.translate(GLOW_PADDING, GLOW_PADDING);
  drawCursorGlowShape(cr, fillColor);
  cr.$dispose();
  return surface;
}

function sharedFloatMotion(progress) {
  const angle = progress * Math.PI * 2;
  return {
    dx: Math.sin(angle) * 5,
    dy: Math.cos(angle) * 6 - 5,
    rotation: Math.cos(angle) * ((2.5 * Math.PI) / 180),
    scale: 1,
  };
}

function cursorBodyMotion(progress, action) {
  let dx = 0;
  let dy = 0;
  let rotation = 0;
  let scale = 1;

  if (action === 'click') {
    if (progress < 0.35) scale = 1 - easeInOut(progress / 0.35) * 0.07;
    else if (progress < 0.6) scale = 0.93 + easeInOut((progress - 0.35) / 0.25) * 0.1;
    else scale = 1.03 - easeInOut((progress - 0.6) / 0.4) * 0.03;
  } else if (action === 'drag') {
    const held = easeInOut(triangleWave(progress));
    dx = held * 7;
    dy = held * 3;
  }

  return { dx, dy, rotation, scale };
}

function applyCursorBodyMotion(cr, motion) {
  cr.translate(64 + motion.dx, 64 + motion.dy);
  cr.rotate(motion.rotation);
  cr.scale(motion.scale, motion.scale);
  cr.translate(-64, -64);
}

function drawCursorGlow(cr, progress, action, glowSurface) {
  const motion = cursorBodyMotion(progress, action);
  cr.save();
  applyCursorBodyMotion(cr, motion);
  cr.translate(-GLOW_PADDING, -GLOW_PADDING);
  cr.scale(1 / GLOW_SURFACE_SCALE, 1 / GLOW_SURFACE_SCALE);
  cr.setSourceSurface(glowSurface, 0, 0);
  cr.paint();
  cr.restore();
}

function drawCursorBody(cr, progress, action, fillColor) {
  const motion = cursorBodyMotion(progress, action);
  cr.save();
  applyCursorBodyMotion(cr, motion);
  traceCursorBody(cr);
  setFill(cr, fillColor);
  cr.fillPreserve();
  cr.setLineWidth(5);
  cr.setLineCap(Cairo.LineCap.ROUND);
  cr.setLineJoin(Cairo.LineJoin.ROUND);
  setPaper(cr);
  cr.stroke();
  cr.restore();
}

function drawActionCue(cr, action, progress, fillColor) {
  const wave = triangleWave(progress);
  const strokeCue = (width, alpha = 1) => strokePath(cr, width, alpha, fillColor);
  const lineCue = (points, width = 4, alpha = 1) => linePath(cr, points, width, alpha, fillColor);
  cr.save();
  switch (action) {
    case 'observe': {
      const opacity = Math.min(1, progress * 7);
      cr.translate(8, -10);
      cr.moveTo(38, 28);
      cr.curveTo(27, 29, 20, 38, 20, 49);
      strokeCue(4, opacity);
      cr.moveTo(42, 19);
      cr.curveTo(23, 19, 11, 33, 11, 51);
      strokeCue(4, opacity);
      break;
    }
    case 'click': {
      const cueProgress = Math.max(0, Math.min(1, progress / 0.65));
      const opacity = Math.max(
        0,
        Math.min(1, Math.min(1 - Math.pow(cueProgress, 1.35), cueProgress * 5))
      );
      const cueScale = 1.1 + easeInOut(cueProgress) * 0.4;
      cr.translate(35, 28);
      cr.scale(cueScale, cueScale);
      cr.translate(-25, -25);
      lineCue(
        [
          [35, 20],
          [34, 11],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [27, 25],
          [19, 19],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [25, 34],
          [15, 34],
        ],
        4,
        opacity
      );
      break;
    }
    case 'drag': {
      const offset = easeInOut(wave) * 7;
      cr.translate(offset, offset * 0.43);
      lineCue(
        [
          [28, 38],
          [16, 35],
        ],
        4,
        0.2 + wave * 0.8
      );
      lineCue(
        [
          [26, 48],
          [12, 45],
        ],
        4,
        0.2 + wave * 0.8
      );
      break;
    }
    case 'scroll': {
      cr.translate(-5, 4 - wave * 8);
      lineCue(
        [
          [23, 31],
          [31, 22],
          [39, 31],
        ],
        4,
        0.42 + wave * 0.58
      );
      lineCue(
        [
          [23, 49],
          [31, 58],
          [39, 49],
        ],
        4,
        0.42 + wave * 0.58
      );
      break;
    }
    case 'text': {
      const opacity = progress < 0.34 || progress > 0.64 ? 1 : 0.18;
      cr.translate(-4, 0);
      lineCue(
        [
          [31, 22],
          [31, 58],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [24, 22],
          [38, 22],
        ],
        4,
        opacity
      );
      lineCue(
        [
          [24, 58],
          [38, 58],
        ],
        4,
        opacity
      );
      break;
    }
    case 'key': {
      const bounce = Math.sin(progress * Math.PI * 2) * (1 - progress) * 3;
      cr.translate(-9, bounce);
      roundedRect(cr, 14, 25, 28, 28, 6);
      strokeCue(3.5);
      lineCue(
        [
          [23, 32],
          [23, 46],
          [23, 39],
          [33, 32],
          [24, 39],
          [34, 46],
        ],
        3.5
      );
      break;
    }
    case 'navigate': {
      cr.translate(-10 + easeInOut(progress) * 9, 0);
      lineCue(
        [
          [15, 29],
          [25, 40],
          [15, 51],
        ],
        4,
        0.2 + wave * 0.8
      );
      lineCue(
        [
          [29, 29],
          [39, 40],
          [29, 51],
        ],
        4,
        0.2 + wave * 0.8
      );
      break;
    }
    case 'app': {
      const s = 0.2 + easeInOut(Math.min(1, progress * 2)) * 0.8;
      cr.translate(21, 39);
      cr.scale(s, s);
      cr.translate(-26, -39);
      for (const [x, y] of [
        [13, 26],
        [29, 26],
        [13, 42],
        [29, 42],
      ]) {
        roundedRect(cr, x, y, 10, 10, 2);
        strokeCue(3.5);
      }
      break;
    }
    case 'transfer':
      cr.translate(-9, 6 - wave * 12);
      lineCue(
        [
          [22, 50],
          [22, 20],
          [14, 28],
          [22, 20],
          [30, 28],
        ],
        4,
        0.38 + wave * 0.62
      );
      lineCue(
        [
          [37, 28],
          [37, 58],
          [29, 50],
          [37, 58],
          [45, 50],
        ],
        4,
        0.38 + wave * 0.62
      );
      break;
    case 'record':
      cr.translate(-12, 0);
      cr.arc(29, 39, 17, 0, Math.PI * 2);
      strokeCue(4);
      cr.arc(29, 39, 3.6 + wave * 2.1, 0, Math.PI * 2);
      fillPath(cr, (3.6 + wave * 2.1) * 1.25, 0.42 + wave * 0.58, fillColor);
      break;
    case 'system': {
      cr.translate(15, 39);
      cr.rotate(((easeInOut(progress) * 68 - 18) * Math.PI) / 180);
      cr.translate(-29, -39);
      for (const radius of [12, 4]) {
        cr.arc(29, 39, radius, 0, Math.PI * 2);
        strokeCue(3.5);
      }
      for (const points of [
        [
          [29, 20],
          [29, 25],
        ],
        [
          [29, 53],
          [29, 58],
        ],
        [
          [10, 39],
          [15, 39],
        ],
        [
          [43, 39],
          [48, 39],
        ],
        [
          [16, 26],
          [20, 30],
        ],
        [
          [38, 48],
          [42, 52],
        ],
        [
          [16, 52],
          [20, 48],
        ],
        [
          [38, 30],
          [42, 26],
        ],
      ])
        lineCue(points, 3.5);
      break;
    }
    default:
      break;
  }
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
    this._fillColor = [94, 192, 232];
    this._fillColorCss = '#5ec0e8';
    this._glowSurface = createGlowSurface(this._fillColor);
    this._cursor = new St.DrawingArea({
      width: ACTOR_SIZE,
      height: ACTOR_SIZE,
      visible: false,
      reactive: false,
      can_focus: false,
    });
    this._cursor.connect('repaint', (area) => {
      const cr = area.get_context();
      const duration = ACTION_DURATIONS[this._action] ?? 1.6;
      const elapsed = nowSeconds() - this._actionStarted;
      const progress = (elapsed % duration) / duration;
      const floatProgress = (elapsed % FLOAT_DURATION) / FLOAT_DURATION;
      cr.save();
      cr.translate(ACTOR_CENTER, ACTOR_CENTER);
      cr.scale(SCALE, SCALE);
      cr.translate(-CANVAS_SIZE / 2, -CANVAS_SIZE / 2);
      applyCursorBodyMotion(cr, sharedFloatMotion(floatProgress));
      drawCursorGlow(cr, progress, this._action, this._glowSurface);
      drawActionCue(cr, this._action, progress, this._fillColor);
      drawCursorBody(cr, progress, this._action, this._fillColor);
      cr.restore();
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
    this._badge?.destroy();
    this._chipPill?.destroy();
    this._keycaps?.destroy();
    this._minimalChip = null;
    this._keycapLayout = null;
    if (this._glowSurface) {
      this._glowSurface.finish();
      this._glowSurface = null;
    }
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
    // Only a real move is a glide the minimal chip waits out.
    if (x !== this._cursorX || y !== this._cursorY)
      this._glideEndsAt = nowSeconds() + CURSOR_GLIDE_MS / 1000;
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
    if (this._glowSurface) this._glowSurface.finish();
    this._glowSurface = createGlowSurface(this._fillColor);
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
    if (this._badge) this._badge.hide();
    this._clearMinimalChip();
    this._badgeRevealedAt = null;
    this._modifierFadeAt = null;
    this._delivery = '';
    this._target = '';
    this._updateModifierChips();
  }
}
