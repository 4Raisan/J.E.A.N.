//! On-demand per-window capture: pixels as a second witness, with the blank cases kept apart.
//!
//! Capture exists because some content is only available as imagery, and it is deliberately the *last*
//! thing tried (D-004): everything here is a fallback for a question the semantic path could not answer.
//! Two rules give the module its shape.
//!
//! ## A frame is evidence, never a verifier
//!
//! R-21 is the reason this crate does not use a screenshot to decide that an action worked. A difference
//! between two frames says the screen changed; it says nothing about whether the change was the one
//! requested, and the failure mode of screenshot-diff verification is that it is confidently right almost
//! always and confidently wrong at exactly the moment it matters. So [`Capture`] reports what the frame
//! *is* — its size, its brightness spread, whether every pixel is the same colour — and leaves "did the
//! task happen" to the accessibility read that can answer it.
//!
//! ## Four kinds of nothing
//!
//! A blank capture is not one fact but several, and `docs/desktop-control-plan.md` requires that they not
//! be collapsed:
//!
//! | What was observed | Named here | Detected by |
//! |---|---|---|
//! | the window asked not to be captured | explicit protection | `GetWindowDisplayAffinity`, a property of the window and not of the attempt |
//! | every pixel is the same colour | [`FrameQuality::UniformBlack`] / `UniformColor` | [`Capture::quality`] |
//! | the window has no surface to draw from | minimised, zero-size | `IsIconic`, the window rectangle |
//! | the frame is old | stale | [`Capture::same_content_as`] against the previous frame, inside the recovery rules below |
//!
//! Only the first is a protection. The others are conditions, and reporting a black frame as "protected"
//! is the specific error the plan names — it turns a graphics glitch into a policy conclusion.
//!
//! ## Bounded recovery, and no more
//!
//! The plan's limit is one retry after re-checking the target, then one alternate backend, then escalate
//! with evidence. That is [`capture_with_recovery`], and the reason it is spelled as a sequence rather
//! than a loop is that a loop is where "and then try the next backend" comes from. Nothing here tries
//! another backend after an explicit exclusion: a restriction on content access is not a cue to obtain
//! the content through another path.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, PW_RENDERFULLCONTENT};

use crate::error::{WindowsError, WindowsResult};
use crate::identity::{
    display_affinity_of, dpi_of, now_ms, rect_of, DisplayAffinity, Rect, WindowId,
};
use crate::worker::{self, Body, ReplyBody, Stop};

/// The default ceiling on one capture, in pixels.
///
/// The same number the settings document uses for `[vision] max_capture_pixels`: 4 MP, larger than any
/// window on any monitor this build is expected to see. It is a *guard* and not a scaler — there is no
/// image library in this tree — so a window bigger than the cap is refused rather than resized, and the
/// refusal is the honest answer.
pub const DEFAULT_MAX_PIXELS: usize = 4_194_304;

/// How far two frame hashes may differ and still be called the same picture.
///
/// The hash is 64-bit, so 3 differing bits is a caret blink or a focus ring and a rebuilt layout is tens.
/// The threshold feeds [`Capture::same_content_as`] and nothing else — it decides whether a frame looks
/// *unchanged*, never whether a task succeeded.
pub const STALENESS_BITS: u32 = 3;

