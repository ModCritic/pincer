// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Renderer-owned public session label shown below an active agent cursor.
//!
//! The label is display metadata only. Cursor ownership continues to use the
//! private runtime key, so rendering a friendly name never weakens session
//! isolation.

use crate::{
    badge_glyphs::{paint_badge_chip, BadgeChip, BadgeGlyph},
    badge_mode::KeyCombo,
    DeliveryModifier, TargetModifier,
};
use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
use std::borrow::Cow;
use std::sync::OnceLock;
use tiny_skia::{Color, Paint, Path, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform};

pub const MAX_SESSION_LABEL_CHARS: usize = 28;
pub const BADGE_MAX_WIDTH: f32 = 188.0;
pub const BADGE_HEIGHT: f32 = 28.0;
pub const BADGE_CURSOR_GAP: f32 = 25.0;
pub const BADGE_CHIP_SIZE: f32 = 18.0;
pub const BADGE_CHIP_GAP: f32 = 4.0;
pub const BADGE_CHIP_GROUP_GAP: f32 = 7.0;

const FONT_BYTES: &[u8] = include_bytes!("../assets/Inter.ttf");
const FONT_SIZE: f32 = 11.5;
const HORIZONTAL_PADDING: f32 = 10.0;

// The word pill: the session color with white text and a darker rim.
const PILL_FILL_ALPHA: f32 = 245.0;
const PILL_RIM_SHADE: f32 = 0.35;
const PILL_RIM_ALPHA: f32 = 220.0;
// Filled chips on the pill sit in a darker shade of the session color, so
// their white glyphs keep a well of their own.
const PILL_CHIP_SHADE: f32 = 0.4;
const PILL_TEXT: [u8; 3] = [255, 255, 255];

// Key combos: one light cap per key over a session-colored bevel, in a dark
// tray. Cap, bevel and padding fill exactly the badge height.
const KEYCAP_FONT_SIZE: f32 = 10.5;
const KEYCAP_HEIGHT: f32 = 18.0;
const KEYCAP_BEVEL: f32 = 2.0;
const KEYCAP_RADIUS: f32 = 4.5;
const KEYCAP_TEXT_PADDING: f32 = 5.0;
/// Room for the `+` between two caps.
const KEYCAP_JOIN_WIDTH: f32 = 10.0;
/// Gap between two caps in the macOS spelling, which has no `+`.
const KEYCAP_MAC_GAP: f32 = 3.0;
const KEYCAP_TRAY_PADDING: f32 = (BADGE_HEIGHT - KEYCAP_HEIGHT - KEYCAP_BEVEL) * 0.5;
const KEYCAP_TRAY_RADIUS: f32 = 8.0;
const KEYCAP_TRAY: [u8; 4] = [22, 22, 26, 215];
const KEYCAP_FACE: [u8; 3] = [246, 246, 248];
const KEYCAP_TEXT: [u8; 3] = [30, 30, 36];
const KEYCAP_JOIN_TEXT: [u8; 3] = [196, 196, 204];

const SHADOW_ALPHA: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BadgeExtents {
    pub horizontal: f32,
    pub above: f32,
    pub below: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BadgeLabelLayout {
    pub text: String,
    pub origin: (f32, f32),
}

/// One key of a key combo, drawn as a keycap.
#[derive(Debug, Clone, PartialEq)]
pub struct BadgeKeycap {
    /// The light face. The session-colored bevel shows below it.
    pub face: Rect,
    pub label: BadgeLabelLayout,
}

/// A key combo drawn as keycaps in a dark tray.
#[derive(Debug, Clone, PartialEq)]
pub struct BadgeKeycaps {
    pub caps: Vec<BadgeKeycap>,
    /// Text origins of the `+` between caps. Empty in the macOS spelling.
    pub joins: Vec<(f32, f32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionBadgeLayout {
    pub rect: Rect,
    pub corner_radius: f32,
    pub label: Option<BadgeLabelLayout>,
    /// Set when the badge is a keycap tray rather than the word pill.
    pub keycaps: Option<BadgeKeycaps>,
    pub delivery_chip: Option<BadgeChip>,
    pub target_chip: Option<BadgeChip>,
    pub pill_alpha: f32,
    pub label_alpha: f32,
    pub chip_alpha: f32,
    pub scale: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct SessionBadgeInput<'a> {
    pub label: Option<&'a str>,
    /// A held key combo. It takes the label's place and draws as keycaps,
    /// or as its text in the pill when the caps do not fit.
    pub key_combo: Option<&'a KeyCombo>,
    pub delivery: Option<DeliveryModifier>,
    pub target: Option<TargetModifier>,
    pub cursor: (f32, f32),
    pub backing_scale: f32,
    pub label_alpha: f32,
    pub chip_alpha: f32,
    pub clip: Option<(f32, f32)>,
}

pub const fn session_badge_extents() -> BadgeExtents {
    const GLOW_ALLOWANCE: f32 = 8.0;
    BadgeExtents {
        horizontal: BADGE_MAX_WIDTH * 0.5 + GLOW_ALLOWANCE,
        above: GLOW_ALLOWANCE,
        below: BADGE_CURSOR_GAP + BADGE_HEIGHT + GLOW_ALLOWANCE,
    }
}

fn font() -> Option<&'static fontdue::Font> {
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        fontdue::Font::from_bytes(
            FONT_BYTES,
            fontdue::FontSettings {
                scale: 40.0,
                ..Default::default()
            },
        )
        .ok()
    })
    .as_ref()
}

#[derive(Debug, Clone, Copy)]
struct TextBounds {
    min_x: f32,
    min_y: f32,
    width: f32,
    height: f32,
}

fn text_layout(font: &fontdue::Font, text: &str, font_size: f32) -> Layout {
    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings::default());
    layout.append(&[font], &TextStyle::new(text, font_size, 0));
    layout
}

