//! Geometry and timing for `move_along_path`.
//!
//! A path arrives as one of three forms: `segments` (a start point plus
//! line, quadratic, cubic and arc segments), `points` (a polyline, optionally
//! smoothed into a Catmull-Rom curve), or `parametric` (x(t) and y(t)
//! expressions). Sampling runs in two passes. The path is first flattened
//! into a dense polyline whose chords are at most half a pixel on curves.
//! That polyline is then resampled at an even arc-length step no larger than
//! `spacing_px`, so the samples are evenly spaced along the path whatever
//! the form. Each sample's time is the inverse of the easing curve applied
//! to its arc-length fraction: with `linear` the pointer moves at an even
//! speed, and the other curves only change the speed, never the samples.
//!
//! Everything here is pure. Mapping the samples to the screen, checking that
//! they are on it and delivering them are the caller's job.

use anyhow::{anyhow, bail, ensure, Result};
use serde_json::{Map, Value};

/// Most samples one call may deliver.
pub const MAX_SAMPLES: usize = 4000;
/// Longest glide one call may take.
pub const MAX_DURATION_MS: u64 = 30_000;
pub const DEFAULT_DURATION_MS: u64 = 600;
pub const DEFAULT_SPACING_PX: f64 = 4.0;
pub const MIN_SPACING_PX: f64 = 1.0;
pub const MAX_SPACING_PX: f64 = 1000.0;
/// Most entries in `segments`.
pub const MAX_SEGMENTS: usize = 256;
/// Most entries in `points`.
pub const MAX_POINTS: usize = 2000;
/// Largest arc sweep, ten full turns. Longer spirals belong in `parametric`.
pub const MAX_ARC_SWEEP_DEG: f64 = 3600.0;
/// Longest chord of the dense flattening pass on a curve, in pixels.
const FLATTEN_STEP_PX: f64 = 0.5;
/// Bound on the dense flattening pass, so absurd input fails fast instead
/// of allocating without limit. It is far above anything `MAX_SAMPLES` can
/// deliver at the smallest spacing.
const MAX_DENSE_POINTS: usize = 400_000;
/// Parametric curves are probed at this many values of `t` to estimate
/// their length before the dense pass.
const PARAMETRIC_PROBE: usize = 1024;

pub type Point = (f64, f64);

#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Line {
        to: Point,
    },
    Quad {
        control: Point,
        to: Point,
    },
    Cubic {
        control1: Point,
        control2: Point,
        to: Point,
    },
    /// An arc from the current point around `center`. A positive sweep is
    /// clockwise on screen (y grows downward); the radius is the distance
    /// from the current point to the center.
    Arc {
        center: Point,
        sweep_deg: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathSpec {
    Segments {
        start: Point,
        segments: Vec<Segment>,
    },
    Points {
        points: Vec<Point>,
        smooth: bool,
    },
    Parametric {
        x: String,
        y: String,
        t_from: f64,
        t_to: f64,
    },
}

impl PathSpec {
    /// Pieces the caller described: segments, polyline spans, or one curve.
    pub fn segment_count(&self) -> usize {
        match self {
            Self::Segments { segments, .. } => segments.len(),
            Self::Points { points, .. } => points.len().saturating_sub(1),
            Self::Parametric { .. } => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Easing {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
}

impl Easing {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "linear" => Self::Linear,
            "ease_in" => Self::EaseIn,
            "ease_out" => Self::EaseOut,
            "ease_in_out" => Self::EaseInOut,
            other => {
                bail!("easing must be linear, ease_in, ease_out or ease_in_out, not '{other}'")
            }
        })
    }

    /// Fraction of the path covered at time fraction `u` (both 0..=1).
    /// Quadratic curves: gentle enough that the first sample of an eased
    /// path is not held back for a long time.
    pub fn progress(self, u: f64) -> f64 {
        let u = u.clamp(0.0, 1.0);
        match self {
            Self::Linear => u,
            Self::EaseIn => u * u,
            Self::EaseOut => 1.0 - (1.0 - u) * (1.0 - u),
            Self::EaseInOut => {
                if u < 0.5 {
                    2.0 * u * u
                } else {
                    1.0 - 2.0 * (1.0 - u) * (1.0 - u)
                }
            }
        }
    }

    /// Time fraction at which the path fraction `p` is reached: the inverse
    /// of [`Easing::progress`].
    pub fn time_at(self, p: f64) -> f64 {
        let p = p.clamp(0.0, 1.0);
        match self {
            Self::Linear => p,
            Self::EaseIn => p.sqrt(),
            Self::EaseOut => 1.0 - (1.0 - p).sqrt(),
            Self::EaseInOut => {
                if p < 0.5 {
                    (p / 2.0).sqrt()
                } else {
                    1.0 - ((1.0 - p) / 2.0).sqrt()
                }
            }
        }
    }
}

/// A click after the glide, at its last point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndClick {
    Left,
    Right,
    Middle,
    Double,
}