/// One 32-bit BGRA pixel buffer, exactly as GDI produced it.
///
/// Deliberately not `Clone`: a frame is megabytes, and a type that can be cloned quietly is a type that
/// will be cloned quietly somewhere in a report. A caller that needs a second copy asks for one.
///
/// `Debug` is written by hand and prints *nothing* about the content. This type is the only place in the
/// crate that holds pixels, and a derived `Debug` would turn every `{:?}` of anything enclosing it —
/// including a log line — into a screenshot dump. Rule 6 of the brief is the reason it is hand-written
/// rather than remembered.
pub struct Capture {
    pub window: WindowId,
    pub width: u32,
    pub height: u32,
    /// BGRA, row-major, top row first, no padding — the layout GDI was asked to produce.
    pub pixels: Vec<u8>,
    pub backend: Backend,
    /// The 64-bit average hash of the frame.
    pub hash: u64,
    /// Mean luma across the frame, 0..=255, computed in the same pass as the hash.
    pub mean_luma: u8,
    /// `true` when every pixel is bit-for-bit the same colour.
    pub uniform: bool,
    /// That colour, when `uniform`.
    pub uniform_argb: Option<u32>,
    /// The DPI the window is rendered at, recorded with the frame it describes.
    ///
    /// A capture without its DPI cannot be used for coordinate grounding at all, and a two-monitor
    /// mixed-DPI machine makes that a daily case rather than an edge one.
    pub render_dpi: Option<u32>,
    pub captured_at_ms: u64,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Capture")
            .field("window", &self.window.hwnd)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("backend", &self.backend)
            .field("hash", &format_args!("{:016x}", self.hash))
            .field("mean_luma", &self.mean_luma)
            .field("uniform", &self.uniform)
            .field("render_dpi", &self.render_dpi)
            .field("captured_at_ms", &self.captured_at_ms)
            .finish_non_exhaustive()
    }
}

impl Capture {
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }

    /// Whether a previous frame and this one are the same picture.
    pub fn same_content_as(&self, previous: &Capture) -> bool {
        hamming(self.hash ^ previous.hash) <= STALENESS_BITS
    }

    /// What the frame says about itself, before anyone asks a question of it.
    pub fn quality(&self) -> FrameQuality {
        if self.uniform {
            // The low three bytes are BGR; an all-zero triple is black whatever the alpha byte claims,
            // and an uninitialised DIB section is zeroed by the OS, which is why "uniform black" and
            // "nothing was ever drawn" are the same observation here and must not be reported as a
            // protection.
            let argb = self.uniform_argb.unwrap_or(0);
            if argb & 0x00FF_FFFF == 0 {
                FrameQuality::UniformBlack
            } else {
                FrameQuality::UniformColor
            }
        } else if self.mean_luma < 8 {
            FrameQuality::NearlyBlack
        } else if self.mean_luma > 247 {
            FrameQuality::NearlyWhite
        } else {
            FrameQuality::Usable
        }
    }
}

/// What a frame's content looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FrameQuality {
    /// There is a picture in here.
    Usable,
    /// Every pixel is the same colour, and that colour is not black.
    UniformColor,
    /// Every pixel is the same colour and that colour is black.
    UniformBlack,
    /// Not uniform, but very dark — the shape of a window that composited nothing.
    NearlyBlack,
    /// Not uniform, but very bright — usually an uninitialised surface.
    NearlyWhite,
}

impl FrameQuality {
    /// Whether anything could be read out of this frame.
    ///
    /// `false` does not mean "protected". It means this frame is not evidence about the content, and the
    /// reason has to come from [`crate::capability`], not from here.
    pub fn is_informative(self) -> bool {
        matches!(self, Self::Usable)
    }
}

/// The capture mechanisms, in the order the recovery rule is allowed to try them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    /// `PrintWindow` with `PW_RENDERFULLCONTENT`: the application is asked to draw itself into our
    /// surface. Works for an occluded or partly off-screen window, because it does not read the screen.
    PrintWindow,
    /// `BitBlt` from the screen DC at the window's rectangle: what the display actually shows.
    ///
    /// The alternate, and only useful when the window is visible, unminimised and not covered — which is
    /// why it is second, and why its own failure is a different fact from the first one's.
    ScreenBitBlt,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::PrintWindow => "print_window",
            Self::ScreenBitBlt => "screen_bitblt",
        }
    }
}

/// The capture boundary, so a different mechanism is a different implementation and not a rewrite.
pub trait CaptureBackend: Send + Sync {
    /// One capture of one window, bounded by `max_pixels`.
    fn capture(&self, window: WindowId, max_pixels: usize) -> WindowsResult<Capture>;
}

/// `PrintWindow` into a 32-bit DIB section.
#[derive(Debug, Clone, Copy)]
pub struct PrintWindowCapture;

/// Screen `BitBlt` of the window's rectangle.
#[derive(Debug, Clone, Copy)]
pub struct ScreenCapture;

