//! `capture_series`: screenshots on a fixed schedule measured from one start
//! time, each optionally cropped to a region, capped on its long edge, and
//! kept only when it changed. The series can end early once the frames stop
//! changing.
//!
//! Platform adapters supply raw frames through [`FrameSource`]. Scheduling,
//! cropping, scaling, change detection, file output and the reply live here
//! so every platform that registers the tool behaves the same.

use crate::image_utils::{self, PixelRect};
use crate::protocol::{Content, ToolResult};
use anyhow::{anyhow, Result};
use image::{ImageBuffer, ImageEncoder, Rgba};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const TOOL_NAME: &str = "capture_series";
/// Most frames one call may capture.
pub const MAX_FRAMES: usize = 120;
/// Longest `interval_ms`.
pub const MAX_INTERVAL_MS: u64 = 60_000;
/// Latest allowed frame offset. The call stays open for the whole schedule
/// and no transport cancels a running tool, so the schedule is bounded.
pub const MAX_SPAN_MS: u64 = 600_000;
pub const DEFAULT_COUNT: u64 = 5;
pub const DEFAULT_INTERVAL_MS: u64 = 1_000;
pub const DEFAULT_MAX_INLINE_IMAGES: usize = 10;
/// Hard cap on images in one reply.
pub const MAX_INLINE_IMAGES: usize = 30;
pub const DEFAULT_CHANGE_THRESHOLD: f64 = 0.001;
pub const DEFAULT_JPEG_QUALITY: u8 = 80;

type Rgba8Image = ImageBuffer<Rgba<u8>, Vec<u8>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    Png,
    Jpeg { quality: u8 },
}

impl FrameFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg { .. } => "jpeg",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg { .. } => "jpg",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnFrames {
    All,
    Changed,
    Last,
}

impl ReturnFrames {
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Changed => "changed",
            Self::Last => "last",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Inline,
    /// Canonical directory the frames are written into.
    Files(PathBuf),
}

/// A validated `capture_series` request, minus the target.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesRequest {
    /// Frame offsets from the start, in ms, non-decreasing.
    pub offsets_ms: Vec<u64>,
    pub region: Option<PixelRect>,
    /// Long-edge cap after the crop; 0 keeps the cropped size.
    pub max_image_dimension: u32,
    pub format: FrameFormat,
    pub output: Output,
    pub return_frames: ReturnFrames,
    pub max_inline_images: usize,
    pub change_threshold: f64,
    /// End the series once no frame has differed from the one before it for
    /// this long. The first frame starts the clock; a change restarts it.
    pub stop_after_unchanged_ms: Option<u64>,
}

/// One raw frame from a platform capture.
pub struct SourceFrame {
    /// PNG bytes.
    pub png: Vec<u8>,
    /// Position of the PNG's top-left pixel in the full frame: (0, 0) for a
    /// whole frame, the region origin when the source captured only the
    /// requested region.
    pub origin: (u32, u32),
    /// Size of the full frame the region is measured in.
    pub frame_size: (u32, u32),
}

/// A platform's capture of one target (the desktop or one window).
pub trait FrameSource: Send + Sync + 'static {
    /// Size of the full frame when it is known before capturing, so a region
    /// that does not fit fails before the schedule starts.
    fn frame_size(&self) -> Result<Option<(u32, u32)>>;
    /// Capture one frame. A source may return only `region` (and set
    /// `origin`) or the whole frame.
    fn capture(&self, region: Option<PixelRect>) -> Result<SourceFrame>;
    /// What is captured, for the reply ("desktop", "window 42 of pid 7").
    fn label(&self) -> String;
}

// ── Argument parsing ─────────────────────────────────────────────────────────

/// Schema of the `region` argument.
pub fn region_schema(description: &str) -> Value {
    let int = |what: &str| json!({"type": "integer", "minimum": 0, "description": what});
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "x": int("Left edge."),
            "y": int("Top edge."),
            "width": {"type": "integer", "minimum": 1, "description": "Width."},
            "height": {"type": "integer", "minimum": 1, "description": "Height."}
        },
        "required": ["x", "y", "width", "height"],
        "additionalProperties": false
    })
}

/// A non-negative integer argument; integral floats such as `2240.0` count.
fn integer_value(value: &Value, name: &str) -> Result<u64, String> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|v| v.fract() == 0.0 && *v >= 0.0 && *v <= u64::MAX as f64)
                .map(|v| v as u64)
        })
        .ok_or_else(|| format!("`{name}` must be a non-negative integer"))
}

fn integer_arg(args: &Value, name: &str) -> Result<Option<u64>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => integer_value(value, name).map(Some),
    }
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

/// Parse an optional `{x, y, width, height}` region.
pub fn parse_region(value: Option<&Value>) -> Result<Option<PixelRect>, String> {
    let value = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let object = value
        .as_object()
        .ok_or("`region` must be an object {x, y, width, height}")?;
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "x" | "y" | "width" | "height"))
    {
        return Err(format!(
            "`region` accepts only x, y, width and height, not `{unknown}`"
        ));
    }
    let field = |name: &str| -> Result<u32, String> {
        let value = object
            .get(name)
            .ok_or_else(|| format!("`region` needs `{name}`"))?;
        let value = integer_value(value, &format!("region.{name}"))?;
        u32::try_from(value).map_err(|_| format!("`region.{name}` is too large"))
    };
    let region = PixelRect {
        x: field("x")?,
        y: field("y")?,
        width: field("width")?,
        height: field("height")?,
    };
    if region.width == 0 || region.height == 0 {
        return Err("`region` width and height must be at least 1".into());
    }
    Ok(Some(region))
}

/// Refuse a region that does not lie inside a `frame`-sized image.
pub fn check_region(region: PixelRect, frame: (u32, u32)) -> Result<(), String> {
    if region.fits_within(frame.0, frame.1) {
        return Ok(());
    }
    Err(format!(
        "region {},{} {}x{} does not fit the {}x{} frame (needs x+width <= {} and y+height <= {})",
        region.x, region.y, region.width, region.height, frame.0, frame.1, frame.0, frame.1
    ))
}