impl EndClick {
    /// X button number and click count.
    pub fn x_button_and_count(self) -> (u8, usize) {
        match self {
            Self::Left => (1, 1),
            Self::Middle => (2, 1),
            Self::Right => (3, 1),
            Self::Double => (1, 2),
        }
    }
}

/// A validated `move_along_path` request, minus where it is delivered.
#[derive(Clone, Debug, PartialEq)]
pub struct PathRequest {
    pub path: PathSpec,
    /// X button held along the path (1 left, 2 middle, 3 right), or none.
    pub button: Option<u8>,
    pub click_at_end: Option<EndClick>,
    pub duration_ms: u64,
    pub easing: Easing,
    pub spacing_px: f64,
}

/// Evenly spaced samples and the time of each, relative to the first.
#[derive(Clone, Debug, PartialEq)]
pub struct SampledPath {
    pub points: Vec<Point>,
    pub offsets_ms: Vec<f64>,
    pub length_px: f64,
    pub segment_count: usize,
}

fn number(value: &Value, what: &str) -> Result<f64> {
    let n = value
        .as_f64()
        .ok_or_else(|| anyhow!("{what} must be a number"))?;
    ensure!(n.is_finite(), "{what} must be a finite number");
    Ok(n)
}

fn point(value: &Value, what: &str) -> Result<Point> {
    match value.as_array().map(Vec::as_slice) {
        Some([x, y]) => Ok((number(x, what)?, number(y, what)?)),
        _ => bail!("{what} must be a point [x, y]"),
    }
}

fn field<'a>(object: &'a Map<String, Value>, key: &str, what: &str) -> Result<&'a Value> {
    object
        .get(key)
        .ok_or_else(|| anyhow!("{what} needs `{key}`"))
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<()> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!(
            "{what} does not take `{key}`; it takes {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

fn parse_segment(value: &Value, index: usize) -> Result<Segment> {
    let what = format!("segments[{index}]");
    let object = value.as_object().ok_or_else(|| {
        anyhow!("{what} must be an object such as {{\"line\": {{\"to\": [x, y]}}}}")
    })?;
    let kinds: Vec<&str> = object.keys().map(String::as_str).collect();
    let [kind] = kinds.as_slice() else {
        bail!("{what} must have exactly one of line, quad, cubic or arc");
    };
    let body = object[*kind]
        .as_object()
        .ok_or_else(|| anyhow!("{what}.{kind} must be an object"))?;
    let what = format!("{what}.{kind}");
    let at =
        |key: &str| -> Result<Point> { point(field(body, key, &what)?, &format!("{what}.{key}")) };
    Ok(match *kind {
        "line" => {
            only_keys(body, &["to"], &what)?;
            Segment::Line { to: at("to")? }
        }
        "quad" => {
            only_keys(body, &["control", "to"], &what)?;
            Segment::Quad {
                control: at("control")?,
                to: at("to")?,
            }
        }
        "cubic" => {
            only_keys(body, &["control1", "control2", "to"], &what)?;
            Segment::Cubic {
                control1: at("control1")?,
                control2: at("control2")?,
                to: at("to")?,
            }
        }
        "arc" => {
            only_keys(body, &["center", "sweep_deg"], &what)?;
            let sweep_deg = number(
                field(body, "sweep_deg", &what)?,
                &format!("{what}.sweep_deg"),
            )?;
            ensure!(
                sweep_deg.abs() <= MAX_ARC_SWEEP_DEG,
                "{what}.sweep_deg must be within ±{MAX_ARC_SWEEP_DEG} degrees; use parametric for longer spirals"
            );
            Segment::Arc {
                center: at("center")?,
                sweep_deg,
            }
        }
        other => bail!("{what}: unknown segment kind '{other}'; use line, quad, cubic or arc"),
    })
}

fn parse_path(args: &Value) -> Result<PathSpec> {
    let has = |key: &str| args.get(key).is_some_and(|value| !value.is_null());
    let forms: Vec<&str> = ["segments", "points", "parametric"]
        .into_iter()
        .filter(|key| has(key))
        .collect();
    match forms.as_slice() {
        [] => bail!("give the path as exactly one of segments (with start), points or parametric"),
        [_] => {}
        many => bail!(
            "give the path as exactly one of segments, points or parametric, not {}",
            many.join(" and ")
        ),
    }
    if has("start") && forms[0] != "segments" {
        bail!("start belongs to segments; points and parametric carry their own start");
    }
    if has("smooth") && forms[0] != "points" {
        bail!("smooth applies only to points");
    }
    match forms[0] {
        "segments" => {
            let start = point(
                args.get("start")
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| anyhow!("segments needs a start point [x, y]"))?,
                "start",
            )?;
            let items = args["segments"]
                .as_array()
                .ok_or_else(|| anyhow!("segments must be an array"))?;
            ensure!(!items.is_empty(), "segments must not be empty");
            ensure!(
                items.len() <= MAX_SEGMENTS,
                "segments has {} entries; the limit is {MAX_SEGMENTS}",
                items.len()
            );
            let segments = items
                .iter()
                .enumerate()
                .map(|(index, item)| parse_segment(item, index))
                .collect::<Result<Vec<_>>>()?;
            Ok(PathSpec::Segments { start, segments })
        }
        "points" => {
            let items = args["points"]
                .as_array()
                .ok_or_else(|| anyhow!("points must be an array of [x, y]"))?;
            ensure!(items.len() >= 2, "points needs at least 2 points");
            ensure!(
                items.len() <= MAX_POINTS,
                "points has {} entries; the limit is {MAX_POINTS}",
                items.len()
            );
            let points = items
                .iter()
                .enumerate()
                .map(|(index, item)| point(item, &format!("points[{index}]")))
                .collect::<Result<Vec<_>>>()?;
            let smooth = match args.get("smooth") {
                None | Some(Value::Null) => false,
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| anyhow!("smooth must be true or false"))?,
            };
            Ok(PathSpec::Points { points, smooth })
        }
        _ => {
            let object = args["parametric"]
                .as_object()
                .ok_or_else(|| anyhow!("parametric must be an object {{x, y, t_from, t_to}}"))?;
            only_keys(object, &["x", "y", "t_from", "t_to"], "parametric")?;
            let expr = |key: &str| -> Result<String> {
                field(object, key, "parametric")?
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("parametric.{key} must be an expression string"))
            };
            let bound = |key: &str| -> Result<f64> {
                number(
                    field(object, key, "parametric")?,
                    &format!("parametric.{key}"),
                )
            };
            let (t_from, t_to) = (bound("t_from")?, bound("t_to")?);
            ensure!(t_from != t_to, "parametric t_from and t_to must differ");
            Ok(PathSpec::Parametric {
                x: expr("x")?,
                y: expr("y")?,
                t_from,
                t_to,
            })
        }
    }
}

