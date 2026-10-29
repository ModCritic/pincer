//! Windows.Graphics.Capture (WGC) backend for window screenshots.
//!
//! Used when PrintWindow returns black (UWP / DirectComposition) AND/OR
//! when the target window is occluded by another window. WGC captures
//! the window's own composited frames out of DWM regardless of z-order,
//! which is the only correct way to screenshot UWP apps like Calculator
//! when they're behind the user's terminal.
//!
//! Constraints:
//! - Minimum OS: Windows 10 version 1903 (~April 2019). ~100% of users
//!   today; we surface a structured error on older builds.
//! - Cannot capture **minimized** windows. They have no rendered content.
//!   `get_window_state` still returns the UIA tree for a minimized target
//!   (the screenshot is reported unavailable). We surface this as an
//!   error with the recommended fallback.
//! - First call per process pays ~50ms for D3D11 device creation +
//!   COM activation factory lookup. Subsequent calls don't cache the
//!   device — could be optimized later, but a single-shot screenshot
//!   tool isn't the place to fight that.
//! - `GraphicsCapture.dll` can throw a C++ exception out of
//!   `CreateForWindow` instead of returning an error. Seen on Tiny10
//!   (Windows 10 21H2 with components removed, in a VM with no GPU):
//!   `winrt::hresult_error` E_FAIL. Rust can't catch it. Left alone on
//!   Tiny10, which has no Windows Error Reporting, Windows shows an error
//!   dialog and the next throw ends the process. With Error Reporting the
//!   first throw most likely ends it (not reproduced).
//!   `screenshot_window_via_wgc` stops both; see its comment.

use anyhow::{anyhow, bail, Context, Result};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Once, OnceLock};
use std::time::Duration;

use windows::{
    core::Interface,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::IDXGIDevice,
        },
        System::Diagnostics::Debug::{
            SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
            LPTOP_LEVEL_EXCEPTION_FILTER,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
        UI::WindowsAndMessaging::IsIconic,
    },
};

/// How long a caller waits for one WGC capture. A working capture takes
/// about 50 ms plus at most the 1500 ms frame wait below; the rest is slack.
/// It has to stay under `get_window_state`'s 4 s screenshot budget, so the
/// caller still has time for the screen-region fallback.
const WGC_CAPTURE_TIMEOUT: Duration = Duration::from_millis(2500);

/// MSVC's exception code for a C++ `throw`.
const CPP_EXCEPTION: u32 = 0xE06D_7363;

type Capture = Result<(Vec<u8>, u32, u32)>;

/// Set while a WGC capture thread is running, so a second capture never
/// starts beside one that hasn't returned.
static WGC_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Set for good once WGC has thrown in this process.
static WGC_THREW: AtomicBool = AtomicBool::new(false);
static INSTALL_FILTER: Once = Once::new();
static PREVIOUS_FILTER: OnceLock<LPTOP_LEVEL_EXCEPTION_FILTER> = OnceLock::new();

thread_local! {
    /// Where a `pincer-wgc` thread sends its result. Only those threads set
    /// it, which is how the exception filter knows a throw came from WGC.
    static RESULT_SENDER: RefCell<Option<mpsc::Sender<Capture>>> = const { RefCell::new(None) };
}

struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        WGC_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Windows calls this before its own crash handling, for any exception no
/// frame handled. A C++ exception on a `pincer-wgc` thread gets its error
/// sent to the waiting caller, and then the thread sleeps forever: it can't
/// return into the code that threw, and ending it would run DLL thread-detach
/// code in the middle of that call. Everything else goes on to the filter
/// that was there before.
unsafe extern "system" fn wgc_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    let code = (*(*info).ExceptionRecord).ExceptionCode.0 as u32;
    let sender = if code == CPP_EXCEPTION {
        RESULT_SENDER
            .try_with(|cell| cell.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
            .ok()
            .flatten()
    } else {
        None
    };
    let Some(sender) = sender else {
        return match PREVIOUS_FILTER.get().copied().flatten() {
            Some(previous) => previous(info),
            None => EXCEPTION_CONTINUE_SEARCH,
        };
    };
    WGC_THREW.store(true, Ordering::Release);
    let _ = sender.send(Err(anyhow!(
        "Windows.Graphics.Capture threw a C++ exception instead of returning \
         an error; WGC is off for the rest of this process"
    )));
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Capture a window via WGC, returning BGRA pixels + (width, height).
/// Caller is responsible for encoding to PNG (or whatever format) via
/// `pincer_core::image_utils::encode_bgra_to_png`.
///
/// The capture runs on its own `pincer-wgc` thread, under the exception
/// filter above, so a throw costs this one capture instead of the process.
/// The caller stops waiting after `WGC_CAPTURE_TIMEOUT` and can fall back.
/// No capture starts while an earlier one is still running.
pub fn screenshot_window_via_wgc(hwnd: u64) -> Result<(Vec<u8>, u32, u32)> {
    if WGC_THREW.load(Ordering::Acquire) {
        bail!(
            "skipped Windows.Graphics.Capture: it threw a C++ exception earlier \
             in this process"
        );
    }
    if WGC_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        bail!("skipped Windows.Graphics.Capture: an earlier WGC capture has not returned");
    }
    INSTALL_FILTER.call_once(|| {
        let previous = unsafe { SetUnhandledExceptionFilter(Some(wgc_exception_filter)) };
        let _ = PREVIOUS_FILTER.set(previous);
    });
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("pincer-wgc".into())
        .spawn(move || {
            let in_flight = InFlight;
            RESULT_SENDER.with(|cell| *cell.borrow_mut() = Some(sender));
            let _ = unsafe {
                windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                )
            };
            let result = unsafe { wgc_capture_impl(HWND(hwnd as *mut _)) };
            // Clear the flag before sending, so a caller that captures again
            // right after this one returns isn't turned away.
            drop(in_flight);
            if let Some(sender) = RESULT_SENDER.with(|cell| cell.borrow_mut().take()) {
                let _ = sender.send(result);
            }
        });
    if let Err(error) = spawned {
        WGC_IN_FLIGHT.store(false, Ordering::Release);
        return Err(error).context("failed to start the WGC capture thread");
    }
    match receiver.recv_timeout(WGC_CAPTURE_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => bail!(
            "Windows.Graphics.Capture did not return within {} ms",
            WGC_CAPTURE_TIMEOUT.as_millis()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the WGC capture thread ended without a result")
        }
    }
}