/// Frame offsets in ms from `count` + `interval_ms` or from `offsets_ms`.
pub fn schedule_from_args(args: &Value) -> Result<Vec<u64>, String> {
    let count = integer_arg(args, "count")?;
    let interval = integer_arg(args, "interval_ms")?;
    if let Some(offsets) = args.get("offsets_ms").filter(|value| !value.is_null()) {
        if count.is_some() || interval.is_some() {
            return Err("pass either count/interval_ms or offsets_ms, not both".into());
        }
        let list = offsets
            .as_array()
            .ok_or("`offsets_ms` must be an array of integers")?;
        if list.is_empty() || list.len() > MAX_FRAMES {
            return Err(format!("`offsets_ms` must hold 1 to {MAX_FRAMES} entries"));
        }
        let mut values = Vec::with_capacity(list.len());
        for (index, value) in list.iter().enumerate() {
            let value = integer_value(value, &format!("offsets_ms[{index}]"))?;
            if let Some(&previous) = values.last() {
                if value <= previous {
                    return Err(format!(
                        "`offsets_ms` must be strictly increasing: {previous} is followed by {value} at index {index}"
                    ));
                }
            }
            values.push(value);
        }
        let last = *values.last().unwrap_or(&0);
        if last > MAX_SPAN_MS {
            return Err(format!(
                "`offsets_ms` ends at {last} ms; the limit is {MAX_SPAN_MS} ms"
            ));
        }
        return Ok(values);
    }
    let count = count.unwrap_or(DEFAULT_COUNT);
    if count == 0 || count > MAX_FRAMES as u64 {
        return Err(format!("`count` must be 1 to {MAX_FRAMES}"));
    }
    let interval = interval.unwrap_or(DEFAULT_INTERVAL_MS);
    if interval > MAX_INTERVAL_MS {
        return Err(format!("`interval_ms` must be 0 to {MAX_INTERVAL_MS}"));
    }
    let span = (count - 1) * interval;
    if span > MAX_SPAN_MS {
        return Err(format!(
            "count {count} at interval_ms {interval} spans {span} ms; the limit is {MAX_SPAN_MS} ms"
        ));
    }
    Ok((0..count).map(|index| index * interval).collect())
}

impl SeriesRequest {
    /// Validate everything but the target. Checks `out_dir` on disk.
    pub fn parse(args: &Value) -> Result<Self, String> {
        let offsets_ms = schedule_from_args(args)?;
        let region = parse_region(args.get("region"))?;
        let max_image_dimension = match integer_arg(args, "max_image_dimension")? {
            None => 0,
            Some(value) => {
                u32::try_from(value).map_err(|_| "`max_image_dimension` is too large".to_owned())?
            }
        };
        let quality = integer_arg(args, "quality")?;
        let format = match string_arg(args, "format")?.unwrap_or("png") {
            "png" => {
                if quality.is_some() {
                    return Err("`quality` applies only to format \"jpeg\"".into());
                }
                FrameFormat::Png
            }
            "jpeg" => {
                let quality = quality.unwrap_or(u64::from(DEFAULT_JPEG_QUALITY));
                if !(1..=100).contains(&quality) {
                    return Err("`quality` must be 1 to 100".into());
                }
                FrameFormat::Jpeg {
                    quality: quality as u8,
                }
            }
            other => {
                return Err(format!(
                    "`format` must be \"png\" or \"jpeg\", not \"{other}\""
                ))
            }
        };
        let out_dir = string_arg(args, "out_dir")?;
        let output = match (string_arg(args, "output")?.unwrap_or("inline"), out_dir) {
            ("inline", None) => Output::Inline,
            ("inline", Some(_)) => return Err("`out_dir` needs output \"files\"".into()),
            ("files", Some(dir)) => Output::Files(validate_out_dir(dir)?),
            ("files", None) => return Err("output \"files\" needs `out_dir`".into()),
            (other, _) => {
                return Err(format!(
                    "`output` must be \"inline\" or \"files\", not \"{other}\""
                ))
            }
        };
        let return_frames = match string_arg(args, "return_frames")?.unwrap_or("all") {
            "all" => ReturnFrames::All,
            "changed" => ReturnFrames::Changed,
            "last" => ReturnFrames::Last,
            other => {
                return Err(format!(
                    "`return_frames` must be \"all\", \"changed\" or \"last\", not \"{other}\""
                ))
            }
        };
        let max_inline_images = match integer_arg(args, "max_inline_images")? {
            None => DEFAULT_MAX_INLINE_IMAGES,
            Some(value) if (1..=MAX_INLINE_IMAGES as u64).contains(&value) => value as usize,
            Some(_) => {
                return Err(format!(
                    "`max_inline_images` must be 1 to {MAX_INLINE_IMAGES}"
                ))
            }
        };
        let change_threshold = match args.get("change_threshold") {
            None | Some(Value::Null) => DEFAULT_CHANGE_THRESHOLD,
            Some(value) => value
                .as_f64()
                .filter(|value| (0.0..=1.0).contains(value))
                .ok_or("`change_threshold` must be a number from 0 to 1")?,
        };
        let stop_after_unchanged_ms = match integer_arg(args, "stop_after_unchanged_ms")? {
            Some(value) if value > MAX_SPAN_MS => {
                return Err(format!(
                    "`stop_after_unchanged_ms` must be 0 to {MAX_SPAN_MS}"
                ))
            }
            value => value,
        };
        if output == Output::Inline
            && return_frames == ReturnFrames::All
            && offsets_ms.len() > max_inline_images
        {
            return Err(format!(
                "{} frames with return_frames \"all\" exceed max_inline_images {max_inline_images}; \
                 use output \"files\", return_frames \"changed\" or \"last\", or raise \
                 max_inline_images (at most {MAX_INLINE_IMAGES})",
                offsets_ms.len()
            ));
        }
        Ok(Self {
            offsets_ms,
            region,
            max_image_dimension,
            format,
            output,
            return_frames,
            max_inline_images,
            change_threshold,
            stop_after_unchanged_ms,
        })
    }
}

/// `invalid_arguments` refusal in the shape `parse_typed_input` uses.
pub fn invalid_arguments(detail: &str) -> ToolResult {
    ToolResult::error(format!("{TOOL_NAME}: invalid arguments: {detail}")).with_structured(json!({
        "code": "invalid_arguments",
        "tool": TOOL_NAME,
        "detail": detail,
    }))
}

// ── Output directory ─────────────────────────────────────────────────────────

/// Accept an existing directory named by an absolute path whose last
/// component is not a symbolic link, and return its canonical form. Frames
/// are written only inside that canonical directory.
pub fn validate_out_dir(raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("`out_dir` must be an absolute path".into());
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err("`out_dir` must not contain `..`".into());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "`out_dir` does not exist; create the directory first".to_owned())?;
    if metadata.file_type().is_symlink() {
        return Err("`out_dir` is a symbolic link; pass the directory it points to".into());
    }
    if !metadata.is_dir() {
        return Err("`out_dir` is not a directory".into());
    }
    std::fs::canonicalize(path).map_err(|error| format!("`out_dir` cannot be resolved: {error}"))
}

/// `frame-0001.png` for the first frame.
pub fn frame_file_name(index: usize, format: FrameFormat) -> String {
    format!("frame-{:04}.{}", index + 1, format.extension())
}