fn parse_button(value: Option<&Value>) -> Result<Option<u8>> {
    let name = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow!("button must be a string"))?,
    };
    Ok(match name {
        "none" => None,
        "left" => Some(1),
        "middle" => Some(2),
        "right" => Some(3),
        other => bail!("button must be none, left, right or middle, not '{other}'"),
    })
}

fn parse_end_click(value: Option<&Value>) -> Result<Option<EndClick>> {
    let name = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow!("click_at_end must be a string"))?,
    };
    Ok(Some(match name {
        "left" => EndClick::Left,
        "right" => EndClick::Right,
        "middle" => EndClick::Middle,
        "double" => EndClick::Double,
        other => bail!("click_at_end must be left, right, middle or double, not '{other}'"),
    }))
}

impl PathRequest {
    /// Parse and validate the path, button and timing arguments. Scope,
    /// pid and window_id are the caller's.
    pub fn from_args(args: &Value) -> Result<Self> {
        let path = parse_path(args)?;
        let button = parse_button(args.get("button"))?;
        let click_at_end = parse_end_click(args.get("click_at_end"))?;
        if click_at_end.is_some() && button.is_some() {
            bail!("click_at_end needs no button (or button \"none\"); a held button is already released at the end");
        }
        let duration_ms = match args.get("duration_ms") {
            None | Some(Value::Null) => DEFAULT_DURATION_MS,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| anyhow!("duration_ms must be a whole number of milliseconds"))?,
        };
        ensure!(
            duration_ms <= MAX_DURATION_MS,
            "duration_ms must be at most {MAX_DURATION_MS}"
        );
        let easing = match args.get("easing") {
            None | Some(Value::Null) => Easing::EaseInOut,
            Some(value) => Easing::parse(
                value
                    .as_str()
                    .ok_or_else(|| anyhow!("easing must be a string"))?,
            )?,
        };
        let spacing_px = match args.get("spacing_px") {
            None | Some(Value::Null) => DEFAULT_SPACING_PX,
            Some(value) => number(value, "spacing_px")?,
        };
        ensure!(
            (MIN_SPACING_PX..=MAX_SPACING_PX).contains(&spacing_px),
            "spacing_px must be between {MIN_SPACING_PX} and {MAX_SPACING_PX}"
        );
        Ok(Self {
            path,
            button,
            click_at_end,
            duration_ms,
            easing,
            spacing_px,
        })
    }
}

fn distance(a: Point, b: Point) -> f64 {
    (b.0 - a.0).hypot(b.1 - a.1)
}

/// Chord count for a curve piece of (estimated) length `length`.
fn pieces(length: f64) -> usize {
    ((length / FLATTEN_STEP_PX).ceil() as usize).max(1)
}