/// Settings the capture path runs under.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub call_timeout: Duration,
    pub max_pixels: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(10),
            max_pixels: DEFAULT_MAX_PIXELS,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> WindowsResult<()> {
        if self.call_timeout.is_zero() {
            return Err(WindowsError::InvalidSetting {
                reason: "desktop.capture_timeout_secs must be at least 1".to_owned(),
            });
        }
        if self.max_pixels == 0 {
            return Err(WindowsError::InvalidSetting {
                reason: "desktop.max_capture_pixels must be at least 1".to_owned(),
            });
        }
        Ok(())
    }
}

impl CaptureBackend for PrintWindowCapture {
    fn capture(&self, window: WindowId, max_pixels: usize) -> WindowsResult<Capture> {
        // The two conditions under which there is nothing to draw, checked before any surface exists: a
        // minimised window has no client area to render, and a zero-size rectangle would ask GDI for a
        // zero-pixel DIB and then hand back an empty image that reads as a black one.
        if bool::from(unsafe { IsIconic(window.to_raw()) }) {
            return Err(WindowsError::CaptureUnusable {
                hwnd: window.hwnd,
                reason: "minimised: the window has no surface to render".to_owned(),
            });
        }
        let (width, height) = bounds(window, max_pixels)?;
        // `PW_RENDERFULLCONTENT` is what makes a DirectComposition-backed window — every modern browser,
        // and the UWP shell frames — draw anything at all. Without it `PrintWindow` returns success and
        // paints nothing, which is the single most misleading result this module could produce: it looks
        // exactly like a protected window.
        let pixels = draw_window(window, width, height, |memory, _screen| {
            let drew = unsafe {
                PrintWindow(
                    window.to_raw(),
                    memory,
                    PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT),
                )
            };
            if !bool::from(drew) {
                let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
                return Err(WindowsError::CaptureUnusable {
                    hwnd: window.hwnd,
                    reason: format!(
                        "PrintWindow refused (Win32 error {last_error:?}); the window may not own a device context"
                    ),
                });
            }
            Ok(())
        })?;
        Ok(describe(
            window,
            width,
            height,
            pixels,
            Backend::PrintWindow,
        ))
    }
}

impl CaptureBackend for ScreenCapture {
    fn capture(&self, window: WindowId, max_pixels: usize) -> WindowsResult<Capture> {
        let (width, height) = bounds(window, max_pixels)?;
        let rect = rect_of(window).expect("bounds() already read the rectangle");
        // This is the only place in the crate that reads the *display*, and therefore the only place a
        // misleading overlay can mislead it — which is why the input path re-checks the foreground itself
        // and never trusts this frame about what is on top.
        let pixels = draw_window(window, width, height, |memory, screen| {
            unsafe {
                BitBlt(
                    memory,
                    0,
                    0,
                    width,
                    height,
                    Some(screen),
                    rect.left,
                    rect.top,
                    SRCCOPY,
                )
            }
            .map_err(|error| WindowsError::CaptureUnusable {
                hwnd: window.hwnd,
                reason: format!("BitBlt of the screen region failed: {error}"),
            })
        })?;
        Ok(describe(
            window,
            width,
            height,
            pixels,
            Backend::ScreenBitBlt,
        ))
    }
}

/// The size a capture of `window` would produce, or the reason it may not be produced at all.
fn bounds(window: WindowId, max_pixels: usize) -> WindowsResult<(i32, i32)> {
    let rect = rect_of(window).ok_or_else(|| WindowsError::CaptureUnusable {
        hwnd: window.hwnd,
        reason: "the window rectangle could not be read".to_owned(),
    })?;
    if rect.is_empty() {
        return Err(WindowsError::CaptureUnusable {
            hwnd: window.hwnd,
            reason: format!("the window is {}x{}", rect.width(), rect.height()),
        });
    }
    let (width, height) = (rect.width(), rect.height());
    if width as usize * height as usize > max_pixels {
        // Refused, not resized: there is no scaler in this tree, and a frame that silently came back
        // smaller than the window would make every coordinate claim built from it a lie.
        return Err(WindowsError::CaptureUnusable {
            hwnd: window.hwnd,
            reason: format!("{width}x{height} exceeds the {max_pixels} pixel capture bound"),
        });
    }
    Ok((width, height))
}