fn text_bounds(layout: &Layout) -> Option<TextBounds> {
    let glyphs = layout.glyphs();
    if glyphs.is_empty() {
        return None;
    }
    let min_x = glyphs
        .iter()
        .map(|glyph| glyph.x)
        .fold(f32::INFINITY, f32::min);
    let max_x = glyphs
        .iter()
        .map(|glyph| glyph.x + glyph.width as f32)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = glyphs
        .iter()
        .map(|glyph| glyph.y)
        .fold(f32::INFINITY, f32::min);
    let max_y = glyphs
        .iter()
        .map(|glyph| glyph.y + glyph.height as f32)
        .fold(f32::NEG_INFINITY, f32::max);
    Some(TextBounds {
        min_x,
        min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

fn ellipsize_to_width(
    font: &fontdue::Font,
    label: &str,
    font_size: f32,
    max_width: f32,
) -> Option<(String, TextBounds)> {
    let layout = text_layout(font, label, font_size);
    let bounds = text_bounds(&layout)?;
    if bounds.width <= max_width {
        return Some((label.to_owned(), bounds));
    }

    let mut characters = label.chars().collect::<Vec<_>>();
    if characters.last() == Some(&'…') {
        characters.pop();
    }
    while !characters.is_empty() {
        characters.pop();
        let candidate = format!("{}…", characters.iter().collect::<String>());
        let layout = text_layout(font, &candidate, font_size);
        let bounds = text_bounds(&layout)?;
        if bounds.width <= max_width {
            return Some((candidate, bounds));
        }
    }
    let layout = text_layout(font, "…", font_size);
    text_bounds(&layout).map(|bounds| ("…".to_owned(), bounds))
}

/// Convert an untrusted public session label into compact display text.
///
/// Controls are removed, all whitespace runs collapse to one ASCII space,
/// and the visible value is bounded by Unicode scalar count. Runtime keys and
/// transport identifiers must never be passed to this function.
pub fn sanitize_session_label(input: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut pending_space = false;
    for character in input.chars() {
        if character.is_control() {
            continue;
        }
        if character.is_whitespace() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(character);
    }

    let normalized = normalized.trim();
    if normalized.is_empty() {
        return None;
    }
    let mut chars = normalized.chars();
    let mut result: String = chars.by_ref().take(MAX_SESSION_LABEL_CHARS).collect();
    if chars.next().is_some() {
        result.pop();
        result.push('…');
    }
    Some(result)
}

/// Text origin that centers text vertically on `center_y`. With
/// `shared_baseline` it centers the ink of a capital H at the same size, so
/// texts side by side share one baseline. Without it the text centers its
/// own ink, which suits symbols such as ⌘ that reach below the baseline.
/// Both snap to whole device pixels so the glyph rounding cannot shift them.
///
/// The H is laid out rather than taken from the line metrics because the
/// layout puts the baseline on the next whole pixel below the ascent (10.17
/// becomes 11 at 10.5 px), which left cap-height text about 1 px low.
fn centered_origin_y(
    font: &fontdue::Font,
    bounds: TextBounds,
    font_size: f32,
    center_y: f32,
    shared_baseline: bool,
) -> f32 {
    let ink = if shared_baseline {
        text_bounds(&text_layout(font, "H", font_size)).unwrap_or(bounds)
    } else {
        bounds
    };
    (center_y - ink.height * 0.5 - ink.min_y).round()
}

fn rounded_rect(rect: Rect, radius: f32) -> Option<Path> {
    const K: f32 = 0.552_284_8;
    let x = rect.x();
    let y = rect.y();
    let width = rect.width();
    let height = rect.height();
    let radius = radius.min(width * 0.5).min(height * 0.5).max(0.0);
    let mut builder = PathBuilder::new();
    builder.move_to(x + radius, y);
    builder.line_to(x + width - radius, y);
    builder.cubic_to(
        x + width - radius + radius * K,
        y,
        x + width,
        y + radius - radius * K,
        x + width,
        y + radius,
    );
    builder.line_to(x + width, y + height - radius);
    builder.cubic_to(
        x + width,
        y + height - radius + radius * K,
        x + width - radius + radius * K,
        y + height,
        x + width - radius,
        y + height,
    );
    builder.line_to(x + radius, y + height);
    builder.cubic_to(
        x + radius - radius * K,
        y + height,
        x,
        y + height - radius + radius * K,
        x,
        y + height - radius,
    );
    builder.line_to(x, y + radius);
    builder.cubic_to(
        x,
        y + radius - radius * K,
        x + radius - radius * K,
        y,
        x + radius,
        y,
    );
    builder.close();
    builder.finish()
}

/// The part of a keycap's bevel that shows below its face: the face's lower
/// outline moved down by `depth`, closed along the face's own lower outline
/// raised by `overlap`. Only the thin overlap sits under the face, so a
/// fading cap does not tint its face with the bevel and no seam opens
/// between the two at full opacity.
fn keycap_bevel(face: Rect, radius: f32, depth: f32, overlap: f32) -> Option<Path> {
    const K: f32 = 0.552_284_8;
    let x = face.x();
    let width = face.width();
    let bottom = face.y() + face.height();
    let r = radius.min(width * 0.5).min(face.height() * 0.5).max(0.0);
    let low = bottom + depth;
    let high = bottom - overlap;
    let mut builder = PathBuilder::new();
    builder.move_to(x, high - r);
    builder.line_to(x, low - r);
    builder.cubic_to(x, low - r + r * K, x + r - r * K, low, x + r, low);
    builder.line_to(x + width - r, low);
    builder.cubic_to(
        x + width - r + r * K,
        low,
        x + width,
        low - r + r * K,
        x + width,
        low - r,
    );
    builder.line_to(x + width, high - r);
    builder.cubic_to(
        x + width,
        high - r + r * K,
        x + width - r + r * K,
        high,
        x + width - r,
        high,
    );
    builder.line_to(x + r, high);
    builder.cubic_to(x + r - r * K, high, x, high - r + r * K, x, high - r);
    builder.close();
    builder.finish()
}

fn mixed_channel(base: u8, accent: u8, accent_weight: f32) -> u8 {
    let weight = accent_weight.clamp(0.0, 1.0);
    ((base as f32 * (1.0 - weight)) + (accent as f32 * weight)).round() as u8
}

/// `color` moved `amount` of the way toward black.
fn shaded(color: [u8; 4], amount: f32) -> [u8; 3] {
    [0, 1, 2].map(|channel| mixed_channel(color[channel], 0, amount))
}

fn solid_paint(rgb: [u8; 3], alpha: f32) -> Paint<'static> {
    Paint {
        shader: tiny_skia::Shader::SolidColor(Color::from_rgba8(
            rgb[0],
            rgb[1],
            rgb[2],
            alpha.round().clamp(0.0, 255.0) as u8,
        )),
        anti_alias: true,
        ..Default::default()
    }
}

fn fill(pixmap: &mut Pixmap, path: &Path, rgb: [u8; 3], alpha: f32) {
    pixmap.fill_path(
        path,
        &solid_paint(rgb, alpha),
        tiny_skia::FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn paint_diffuse_glow(
    pixmap: &mut Pixmap,
    rect: Rect,
    radius: f32,
    scale: f32,
    color: [u8; 3],
    alpha: f32,
    y_offset: f32,
) {
    for step in (1..=8).rev() {
        let spread = step as f32 * scale;
        let Some(layer_rect) = Rect::from_xywh(
            rect.x() - spread,
            rect.y() - spread * 0.55 + y_offset * scale,
            rect.width() + spread * 2.0,
            rect.height() + spread * 1.1,
        ) else {
            continue;
        };
        let Some(layer) = rounded_rect(layer_rect, radius + spread) else {
            continue;
        };
        let strength = 0.35 + (8 - step) as f32 * 0.085;
        fill(pixmap, &layer, color, alpha * strength);
    }
}

/// Paint `text` with its layout origin at `origin`.
fn paint_text(
    pixmap: &mut Pixmap,
    font: &fontdue::Font,
    text: &str,
    font_size: f32,
    origin: (f32, f32),
    rgb: [u8; 3],
    opacity: f32,
) {
    let layout = text_layout(font, text, font_size);
    for glyph in layout.glyphs() {
        let (metrics, bitmap) = font.rasterize_config(glyph.key);
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }
        let Some(mut glyph_pixmap) = Pixmap::new(metrics.width as u32, metrics.height as u32)
        else {
            continue;
        };
        for (pixel, coverage) in glyph_pixmap
            .data_mut()
            .chunks_exact_mut(4)
            .zip(bitmap.iter().copied())
        {
            let alpha = (coverage as f32) * opacity;
            let channel = |value: u8| (value as f32 * alpha / 255.0) as u8;
            pixel.copy_from_slice(&[
                channel(rgb[0]),
                channel(rgb[1]),
                channel(rgb[2]),
                alpha as u8,
            ]);
        }
        pixmap.draw_pixmap(
            (origin.0 + glyph.x).round() as i32,
            (origin.1 + glyph.y).round() as i32,
            glyph_pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
}

/// Top-left corner of a `width` by `height` badge under the cursor, kept
/// inside `clip` when there is one.
fn badge_origin(input: &SessionBadgeInput<'_>, width: f32, height: f32, scale: f32) -> (f32, f32) {
    let unclamped_x = input.cursor.0 - width * 0.5;
    let unclamped_y = input.cursor.1 + BADGE_CURSOR_GAP * scale;
    if let Some((clip_width, clip_height)) = input.clip {
        (
            unclamped_x.clamp(
                2.0 * scale,
                (clip_width - width - 2.0 * scale).max(2.0 * scale),
            ),
            unclamped_y.clamp(
                2.0 * scale,
                (clip_height - height - 2.0 * scale).max(2.0 * scale),
            ),
        )
    } else {
        (unclamped_x, unclamped_y)
    }
}

/// Lay the delivery chip and then the target chip out from `x`, vertically
/// centered in the badge.
fn chip_layouts(
    delivery: Option<DeliveryModifier>,
    target: Option<TargetModifier>,
    mut x: f32,
    badge: Rect,
    scale: f32,
) -> (Option<BadgeChip>, Option<BadgeChip>) {
    let size = BADGE_CHIP_SIZE * scale;
    let y = badge.y() + (badge.height() - size) * 0.5;
    let delivery_chip = delivery.and_then(|delivery| {
        let rect = Rect::from_xywh(x, y, size, size)?;
        x += (BADGE_CHIP_SIZE + BADGE_CHIP_GAP) * scale;
        Some(BadgeChip {
            glyph: BadgeGlyph::from(delivery),
            rect,
            filled: true,
        })
    });
    let target_chip = target.and_then(|target| {
        Some(BadgeChip {
            glyph: BadgeGlyph::from(target),
            rect: Rect::from_xywh(x, y, size, size)?,
            filled: false,
        })
    });
    (delivery_chip, target_chip)
}

/// Lay `combo` out as keycaps in a tray, followed by any chips. `None` when
/// the tray would be wider than the badge allows, so the caller falls back
/// to the combo's text in the pill.
fn keycap_badge_layout(
    font: &fontdue::Font,
    combo: &KeyCombo,
    input: &SessionBadgeInput<'_>,
    delivery: Option<DeliveryModifier>,
    target: Option<TargetModifier>,
    chip_width: f32,
    scale: f32,
) -> Option<(Rect, BadgeKeycaps, Option<BadgeChip>, Option<BadgeChip>)> {
    let font_size = KEYCAP_FONT_SIZE * scale;
    let cap_height = KEYCAP_HEIGHT * scale;
    let padding = KEYCAP_TRAY_PADDING * scale;
    let join_width = if combo.mac {
        KEYCAP_MAC_GAP
    } else {
        KEYCAP_JOIN_WIDTH
    } * scale;
    // Whole-pixel cap widths keep the small caps' edges crisp.
    let keys = combo
        .keys
        .iter()
        .map(|key| {
            let bounds = text_bounds(&text_layout(font, key, font_size))?;
            let width = (bounds.width + KEYCAP_TEXT_PADDING * 2.0 * scale)
                .max(cap_height)
                .ceil();
            Some((key, bounds, width))
        })
        .collect::<Option<Vec<_>>>()?;
    if keys.is_empty() {
        return None;
    }
    let caps_width =
        keys.iter().map(|(_, _, width)| width).sum::<f32>() + join_width * (keys.len() - 1) as f32;
    let chips_width = if chip_width > 0.0 {
        BADGE_CHIP_GROUP_GAP * scale + chip_width
    } else {
        0.0
    };
    let width = caps_width + chips_width + padding * 2.0;
    if width > BADGE_MAX_WIDTH * scale {
        return None;
    }
    let height = BADGE_HEIGHT * scale;
    let (x, y) = badge_origin(input, width, height, scale);
    let rect = Rect::from_xywh(x.round(), y.round(), width, height)?;

    let face_y = rect.y() + padding;
    let face_center = face_y + cap_height * 0.5;
    // The `+` sits on the middle of the whole cap, bevel included.
    let join_center = face_y + (cap_height + KEYCAP_BEVEL * scale) * 0.5;
    let plus = text_bounds(&text_layout(font, "+", font_size))?;
    let mut caps = Vec::with_capacity(keys.len());
    let mut joins = Vec::new();
    let mut cursor_x = rect.x() + padding;
    for (index, (key, bounds, cap_width)) in keys.into_iter().enumerate() {
        if index > 0 {
            if !combo.mac {
                joins.push((
                    cursor_x + (join_width - plus.width) * 0.5 - plus.min_x,
                    (join_center - plus.height * 0.5 - plus.min_y).round(),
                ));
            }
            cursor_x += join_width;
        }
        let face = Rect::from_xywh(cursor_x, face_y, cap_width, cap_height)?;
        // Keys with a letter or digit share one baseline; symbol keys such
        // as ⌘ or + center their own ink.
        let origin = (
            cursor_x + (cap_width - bounds.width) * 0.5 - bounds.min_x,
            centered_origin_y(
                font,
                bounds,
                font_size,
                face_center,
                key.chars().any(char::is_alphanumeric),
            ),
        );
        caps.push(BadgeKeycap {
            face,
            label: BadgeLabelLayout {
                text: key.clone(),
                origin,
            },
        });
        cursor_x += cap_width;
    }
    let (delivery_chip, target_chip) = chip_layouts(
        delivery,
        target,
        cursor_x + BADGE_CHIP_GROUP_GAP * scale,
        rect,
        scale,
    );
    Some((
        rect,
        BadgeKeycaps { caps, joins },
        delivery_chip,
        target_chip,
    ))
}

pub fn session_badge_layout(input: SessionBadgeInput<'_>) -> Option<SessionBadgeLayout> {
    let font = font()?;
    let scale = input.backing_scale.max(1.0);
    let label_alpha = input.label_alpha.clamp(0.0, 1.0);
    let chip_alpha = input.chip_alpha.clamp(0.0, 1.0);
    let show_label = (input.key_combo.is_some() || input.label.is_some()) && label_alpha > 0.001;
    let delivery = (chip_alpha > 0.001).then_some(input.delivery).flatten();
    let target = (chip_alpha > 0.001).then_some(input.target).flatten();
    let chip_count = usize::from(delivery.is_some()) + usize::from(target.is_some());
    if !show_label && chip_count == 0 {
        return None;
    }

    let chip_width = match chip_count {
        0 => 0.0,
        1 => BADGE_CHIP_SIZE,
        _ => BADGE_CHIP_SIZE * 2.0 + BADGE_CHIP_GAP,
    } * scale;

    if let Some(combo) = input.key_combo.filter(|_| show_label) {
        if let Some((rect, keycaps, delivery_chip, target_chip)) =
            keycap_badge_layout(font, combo, &input, delivery, target, chip_width, scale)
        {
            return Some(SessionBadgeLayout {
                rect,
                corner_radius: KEYCAP_TRAY_RADIUS * scale,
                label: None,
                keycaps: Some(keycaps),
                delivery_chip,
                target_chip,
                pill_alpha: label_alpha.max(chip_alpha),
                label_alpha,
                chip_alpha,
                scale,
            });
        }
    }

    let font_size = FONT_SIZE * scale;
    let fixed_width = HORIZONTAL_PADDING * 2.0 * scale
        + chip_width
        + if show_label && chip_count > 0 {
            BADGE_CHIP_GROUP_GAP * scale
        } else {
            0.0
        };
    let max_text_width = (BADGE_MAX_WIDTH * scale - fixed_width).max(0.0);
    // A combo whose caps do not fit shows as its text instead.
    let label_text = match input.key_combo {
        Some(combo) => Some(Cow::Owned(combo.text())),
        None => input.label.map(Cow::Borrowed),
    };
    let label_data = label_text
        .filter(|_| show_label)
        .and_then(|label| ellipsize_to_width(font, &label, font_size, max_text_width));
    let text_width = label_data.as_ref().map_or(0.0, |(_, bounds)| bounds.width);
    let minimum_width = if show_label {
        55.0 * scale
    } else {
        fixed_width
    };
    let badge_width = (fixed_width + text_width)
        .min(BADGE_MAX_WIDTH * scale)
        .max(minimum_width);
    let badge_height = BADGE_HEIGHT * scale;
    let (x, y) = badge_origin(&input, badge_width, badge_height, scale);
    let rect = Rect::from_xywh(x, y, badge_width, badge_height)?;
    let corner_radius = badge_height * 0.5;

    // Centre the label and chips as one group, so a short label in a
    // minimum-width pill is not left-aligned.
    let group_gap = if label_data.is_some() && chip_count > 0 {
        BADGE_CHIP_GROUP_GAP * scale
    } else {
        0.0
    };
    let content_width = text_width + group_gap + chip_width;
    let mut cursor_x = x + ((badge_width - content_width) * 0.5).max(HORIZONTAL_PADDING * scale);
    let label = label_data.map(|(text, bounds)| {
        // Names share one baseline. Labels without lowercase (key combo
        // text such as ⌘A) center their own ink.
        let origin_y = centered_origin_y(
            font,
            bounds,
            font_size,
            y + badge_height * 0.5,
            text.chars().any(char::is_lowercase),
        );
        let origin = (cursor_x - bounds.min_x, origin_y);
        cursor_x += bounds.width + BADGE_CHIP_GROUP_GAP * scale;
        BadgeLabelLayout { text, origin }
    });
    let (delivery_chip, target_chip) = chip_layouts(delivery, target, cursor_x, rect, scale);

    Some(SessionBadgeLayout {
        rect,
        corner_radius,
        label,
        keycaps: None,
        delivery_chip,
        target_chip,
        pill_alpha: label_alpha.max(chip_alpha),
        label_alpha,
        chip_alpha,
        scale,
    })
}

/// Paint the badge below the cursor without rotating it with cursor heading.
pub fn paint_session_badge(
    pixmap: &mut Pixmap,
    layout: &SessionBadgeLayout,
    session_fill: [u8; 4],
    alpha_scale: f32,
) {
    let Some(font) = font() else {
        return;
    };
    let rect = layout.rect;
    let scale = layout.scale;
    let opacity = (alpha_scale * layout.pill_alpha).clamp(0.0, 1.0);
    if opacity <= 0.0 {
        return;
    }
    let Some(path) = rounded_rect(rect, layout.corner_radius) else {
        return;
    };
    let session_rgb = [session_fill[0], session_fill[1], session_fill[2]];
    let label_opacity = (alpha_scale * layout.label_alpha).clamp(0.0, 1.0);

    paint_diffuse_glow(
        pixmap,
        rect,
        layout.corner_radius,
        scale,
        [0, 0, 0],
        SHADOW_ALPHA * opacity,
        2.0,
    );

    let chip_well = match &layout.keycaps {
        Some(keycaps) => {
            let [red, green, blue, alpha] = KEYCAP_TRAY;
            fill(
                pixmap,
                &path,
                [red, green, blue],
                f32::from(alpha) * opacity,
            );
            let font_size = KEYCAP_FONT_SIZE * scale;
            let cap_alpha = 255.0 * label_opacity;
            for cap in &keycaps.caps {
                let radius = KEYCAP_RADIUS * scale;
                if let Some(bevel) =
                    keycap_bevel(cap.face, radius, KEYCAP_BEVEL * scale, 0.75 * scale)
                {
                    fill(pixmap, &bevel, session_rgb, cap_alpha);
                }
                if let Some(face) = rounded_rect(cap.face, radius) {
                    fill(pixmap, &face, KEYCAP_FACE, cap_alpha);
                }
                paint_text(
                    pixmap,
                    font,
                    &cap.label.text,
                    font_size,
                    cap.label.origin,
                    KEYCAP_TEXT,
                    label_opacity,
                );
            }
            for &origin in &keycaps.joins {
                paint_text(
                    pixmap,
                    font,
                    "+",
                    font_size,
                    origin,
                    KEYCAP_JOIN_TEXT,
                    label_opacity,
                );
            }
            // The dark tray shows the session color itself well.
            session_rgb
        }
        None => {
            fill(pixmap, &path, session_rgb, PILL_FILL_ALPHA * opacity);
            pixmap.stroke_path(
                &path,
                &solid_paint(
                    shaded(session_fill, PILL_RIM_SHADE),
                    PILL_RIM_ALPHA * opacity,
                ),
                &Stroke {
                    width: scale,
                    ..Default::default()
                },
                Transform::identity(),
                None,
            );
            if let Some(label) = &layout.label {
                paint_text(
                    pixmap,
                    font,
                    &label.text,
                    FONT_SIZE * scale,
                    label.origin,
                    PILL_TEXT,
                    label_opacity,
                );
            }
            shaded(session_fill, PILL_CHIP_SHADE)
        }
    };

    let chip_opacity = (alpha_scale * layout.chip_alpha).clamp(0.0, 1.0);
    if let Some(chip) = layout.delivery_chip {
        paint_badge_chip(pixmap, chip, chip_well, chip_opacity);
    }
    if let Some(chip) = layout.target_chip {
        paint_badge_chip(pixmap, chip, chip_well, chip_opacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_public_labels() {
        assert_eq!(
            sanitize_session_label("  Research\n\t run \u{0007} "),
            Some("Research run".to_owned())
        );
        assert_eq!(sanitize_session_label("\n\t"), None);
        let long = sanitize_session_label("abcdefghijklmnopqrstuvwxyz0123456789").unwrap();
        assert_eq!(long.chars().count(), MAX_SESSION_LABEL_CHARS);
        assert!(long.ends_with('…'));
    }

    const ORANGE: [u8; 4] = crate::DEFAULT_CURSOR_FILL;
    const DEEP_RED: [u8; 4] = [133, 39, 35, 255];

    fn input<'a>(
        label: Option<&'a str>,
        key_combo: Option<&'a KeyCombo>,
        scale: f32,
    ) -> SessionBadgeInput<'a> {
        SessionBadgeInput {
            label,
            key_combo,
            delivery: None,
            target: None,
            cursor: (200.0 * scale, 60.0 * scale),
            backing_scale: scale,
            label_alpha: 1.0,
            chip_alpha: 1.0,
            clip: None,
        }
    }

    fn combo(values: &[&str], mac: bool) -> KeyCombo {
        let keys = values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        crate::badge_mode::key_combo_parts_for(&keys, mac).unwrap()
    }

    /// Unpremultiplied color of the device pixel that holds `(x, y)`.
    fn pixel(pixmap: &Pixmap, x: f32, y: f32) -> [u8; 4] {
        let color = pixmap.pixel(x as u32, y as u32).unwrap().demultiply();
        [color.red(), color.green(), color.blue(), color.alpha()]
    }

    fn near(actual: [u8; 4], expected: [u8; 3], tolerance: u8) -> bool {
        (0..3).all(|channel| actual[channel].abs_diff(expected[channel]) <= tolerance)
    }

    #[test]
    fn pill_is_the_session_color_with_a_darker_rim_and_zero_alpha_paints_nothing() {
        for scale in [1.0f32, 2.0] {
            let (width, height) = ((400.0 * scale) as u32, (160.0 * scale) as u32);
            let layout = session_badge_layout(SessionBadgeInput {
                delivery: Some(DeliveryModifier::Foreground),
                target: Some(TargetModifier::Browser),
                clip: Some((width as f32, height as f32)),
                ..input(Some("Research run"), None, scale)
            })
            .unwrap();
            assert!(layout.keycaps.is_none());
            assert_eq!(layout.corner_radius, layout.rect.height() * 0.5);
            let rect = layout.rect;
            let mid_y = rect.y() + rect.height() * 0.5;
            let mut painted = Vec::new();
            for fill in [ORANGE, DEEP_RED] {
                let mut pixmap = Pixmap::new(width, height).unwrap();
                paint_session_badge(&mut pixmap, &layout, fill, 1.0);
                // Inside the rounded end, clear of the text.
                let body = pixel(&pixmap, rect.x() + 7.0 * scale, mid_y);
                assert!(
                    near(body, [fill[0], fill[1], fill[2]], 6),
                    "{body:?} {scale}x"
                );
                assert!(body[3] >= 245, "{body:?}");
                // The rim is darker than the fill.
                let rim = pixel(&pixmap, rect.x() + rect.width() * 0.5, rect.y());
                let sum = |color: [u8; 4]| color[..3].iter().map(|&c| u32::from(c)).sum::<u32>();
                assert!(sum(rim) * 10 < sum(body) * 9, "rim {rim:?} body {body:?}");

                // The filled delivery chip sits in a darker well, and both
                // chips keep white glyphs.
                let delivery = layout.delivery_chip.unwrap().rect;
                // Below the glyph, above the well's rim.
                let well = pixel(
                    &pixmap,
                    delivery.x() + delivery.width() * 0.5,
                    delivery.y() + 16.0 * scale,
                );
                assert!(
                    (0..3).all(|channel| well[channel] < body[channel] || body[channel] < 40),
                    "well {well:?} body {body:?}"
                );
                for chip in [layout.delivery_chip, layout.target_chip] {
                    let chip = chip.unwrap().rect;
                    let has_white_glyph = (0..chip.height() as u32).any(|dy| {
                        (0..chip.width() as u32).any(|dx| {
                            let color = pixel(&pixmap, chip.x() + dx as f32, chip.y() + dy as f32);
                            color[..3].iter().all(|&c| c > 225)
                        })
                    });
                    assert!(has_white_glyph, "{scale}x");
                }
                painted.push(pixmap);
            }
            assert_ne!(painted[0].data(), painted[1].data(), "{scale}x");

            let mut hidden = Pixmap::new(width, height).unwrap();
            paint_session_badge(&mut hidden, &layout, ORANGE, 0.0);
            assert!(
                hidden.data().chunks_exact(4).all(|pixel| pixel[3] == 0),
                "{scale}x"
            );
        }
    }

    /// Painted ink box of a text layout at `font_size`, `(left, top, right,
    /// bottom)` in device pixels, using the same rounding as the painter.
    fn text_ink(label: &BadgeLabelLayout, font_size: f32) -> (f32, f32, f32, f32) {
        let font = font().unwrap();
        let text = text_layout(font, &label.text, font_size);
        let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for glyph in text.glyphs() {
            if glyph.width == 0 || glyph.height == 0 {
                continue;
            }
            let x = (label.origin.0 + glyph.x).round();
            let y = (label.origin.1 + glyph.y).round();
            l = l.min(x);
            t = t.min(y);
            r = r.max(x + glyph.width as f32);
            b = b.max(y + glyph.height as f32);
        }
        (l, t, r, b)
    }

    #[test]
    fn key_combos_draw_one_cap_per_key_inside_the_tray() {
        let cases = [
            (combo(&["ctrl", "c"], false), &["Ctrl", "C"][..], 1),
            (
                combo(&["ctrl", "shift", "z"], false),
                &["Ctrl", "Shift", "Z"],
                2,
            ),
            (
                combo(&["alt", "page_down"], false),
                &["Alt", "Page down"],
                1,
            ),
            (combo(&["ctrl", "+"], false), &["Ctrl", "+"], 1),
            (combo(&["shift", "cmd", "z"], true), &["⇧", "⌘", "Z"], 0),
            (combo(&["cmd", "+"], true), &["⌘", "+"], 0),
        ];
        for scale in [1.0f32, 2.0, 3.0] {
            let tolerance = scale + 0.01;
            for (combo, keys, joins) in &cases {
                for foreground in [false, true] {
                    let layout = session_badge_layout(SessionBadgeInput {
                        delivery: foreground.then_some(DeliveryModifier::Foreground),
                        ..input(None, Some(combo), scale)
                    })
                    .unwrap();
                    let what = format!("{} @{scale}x fg {foreground}", combo.text());
                    let tray = layout.rect;
                    let keycaps = layout.keycaps.as_ref().expect(&what);
                    assert!(layout.label.is_none(), "{what}");
                    assert_eq!(
                        keycaps
                            .caps
                            .iter()
                            .map(|cap| cap.label.text.as_str())
                            .collect::<Vec<_>>(),
                        *keys,
                        "{what}"
                    );
                    assert_eq!(keycaps.joins.len(), *joins, "{what}");
                    assert!(
                        (tray.height() - BADGE_HEIGHT * scale).abs() < 0.01,
                        "{what}"
                    );
                    assert!(tray.width() <= BADGE_MAX_WIDTH * scale, "{what}");
                    assert_eq!(layout.corner_radius, KEYCAP_TRAY_RADIUS * scale);

                    let padding = KEYCAP_TRAY_PADDING * scale;
                    let mut previous_right = tray.x();
                    for cap in &keycaps.caps {
                        let face = cap.face;
                        assert!((face.height() - KEYCAP_HEIGHT * scale).abs() < 0.01);
                        assert!(face.width() >= face.height(), "{what}");
                        assert!(face.x() >= previous_right, "caps do not overlap: {what}");
                        assert!((face.y() - (tray.y() + padding)).abs() < 0.01, "{what}");
                        let bevel_bottom = face.y() + face.height() + KEYCAP_BEVEL * scale;
                        assert!(
                            (tray.y() + tray.height() - bevel_bottom - padding).abs() < 0.01,
                            "{what}"
                        );
                        previous_right = face.x() + face.width();

                        // Each key's ink is centered on its face.
                        let (l, t, r, b) = text_ink(&cap.label, KEYCAP_FONT_SIZE * scale);
                        assert!(
                            ((l - face.x()) - (face.x() + face.width() - r)).abs() <= tolerance,
                            "{} in {what}",
                            cap.label.text
                        );
                        assert!(t >= face.y() && b <= face.y() + face.height(), "{what}");
                        if !cap.label.text.chars().any(char::is_lowercase) {
                            let top_gap = t - face.y();
                            let bottom_gap = face.y() + face.height() - b;
                            assert!(
                                (top_gap - bottom_gap).abs() <= tolerance,
                                "{} in {what}: top {top_gap} bottom {bottom_gap}",
                                cap.label.text
                            );
                        }
                    }
                    let first = keycaps.caps[0].face.x();
                    assert!((first - (tray.x() + padding)).abs() < 0.01, "{what}");
                    let right = match layout.delivery_chip {
                        Some(chip) => {
                            assert!(chip.rect.x() > previous_right, "{what}");
                            let chip_mid = chip.rect.y() + chip.rect.height() * 0.5;
                            assert!((chip_mid - (tray.y() + tray.height() * 0.5)).abs() < 0.01);
                            chip.rect.x() + chip.rect.width()
                        }
                        None => previous_right,
                    };
                    assert!(
                        (tray.x() + tray.width() - right - padding).abs() < 0.01,
                        "{what}"
                    );
                    for &(x, _) in &keycaps.joins {
                        assert!(x > first && x < previous_right, "{what}");
                    }
                }
            }
        }
    }

    #[test]
    fn keys_with_letters_share_one_baseline_in_the_tray() {
        let combos = [
            combo(&["ctrl", "shift", "z"], false),
            combo(&["alt", "page_down"], false),
            combo(&["ctrl", "f4"], false),
            combo(&["shift", "cmd", "z"], true),
        ];
        for scale in [1.0f32, 1.5, 2.0, 3.0] {
            let mut origins = Vec::new();
            for combo in &combos {
                for cursor_y in [60.0, 60.3, 60.5, 60.8] {
                    let layout = session_badge_layout(SessionBadgeInput {
                        cursor: (200.0 * scale, cursor_y * scale),
                        ..input(None, Some(combo), scale)
                    })
                    .unwrap();
                    let keycaps = layout.keycaps.unwrap();
                    let face_y = keycaps.caps[0].face.y();
                    for cap in &keycaps.caps {
                        if cap.label.text.chars().any(char::is_alphanumeric) {
                            origins.push((cap.label.text.clone(), cap.label.origin.1 - face_y));
                        }
                    }
                }
            }
            let first = origins[0].1;
            for (text, origin) in &origins {
                assert_eq!(*origin, first, "{text} @{scale}x: {origins:?}");
            }
        }
    }

    #[test]
    fn cap_height_text_is_centered_in_the_pill_and_the_caps() {
        // Letters with no overshoot, ascender or descender, so their ink
        // spans exactly the cap height.
        for scale in [1.0f32, 2.0, 3.0] {
            let pill = session_badge_layout(input(Some("Hmm"), None, scale)).unwrap();
            let (_, t, _, b) = label_ink(&pill);
            let rect = pill.rect;
            let (top_gap, bottom_gap) = (t - rect.y(), rect.y() + rect.height() - b);
            assert!(
                (top_gap - bottom_gap).abs() <= 1.01,
                "pill @{scale}x: top {top_gap} bottom {bottom_gap}"
            );
            for combo in [
                combo(&["ctrl", "z"], false),
                combo(&["ctrl", "shift", "f4"], false),
                combo(&["cmd", "k"], true),
            ] {
                let layout = session_badge_layout(input(None, Some(&combo), scale)).unwrap();
                let cap = layout.keycaps.unwrap().caps.pop().unwrap();
                let (_, t, _, b) = text_ink(&cap.label, KEYCAP_FONT_SIZE * scale);
                let face = cap.face;
                let (top_gap, bottom_gap) = (t - face.y(), face.y() + face.height() - b);
                assert!(
                    (top_gap - bottom_gap).abs() <= 1.01,
                    "{} @{scale}x: top {top_gap} bottom {bottom_gap}",
                    cap.label.text
                );
            }
        }
    }

    #[test]
    fn a_key_combo_too_wide_for_keycaps_falls_back_to_its_text() {
        let wide = combo(&["ctrl", "alt", "shift", "win", "print_screen"], false);
        for scale in [1.0f32, 2.0] {
            let layout = session_badge_layout(input(None, Some(&wide), scale)).unwrap();
            assert!(layout.keycaps.is_none(), "{scale}x");
            assert_eq!(layout.corner_radius, layout.rect.height() * 0.5);
            assert!(layout.rect.width() <= BADGE_MAX_WIDTH * scale + 0.01);
            let label = layout.label.unwrap();
            assert!(
                label.text == wide.text()
                    || (label.text.ends_with('…') && wide.text().starts_with("Ctrl+Alt")),
                "{}",
                label.text
            );
            assert!(label.text.starts_with("Ctrl+Alt"), "{}", label.text);
        }
        // A combo with no visible label paints only its chips.
        let short = combo(&["ctrl", "c"], false);
        let chips_only = session_badge_layout(SessionBadgeInput {
            delivery: Some(DeliveryModifier::Foreground),
            label_alpha: 0.0,
            ..input(None, Some(&short), 1.0)
        })
        .unwrap();
        assert!(chips_only.keycaps.is_none() && chips_only.label.is_none());
        assert!(chips_only.delivery_chip.is_some());
    }

    #[test]
    fn badges_paint_inside_the_extents_and_keep_inside_the_clip() {
        let extents = session_badge_extents();
        let wide_caps = combo(&["ctrl", "shift", "page_down"], false);
        for scale in [1.0f32, 2.0] {
            let cases = [
                (
                    SessionBadgeInput {
                        delivery: Some(DeliveryModifier::Foreground),
                        ..input(None, Some(&wide_caps), scale)
                    },
                    true,
                ),
                (
                    SessionBadgeInput {
                        delivery: Some(DeliveryModifier::Foreground),
                        target: Some(TargetModifier::Desktop),
                        ..input(Some("A very long public session label"), None, scale)
                    },
                    false,
                ),
            ];
            for (case, keycaps) in cases {
                let (width, height) = ((400.0 * scale) as u32, (200.0 * scale) as u32);
                let cursor = (200.0 * scale, 40.0 * scale);
                let layout = session_badge_layout(SessionBadgeInput { cursor, ..case }).unwrap();
                assert_eq!(layout.keycaps.is_some(), keycaps, "{scale}x");
                let mut pixmap = Pixmap::new(width, height).unwrap();
                paint_session_badge(&mut pixmap, &layout, ORANGE, 1.0);
                let left = cursor.0 - extents.horizontal * scale - 1.0;
                let right = cursor.0 + extents.horizontal * scale + 1.0;
                let top = cursor.1 - extents.above * scale - 1.0;
                let bottom = cursor.1 + extents.below * scale + 1.0;
                for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
                    if pixel[3] == 0 {
                        continue;
                    }
                    let x = (index as u32 % width) as f32;
                    let y = (index as u32 / width) as f32;
                    assert!(
                        x >= left && x + 1.0 <= right && y >= top && y + 1.0 <= bottom,
                        "pixel ({x}, {y}) outside the extents, keycaps {keycaps} @{scale}x"
                    );
                }

                // Near every corner of a clip, the badge stays inside it.
                let clip = (300.0 * scale, 100.0 * scale);
                for cursor in [
                    (0.0, -40.0 * scale),
                    (clip.0, clip.1),
                    (clip.0 - 4.0 * scale, 0.0),
                    (4.0 * scale, clip.1 - 10.0 * scale),
                ] {
                    let layout = session_badge_layout(SessionBadgeInput {
                        cursor,
                        clip: Some(clip),
                        ..case
                    })
                    .unwrap();
                    let rect = layout.rect;
                    let margin = 2.0 * scale - 0.5;
                    assert!(
                        rect.x() >= margin
                            && rect.y() >= margin
                            && rect.x() + rect.width() <= clip.0 - margin
                            && rect.y() + rect.height() <= clip.1 - margin,
                        "{rect:?} at {cursor:?} @{scale}x"
                    );
                    for cap in layout.keycaps.iter().flat_map(|keycaps| &keycaps.caps) {
                        let face = cap.face;
                        assert!(
                            face.x() >= rect.x()
                                && face.x() + face.width() <= rect.x() + rect.width()
                                && face.y() >= rect.y()
                                && face.y() + face.height() + KEYCAP_BEVEL * scale
                                    <= rect.y() + rect.height(),
                            "{face:?} in {rect:?}"
                        );
                    }
                    for chip in [layout.delivery_chip, layout.target_chip]
                        .into_iter()
                        .flatten()
                    {
                        assert!(
                            chip.rect.x() + chip.rect.width() <= rect.x() + rect.width() + 0.01
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn keycaps_are_light_faces_on_a_session_colored_bevel_in_a_dark_tray() {
        let ctrl_c = combo(&["ctrl", "c"], false);
        for scale in [1.0f32, 2.0] {
            let (width, height) = ((400.0 * scale) as u32, (160.0 * scale) as u32);
            let layout = session_badge_layout(input(None, Some(&ctrl_c), scale)).unwrap();
            let keycaps = layout.keycaps.as_ref().unwrap();
            let tray = layout.rect;
            for fill in [ORANGE, DEEP_RED] {
                let mut pixmap = Pixmap::new(width, height).unwrap();
                paint_session_badge(&mut pixmap, &layout, fill, 1.0);
                for cap in &keycaps.caps {
                    let face = cap.face;
                    // The face, clear of its text.
                    let at_face = pixel(
                        &pixmap,
                        face.x() + 2.5 * scale,
                        face.y() + face.height() * 0.5,
                    );
                    assert!(near(at_face, KEYCAP_FACE, 3), "{at_face:?} @{scale}x");
                    // The bevel under the middle of the face.
                    let at_bevel = pixel(
                        &pixmap,
                        face.x() + face.width() * 0.5,
                        face.y() + face.height() + KEYCAP_BEVEL * scale * 0.5,
                    );
                    assert!(
                        near(at_bevel, [fill[0], fill[1], fill[2]], 3),
                        "{at_bevel:?} @{scale}x"
                    );
                    // Dark text on the face.
                    let (l, t, r, b) = text_ink(&cap.label, KEYCAP_FONT_SIZE * scale);
                    let darkest = (t as u32..b as u32)
                        .flat_map(|y| (l as u32..r as u32).map(move |x| (x, y)))
                        .map(|(x, y)| pixel(&pixmap, x as f32, y as f32))
                        .map(|color| color[..3].iter().map(|&c| u32::from(c)).sum::<u32>())
                        .min()
                        .unwrap();
                    assert!(darkest < 3 * 80, "{} @{scale}x", cap.label.text);
                }
                let at_tray = pixel(
                    &pixmap,
                    tray.x() + 2.0 * scale,
                    tray.y() + tray.height() * 0.5,
                );
                assert!(at_tray[..3].iter().all(|&c| c < 40), "{at_tray:?}");
                assert!(at_tray[3] >= 215, "{at_tray:?}");
            }
            let mut hidden = Pixmap::new(width, height).unwrap();
            paint_session_badge(&mut hidden, &layout, ORANGE, 0.0);
            assert!(hidden.data().chunks_exact(4).all(|pixel| pixel[3] == 0));
        }
    }

    #[test]
    fn keycap_tray_fills_the_badge_height() {
        assert_eq!(
            KEYCAP_TRAY_PADDING * 2.0 + KEYCAP_HEIGHT + KEYCAP_BEVEL,
            BADGE_HEIGHT
        );
        assert_eq!(KEYCAP_TRAY_PADDING, 4.0);
    }

    #[test]
    fn modifier_only_badge_is_compact_and_keeps_delivery_before_target() {
        let layout = session_badge_layout(SessionBadgeInput {
            label: None,
            key_combo: None,
            delivery: Some(DeliveryModifier::Background),
            target: Some(TargetModifier::Pixel),
            cursor: (120.0, 60.0),
            backing_scale: 1.0,
            label_alpha: 0.0,
            chip_alpha: 1.0,
            clip: None,
        })
        .unwrap();
        assert!(layout.label.is_none());
        assert!(layout.rect.width() < 80.0);
        let delivery = layout.delivery_chip.unwrap();
        let target = layout.target_chip.unwrap();
        assert!(delivery.filled);
        assert!(!target.filled);
        assert!(delivery.rect.x() < target.rect.x());
    }

    #[test]
    fn label_starts_at_the_leading_padding_with_no_reserved_marker() {
        let layout = session_badge_layout(SessionBadgeInput {
            label: Some("Research run"),
            key_combo: None,
            delivery: None,
            target: None,
            cursor: (120.0, 60.0),
            backing_scale: 1.0,
            label_alpha: 1.0,
            chip_alpha: 0.0,
            clip: None,
        })
        .unwrap();
        let label = layout.label.unwrap();
        assert!((label.origin.0 - (layout.rect.x() + HORIZONTAL_PADDING)).abs() < 1.5);
    }

    /// Painted ink box of a pill label, `(left, top, right, bottom)` in
    /// device pixels, using the same rounding as `paint_session_badge`.
    fn label_ink(layout: &SessionBadgeLayout) -> (f32, f32, f32, f32) {
        text_ink(layout.label.as_ref().unwrap(), FONT_SIZE * layout.scale)
    }

    #[test]
    fn every_badge_kind_centres_its_content_at_every_scale() {
        // Labels whose ink spans exactly the cap height are vertically
        // centred; every label and chip group is horizontally centred.
        let cases: &[(&str, bool, bool)] = &[
            ("⌘A", false, true),
            ("⌘C", false, true),
            ("⇧⌘Z", false, true),
            ("⌃⌥⇧⌘K", false, true),
            ("CTRL+C", false, true),
            ("Ctrl+C", false, false),
            ("research", false, false),
            ("writer", false, false),
            ("foreground", true, false),
            ("⌘V", true, true),
            ("A very long public session label", false, false),
        ];
        for scale in [1.0f32, 2.0, 3.0] {
            for &(text, foreground, caps) in cases {
                let layout = session_badge_layout(SessionBadgeInput {
                    label: Some(text),
                    key_combo: None,
                    delivery: foreground.then_some(DeliveryModifier::Foreground),
                    target: None,
                    cursor: (400.0 * scale, 60.0 * scale),
                    backing_scale: scale,
                    label_alpha: 1.0,
                    chip_alpha: 1.0,
                    clip: None,
                })
                .unwrap();
                let rect = layout.rect;
                let (l, t, r, b) = label_ink(&layout);
                let right_edge = layout
                    .delivery_chip
                    .map_or(r, |chip| chip.rect.x() + chip.rect.width());
                let left_gap = l - rect.x();
                let right_gap = rect.x() + rect.width() - right_edge;
                let tolerance = 1.0 * scale.max(1.0) + 0.01;
                assert!(
                    (left_gap - right_gap).abs() <= tolerance,
                    "{text} @{scale}x horizontal: left {left_gap} right {right_gap}"
                );
                if caps {
                    let top_gap = t - rect.y();
                    let bottom_gap = rect.y() + rect.height() - b;
                    assert!(
                        (top_gap - bottom_gap).abs() <= tolerance,
                        "{text} @{scale}x vertical: top {top_gap} bottom {bottom_gap}"
                    );
                }
                if let Some(chip) = layout.delivery_chip {
                    let chip_mid = chip.rect.y() + chip.rect.height() * 0.5;
                    assert!((chip_mid - (rect.y() + rect.height() * 0.5)).abs() < 0.01);
                }
            }
        }
    }

    #[test]
    fn labels_share_one_baseline() {
        for scale in [1.0f32, 2.0] {
            let origin_y = |text: &str| {
                session_badge_layout(SessionBadgeInput {
                    label: Some(text),
                    key_combo: None,
                    delivery: None,
                    target: None,
                    cursor: (200.0, 60.0),
                    backing_scale: scale,
                    label_alpha: 1.0,
                    chip_alpha: 0.0,
                    clip: None,
                })
                .unwrap()
                .label
                .unwrap()
                .origin
                .1
            };
            assert_eq!(origin_y("research"), origin_y("Typing"));
            assert_eq!(origin_y("research"), origin_y("writer"));
            assert_eq!(origin_y("research"), origin_y("Ctrl+C"));
        }
    }

    #[test]
    fn badge_font_has_the_mac_modifier_symbols() {
        let font = font().expect("badge font");
        for symbol in ['⌘', '⇧', '⌥', '⌃'] {
            assert_ne!(font.lookup_glyph_index(symbol), 0, "{symbol}");
        }
    }

    #[test]
    fn long_label_stays_inside_the_bounded_badge() {
        let layout = session_badge_layout(SessionBadgeInput {
            label: Some("A very long public session label"),
            key_combo: None,
            delivery: Some(DeliveryModifier::Foreground),
            target: Some(TargetModifier::Desktop),
            cursor: (120.0, 60.0),
            backing_scale: 1.0,
            label_alpha: 1.0,
            chip_alpha: 1.0,
            clip: None,
        })
        .unwrap();
        assert!(layout.rect.width() <= BADGE_MAX_WIDTH);
        assert!(layout.label.unwrap().text.ends_with('…'));
    }
}