fn quad_at(p0: Point, c: Point, p1: Point, t: f64) -> Point {
    let s = 1.0 - t;
    (
        s * s * p0.0 + 2.0 * s * t * c.0 + t * t * p1.0,
        s * s * p0.1 + 2.0 * s * t * c.1 + t * t * p1.1,
    )
}

fn cubic_at(p0: Point, c1: Point, c2: Point, p1: Point, t: f64) -> Point {
    let s = 1.0 - t;
    let (a, b, c, d) = (s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t);
    (
        a * p0.0 + b * c1.0 + c * c2.0 + d * p1.0,
        a * p0.1 + b * c1.1 + c * c2.1 + d * p1.1,
    )
}

/// Dense polyline builder that refuses to grow past [`MAX_DENSE_POINTS`].
struct Dense(Vec<Point>);

impl Dense {
    fn reserve(&mut self, more: usize) -> Result<()> {
        ensure!(
            self.0.len().saturating_add(more) <= MAX_DENSE_POINTS,
            "the path is too long to sample; at most {MAX_SAMPLES} samples can be sent"
        );
        self.0.reserve(more);
        Ok(())
    }

    fn last(&self) -> Point {
        *self.0.last().expect("dense path starts with a point")
    }

    fn quad(&mut self, control: Point, to: Point) -> Result<()> {
        let from = self.last();
        let n = pieces(distance(from, control) + distance(control, to));
        self.reserve(n)?;
        for i in 1..n {
            self.0.push(quad_at(from, control, to, i as f64 / n as f64));
        }
        self.0.push(to);
        Ok(())
    }

    fn cubic(&mut self, control1: Point, control2: Point, to: Point) -> Result<()> {
        let from = self.last();
        let n = pieces(
            distance(from, control1) + distance(control1, control2) + distance(control2, to),
        );
        self.reserve(n)?;
        for i in 1..n {
            self.0
                .push(cubic_at(from, control1, control2, to, i as f64 / n as f64));
        }
        self.0.push(to);
        Ok(())
    }

    fn arc(&mut self, center: Point, sweep_deg: f64) -> Result<()> {
        let from = self.last();
        let radius = distance(center, from);
        ensure!(
            radius > 1e-9,
            "arc center ({}, {}) is the current point; an arc needs a nonzero radius",
            center.0,
            center.1
        );
        let start = (from.1 - center.1).atan2(from.0 - center.0);
        let sweep = sweep_deg.to_radians();
        let n = pieces(radius * sweep.abs());
        self.reserve(n)?;
        for i in 1..=n {
            let angle = start + sweep * i as f64 / n as f64;
            self.0.push((
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            ));
        }
        Ok(())
    }
}

/// Uniform Catmull-Rom spline through `points`, as cubic Bezier controls for
/// each span. The curve passes through every point; the end spans use the
/// end points themselves as their outer neighbors.
fn catmull_rom_controls(points: &[Point]) -> Vec<(Point, Point, Point)> {
    let n = points.len();
    (0..n - 1)
        .map(|i| {
            let p0 = points[i.saturating_sub(1)];
            let p1 = points[i];
            let p2 = points[i + 1];
            let p3 = points[(i + 2).min(n - 1)];
            (
                (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0),
                (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 - (p3.1 - p1.1) / 6.0),
                p2,
            )
        })
        .collect()
}

fn parametric_points(
    f: &dyn Fn(f64) -> f64,
    g: &dyn Fn(f64) -> f64,
    t_from: f64,
    t_to: f64,
    n: usize,
) -> Result<Vec<Point>> {
    (0..=n)
        .map(|i| {
            // Exact bounds at both ends, so the endpoints are x(t_from) and
            // x(t_to) rather than a rounding of them.
            let t = if i == n {
                t_to
            } else {
                t_from + (t_to - t_from) * i as f64 / n as f64
            };
            let (x, y) = (f(t), g(t));
            ensure!(
                x.is_finite() && y.is_finite(),
                "parametric path is not finite at t = {t} (x = {x}, y = {y})"
            );
            Ok((x, y))
        })
        .collect()
}