/// Allocate a top-down 32-bit DIB, let `draw` paint into it, and hand the bytes back.
///
/// The surface is always released. `render_into` is the only place in this module that owns a GDI object,
/// so there is exactly one leak site to audit rather than one per backend.
fn draw_window(
    window: WindowId,
    width: i32,
    height: i32,
    draw: impl FnOnce(
        windows::Win32::Graphics::Gdi::HDC,
        windows::Win32::Graphics::Gdi::HDC,
    ) -> WindowsResult<()>,
) -> WindowsResult<Vec<u8>> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // A negative height asks GDI for a top-down DIB, so row 0 is the top of the window. Without
            // it the image is vertically mirrored and every coordinate derived from it is wrong.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();

    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(WindowsError::CaptureUnusable {
                hwnd: window.hwnd,
                reason: "the screen device context could not be obtained".to_owned(),
            });
        }
        let memory = CreateCompatibleDC(Some(screen));
        // The surface is resolved to an owned handle *before* the drawing closure runs, so the cleanup
        // below can always see it. A DIB created and then leaked on an error path is exactly the GDI leak
        // that only shows up after a long session, and the only way to be sure of releasing it is to keep
        // the handle outside the code that can fail.
        let surface =
            match CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    if !memory.is_invalid() {
                        let _ = DeleteDC(memory);
                    }
                    let _ = ReleaseDC(None, screen);
                    return Err(WindowsError::CaptureUnusable {
                        hwnd: window.hwnd,
                        reason: format!("CreateDIBSection failed: {error}"),
                    });
                }
            };
        let outcome = (|| -> WindowsResult<Vec<u8>> {
            let bitmap = surface.ok_or_else(|| WindowsError::CaptureUnusable {
                hwnd: window.hwnd,
                reason: "the capture surface could not be created".to_owned(),
            })?;
            if bits.is_null() || memory.is_invalid() {
                return Err(WindowsError::CaptureUnusable {
                    hwnd: window.hwnd,
                    reason: "the capture surface was created without usable bits".to_owned(),
                });
            }
            let previous = SelectObject(memory, bitmap.into());
            let drawn = draw(memory, screen);
            // The pixel copy happens even when `draw` failed, because the caller has to be able to tell a
            // black frame from an absent one; the error is returned after, unchanged.
            let length = width as usize * height as usize * 4;
            let mut pixels = vec![0u8; length];
            std::ptr::copy_nonoverlapping(bits as *const u8, pixels.as_mut_ptr(), length);
            SelectObject(memory, previous);
            drawn?;
            Ok(pixels)
        })();
        if let Some(bitmap) = surface {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
        }
        if !memory.is_invalid() {
            let _ = DeleteDC(memory);
        }
        // `ReleaseDC` takes the HWND the DC came from, which is `None` for the screen.
        let _ = ReleaseDC(None, screen);
        outcome
    }
}

/// Compute the frame's own statistics in one pass.
///
/// The hash is an average hash over an 8x8 lattice of box-filtered means, gathered in the same pass as
/// the uniformity check. It carries no perceptual guarantees: it is stable across a caret blink and
/// unstable across a re-layout, which is the property staleness detection needs and the property a
/// verifier must not be trusted to need.
fn describe(
    window: WindowId,
    width: i32,
    height: i32,
    pixels: Vec<u8>,
    backend: Backend,
) -> Capture {
    let first = if pixels.len() >= 4 {
        u32::from_le_bytes([pixels[0], pixels[1], pixels[2], pixels[3]])
    } else {
        0
    };
    let uniform = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| u32::from_le_bytes(*pixel) == first);

    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    let mut total: u64 = 0;
    let mut cells = [0u64; 64];
    let mut counts = [0u64; 64];
    for (index, pixel) in pixels.as_chunks::<4>().0.iter().enumerate() {
        // Rec. 601 luma in integer arithmetic. Precision is irrelevant: the value is compared against the
        // mean of its own cell, never against a threshold someone tuned.
        let luma = ((pixel[2] as u64) * 76 + (pixel[1] as u64) * 150 + (pixel[0] as u64) * 30) >> 8;
        total += luma;
        let x = index % w;
        let y = index / w;
        if y < h {
            let cell = (x * 8 / w).min(7) + (y * 8 / h).min(7) * 8;
            cells[cell] += luma;
            counts[cell] += 1;
        }
    }
    let count = (pixels.len() / 4).max(1);
    let mean_luma = (total / count as u64).min(255) as u8;

    let means: Vec<u64> = cells
        .iter()
        .zip(counts.iter())
        .map(|(sum, seen)| seen.checked_div(*sum).unwrap_or(0))
        .collect();
    let average = means.iter().sum::<u64>() / 64;
    let mut hash = 0u64;
    for (bit, mean) in means.iter().enumerate() {
        if *mean > average {
            hash |= 1u64 << bit;
        }
    }

    Capture {
        window,
        width: width as u32,
        height: height as u32,
        pixels,
        backend,
        hash,
        mean_luma,
        uniform,
        uniform_argb: uniform.then_some(first),
        render_dpi: dpi_of(window),
        captured_at_ms: now_ms(),
    }
}