/// Refuse to start when any file the series could write already exists.
pub fn check_names_free(dir: &Path, frames: usize, format: FrameFormat) -> Result<(), String> {
    for index in 0..frames {
        let path = dir.join(frame_file_name(index, format));
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "{} already exists; capture_series never overwrites files, so use an empty directory",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Create `path` and write `bytes`. Fails when anything, a symbolic link
/// included, already exists there.
pub fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| anyhow!("cannot create {}: {error}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

// ── Frame processing ─────────────────────────────────────────────────────────

/// Crop a source frame to `region` and cap its long edge.
pub fn prepare_frame(
    source: SourceFrame,
    region: Option<PixelRect>,
    max_image_dimension: u32,
) -> Result<Rgba8Image> {
    if let Some(region) = region {
        check_region(region, source.frame_size).map_err(|message| anyhow!(message))?;
    }
    let image = image_utils::decode_png_to_rgba8(&source.png)?;
    let (origin_x, origin_y) = source.origin;
    let wanted = region.unwrap_or(PixelRect {
        x: 0,
        y: 0,
        width: source.frame_size.0,
        height: source.frame_size.1,
    });
    let rect = PixelRect {
        x: wanted
            .x
            .checked_sub(origin_x)
            .ok_or_else(|| anyhow!("the captured image starts right of the requested area"))?,
        y: wanted
            .y
            .checked_sub(origin_y)
            .ok_or_else(|| anyhow!("the captured image starts below the requested area"))?,
        width: wanted.width,
        height: wanted.height,
    };
    let whole = rect.x == 0 && rect.y == 0 && (rect.width, rect.height) == image.dimensions();
    let image = if whole {
        image
    } else {
        image_utils::crop_rgba8(&image, rect)?
    };
    Ok(image_utils::fit_rgba8_long_edge(image, max_image_dimension))
}

/// Fraction of pixels whose RGB differs; 1.0 when the sizes differ.
pub fn diff_fraction(a: &Rgba8Image, b: &Rgba8Image) -> f64 {
    if a.dimensions() != b.dimensions() {
        return 1.0;
    }
    let total = u64::from(a.width()) * u64::from(a.height());
    if total == 0 {
        return 0.0;
    }
    let differing = a
        .as_raw()
        .chunks_exact(4)
        .zip(b.as_raw().chunks_exact(4))
        .filter(|(left, right)| left[..3] != right[..3])
        .count() as u64;
    differing as f64 / total as f64
}

fn encode(image: &Rgba8Image, format: FrameFormat) -> Result<Vec<u8>> {
    let (width, height) = image.dimensions();
    let mut out = Vec::new();
    let rgb = || -> Vec<u8> {
        image
            .as_raw()
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect()
    };
    match format {
        // Screenshots are opaque; dropping the alpha channel makes the PNG
        // smaller without changing a pixel.
        FrameFormat::Png if image.pixels().all(|pixel| pixel[3] == u8::MAX) => {
            image::codecs::png::PngEncoder::new(&mut out).write_image(
                &rgb(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )?
        }
        FrameFormat::Png => image::codecs::png::PngEncoder::new(&mut out).write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )?,
        FrameFormat::Jpeg { quality } => image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut out, quality,
        )
        .encode(&rgb(), width, height, image::ExtendedColorType::Rgb8)?,
    }
    Ok(out)
}

// ── The series ───────────────────────────────────────────────────────────────

/// What the reply says about one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameRecord {
    pub index: usize,
    pub scheduled_ms: u64,
    pub t_ms: u64,
    /// The previous frame's capture ran past this frame's scheduled time.
    pub late: bool,
    pub width: u32,
    pub height: u32,
    pub changed: bool,
    pub diff_fraction: Option<f64>,
    /// Returned inline or written to a file.
    pub returned: bool,
    /// Kept by `return_frames` but left out of an inline reply that was full.
    pub omitted: bool,
    pub path: Option<String>,
    pub bytes: Option<usize>,
}

impl FrameRecord {
    fn json(&self) -> Value {
        let mut value = json!({
            "index": self.index + 1,
            "scheduled_ms": self.scheduled_ms,
            "t_ms": self.t_ms,
            "late": self.late,
            "width": self.width,
            "height": self.height,
            "changed": self.changed,
            "diff_fraction": self.diff_fraction,
            "returned": self.returned,
        });
        if self.omitted {
            value["omitted"] = json!(true);
        }
        if let Some(path) = &self.path {
            value["path"] = json!(path);
        }
        if let Some(bytes) = self.bytes {
            value["bytes"] = json!(bytes);
        }
        value
    }

    fn line(&self) -> String {
        let mut line = format!(
            "#{} t={}ms{} {}x{}",
            self.index + 1,
            self.t_ms,
            if self.late { " LATE" } else { "" },
            self.width,
            self.height
        );
        match self.diff_fraction {
            Some(diff) => line.push_str(&format!(
                " diff={diff:.4} {}",
                if self.changed { "changed" } else { "same" }
            )),
            None => line.push_str(" first"),
        }
        if let Some(path) = &self.path {
            let name = Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            line.push_str(&format!(" -> {name}"));
        } else if self.omitted {
            line.push_str(" (image omitted: inline limit)");
        } else if self.returned {
            line.push_str(" (image)");
        }
        line
    }
}

struct FrameOutcome {
    image: Rgba8Image,
    record: FrameRecord,
    inline: Option<Vec<u8>>,
    /// Differs from the previous frame (always true for the first one).
    moved: bool,
    /// Ends the series under `stop_after_unchanged_ms`.
    stops: bool,
}

/// The `stop_after_unchanged_ms` clock as it stands when a frame is taken.
#[derive(Clone, Copy)]
struct StopClock {
    limit_ms: u64,
    /// `t_ms` of the frame that started the current unchanged stretch.
    quiet_since_ms: u64,
}

fn capture_one(
    source: &dyn FrameSource,
    request: &SeriesRequest,
    reference: Option<&Rgba8Image>,
    previous: Option<&Rgba8Image>,
    clock: Option<StopClock>,
    mut record: FrameRecord,
    scheduled_last: bool,
) -> Result<FrameOutcome> {
    let raw = source.capture(request.region)?;
    let image = prepare_frame(raw, request.region, request.max_image_dimension)?;
    let diff = reference.map(|reference| diff_fraction(reference, &image));
    let changed = diff.is_none_or(|diff| diff > request.change_threshold);
    // The stop clock always compares with the previous frame. That is also
    // the reference unless `changed` skipped the previous frame.
    let moved = match previous {
        None => true,
        Some(previous) if reference.is_some_and(|reference| std::ptr::eq(reference, previous)) => {
            changed
        }
        Some(previous) => diff_fraction(previous, &image) > request.change_threshold,
    };
    // Reaching the limit on the last scheduled frame is not an early stop.
    let stops = !scheduled_last
        && !moved
        && clock.is_some_and(|clock| {
            record.t_ms.saturating_sub(clock.quiet_since_ms) >= clock.limit_ms
        });
    let is_last = scheduled_last || stops;
    let keep = match request.return_frames {
        ReturnFrames::All => true,
        ReturnFrames::Changed => changed,
        ReturnFrames::Last => is_last,
    };
    record.width = image.width();
    record.height = image.height();
    record.diff_fraction = diff;
    record.changed = changed;
    record.returned = keep;
    let mut inline = None;
    if keep {
        let bytes = encode(&image, request.format)?;
        record.bytes = Some(bytes.len());
        match &request.output {
            Output::Files(dir) => {
                let path = dir.join(frame_file_name(record.index, request.format));
                write_new_file(&path, &bytes)?;
                record.path = Some(path.to_string_lossy().into_owned());
            }
            Output::Inline => inline = Some(bytes),
        }
    }
    Ok(FrameOutcome {
        image,
        record,
        inline,
        moved,
        stops,
    })
}

/// Run the whole schedule against `source` and build the reply.
pub async fn run(request: SeriesRequest, source: Arc<dyn FrameSource>) -> ToolResult {
    if let Some(region) = request.region {
        let probe = source.clone();
        match tokio::task::spawn_blocking(move || probe.frame_size()).await {
            Ok(Ok(Some(size))) => {
                if let Err(message) = check_region(region, size) {
                    return invalid_arguments(&message);
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => return ToolResult::error(format!("Capture error: {error}")),
            Err(error) => return ToolResult::error(format!("Task error: {error}")),
        }
    }
    if let Output::Files(dir) = &request.output {
        if let Err(message) = check_names_free(dir, request.offsets_ms.len(), request.format) {
            return invalid_arguments(&message);
        }
    }

    let request = Arc::new(request);
    let total = request.offsets_ms.len();
    let mut records: Vec<FrameRecord> = Vec::with_capacity(total);
    let mut inline: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut reference: Option<Arc<Rgba8Image>> = None;
    // Kept only for `stop_after_unchanged_ms`, with the (index, t_ms) of the
    // frame that started the current unchanged stretch.
    let mut previous: Option<Arc<Rgba8Image>> = None;
    let mut quiet_since: Option<(usize, u64)> = None;
    let mut stop_reason: Option<String> = None;
    // Every frame is scheduled against this one monotonic start, so a slow
    // capture delays only the frames whose time it overran.
    let start = tokio::time::Instant::now();
    for (index, &offset) in request.offsets_ms.iter().enumerate() {
        let due = start + Duration::from_millis(offset);
        let now = tokio::time::Instant::now();
        let late = index > 0 && offset > request.offsets_ms[index - 1] && now > due;
        if now < due {
            tokio::time::sleep_until(due).await;
        }
        let record = FrameRecord {
            index,
            scheduled_ms: offset,
            t_ms: start.elapsed().as_millis() as u64,
            late,
            width: 0,
            height: 0,
            changed: false,
            diff_fraction: None,
            returned: false,
            omitted: false,
            path: None,
            bytes: None,
        };
        let job_source = source.clone();
        let job_request = request.clone();
        let job_reference = reference.clone();
        let job_previous = previous.clone();
        let clock = request.stop_after_unchanged_ms.zip(quiet_since).map(
            |(limit_ms, (_, quiet_since_ms))| StopClock {
                limit_ms,
                quiet_since_ms,
            },
        );
        let scheduled_last = index + 1 == total;
        let outcome = tokio::task::spawn_blocking(move || {
            capture_one(
                job_source.as_ref(),
                &job_request,
                job_reference.as_deref(),
                job_previous.as_deref(),
                clock,
                record,
                scheduled_last,
            )
        })
        .await
        .map_err(|error| anyhow!("task error: {error}"))
        .and_then(|outcome| outcome);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => return failure(&request, source.as_ref(), &records, index, &error),
        };
        // `changed` compares with the last kept frame; the other modes
        // compare with the previous frame.
        let image = Arc::new(outcome.image);
        if request.return_frames != ReturnFrames::Changed || outcome.record.returned {
            reference = Some(image.clone());
        }
        if let Some(limit) = request.stop_after_unchanged_ms {
            let t_ms = outcome.record.t_ms;
            if outcome.moved {
                quiet_since = Some((index, t_ms));
            }
            if outcome.stops {
                let (since_index, since_ms) = quiet_since.unwrap_or((0, 0));
                stop_reason = Some(format!(
                    "no frame changed for {} ms (frames {}-{}); stop_after_unchanged_ms is {limit}",
                    t_ms.saturating_sub(since_ms),
                    since_index + 1,
                    index + 1
                ));
            }
            previous = Some(image);
        }
        records.push(outcome.record);
        if let Some(bytes) = outcome.inline {
            inline.push((index, bytes));
            if inline.len() > request.max_inline_images {
                // Keep the first frame (the baseline) and the latest ones.
                let (evicted, _) = inline.remove(1);
                records[evicted].omitted = true;
                records[evicted].returned = false;
            }
        }
        if stop_reason.is_some() {
            break;
        }
    }
    success(
        &request,
        source.as_ref(),
        records,
        inline,
        stop_reason.as_deref(),
    )
}

fn summary_header(
    request: &SeriesRequest,
    source: &dyn FrameSource,
    records: &[FrameRecord],
) -> String {
    let what = match request.region {
        Some(region) => format!(
            "{} region {},{} {}x{}",
            source.label(),
            region.x,
            region.y,
            region.width,
            region.height
        ),
        None => source.label(),
    };
    let span = records.last().map(|record| record.t_ms).unwrap_or(0);
    let late = records.iter().filter(|record| record.late).count();
    let returned = records.iter().filter(|record| record.returned).count();
    let destination = match &request.output {
        Output::Inline => "inline".to_owned(),
        Output::Files(dir) => format!("written to {}", dir.display()),
    };
    format!(
        "capture_series: {} frame(s) of {what} over {span} ms ({late} late), {} {}; return_frames {}: {returned} {destination}.",
        records.len(),
        request.format.name(),
        match request.max_image_dimension {
            0 => "uncapped".to_owned(),
            cap => format!("long edge <= {cap}"),
        },
        request.return_frames.name(),
    )
}

fn structured(
    request: &SeriesRequest,
    source: &dyn FrameSource,
    records: &[FrameRecord],
    stop_reason: Option<&str>,
) -> Value {
    let mut value = json!({
        "target": source.label(),
        "region": request.region.map(|region| json!({
            "x": region.x, "y": region.y, "width": region.width, "height": region.height,
        })),
        "format": request.format.name(),
        "output": match request.output { Output::Inline => "inline", Output::Files(_) => "files" },
        "out_dir": match &request.output {
            Output::Files(dir) => Some(dir.to_string_lossy().into_owned()),
            Output::Inline => None,
        },
        "return_frames": request.return_frames.name(),
        "scheduled_count": request.offsets_ms.len(),
        "frame_count": records.len(),
        "returned_count": records.iter().filter(|record| record.returned).count(),
        "late_count": records.iter().filter(|record| record.late).count(),
        "stopped_early": stop_reason.is_some(),
        "frames": records.iter().map(FrameRecord::json).collect::<Vec<_>>(),
    });
    if let Some(reason) = stop_reason {
        value["stop_reason"] = json!(reason);
    }
    value
}

fn success(
    request: &SeriesRequest,
    source: &dyn FrameSource,
    records: Vec<FrameRecord>,
    inline: Vec<(usize, Vec<u8>)>,
    stop_reason: Option<&str>,
) -> ToolResult {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let mut lines = vec![summary_header(request, source, &records)];
    if let Some(reason) = stop_reason {
        lines.push(format!(
            "Stopped early after frame {} of {}: {reason}.",
            records.len(),
            request.offsets_ms.len()
        ));
    }
    lines.extend(records.iter().map(FrameRecord::line));
    let mut content = vec![Content::text(lines.join("\n"))];
    for (_, bytes) in inline {
        let data = B64.encode(bytes);
        content.push(match request.format {
            FrameFormat::Png => Content::image_png(data),
            FrameFormat::Jpeg { .. } => Content::image_jpeg(data),
        });
    }
    ToolResult {
        content,
        is_error: None,
        structured_content: Some(structured(request, source, &records, stop_reason)),
        action_record: None,
    }
}

fn failure(
    request: &SeriesRequest,
    source: &dyn FrameSource,
    records: &[FrameRecord],
    index: usize,
    error: &anyhow::Error,
) -> ToolResult {
    let mut lines = vec![format!(
        "capture_series: frame {} of {} failed: {error}",
        index + 1,
        request.offsets_ms.len()
    )];
    if !records.is_empty() {
        lines.push(summary_header(request, source, records));
        lines.extend(records.iter().map(FrameRecord::line));
    }
    let mut value = structured(request, source, records, None);
    value["code"] = json!("capture_failed");
    value["failed_frame"] = json!(index + 1);
    value["error"] = json!(error.to_string());
    ToolResult::error(lines.join("\n")).with_structured(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn schedule_defaults_to_five_frames_one_second_apart() {
        assert_eq!(
            schedule_from_args(&json!({})).unwrap(),
            vec![0, 1000, 2000, 3000, 4000]
        );
        assert_eq!(
            schedule_from_args(&json!({"count": 3, "interval_ms": 250})).unwrap(),
            vec![0, 250, 500]
        );
        // Back to back.
        assert_eq!(
            schedule_from_args(&json!({"count": 3, "interval_ms": 0})).unwrap(),
            vec![0, 0, 0]
        );
        assert_eq!(
            schedule_from_args(&json!({"offsets_ms": [0, 100.0, 2500]})).unwrap(),
            vec![0, 100, 2500]
        );
    }

    #[test]
    fn schedule_refuses_mixed_or_out_of_range_timing() {
        let cases = [
            (
                json!({"count": 2, "offsets_ms": [0, 10]}),
                "either count/interval_ms or offsets_ms",
            ),
            (
                json!({"interval_ms": 5, "offsets_ms": [0]}),
                "either count/interval_ms or offsets_ms",
            ),
            (json!({"count": 0}), "`count` must be 1 to 120"),
            (json!({"count": 121}), "`count` must be 1 to 120"),
            (json!({"count": -1}), "non-negative integer"),
            (
                json!({"interval_ms": 60_001}),
                "`interval_ms` must be 0 to 60000",
            ),
            (
                json!({"count": 120, "interval_ms": 60_000}),
                "spans 7140000 ms; the limit is 600000 ms",
            ),
            (json!({"offsets_ms": []}), "1 to 120 entries"),
            (json!({"offsets_ms": [0, 5, 5]}), "strictly increasing"),
            (json!({"offsets_ms": [10, 3]}), "strictly increasing"),
            (
                json!({"offsets_ms": [0, 600_001]}),
                "the limit is 600000 ms",
            ),
            (json!({"offsets_ms": [0, 1.5]}), "non-negative integer"),
            (json!({"offsets_ms": "0,1"}), "must be an array"),
        ];
        for (value, expected) in cases {
            let error = schedule_from_args(&value).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
        let offsets: Vec<u64> = (0..121).collect();
        assert!(schedule_from_args(&json!({ "offsets_ms": offsets }))
            .unwrap_err()
            .contains("1 to 120 entries"));
        // The longest schedules that fit.
        assert_eq!(
            schedule_from_args(&json!({"count": 11, "interval_ms": 60_000}))
                .unwrap()
                .last(),
            Some(&600_000)
        );
    }

    #[test]
    fn region_parsing_is_exact() {
        assert_eq!(
            parse_region(Some(
                &json!({"x": 2240, "y": 1380.0, "width": 320, "height": 60})
            ))
            .unwrap(),
            Some(PixelRect {
                x: 2240,
                y: 1380,
                width: 320,
                height: 60
            })
        );
        assert_eq!(parse_region(None).unwrap(), None);
        assert_eq!(parse_region(Some(&Value::Null)).unwrap(), None);
        for (value, expected) in [
            (json!({"x": 0, "y": 0, "width": 10}), "needs `height`"),
            (
                json!({"x": 0, "y": 0, "width": 10, "height": 0}),
                "at least 1",
            ),
            (
                json!({"x": -1, "y": 0, "width": 1, "height": 1}),
                "non-negative integer",
            ),
            (
                json!({"x": 0.5, "y": 0, "width": 1, "height": 1}),
                "non-negative integer",
            ),
            (
                json!({"x": 0, "y": 0, "width": 1, "height": 1, "w": 1}),
                "not `w`",
            ),
            (
                json!({"x": 5_000_000_000u64, "y": 0, "width": 1, "height": 1}),
                "too large",
            ),
            (json!([0, 0, 1, 1]), "must be an object"),
        ] {
            let error = parse_region(Some(&value)).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
    }

    #[test]
    fn region_bounds_are_checked_against_the_frame() {
        let clock = PixelRect {
            x: 2240,
            y: 1380,
            width: 320,
            height: 60,
        };
        assert!(check_region(clock, (2560, 1440)).is_ok());
        let error = check_region(
            PixelRect {
                width: 321,
                ..clock
            },
            (2560, 1440),
        )
        .unwrap_err();
        assert!(
            error.contains("does not fit the 2560x1440 frame"),
            "{error}"
        );
        assert!(check_region(PixelRect { y: 1381, ..clock }, (2560, 1440)).is_err());
    }

    fn rgba(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Rgba8Image {
        ImageBuffer::from_fn(width, height, |x, y| Rgba(f(x, y)))
    }

    fn png_of(image: &Rgba8Image) -> Vec<u8> {
        image_utils::encode_rgba_to_png(image.as_raw(), image.width(), image.height()).unwrap()
    }

    #[test]
    fn crop_math_uses_the_source_origin() {
        // A 100x50 frame where every pixel encodes its own coordinates.
        let frame = rgba(100, 50, |x, y| [x as u8, y as u8, 0, 255]);
        let region = PixelRect {
            x: 60,
            y: 20,
            width: 30,
            height: 10,
        };
        let whole = SourceFrame {
            png: png_of(&frame),
            origin: (0, 0),
            frame_size: (100, 50),
        };
        let cropped = prepare_frame(whole, Some(region), 0).unwrap();
        assert_eq!(cropped.dimensions(), (30, 10));
        assert_eq!(cropped.get_pixel(0, 0).0, [60, 20, 0, 255]);
        assert_eq!(cropped.get_pixel(29, 9).0, [89, 29, 0, 255]);

        // A source that captured only the region returns it unchanged.
        let only_region = image::imageops::crop_imm(&frame, 60, 20, 30, 10).to_image();
        let at_source = SourceFrame {
            png: png_of(&only_region),
            origin: (60, 20),
            frame_size: (100, 50),
        };
        assert_eq!(prepare_frame(at_source, Some(region), 0).unwrap(), cropped);

        // The long-edge cap applies after the crop.
        let capped = prepare_frame(
            SourceFrame {
                png: png_of(&frame),
                origin: (0, 0),
                frame_size: (100, 50),
            },
            Some(region),
            15,
        )
        .unwrap();
        assert_eq!(capped.dimensions(), (15, 5));

        // No region: the whole frame, capped.
        let full = prepare_frame(
            SourceFrame {
                png: png_of(&frame),
                origin: (0, 0),
                frame_size: (100, 50),
            },
            None,
            50,
        )
        .unwrap();
        assert_eq!(full.dimensions(), (50, 25));
    }

    #[test]
    fn crop_refuses_a_region_outside_the_frame_or_the_captured_image() {
        let frame = rgba(100, 50, |_, _| [0, 0, 0, 255]);
        let outside = PixelRect {
            x: 90,
            y: 0,
            width: 20,
            height: 10,
        };
        let error = prepare_frame(
            SourceFrame {
                png: png_of(&frame),
                origin: (0, 0),
                frame_size: (100, 50),
            },
            Some(outside),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not fit"), "{error}");
        // The source captured a different area than the one asked for.
        let error = prepare_frame(
            SourceFrame {
                png: png_of(&rgba(10, 10, |_, _| [0, 0, 0, 255])),
                origin: (50, 0),
                frame_size: (100, 50),
            },
            Some(PixelRect {
                x: 10,
                y: 0,
                width: 5,
                height: 5,
            }),
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("starts right of"), "{error}");
    }

    #[test]
    fn diff_counts_changed_pixels_and_ignores_alpha() {
        let a = rgba(10, 10, |_, _| [1, 2, 3, 255]);
        assert_eq!(diff_fraction(&a, &a.clone()), 0.0);
        let mut b = a.clone();
        b.put_pixel(4, 4, Rgba([9, 2, 3, 255]));
        assert!((diff_fraction(&a, &b) - 0.01).abs() < 1e-12);
        let mut alpha_only = a.clone();
        alpha_only.put_pixel(0, 0, Rgba([1, 2, 3, 0]));
        assert_eq!(diff_fraction(&a, &alpha_only), 0.0);
        assert_eq!(diff_fraction(&a, &rgba(10, 9, |_, _| [1, 2, 3, 255])), 1.0);
    }

    #[test]
    fn png_frames_keep_every_pixel() {
        let opaque = rgba(7, 3, |x, y| [x as u8 * 30, y as u8 * 80, 5, 255]);
        let png = encode(&opaque, FrameFormat::Png).unwrap();
        assert_eq!(image_utils::decode_png_to_rgba8(&png).unwrap(), opaque);
        let translucent = rgba(2, 2, |x, _| [1, 2, 3, if x == 0 { 255 } else { 7 }]);
        let png = encode(&translucent, FrameFormat::Png).unwrap();
        assert_eq!(image_utils::decode_png_to_rgba8(&png).unwrap(), translucent);
        let jpeg = encode(&opaque, FrameFormat::Jpeg { quality: 90 }).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn out_dir_must_be_an_existing_absolute_real_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("frames");
        std::fs::create_dir(&dir).unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(validate_out_dir(dir.to_str().unwrap()).unwrap(), canonical);

        assert!(validate_out_dir("relative/frames")
            .unwrap_err()
            .contains("absolute"));
        let missing = temp.path().join("missing");
        assert!(validate_out_dir(missing.to_str().unwrap())
            .unwrap_err()
            .contains("does not exist"));
        let file = temp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_out_dir(file.to_str().unwrap())
            .unwrap_err()
            .contains("not a directory"));
        let dotdot = format!("{}/frames/../frames", temp.path().display());
        assert!(validate_out_dir(&dotdot).unwrap_err().contains("`..`"));
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert!(validate_out_dir(link.to_str().unwrap())
                .unwrap_err()
                .contains("symbolic link"));
        }
    }

    #[test]
    fn frame_files_are_numbered_and_never_overwritten() {
        assert_eq!(frame_file_name(0, FrameFormat::Png), "frame-0001.png");
        assert_eq!(
            frame_file_name(119, FrameFormat::Jpeg { quality: 80 }),
            "frame-0120.jpg"
        );
        let temp = tempfile::tempdir().unwrap();
        assert!(check_names_free(temp.path(), 3, FrameFormat::Png).is_ok());
        let existing = temp.path().join("frame-0002.png");
        std::fs::write(&existing, b"old").unwrap();
        assert!(check_names_free(temp.path(), 3, FrameFormat::Png)
            .unwrap_err()
            .contains("frame-0002.png already exists"));
        // A different extension does not collide.
        assert!(check_names_free(temp.path(), 3, FrameFormat::Jpeg { quality: 80 }).is_ok());
        assert!(write_new_file(&existing, b"new").is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"old");
        #[cfg(unix)]
        {
            let target = temp.path().join("target");
            let link = temp.path().join("frame-0003.png");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(check_names_free(temp.path(), 3, FrameFormat::Png).is_err());
            assert!(write_new_file(&link, b"x").is_err());
            assert!(!target.exists());
        }
    }

    #[test]
    fn request_validation_covers_output_and_inline_limits() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap().to_owned();
        let request = SeriesRequest::parse(&json!({
            "count": 4, "interval_ms": 10, "format": "jpeg", "quality": 70,
            "output": "files", "out_dir": dir, "return_frames": "changed",
            "change_threshold": 0, "max_image_dimension": 640,
            "region": {"x": 1, "y": 2, "width": 3, "height": 4}
        }))
        .unwrap();
        assert_eq!(request.format, FrameFormat::Jpeg { quality: 70 });
        assert_eq!(
            request.output,
            Output::Files(std::fs::canonicalize(temp.path()).unwrap())
        );
        assert_eq!(request.return_frames, ReturnFrames::Changed);
        assert_eq!(request.change_threshold, 0.0);
        assert_eq!(request.max_image_dimension, 640);

        let defaults = SeriesRequest::parse(&json!({})).unwrap();
        assert_eq!(defaults.format, FrameFormat::Png);
        assert_eq!(defaults.output, Output::Inline);
        assert_eq!(defaults.return_frames, ReturnFrames::All);
        assert_eq!(defaults.max_inline_images, DEFAULT_MAX_INLINE_IMAGES);
        assert_eq!(defaults.change_threshold, DEFAULT_CHANGE_THRESHOLD);
        assert_eq!(defaults.stop_after_unchanged_ms, None);
        for value in [0, 2000, MAX_SPAN_MS] {
            let request =
                SeriesRequest::parse(&json!({ "stop_after_unchanged_ms": value })).unwrap();
            assert_eq!(request.stop_after_unchanged_ms, Some(value));
        }

        for (value, expected) in [
            (json!({"quality": 50}), "only to format \"jpeg\""),
            (json!({"format": "jpeg", "quality": 0}), "1 to 100"),
            (json!({"format": "gif"}), "\"png\" or \"jpeg\""),
            (json!({"output": "files"}), "needs `out_dir`"),
            (json!({"out_dir": "/tmp"}), "needs output \"files\""),
            (json!({"output": "disk"}), "\"inline\" or \"files\""),
            (
                json!({"return_frames": "first"}),
                "\"all\", \"changed\" or \"last\"",
            ),
            (json!({"max_inline_images": 31}), "1 to 30"),
            (json!({"change_threshold": 1.5}), "from 0 to 1"),
            (
                json!({"stop_after_unchanged_ms": -1}),
                "non-negative integer",
            ),
            (
                json!({"stop_after_unchanged_ms": 2.5}),
                "non-negative integer",
            ),
            (
                json!({"stop_after_unchanged_ms": "2000"}),
                "non-negative integer",
            ),
            (
                json!({"stop_after_unchanged_ms": 600_001}),
                "`stop_after_unchanged_ms` must be 0 to 600000",
            ),
            (json!({"count": 11}), "exceed max_inline_images 10"),
            (
                json!({"output": "files", "out_dir": "/nonexistent/pincer-capture-series"}),
                "does not exist",
            ),
        ] {
            let error = SeriesRequest::parse(&value).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
        // More frames than the inline limit are fine when not all are returned.
        assert!(SeriesRequest::parse(&json!({"count": 30, "return_frames": "changed"})).is_ok());
        assert!(SeriesRequest::parse(&json!({"count": 30, "max_inline_images": 30})).is_ok());
    }

    /// Replays a fixed list of frames, optionally taking `delay` per capture.
    struct FakeSource {
        frames: Mutex<Vec<Rgba8Image>>,
        delay: Duration,
    }

    impl FakeSource {
        fn new(frames: Vec<Rgba8Image>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(frames.into_iter().rev().collect()),
                delay,
            })
        }
    }

    impl FrameSource for FakeSource {
        fn frame_size(&self) -> Result<Option<(u32, u32)>> {
            Ok(Some((20, 10)))
        }

        fn capture(&self, _region: Option<PixelRect>) -> Result<SourceFrame> {
            std::thread::sleep(self.delay);
            let frame = self
                .frames
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow!("fake source ran out of frames"))?;
            Ok(SourceFrame {
                png: png_of(&frame),
                origin: (0, 0),
                frame_size: frame.dimensions(),
            })
        }

        fn label(&self) -> String {
            "fake".into()
        }
    }

    fn solid(value: u8) -> Rgba8Image {
        rgba(20, 10, |_, _| [value, value, value, 255])
    }

    fn frames_of(result: &ToolResult) -> Vec<Value> {
        result.structured_content.as_ref().unwrap()["frames"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn image_count(result: &ToolResult) -> usize {
        result
            .content
            .iter()
            .filter(|content| matches!(content, Content::Image { .. }))
            .count()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn frames_follow_the_schedule_from_one_start() {
        let source = FakeSource::new((0..5).map(solid).collect(), Duration::ZERO);
        let request = SeriesRequest::parse(&json!({"count": 5, "interval_ms": 60})).unwrap();
        let result = run(request, source).await;
        assert_eq!(result.is_error, None, "{:?}", result.content);
        let frames = frames_of(&result);
        assert_eq!(frames.len(), 5);
        assert_eq!(image_count(&result), 5);
        for (index, frame) in frames.iter().enumerate() {
            let scheduled = 60 * index as u64;
            let t = frame["t_ms"].as_u64().unwrap();
            assert_eq!(frame["scheduled_ms"], scheduled);
            assert!(t >= scheduled && t < scheduled + 50, "{frame}");
            assert_eq!(frame["late"], false);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_capture_that_overruns_its_slot_makes_the_next_frame_late_without_drift() {
        // Each capture takes 50 ms; frames are 20 ms apart, then a long gap.
        let source = FakeSource::new((0..4).map(solid).collect(), Duration::from_millis(50));
        let request = SeriesRequest::parse(&json!({"offsets_ms": [0, 20, 40, 400]})).unwrap();
        let result = run(request, source).await;
        let frames = frames_of(&result);
        let late: Vec<bool> = frames
            .iter()
            .map(|frame| frame["late"].as_bool().unwrap())
            .collect();
        assert_eq!(late, vec![false, true, true, false]);
        // The last frame is still taken at its own offset, not 400 ms after
        // the late ones.
        let t = frames[3]["t_ms"].as_u64().unwrap();
        assert!((400..450).contains(&t), "{t}");
        let Content::Text { text, .. } = &result.content[0] else {
            panic!("first content block is not text");
        };
        assert!(text.contains("(2 late)"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changed_keeps_only_frames_that_differ_from_the_last_kept_one() {
        let source = FakeSource::new(
            vec![solid(1), solid(1), solid(2), solid(2), solid(1)],
            Duration::ZERO,
        );
        let request = SeriesRequest::parse(
            &json!({"count": 5, "interval_ms": 0, "return_frames": "changed"}),
        )
        .unwrap();
        let result = run(request, source).await;
        let returned: Vec<bool> = frames_of(&result)
            .iter()
            .map(|frame| frame["returned"].as_bool().unwrap())
            .collect();
        assert_eq!(returned, vec![true, false, true, false, true]);
        assert_eq!(image_count(&result), 3);
        assert_eq!(frames_of(&result)[1]["diff_fraction"], 0.0);
        assert_eq!(frames_of(&result)[2]["diff_fraction"], 1.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_inline_reply_keeps_the_first_and_latest_changed_frames() {
        let source = FakeSource::new((0..4).map(solid).collect(), Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "count": 4, "interval_ms": 0, "return_frames": "changed", "max_inline_images": 2
        }))
        .unwrap();
        let result = run(request, source).await;
        let frames = frames_of(&result);
        let returned: Vec<bool> = frames
            .iter()
            .map(|frame| frame["returned"].as_bool().unwrap())
            .collect();
        assert_eq!(returned, vec![true, false, false, true]);
        assert_eq!(frames[1]["omitted"], true);
        assert_eq!(image_count(&result), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_returns_one_frame_and_files_are_written_once() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap().to_owned();
        let request = SeriesRequest::parse(&json!({
            "count": 3, "interval_ms": 0, "return_frames": "changed",
            "output": "files", "out_dir": dir
        }))
        .unwrap();
        let source = FakeSource::new(vec![solid(1), solid(1), solid(3)], Duration::ZERO);
        let result = run(request.clone(), source).await;
        assert_eq!(result.is_error, None);
        assert_eq!(image_count(&result), 0);
        let mut names: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["frame-0001.png", "frame-0003.png"]);
        let written = image_utils::decode_png_to_rgba8(
            &std::fs::read(temp.path().join("frame-0003.png")).unwrap(),
        )
        .unwrap();
        assert_eq!(written, solid(3));

        // Running again into the same directory refuses before capturing.
        let source = FakeSource::new(vec![solid(1); 3], Duration::ZERO);
        let again = run(request, source).await;
        assert_eq!(again.is_error, Some(true));
        assert_eq!(
            again.structured_content.as_ref().unwrap()["code"],
            "invalid_arguments"
        );

        let source = FakeSource::new(vec![solid(1), solid(2), solid(3)], Duration::ZERO);
        let last =
            SeriesRequest::parse(&json!({"count": 3, "interval_ms": 0, "return_frames": "last"}))
                .unwrap();
        let result = run(last, source).await;
        assert_eq!(image_count(&result), 1);
        assert_eq!(frames_of(&result)[2]["returned"], true);
    }

    fn structured_of(result: &ToolResult) -> &Value {
        result.structured_content.as_ref().unwrap()
    }

    fn text_of(result: &ToolResult) -> &str {
        let Content::Text { text, .. } = &result.content[0] else {
            panic!("first content block is not text");
        };
        text
    }

    /// Offsets 100 ms apart, so a stop limit between two of them is never
    /// reached early by scheduling jitter.
    fn spaced(count: u64) -> Vec<u64> {
        (0..count).map(|index| index * 100).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_series_stops_once_nothing_changed_for_the_limit() {
        let source = FakeSource::new(vec![solid(1); 8], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(8), "stop_after_unchanged_ms": 250
        }))
        .unwrap();
        let result = run(request, source).await;
        assert_eq!(result.is_error, None, "{:?}", result.content);
        let structured = structured_of(&result);
        // The first frame starts the clock at 0 ms; frame 4 at 300 ms is the
        // first one 250 ms or more after it.
        assert_eq!(structured["frame_count"], 4);
        assert_eq!(structured["scheduled_count"], 8);
        assert_eq!(structured["stopped_early"], true);
        let reason = structured["stop_reason"].as_str().unwrap();
        assert!(reason.contains("(frames 1-4)"), "{reason}");
        assert!(
            reason.contains("stop_after_unchanged_ms is 250"),
            "{reason}"
        );
        assert!(
            text_of(&result).contains("Stopped early after frame 4 of 8"),
            "{}",
            text_of(&result)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_restarts_the_unchanged_clock() {
        let frames = vec![
            solid(1),
            solid(1),
            solid(2),
            solid(2),
            solid(2),
            solid(2),
            solid(2),
            solid(2),
        ];
        let source = FakeSource::new(frames, Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(8), "stop_after_unchanged_ms": 250
        }))
        .unwrap();
        let result = run(request, source).await;
        let structured = structured_of(&result);
        // Frame 3 changed at 200 ms, so the stop comes at frame 6 (500 ms).
        assert_eq!(structured["frame_count"], 6);
        assert_eq!(structured["stopped_early"], true);
        let reason = structured["stop_reason"].as_str().unwrap();
        assert!(reason.contains("(frames 3-6)"), "{reason}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_series_that_keeps_changing_runs_its_full_schedule() {
        let source = FakeSource::new((0..5).map(solid).collect(), Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "count": 5, "interval_ms": 0, "stop_after_unchanged_ms": 0
        }))
        .unwrap();
        let result = run(request, source).await;
        let structured = structured_of(&result);
        assert_eq!(structured["frame_count"], 5);
        assert_eq!(structured["stopped_early"], false);
        assert!(structured.get("stop_reason").is_none());
        assert!(!text_of(&result).contains("Stopped early"));

        // Reaching the limit only on the last scheduled frame is not early.
        let source = FakeSource::new(vec![solid(1); 4], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(4), "stop_after_unchanged_ms": 300
        }))
        .unwrap();
        let result = run(request, source).await;
        assert_eq!(structured_of(&result)["frame_count"], 4);
        assert_eq!(structured_of(&result)["stopped_early"], false);

        // Without the option the series never stops early, even when still.
        let source = FakeSource::new(vec![solid(1); 5], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({"count": 5, "interval_ms": 0})).unwrap();
        let result = run(request, source).await;
        assert_eq!(structured_of(&result)["frame_count"], 5);
        assert_eq!(structured_of(&result)["stopped_early"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn zero_stops_at_the_first_frame_that_matches_the_one_before_it() {
        let frames = vec![solid(1), solid(2), solid(2), solid(3)];
        let source = FakeSource::new(frames, Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "count": 4, "interval_ms": 0, "stop_after_unchanged_ms": 0
        }))
        .unwrap();
        let result = run(request, source).await;
        assert_eq!(structured_of(&result)["frame_count"], 3);
        assert_eq!(structured_of(&result)["stopped_early"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_returns_the_frame_that_ended_the_series() {
        let source = FakeSource::new(vec![solid(1); 8], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(8), "stop_after_unchanged_ms": 250,
            "return_frames": "last"
        }))
        .unwrap();
        let result = run(request, source).await;
        let returned: Vec<bool> = frames_of(&result)
            .iter()
            .map(|frame| frame["returned"].as_bool().unwrap())
            .collect();
        assert_eq!(returned, vec![false, false, false, true]);
        assert_eq!(image_count(&result), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_stop_clock_compares_with_the_previous_frame_in_changed_mode() {
        // Each frame whitens 4 more of the 200 pixels: 0.02 against the
        // previous frame, under the 0.03 threshold, but 0.04 against the
        // frame kept two steps earlier.
        let creeping = |step: u32| {
            rgba(20, 10, move |x, y| {
                if y * 20 + x < step * 4 {
                    [255, 255, 255, 255]
                } else {
                    [0, 0, 0, 255]
                }
            })
        };
        let source = FakeSource::new((0..8).map(creeping).collect(), Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(8), "stop_after_unchanged_ms": 250,
            "return_frames": "changed", "change_threshold": 0.03
        }))
        .unwrap();
        let result = run(request, source).await;
        let frames = frames_of(&result);
        assert_eq!(frames.len(), 4);
        assert_eq!(structured_of(&result)["stopped_early"], true);
        // `changed` still measures against the last kept frame.
        let returned: Vec<bool> = frames
            .iter()
            .map(|frame| frame["returned"].as_bool().unwrap())
            .collect();
        assert_eq!(returned, vec![true, false, true, false]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_written_before_an_early_stop_stay() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap().to_owned();
        let source = FakeSource::new(vec![solid(1); 8], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "offsets_ms": spaced(8), "stop_after_unchanged_ms": 250,
            "output": "files", "out_dir": dir
        }))
        .unwrap();
        let result = run(request, source).await;
        assert_eq!(result.is_error, None);
        let mut names: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "frame-0001.png",
                "frame-0002.png",
                "frame-0003.png",
                "frame-0004.png"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_region_outside_the_frame_fails_before_capturing() {
        let source = FakeSource::new(vec![], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({
            "count": 1, "region": {"x": 15, "y": 0, "width": 10, "height": 5}
        }))
        .unwrap();
        let result = run(request, source).await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["code"], "invalid_arguments");
        assert!(structured["detail"]
            .as_str()
            .unwrap()
            .contains("does not fit the 20x10 frame"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_capture_reports_the_frames_taken_before_it() {
        let source = FakeSource::new(vec![solid(1)], Duration::ZERO);
        let request = SeriesRequest::parse(&json!({"count": 2, "interval_ms": 0})).unwrap();
        let result = run(request, source).await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["code"], "capture_failed");
        assert_eq!(structured["failed_frame"], 2);
        assert_eq!(structured["frames"].as_array().unwrap().len(), 1);
    }
}