unsafe fn wgc_capture_impl(hwnd: HWND) -> Result<(Vec<u8>, u32, u32)> {
    if IsIconic(hwnd).as_bool() {
        bail!(
            "WGC cannot capture a minimized window (no rendered content). \
             Call bring_to_front with this window_id to restore it first. \
             `get_window_state` still returns the \
             UIA tree for a minimized window (the screenshot is reported \
             unavailable)."
        );
    }

    // 1. D3D11 device — feature level 11.0 + BGRA support (required by WGC).
    let mut d3d_device: Option<ID3D11Device> = None;
    let mut d3d_context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        windows::Win32::Foundation::HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut d3d_device),
        None,
        Some(&mut d3d_context),
    )
    .context("D3D11CreateDevice failed (hardware feature-level 11.0 + BGRA)")?;
    let d3d_device = d3d_device.context("D3D11CreateDevice returned a None device")?;
    let d3d_context = d3d_context.context("D3D11CreateDevice returned a None context")?;

    // 2. DXGI device interface for the D3D11 device, then the WinRT
    //    IDirect3DDevice wrapper that GraphicsCaptureFramePool needs.
    let dxgi_device: IDXGIDevice = d3d_device
        .cast()
        .context("ID3D11Device → IDXGIDevice cast")?;
    let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
        .context("CreateDirect3D11DeviceFromDXGIDevice failed")?;
    let direct3d_device: IDirect3DDevice = inspectable
        .cast()
        .context("CreateDirect3D11DeviceFromDXGIDevice → IDirect3DDevice cast")?;

    // 3. GraphicsCaptureItem for our target HWND.
    let interop_factory: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .context("IGraphicsCaptureItemInterop activation factory")?;
    let item: GraphicsCaptureItem = interop_factory.CreateForWindow(hwnd).context(
        "IGraphicsCaptureItemInterop::CreateForWindow failed — \
             requires Win10 1903+ and the target window must exist",
    )?;
    let item_size = item.Size().context("GraphicsCaptureItem::Size")?;
    if item_size.Width <= 0 || item_size.Height <= 0 {
        bail!(
            "WGC item size is {}x{} — target may be hidden/cloaked. \
             Try restoring the window or use UIA.",
            item_size.Width,
            item_size.Height
        );
    }

    // 4. Frame pool + capture session.
    //    `CreateFreeThreaded` (not `Create`) — `Create` requires the
    //    calling thread to have a CoreDispatcher/event loop running
    //    for FrameArrived to fire, which we don't have inside a
    //    daemon's spawn_blocking thread. `CreateFreeThreaded`
    //    dispatches FrameArrived on a system thread-pool thread, which
    //    is exactly what our mpsc channel handler expects. Without
    //    this, FrameArrived silently never fires and our 1500 ms wait
    //    always times out.
    //
    //    Frame count 2 — WGC docs recommend ≥2 to avoid GPU stalls
    //    while the consumer reads the previous frame. We only consume
    //    one but the GPU side is happier with breathing room.
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &direct3d_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        item_size,
    )
    .context("Direct3D11CaptureFramePool::CreateFreeThreaded")?;
    let session = pool
        .CreateCaptureSession(&item)
        .context("Direct3D11CaptureFramePool::CreateCaptureSession")?;

    // 5. Hide the WGC capture indicator (yellow border) if the build
    //    supports it. Available since Win11 (IsBorderRequired property).
    //    Best-effort — older Win10 won't have the setter; ignore failure.
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(false);

    // 6. Start capture, then poll TryGetNextFrame until a frame is
    //    available or we time out. We tried FrameArrived events first
    //    (both `Create` and `CreateFreeThreaded` pool variants) but
    //    the event never fired in our daemon's thread environment —
    //    likely an STA/COM-apartment mismatch we couldn't isolate.
    //    Polling suits single-shot capture anyway: WGC drops frames
    //    in the pool as DWM composes them, and TryGetNextFrame returns Ok(None) /
    //    NULL until one's available. 50 ms polling interval keeps the
    //    "frame finally arrived" detection latency low without burning
    //    CPU on a tight spin.
    session
        .StartCapture()
        .context("GraphicsCaptureSession::StartCapture")?;

    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    let mut frame_opt = None;
    while std::time::Instant::now() < deadline {
        match pool.TryGetNextFrame() {
            Ok(f) => {
                frame_opt = Some(f);
                break;
            }
            Err(_) => {
                // TryGetNextFrame returns an error when no frame is
                // available yet. Wait briefly, retry.
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let frame = frame_opt.ok_or_else(|| {
        anyhow::anyhow!(
            "WGC TryGetNextFrame returned no frame within 1500 ms — \
         DWM may not be compositing this window (cloaked / not actually \
         rendered). Fall through to screen-region BitBlt."
        )
    })?;

    // 8. Pull the underlying ID3D11Texture2D out of the WinRT frame
    //    surface via the IDirect3DDxgiInterfaceAccess shim.
    let surface = frame.Surface().context("frame.Surface")?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .context("IDirect3DSurface → IDirect3DDxgiInterfaceAccess cast")?;
    let frame_texture: ID3D11Texture2D = access
        .GetInterface()
        .context("IDirect3DDxgiInterfaceAccess::GetInterface(ID3D11Texture2D)")?;

    // 9. Create a staging texture we can map for CPU read, copy frame in.
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    frame_texture.GetDesc(&mut desc);
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    let mut staging: Option<ID3D11Texture2D> = None;
    d3d_device
        .CreateTexture2D(&desc, None, Some(&mut staging))
        .context("CreateTexture2D(staging)")?;
    let staging = staging.context("CreateTexture2D returned a None texture")?;
    d3d_context.CopyResource(&staging, &frame_texture);

    // 10. Map the staging texture, copy BGRA out row-by-row (RowPitch may
    //     be padded — can't just copy `Width * 4 * Height` straight).
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    d3d_context
        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .context("ID3D11DeviceContext::Map")?;
    let width = desc.Width as usize;
    let height = desc.Height as usize;
    let stride = mapped.RowPitch as usize;
    let mut bgra = vec![0u8; width * height * 4];
    let src_base = mapped.pData as *const u8;
    for row in 0..height {
        std::ptr::copy_nonoverlapping(
            src_base.add(row * stride),
            bgra.as_mut_ptr().add(row * width * 4),
            width * 4,
        );
    }
    d3d_context.Unmap(&staging, 0);

    // 11. Cleanup: close the session and pool explicitly. RAII would
    //     handle this but being explicit makes the lifetimes easier to
    //     audit. (No FrameArrived to unhook — we polled instead.)
    let _ = session.Close();
    let _ = pool.Close();

    Ok((bgra, desc.Width, desc.Height))
}