fn hamming(mut value: u64) -> u32 {
    let mut bits = 0u32;
    while value != 0 {
        bits += (value & 1) as u32;
        value >>= 1;
    }
    bits
}

/// One bounded capture of one window, taken by a worker process this one can end.
pub fn capture(
    window: WindowId,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Capture> {
    settings.validate()?;
    run(window, "capture", settings, cancel, Backend::PrintWindow)
}

/// The recovery sequence, with the target re-checked between attempts.
///
/// Exactly: one retry of the primary backend after re-checking that the target is still the same window,
/// then one alternate backend, then stop and report everything that was observed.
#[derive(Debug)]
pub struct Recovery {
    pub capture: Option<Capture>,
    /// Every attempt, in order, each with what it produced or why it stopped.
    pub attempts: Vec<Attempt>,
    /// `true` when the window declared an exclusion, which ends the recovery before it starts.
    pub explicitly_protected: bool,
    /// `true` when nothing more is permitted, which is the state that escalates with evidence.
    pub exhausted: bool,
}

/// One capture attempt and what it produced or refused.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub backend: Backend,
    /// `false` when the attempt produced nothing and the reason is in `note`.
    pub succeeded: bool,
    pub quality: Option<FrameQuality>,
    pub note: String,
}

impl Recovery {
    /// The most informative frame the sequence produced, if any.
    pub fn best(&self) -> Option<&Capture> {
        self.capture.as_ref()
    }

    /// The single sentence a capability report needs from this sequence.
    ///
    /// Kept as one method so a caller cannot paraphrase: an explicitly protected window and an
    /// unresponsive one must not end up described by the same phrase, and the way that happens in
    /// practice is a caller inventing its own summary.
    pub fn verdict(&self) -> RecoveryVerdict {
        if self.explicitly_protected {
            return RecoveryVerdict::ExplicitProtection;
        }
        match &self.capture {
            Some(frame) => RecoveryVerdict::Usable(frame.quality()),
            None if self.exhausted => RecoveryVerdict::Unusable(
                self.attempts
                    .last()
                    .map(|attempt| attempt.note.clone())
                    .unwrap_or_else(|| "no backend produced a frame".to_owned()),
            ),
            None => RecoveryVerdict::Uninformative(
                self.attempts
                    .iter()
                    .filter_map(|attempt| attempt.quality)
                    .next()
                    .unwrap_or(FrameQuality::UniformBlack),
            ),
        }
    }
}

/// What the bounded recovery concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryVerdict {
    Usable(FrameQuality),
    /// A frame was produced and it says nothing; the reason is *not* a protection.
    Uninformative(FrameQuality),
    /// The window itself declared a capture exclusion.
    ExplicitProtection,
    /// No backend could produce a frame at all.
    Unusable(String),
}