/// The dense polyline of `path`: exact at its first and last point and at
/// every segment end, with chords of at most half a pixel on curves.
pub fn flatten(path: &PathSpec) -> Result<Vec<Point>> {
    match path {
        PathSpec::Segments { start, segments } => {
            let mut dense = Dense(vec![*start]);
            for segment in segments {
                match *segment {
                    Segment::Line { to } => {
                        dense.reserve(1)?;
                        dense.0.push(to);
                    }
                    Segment::Quad { control, to } => dense.quad(control, to)?,
                    Segment::Cubic {
                        control1,
                        control2,
                        to,
                    } => dense.cubic(control1, control2, to)?,
                    Segment::Arc { center, sweep_deg } => dense.arc(center, sweep_deg)?,
                }
            }
            Ok(dense.0)
        }
        PathSpec::Points {
            points,
            smooth: false,
        } => Ok(points.clone()),
        PathSpec::Points {
            points,
            smooth: true,
        } => {
            let mut dense = Dense(vec![points[0]]);
            for (control1, control2, to) in catmull_rom_controls(points) {
                dense.cubic(control1, control2, to)?;
            }
            Ok(dense.0)
        }
        PathSpec::Parametric { x, y, t_from, t_to } => {
            let f = super::compile_expression("parametric x", x, "t")?;
            let g = super::compile_expression("parametric y", y, "t")?;
            let probe = parametric_points(&f, &g, *t_from, *t_to, PARAMETRIC_PROBE)?;
            let length: f64 = probe.windows(2).map(|w| distance(w[0], w[1])).sum();
            let n = pieces(length);
            if n <= PARAMETRIC_PROBE {
                return Ok(probe);
            }
            ensure!(
                n < MAX_DENSE_POINTS,
                "the parametric path is about {length:.0} px long, too long to sample"
            );
            parametric_points(&f, &g, *t_from, *t_to, n)
        }
    }
}