/// The bounded recovery the plan specifies, and the only recovery this crate performs.
pub fn capture_with_recovery(
    window: WindowId,
    settings: Settings,
    cancel: Option<&AtomicBool>,
) -> WindowsResult<Recovery> {
    settings.validate()?;
    let affinity = display_affinity_of(window);
    if matches!(
        affinity,
        DisplayAffinity::Monitor | DisplayAffinity::ExcludeFromCapture
    ) {
        // Nothing else is attempted. The window said so, in a property of the window, before any pixels
        // were asked for.
        return Ok(Recovery {
            capture: None,
            attempts: vec![Attempt {
                backend: Backend::PrintWindow,
                succeeded: false,
                quality: None,
                note: format!("the window's own display affinity is {affinity:?}"),
            }],
            explicitly_protected: true,
            exhausted: false,
        });
    }

    let mut attempts = Vec::new();
    let mut outcome = run(window, "capture", settings, cancel, Backend::PrintWindow);

    if let Err(error) = &outcome {
        attempts.push(Attempt {
            backend: Backend::PrintWindow,
            succeeded: false,
            quality: None,
            note: error.to_string(),
        });
        // Retry the primary backend once, but only after the target is re-checked: a window that has been
        // destroyed or minimised in between would make the retry meaningless, and a retry that succeeds
        // against a *different* window is worse than no retry at all.
        let still_there = window.is_alive() && !bool::from(unsafe { IsIconic(window.to_raw()) });
        if !still_there {
            attempts.push(Attempt {
                backend: Backend::PrintWindow,
                succeeded: false,
                quality: None,
                note: "the target was minimised or destroyed; the retry was skipped".to_owned(),
            });
            return Ok(Recovery {
                capture: None,
                attempts,
                explicitly_protected: false,
                exhausted: true,
            });
        }
        outcome = run(
            window,
            "capture-retry",
            settings,
            cancel,
            Backend::PrintWindow,
        );
    }

    let mut frame: Option<Capture> = None;
    match outcome {
        Ok(captured) => {
            let quality = captured.quality();
            attempts.push(Attempt {
                backend: Backend::PrintWindow,
                succeeded: true,
                quality: Some(quality),
                note: quality_note(quality),
            });
            if quality.is_informative() {
                frame = Some(captured);
            }
        }
        Err(error) => attempts.push(Attempt {
            backend: Backend::PrintWindow,
            succeeded: false,
            quality: None,
            note: error.to_string(),
        }),
    }

    // The alternate backend runs only when the primary produced nothing or produced nothing informative,
    // and never against a window that declared an exclusion — the branch above already returned.
    if frame.is_none() {
        let alternate = run(
            window,
            "capture-bitblt",
            settings,
            cancel,
            Backend::ScreenBitBlt,
        );
        match alternate {
            Ok(captured) => {
                let quality = captured.quality();
                attempts.push(Attempt {
                    backend: Backend::ScreenBitBlt,
                    succeeded: true,
                    quality: Some(quality),
                    note: quality_note(quality),
                });
                if quality.is_informative() {
                    frame = Some(captured);
                }
            }
            Err(error) => attempts.push(Attempt {
                backend: Backend::ScreenBitBlt,
                succeeded: false,
                quality: None,
                note: error.to_string(),
            }),
        }
    }

    let exhausted = frame.is_none();
    Ok(Recovery {
        capture: frame,
        attempts,
        explicitly_protected: false,
        exhausted,
    })
}

fn quality_note(quality: FrameQuality) -> String {
    match quality {
        FrameQuality::Usable => "the frame carries content".to_owned(),
        other => format!("the frame is {other:?}"),
    }
}

/// One bounded call against a capture backend, in a worker process the parent can terminate.
///
/// The pixels never cross the pipe — see [`crate::worker`] for why a 16 MB frame does not belong in a
/// line protocol — so this returns the frame the worker wrote into the directory this process owns.
///
/// The timeout maps to [`WindowsError::CaptureUnusable`] rather than a provider timeout because the caller
/// asking for pixels has one question, "did I get a usable frame", and the answer is no with a reason. The
/// reason says the worker was ended, so a hung capture is never mistaken for a protected window.
fn run(
    window: WindowId,
    label: &'static str,
    settings: Settings,
    cancel: Option<&AtomicBool>,
    backend: Backend,
) -> WindowsResult<Capture> {
    let body = Body::Capture {
        window: window.hwnd,
        max_pixels: settings.max_pixels,
        backend,
        // [`one`] substitutes the directory the session owns; nothing else writes here.
        frame_dir: None,
    };
    let answer = match worker::one(&body, settings.call_timeout, cancel) {
        Ok(answer) => answer,
        Err(stop) => {
            return Err(match stop {
                Stop::TimedOut { waited } => WindowsError::CaptureUnusable {
                    hwnd: window.hwnd,
                    reason: format!(
                        "the capture worker did not answer within {}s (waited {}ms) and was terminated",
                        settings.call_timeout.as_secs().max(1),
                        waited.as_millis()
                    ),
                },
                Stop::Cancelled { waited } => WindowsError::Cancelled {
                    budget_secs: settings.call_timeout.as_secs().max(1),
                    waited_ms: waited.as_millis() as u64,
                },
                Stop::Lost { reason } | Stop::Unavailable { reason } => {
                    WindowsError::CaptureUnusable {
                        hwnd: window.hwnd,
                        reason: format!("{label}: {reason}"),
                    }
                }
            });
        }
    };
    let tag = answer.tag();
    match answer.into_result()? {
        ReplyBody::Frame(meta) => meta.into_capture(),
        _ => Err(WindowsError::CaptureUnusable {
            hwnd: window.hwnd,
            reason: format!("the worker answered {label} with a {tag} instead of a frame"),
        }),
    }
}

/// Write a frame as a 24-bit BMP, with no image codec in the tree.
///
/// BMP is chosen because it is trivially correct to write by hand: a header and the pixel array, no
/// dependency, and no encoder bug that produces a file the operator opens and finds garbled. The alpha
/// byte is dropped because a screenshot's alpha is meaningless, and rows are padded to 4 bytes and
/// written bottom-up because that is what the format requires.
///
/// Retention is the caller's decision and is recorded by [`crate::capability`]. Nothing here deletes
/// anything: a mechanism module that cleans up its own evidence is a mechanism that loses it.
pub fn write_bmp(capture: &Capture, path: &Path) -> WindowsResult<PathBuf> {
    let row_bytes = (capture.width as usize * 3).div_ceil(4) * 4;
    let pixels_size = row_bytes * capture.height as usize;
    let header_size = 14 + 40;
    let mut out: Vec<u8> = Vec::with_capacity(header_size + pixels_size);

    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((header_size + pixels_size) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(header_size as u32).to_le_bytes());

    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(capture.width as i32).to_le_bytes());
    out.extend_from_slice(&(capture.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(pixels_size as u32).to_le_bytes());
    // 2835 pixels per metre is 72 DPI, the conventional default. The pixels are not resized to match any
    // DPI, so the value says nothing about the window and only stops a viewer guessing 96 is wrong.
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..capture.height as usize).rev() {
        let start_row = out.len();
        let start = y * capture.width as usize * 4;
        let row = &capture.pixels[start..start + capture.width as usize * 4];
        for pixel in row.as_chunks::<4>().0.iter() {
            out.extend_from_slice(&pixel[..3]);
        }
        // The padding is what the row length minus the bytes written says it is, and the row is grown to
        // it rather than padded byte by byte.
        out.resize(start_row + row_bytes, 0);
    }

    std::fs::write(path, out).map_err(|error| WindowsError::File {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    Ok(path.to_path_buf())
}

/// Whether a frame and its window still agree about geometry.
///
/// Checked because a capture of a window that resized between the rectangle read and the paint is a frame
/// of something else, and coordinate grounding onto it would be off by the difference.
pub fn geometry_matches(rect: Rect, capture: &Capture) -> bool {
    rect.width() as u32 == capture.width && rect.height() as u32 == capture.height
}

/// A one-line description of a frame that is safe to log: size, backend, brightness, hash, time.
///
/// No pixel and no control name, which is the entire requirement being met by construction.
pub fn log_line(capture: &Capture) -> String {
    format!(
        "capture hwnd={} {}x{} backend={} luma={} uniform={} hash={:016x} dpi={} at={}",
        capture.window.hwnd,
        capture.width,
        capture.height,
        capture.backend.name(),
        capture.mean_luma,
        capture.uniform,
        capture.hash,
        capture
            .render_dpi
            .map(|dpi| dpi.to_string())
            .unwrap_or_else(|| "unknown".to_owned()),
        capture.captured_at_ms
    )
}