/// Resample `dense` at an even arc-length step of at most `spacing_px`.
/// The first and last samples are exactly the first and last dense points.
/// Returns the samples and the path length.
pub fn resample(dense: &[Point], spacing_px: f64) -> Result<(Vec<Point>, f64)> {
    let first = *dense.first().ok_or_else(|| anyhow!("empty path"))?;
    let last = *dense.last().expect("non-empty");
    let mut cumulative = Vec::with_capacity(dense.len());
    let mut length = 0.0;
    cumulative.push(0.0);
    for w in dense.windows(2) {
        length += distance(w[0], w[1]);
        cumulative.push(length);
    }
    ensure!(length.is_finite(), "the path length is not finite");
    // Intervals of at most spacing_px; a hair of slack keeps an exact
    // multiple (a 400 px line at spacing 4) from gaining an extra interval.
    let intervals = ((length / spacing_px - 1e-9).ceil() as usize).max(1);
    let samples = intervals + 1;
    if samples > MAX_SAMPLES {
        bail!(
            "the path is {length:.0} px long and needs {samples} samples at spacing_px {spacing_px}; \
             the limit is {MAX_SAMPLES}. Raise spacing_px to at least {:.1} or shorten the path",
            (length / (MAX_SAMPLES - 1) as f64 * 10.0).ceil() / 10.0
        );
    }
    let mut out = Vec::with_capacity(samples);
    out.push(first);
    let mut seg = 0;
    for k in 1..intervals {
        let s = length * k as f64 / intervals as f64;
        while seg + 1 < dense.len() - 1 && cumulative[seg + 1] < s {
            seg += 1;
        }
        let (a, b) = (dense[seg], dense[seg + 1]);
        let span = cumulative[seg + 1] - cumulative[seg];
        let f = if span > 0.0 {
            ((s - cumulative[seg]) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
    }
    out.push(last);
    Ok((out, length))
}

/// Sample the request's path and time every sample.
pub fn sample(request: &PathRequest) -> Result<SampledPath> {
    let dense = flatten(&request.path)?;
    let (points, length_px) = resample(&dense, request.spacing_px)?;
    let intervals = (points.len() - 1) as f64;
    let duration = request.duration_ms as f64;
    let offsets_ms = (0..points.len())
        .map(|k| duration * request.easing.time_at(k as f64 / intervals))
        .collect();
    Ok(SampledPath {
        points,
        offsets_ms,
        length_px,
        segment_count: request.path.segment_count(),
    })
}

/// Fail on the first point outside a `width` x `height` screen, after
/// rounding to whole pixels as delivery does.
pub fn check_on_screen(points: &[Point], width: u32, height: u32) -> Result<()> {
    let outside = |p: &Point| {
        let (x, y) = (p.0.round(), p.1.round());
        !(x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height))
    };
    if let Some(index) = points.iter().position(outside) {
        let count = points.iter().filter(|p| outside(p)).count();
        let p = points[index];
        bail!(
            "the path leaves the screen: sample {index} is at ({:.0}, {:.0}) in screen pixels \
             ({count} of {} samples are off screen); the screen is {width}x{height}, so x must be \
             0..{} and y 0..{}",
            p.0,
            p.1,
            points.len(),
            width.saturating_sub(1),
            height.saturating_sub(1)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::f64::consts::PI;

    fn request(args: Value) -> PathRequest {
        PathRequest::from_args(&args).expect("valid request")
    }

    fn sampled(args: Value) -> SampledPath {
        sample(&request(args)).expect("sampled")
    }

    fn error(args: Value) -> String {
        match PathRequest::from_args(&args).and_then(|request| sample(&request)) {
            Ok(_) => panic!("expected an error for {args}"),
            Err(error) => error.to_string(),
        }
    }

    fn close(a: Point, b: Point) -> bool {
        distance(a, b) < 1e-9
    }

    fn max_gap(points: &[Point]) -> f64 {
        points
            .windows(2)
            .map(|w| distance(w[0], w[1]))
            .fold(0.0, f64::max)
    }

    #[test]
    fn a_line_keeps_exact_endpoints_and_even_spacing() {
        let path = sampled(json!({
            "start": [100, 100], "segments": [{"line": {"to": [500, 100]}}], "easing": "linear"
        }));
        assert_eq!(path.points.first(), Some(&(100.0, 100.0)));
        assert_eq!(path.points.last(), Some(&(500.0, 100.0)));
        assert_eq!(
            path.points.len(),
            101,
            "400 px at spacing 4 is 100 intervals"
        );
        assert!((path.length_px - 400.0).abs() < 1e-9);
        for w in path.points.windows(2) {
            assert!((distance(w[0], w[1]) - 4.0).abs() < 1e-9);
        }
        assert_eq!(path.segment_count, 1);
    }

    #[test]
    fn quad_and_cubic_end_exactly_at_their_targets() {
        let path = sampled(json!({
            "start": [10, 10],
            "segments": [
                {"quad": {"control": [200, 0], "to": [300, 200]}},
                {"cubic": {"control1": [400, 300], "control2": [500, 0], "to": [612.5, 101.25]}}
            ]
        }));
        assert_eq!(path.points.first(), Some(&(10.0, 10.0)));
        assert_eq!(path.points.last(), Some(&(612.5, 101.25)));
        assert_eq!(path.segment_count, 2);
        assert!(max_gap(&path.points) <= 4.0 + 1e-9);
        // The quad's end point is on the path: some sample lies within one
        // spacing step of it.
        assert!(path
            .points
            .iter()
            .any(|p| distance(*p, (300.0, 200.0)) <= 2.0 + 1e-9));
        // A Bezier is longer than its chord and shorter than its control net.
        let chord =
            distance((10.0, 10.0), (300.0, 200.0)) + distance((300.0, 200.0), (612.5, 101.25));
        assert!(path.length_px > chord);
    }

    #[test]
    fn an_arc_keeps_its_radius_and_turns_clockwise_for_positive_sweep() {
        let path = sampled(json!({
            "start": [400, 300], "segments": [{"arc": {"center": [300, 300], "sweep_deg": 90}}]
        }));
        // Samples lie on half-pixel chords of the circle, so the radius holds
        // to within the chord sag (0.5^2 / (8 * 100) px).
        for p in &path.points {
            assert!((distance(*p, (300.0, 300.0)) - 100.0).abs() < 1e-3, "{p:?}");
        }
        assert_eq!(path.points.first(), Some(&(400.0, 300.0)));
        // Clockwise on screen from the right-hand point is downward (+y).
        let end = *path.points.last().unwrap();
        assert!(
            close((end.0.round(), end.1.round()), (300.0, 400.0)),
            "{end:?}"
        );
        assert!(path.points[1].1 > 300.0);
        assert!((path.length_px - PI * 50.0).abs() < 0.01);

        let ccw = sampled(json!({
            "start": [400, 300], "segments": [{"arc": {"center": [300, 300], "sweep_deg": -90}}]
        }));
        let end = *ccw.points.last().unwrap();
        assert!(
            close((end.0.round(), end.1.round()), (300.0, 200.0)),
            "{end:?}"
        );
    }

    #[test]
    fn a_polyline_passes_through_its_points() {
        let path = sampled(json!({"points": [[0, 0], [100, 0], [100, 100]]}));
        assert_eq!(path.points.first(), Some(&(0.0, 0.0)));
        assert_eq!(path.points.last(), Some(&(100.0, 100.0)));
        assert!(path.points.iter().any(|p| close(*p, (100.0, 0.0))));
        assert!((path.length_px - 200.0).abs() < 1e-9);
        assert_eq!(path.segment_count, 2);
    }

    #[test]
    fn a_smooth_polyline_passes_near_every_point_and_bends() {
        let corners = [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [200.0, 100.0]];
        let path = sampled(json!({"points": corners, "smooth": true, "spacing_px": 1}));
        assert_eq!(path.points.first(), Some(&(0.0, 0.0)));
        assert_eq!(path.points.last(), Some(&(200.0, 100.0)));
        for corner in corners {
            let nearest = path
                .points
                .iter()
                .map(|p| distance(*p, (corner[0], corner[1])))
                .fold(f64::MAX, f64::min);
            assert!(nearest <= 0.5 + 1e-9, "{corner:?} is {nearest} px away");
        }
        // The curve rounds the corners, so it is not the 300 px polyline.
        assert!((path.length_px - 300.0).abs() > 1.0);
    }

    #[test]
    fn a_parametric_circle_closes_on_its_radius() {
        let path = sampled(json!({"parametric": {
            "x": "1280+300*cos(t)", "y": "720+300*sin(t)", "t_from": 0, "t_to": 2.0 * PI
        }}));
        let (first, last) = (path.points[0], *path.points.last().unwrap());
        assert!(close(first, (1580.0, 720.0)));
        assert!(
            distance(first, last) < 1e-9,
            "the circle closes: {first:?} {last:?}"
        );
        for p in &path.points {
            assert!(
                (distance(*p, (1280.0, 720.0)) - 300.0).abs() < 0.01,
                "{p:?}"
            );
        }
        assert!((path.length_px - 2.0 * PI * 300.0).abs() < 0.1);
        assert_eq!(path.segment_count, 1);
    }

    #[test]
    fn a_parametric_spiral_grows_its_radius() {
        let path = sampled(json!({"parametric": {
            "x": "500+10*t*cos(t)", "y": "500+10*t*sin(t)", "t_from": 0, "t_to": 6.0 * PI
        }}));
        let radii: Vec<f64> = path
            .points
            .iter()
            .map(|p| distance(*p, (500.0, 500.0)))
            .collect();
        assert!(radii[0] < 1e-9);
        assert!((radii.last().unwrap() - 60.0 * PI).abs() < 1e-6);
        assert!(radii.windows(2).all(|w| w[1] + 1e-9 >= w[0]));
    }

    #[test]
    fn easing_curves_are_monotonic_and_inverse() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert_eq!(easing.progress(0.0), 0.0);
            assert_eq!(easing.progress(1.0), 1.0);
            assert_eq!(easing.time_at(0.0), 0.0);
            assert_eq!(easing.time_at(1.0), 1.0);
            let mut previous = (0.0, 0.0);
            for i in 1..=1000 {
                let u = i as f64 / 1000.0;
                let (p, t) = (easing.progress(u), easing.time_at(u));
                assert!(p >= previous.0 && t >= previous.1, "{easing:?} at {u}");
                assert!((easing.time_at(p) - u).abs() < 1e-9, "{easing:?} at {u}");
                previous = (p, t);
            }
        }
        // ease_in starts slow: the first tenth of the time covers 1% of the path.
        assert!((Easing::EaseIn.progress(0.1) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn sample_times_follow_the_easing_and_span_the_duration() {
        let linear = sampled(json!({
            "start": [0, 0], "segments": [{"line": {"to": [400, 0]}}],
            "easing": "linear", "duration_ms": 1000
        }));
        assert_eq!(linear.offsets_ms[0], 0.0);
        assert_eq!(*linear.offsets_ms.last().unwrap(), 1000.0);
        assert!((linear.offsets_ms[50] - 500.0).abs() < 1e-9);
        let eased = sampled(json!({
            "start": [0, 0], "segments": [{"line": {"to": [400, 0]}}], "duration_ms": 1000
        }));
        assert!(eased.offsets_ms.windows(2).all(|w| w[1] >= w[0]));
        // ease_in_out spends longer on the first quarter of the path than
        // linear timing does.
        assert!(eased.offsets_ms[25] > linear.offsets_ms[25]);
        assert!((eased.offsets_ms[50] - 500.0).abs() < 1e-9);
    }

    #[test]
    fn spacing_sets_the_sample_count_and_the_cap_refuses_long_paths() {
        let coarse = sampled(json!({
            "start": [0, 0], "segments": [{"line": {"to": [1000, 0]}}], "spacing_px": 10
        }));
        assert_eq!(coarse.points.len(), 101);
        let fine = sampled(json!({
            "start": [0, 0], "segments": [{"line": {"to": [1000, 0]}}], "spacing_px": 1
        }));
        assert_eq!(fine.points.len(), 1001);
        // A 3 px spacing over 1000 px rounds up to 334 intervals, each no
        // longer than the spacing.
        let odd = sampled(json!({
            "start": [0, 0], "segments": [{"line": {"to": [1000, 0]}}], "spacing_px": 3
        }));
        assert_eq!(odd.points.len(), 335);
        assert!(max_gap(&odd.points) <= 3.0);

        let message = error(json!({
            "start": [0, 0], "segments": [{"line": {"to": [5000, 0]}}], "spacing_px": 1
        }));
        assert!(message.contains("needs 5001 samples"), "{message}");
        assert!(message.contains("limit is 4000"), "{message}");
        assert!(message.contains("spacing_px to at least 1.3"), "{message}");

        let message = error(json!({
            "start": [0, 0], "segments": [{"line": {"to": [10, 0]}}], "spacing_px": 0.5
        }));
        assert!(
            message.contains("spacing_px must be between 1"),
            "{message}"
        );
    }

    #[test]
    fn a_zero_length_path_is_two_samples_at_the_start() {
        let path = sampled(json!({"points": [[5, 5], [5, 5]]}));
        assert_eq!(path.points, vec![(5.0, 5.0), (5.0, 5.0)]);
        assert_eq!(path.length_px, 0.0);
    }

    #[test]
    fn bad_expressions_and_non_finite_values_are_refused() {
        let message = error(json!({"parametric": {
            "x": "1280+300*cos(t", "y": "720", "t_from": 0, "t_to": 1
        }}));
        assert!(message.contains("invalid parametric x"), "{message}");
        let message = error(json!({"parametric": {
            "x": "x*2", "y": "720", "t_from": 0, "t_to": 1
        }}));
        assert!(
            message.contains("parametric x must be in terms of t"),
            "{message}"
        );
        let message = error(json!({"parametric": {
            "x": "100", "y": "ln(t)", "t_from": -1, "t_to": 1
        }}));
        assert!(message.contains("not finite at t = -1"), "{message}");
        let message = error(json!({"parametric": {
            "x": "100/t", "y": "5", "t_from": 0, "t_to": 1
        }}));
        assert!(message.contains("not finite at t = 0"), "{message}");
        let message = error(json!({"parametric": {
            "x": "t", "y": "t", "t_from": 2, "t_to": 2
        }}));
        assert!(message.contains("must differ"), "{message}");
        let message = error(json!({"points": [[0, 0], [1, "a"]]}));
        assert!(message.contains("points[1] must be a number"), "{message}");
    }

    #[test]
    fn exactly_one_path_form_is_required() {
        let message = error(json!({"button": "left"}));
        assert!(message.contains("exactly one of"), "{message}");
        let message = error(json!({
            "points": [[0, 0], [1, 1]],
            "parametric": {"x": "t", "y": "t", "t_from": 0, "t_to": 1}
        }));
        assert!(message.contains("not points and parametric"), "{message}");
        let message = error(json!({"segments": [{"line": {"to": [1, 1]}}]}));
        assert!(message.contains("needs a start point"), "{message}");
        let message = error(json!({"start": [0, 0], "points": [[0, 0], [1, 1]]}));
        assert!(message.contains("start belongs to segments"), "{message}");
        let message =
            error(json!({"start": [0, 0], "segments": [{"line": {"to": [1, 1]}}], "smooth": true}));
        assert!(
            message.contains("smooth applies only to points"),
            "{message}"
        );
        let message = error(json!({
            "start": [0, 0], "segments": [{"line": {"to": [1, 1]}, "arc": {"center": [0, 0], "sweep_deg": 9}}]
        }));
        assert!(
            message.contains("exactly one of line, quad, cubic or arc"),
            "{message}"
        );
        let message = error(
            json!({"start": [0, 0], "segments": [{"arc": {"center": [0, 0], "sweep_deg": 90}}]}),
        );
        assert!(message.contains("nonzero radius"), "{message}");
        let message = error(json!({"points": [[0, 0]]}));
        assert!(message.contains("at least 2 points"), "{message}");
    }

    #[test]
    fn click_at_end_needs_no_held_button() {
        let message = error(json!({
            "points": [[0, 0], [10, 10]], "button": "left", "click_at_end": "double"
        }));
        assert!(
            message.contains("click_at_end needs no button"),
            "{message}"
        );
        let ok = request(json!({"points": [[0, 0], [10, 10]], "click_at_end": "double"}));
        assert_eq!(ok.click_at_end, Some(EndClick::Double));
        assert_eq!(ok.button, None);
        assert_eq!(EndClick::Double.x_button_and_count(), (1, 2));
        let held = request(json!({"points": [[0, 0], [10, 10]], "button": "right"}));
        assert_eq!(held.button, Some(3));
        let hover = request(
            json!({"points": [[0, 0], [10, 10]], "button": "none", "click_at_end": "left"}),
        );
        assert_eq!(
            (hover.button, hover.click_at_end),
            (None, Some(EndClick::Left))
        );
        let message = error(json!({"points": [[0, 0], [10, 10]], "duration_ms": 30001}));
        assert!(message.contains("at most 30000"), "{message}");
    }

    #[test]
    fn off_screen_samples_are_refused() {
        assert!(check_on_screen(&[(0.0, 0.0), (2559.4, 1439.4)], 2560, 1440).is_ok());
        let message = check_on_screen(&[(10.0, 10.0), (2559.6, 10.0), (-3.0, 5.0)], 2560, 1440)
            .unwrap_err()
            .to_string();
        assert!(message.contains("sample 1 is at (2560, 10)"), "{message}");
        assert!(
            message.contains("2 of 3 samples are off screen"),
            "{message}"
        );
    }
}
