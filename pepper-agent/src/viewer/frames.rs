//! A web page's frames to Pepper without mpv. Each JPEG of Chrome's
//! screencast is decoded by the graphics chip's JPEG decoder (VideoToolbox)
//! and copied into an IOSurface of the frame's own size, which goes to
//! Pepper as pepper-play's frames do, through its Mach client
//! (`pepper-play/src/pepper.rs`, shared). Pepper fits it to the box, its
//! shape kept. Chrome sends frames no bigger than the box, so nothing is
//! scaled here. Where VideoToolbox can't, ImageIO decodes on the CPU.
//! notes/agent.md has the three ways measured (this, ImageIO, mpv fed the
//! same JPEGs).

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::layout;
use crate::pepper::{self, BoxSize};
use crate::surface::Surface;

/// Surfaces per stream: one shown, one waiting, one drawn into.
const RING: usize = 3;
/// How long after Pepper didn't take the surfaces they go again (as
/// pepper-play's ATTACH_AGAIN), not with every frame.
const ATTACH_AGAIN: Duration = Duration::from_secs(1);
/// Pepper's limits for a surface (PepperServer.swift).
const MAX_PIXELS: u64 = 6144 * 3456;
const MAX_SIDE: u32 = 8192;

/// What to do once a frame has been dealt with (true if it was shown).
pub type Done = Box<dyn FnOnce(bool) + Send>;

pub enum FrameMsg {
    /// A JPEG, and what to do once it's been dealt with: Chrome sends the
    /// next frame only after.
    Frame(Vec<u8>, Done),
    /// The box, as Pepper says: nothing is drawn while it isn't shown.
    Size(BoxSize),
    /// Pepper didn't take the surfaces (too many streams): offered again
    /// ATTACH_AGAIN later, as pepper-play does.
    Refused,
    /// Pepper answered after this started: frames go to it from now on.
    Client(Arc<pepper::Client>),
    Stop,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub shown: u64,
    /// Frames replaced by a newer one before they could be drawn.
    pub dropped: u64,
    /// Decoding and copying, in all.
    pub draw_ms: f64,
    /// The graphics chip decodes (VideoToolbox), not the CPU (ImageIO).
    pub hardware: bool,
}

pub struct Presenter {
    tx: Sender<FrameMsg>,
    pub stats: Arc<Mutex<Stats>>,
    thread: Option<JoinHandle<()>>,
}

impl Presenter {
    /// `log`: the viewer's, under --debug.
    pub fn start(
        client: Option<Arc<pepper::Client>>,
        token: String,
        log: Option<PathBuf>,
    ) -> Presenter {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(Mutex::new(Stats::default()));
        let thread = {
            let stats = stats.clone();
            std::thread::spawn(move || run(rx, client, token, stats, log))
        };
        Presenter {
            tx,
            stats,
            thread: Some(thread),
        }
    }

    pub fn sender(&self) -> Sender<FrameMsg> {
        self.tx.clone()
    }

    pub fn send(&self, msg: FrameMsg) {
        let _ = self.tx.send(msg);
    }

    /// Takes the stream away from Pepper and ends the thread.
    pub fn stop(mut self) {
        let _ = self.tx.send(FrameMsg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    rx: Receiver<FrameMsg>,
    mut client: Option<Arc<pepper::Client>>,
    token: String,
    stats: Arc<Mutex<Stats>>,
    log: Option<PathBuf>,
) {
    let note = |line: String| {
        if let Some(log) = &log {
            let stamp = crate::sys::iso_time(crate::sys::now());
            let _ = crate::files::append_line(log, &format!("{stamp} frames: {line}"));
        }
    };
    let mut visible = true;
    let mut ring: Vec<Surface> = Vec::new();
    let mut size = (0, 0);
    let mut next = 0;
    let mut attached = false;
    let mut refused: Option<Instant> = None;
    let mut decoder: Option<Decoder> = None;
    // VideoToolbox couldn't make a decoder: ImageIO from then on.
    let mut no_hardware = false;
    // The last picture, for offering again when Pepper hasn't taken the
    // surfaces (it answered late, or refused): a still page sends no more.
    let mut last: Option<Vec<u8>> = None;
    loop {
        let retry = (client.is_some() && visible && !attached && last.is_some()).then(|| {
            refused.map_or(Duration::ZERO, |when| {
                ATTACH_AGAIN.saturating_sub(when.elapsed())
            })
        });
        let first = match retry {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => return,
            },
        };
        let timed_out = first.is_none();
        let mut frame: Option<(Vec<u8>, Done)> = None;
        for msg in first.into_iter().chain(rx.try_iter()) {
            match msg {
                FrameMsg::Frame(jpeg, done) => {
                    if let Some((_, older)) = frame.replace((jpeg, done)) {
                        older(false);
                        stats.lock().unwrap_or_else(|e| e.into_inner()).dropped += 1;
                    }
                }
                FrameMsg::Size(s) => {
                    note(format!(
                        "Pepper's box {}x{}, shown {}",
                        s.width, s.height, s.visible
                    ));
                    visible = s.visible;
                }
                FrameMsg::Refused => {
                    note("Pepper didn't take the surfaces".into());
                    attached = false;
                    refused = Some(Instant::now());
                    ring.clear();
                }
                FrameMsg::Client(late) => {
                    note("Pepper answered".into());
                    client = Some(late);
                    attached = false;
                    refused = None;
                    ring.clear();
                }
                FrameMsg::Stop => {
                    if let Some((_, done)) = frame {
                        done(false);
                    }
                    // Whatever this side thinks (an attach whose answer
                    // timed out, a refusal of an older one): Pepper drops
                    // a stream it has, and ignores one it hasn't.
                    if let Some(client) = &client {
                        client.detach(&token);
                    }
                    return;
                }
            }
        }
        let (jpeg, done): (Vec<u8>, Done) = match (frame, timed_out.then(|| last.take()).flatten())
        {
            (Some(frame), _) => frame,
            (None, Some(again)) => (again, Box::new(|_| {})),
            (None, None) => continue,
        };
        let mut shown = false;
        if let (Some(client), true, Some(own)) = (&client, visible, jpeg_size(&jpeg)) {
            let target = capped(own);
            let again = !attached && refused.is_none_or(|when| when.elapsed() >= ATTACH_AGAIN);
            if target != size || again {
                ring = (0..RING)
                    .filter_map(|_| Surface::new(target.0, target.1))
                    .collect();
                let made = if ring.len() == RING {
                    client.attach(&token, [&ring[0], &ring[1], &ring[2]])
                } else {
                    Err("surfaces couldn't be made".into())
                };
                note(format!("surfaces {}x{}: {made:?}", target.0, target.1));
                attached = made.is_ok();
                refused = (!attached).then(Instant::now);
                if !attached {
                    ring.clear();
                }
                size = target;
            }
            // One the window server isn't showing; if it says all are (as
            // it does of surfaces just handed over), the next, as
            // pepper-play does.
            let slot = attached.then(|| {
                (0..RING)
                    .map(|k| (next + k) % RING)
                    .find(|&k| !ring[k].in_use())
                    .unwrap_or(next)
            });
            if let Some(slot) = slot {
                let started = Instant::now();
                let mut hardware = false;
                let small = own.0 < HARDWARE_FROM || own.1 < HARDWARE_FROM;
                if target == own && !small && !no_hardware {
                    if decoder.as_ref().is_none_or(|d| d.size != own) {
                        decoder = Decoder::new(own);
                        no_hardware = decoder.is_none();
                    }
                    hardware = decoder
                        .as_mut()
                        .and_then(|d| d.decode(&jpeg))
                        .is_some_and(|picture| picture.copy_into(&ring[slot]));
                }
                let drawn = hardware || draw(&jpeg, &ring[slot]);
                if stats.lock().unwrap_or_else(|e| e.into_inner()).shown == 0 {
                    note(format!(
                        "first frame: drawn {drawn}, by the graphics chip {hardware}"
                    ));
                }
                if drawn && client.present(&token, slot) {
                    shown = true;
                    next = (slot + 1) % RING;
                    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.shown += 1;
                    stats.draw_ms += started.elapsed().as_secs_f64() * 1000.0;
                    stats.hardware = hardware;
                }
            } else {
                stats.lock().unwrap_or_else(|e| e.into_inner()).dropped += 1;
            }
        }
        done(shown);
        last = Some(jpeg);
    }
}

/// A size within Pepper's limits, the same shape.
fn capped((w, h): (u32, u32)) -> (u32, u32) {
    let mut scale = 1.0f64;
    if w.max(h) > MAX_SIDE {
        scale = scale.min(MAX_SIDE as f64 / w.max(h) as f64);
    }
    let pixels = w as u64 * h as u64;
    if pixels > MAX_PIXELS {
        scale = scale.min((MAX_PIXELS as f64 / pixels as f64).sqrt());
    }
    if scale >= 1.0 {
        return (w, h);
    }
    (
        ((w as f64 * scale) as u32).max(1),
        ((h as f64 * scale) as u32).max(1),
    )
}

/// A JPEG's width and height, from its frame header.
pub(super) fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xff {
            return None;
        }
        let marker = jpeg[at + 1];
        if marker == 0xff {
            at += 1;
            continue;
        }
        let length = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        // Start of frame: baseline, extended, progressive, lossless.
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let header = jpeg.get(at + 5..at + 9)?;
            let h = u16::from_be_bytes([header[0], header[1]]) as u32;
            let w = u16::from_be_bytes([header[2], header[3]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        if marker == 0xda || length < 2 {
            return None;
        }
        at += 2 + length;
    }
    None
}

type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct OutputCallback {
    callback: extern "C" fn(*mut c_void, *mut c_void, i32, u32, *mut c_void, CMTime, CMTime),
    refcon: *mut c_void,
}

#[repr(C)]
struct DictionaryCallBacks {
    _private: [u8; 0],
}

const JPEG: u32 = u32::from_be_bytes(*b"jpeg");
/// VideoToolbox's JPEG decoder takes frames at least this big each way:
/// smaller ones (4×2, 64×16, 17×63) trap inside it, on macOS 27, and take
/// the process with them. Smaller frames go to ImageIO.
const HARDWARE_FROM: u32 = 64;
const BGRA: i32 = i32::from_be_bytes(*b"BGRA");
const CF_NUMBER_SINT32: isize = 3;
const LOCK_READ_ONLY: u64 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorNull: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: DictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: DictionaryCallBacks;
    fn CFDictionaryCreate(
        alloc: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const DictionaryCallBacks,
        value_callbacks: *const DictionaryCallBacks,
    ) -> CFTypeRef;
    fn CFNumberCreate(alloc: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreate(
        alloc: CFTypeRef,
        codec: u32,
        width: i32,
        height: i32,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> i32;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: CFTypeRef,
        memory: *mut c_void,
        block_length: usize,
        block_allocator: CFTypeRef,
        custom_source: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut CFTypeRef,
    ) -> i32;
    fn CMSampleBufferCreateReady(
        alloc: CFTypeRef,
        data: CFTypeRef,
        format: CFTypeRef,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes: isize,
        size: *const usize,
        out: *mut CFTypeRef,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        alloc: CFTypeRef,
        format: CFTypeRef,
        decoder: CFTypeRef,
        image_attributes: CFTypeRef,
        callback: *const OutputCallback,
        session: *mut CFTypeRef,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        session: CFTypeRef,
        sample: CFTypeRef,
        flags: u32,
        frame: *mut c_void,
        info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(session: CFTypeRef) -> i32;
    fn VTDecompressionSessionInvalidate(session: CFTypeRef);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFTypeRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    fn CVPixelBufferLockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: CFTypeRef) -> *const u8;
    fn CVPixelBufferGetBytesPerRow(buffer: CFTypeRef) -> usize;
    fn CVPixelBufferGetWidth(buffer: CFTypeRef) -> usize;
    fn CVPixelBufferGetHeight(buffer: CFTypeRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(buffer: CFTypeRef) -> u32;
}

/// VideoToolbox's JPEG decoder, for frames of one size.
struct Decoder {
    session: CFTypeRef,
    format: CFTypeRef,
    size: (u32, u32),
}

/// A decoded frame: VideoToolbox's BGRA buffer.
struct Picture(CFTypeRef);

extern "C" fn decoded(
    _session: *mut c_void,
    frame: *mut c_void,
    status: i32,
    _info: u32,
    image: *mut c_void,
    _pts: CMTime,
    _duration: CMTime,
) {
    if status == 0 && !image.is_null() {
        // SAFETY: `frame` is the slot decode passed for this frame; the
        // image is retained for it.
        unsafe { *(frame as *mut CFTypeRef) = CFRetain(image) };
    }
}

impl Decoder {
    fn new((w, h): (u32, u32)) -> Option<Decoder> {
        if w < HARDWARE_FROM || h < HARDWARE_FROM {
            return None;
        }
        // SAFETY: CoreFoundation, CoreMedia and VideoToolbox calls on what's
        // made here; what isn't kept is released.
        unsafe {
            let mut format = std::ptr::null();
            if CMVideoFormatDescriptionCreate(
                std::ptr::null(),
                JPEG,
                w as i32,
                h as i32,
                std::ptr::null(),
                &mut format,
            ) != 0
            {
                return None;
            }
            let format_type = BGRA;
            let bgra = CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_SINT32,
                (&raw const format_type).cast(),
            );
            let none = CFDictionaryCreate(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            let keys = [
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            ];
            let values = [bgra, none];
            let attributes = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                2,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            );
            CFRelease(bgra);
            CFRelease(none);
            let callback = OutputCallback {
                callback: decoded,
                refcon: std::ptr::null_mut(),
            };
            let mut session = std::ptr::null();
            let made = VTDecompressionSessionCreate(
                std::ptr::null(),
                format,
                std::ptr::null(),
                attributes,
                &callback,
                &mut session,
            );
            CFRelease(attributes);
            if made != 0 || session.is_null() {
                CFRelease(format);
                return None;
            }
            Some(Decoder {
                session,
                format,
                size: (w, h),
            })
        }
    }

    fn decode(&mut self, jpeg: &[u8]) -> Option<Picture> {
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: the block buffer points into `jpeg` without owning it
        // (kCFAllocatorNull) and is gone before this returns, as is the
        // sample; the decode is waited for, so `out` is written (or not)
        // by then.
        unsafe {
            let mut block = std::ptr::null();
            if CMBlockBufferCreateWithMemoryBlock(
                std::ptr::null(),
                jpeg.as_ptr().cast_mut().cast(),
                jpeg.len(),
                kCFAllocatorNull,
                std::ptr::null(),
                0,
                jpeg.len(),
                0,
                &mut block,
            ) != 0
            {
                return None;
            }
            let mut sample = std::ptr::null();
            let size = jpeg.len();
            let made = CMSampleBufferCreateReady(
                std::ptr::null(),
                block,
                self.format,
                1,
                0,
                std::ptr::null(),
                1,
                &size,
                &mut sample,
            );
            CFRelease(block);
            if made != 0 {
                return None;
            }
            let mut info = 0;
            let status = VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                0,
                (&raw mut out).cast(),
                &mut info,
            );
            VTDecompressionSessionWaitForAsynchronousFrames(self.session);
            CFRelease(sample);
            if status != 0 && !out.is_null() {
                CFRelease(out);
                return None;
            }
        }
        (!out.is_null()).then_some(Picture(out))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: ours, from new.
        unsafe {
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            CFRelease(self.format);
        }
    }
}

impl Picture {
    /// Copies it into `surface`, which is its size: row by row, no scaling.
    fn copy_into(&self, surface: &Surface) -> bool {
        // SAFETY: both are locked while their memory is read and written,
        // each row within both's row lengths and heights.
        unsafe {
            let buffer = self.0;
            let (w, h) = (
                CVPixelBufferGetWidth(buffer),
                CVPixelBufferGetHeight(buffer),
            );
            if (w, h) != (surface.width as usize, surface.height as usize)
                || CVPixelBufferGetPixelFormatType(buffer) != BGRA as u32
                || CVPixelBufferLockBaseAddress(buffer, LOCK_READ_ONLY) != 0
            {
                return false;
            }
            let raw = surface.raw();
            let mut copied = false;
            if IOSurfaceLock(raw, 0, std::ptr::null_mut()) == 0 {
                let (from, from_row) = (
                    CVPixelBufferGetBaseAddress(buffer),
                    CVPixelBufferGetBytesPerRow(buffer),
                );
                let (to, to_row) = (
                    IOSurfaceGetBaseAddress(raw).cast::<u8>(),
                    IOSurfaceGetBytesPerRow(raw),
                );
                let length = (w * 4).min(from_row).min(to_row);
                if !from.is_null() && !to.is_null() {
                    if from_row == to_row {
                        std::ptr::copy_nonoverlapping(from, to, from_row * h);
                    } else {
                        for y in 0..h {
                            std::ptr::copy_nonoverlapping(
                                from.add(y * from_row),
                                to.add(y * to_row),
                                length,
                            );
                        }
                    }
                    copied = true;
                }
                IOSurfaceUnlock(raw, 0, std::ptr::null_mut());
            }
            CVPixelBufferUnlockBaseAddress(buffer, LOCK_READ_ONLY);
            copied
        }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: retained for us in the callback.
        unsafe { CFRelease(self.0) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

const ALPHA_PREMULTIPLIED_FIRST: u32 = 2;
const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
const INTERPOLATION_MEDIUM: i32 = 4;

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: CFTypeRef, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceCreateImageAtIndex(
        source: CFTypeRef,
        index: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: CFTypeRef;
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> CFTypeRef;
    fn CGColorSpaceRelease(space: CFTypeRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        info: u32,
    ) -> CFTypeRef;
    fn CGContextRelease(context: CFTypeRef);
    fn CGContextDrawImage(context: CFTypeRef, rect: CGRect, image: CFTypeRef);
    fn CGContextSetRGBFillColor(context: CFTypeRef, r: f64, g: f64, b: f64, a: f64);
    fn CGContextFillRect(context: CFTypeRef, rect: CGRect);
    fn CGContextSetInterpolationQuality(context: CFTypeRef, quality: i32);
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGImageRelease(image: CFTypeRef);
}

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
}

/// Decodes `jpeg` on the CPU (ImageIO) into `surface`, its shape kept and
/// centred, black around: for what VideoToolbox doesn't take.
fn draw(jpeg: &[u8], surface: &Surface) -> bool {
    let (w, h) = (surface.width as usize, surface.height as usize);
    // SAFETY: CoreFoundation, ImageIO and CoreGraphics calls on objects made
    // here and released here; the bitmap context draws into the surface's
    // memory, locked for the time, of the size and row length it says.
    unsafe {
        let data = CFDataCreate(std::ptr::null(), jpeg.as_ptr(), jpeg.len() as isize);
        if data.is_null() {
            return false;
        }
        let source = CGImageSourceCreateWithData(data, std::ptr::null());
        CFRelease(data);
        if source.is_null() {
            return false;
        }
        let image = CGImageSourceCreateImageAtIndex(source, 0, std::ptr::null());
        CFRelease(source);
        if image.is_null() {
            return false;
        }
        let (iw, ih) = (CGImageGetWidth(image), CGImageGetHeight(image));
        let raw = surface.raw();
        let mut drawn = false;
        if iw > 0 && ih > 0 && IOSurfaceLock(raw, 0, std::ptr::null_mut()) == 0 {
            let base = IOSurfaceGetBaseAddress(raw);
            let row = IOSurfaceGetBytesPerRow(raw);
            let space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
            let context = CGBitmapContextCreate(
                base,
                w,
                h,
                8,
                row,
                space,
                ALPHA_PREMULTIPLIED_FIRST | BYTE_ORDER_32_LITTLE,
            );
            CGColorSpaceRelease(space);
            if !context.is_null() {
                let (x, y, cw, ch) = layout::content(w as f64, h as f64, iw as f64 / ih as f64);
                if cw < w as f64 || ch < h as f64 {
                    CGContextSetRGBFillColor(context, 0.0, 0.0, 0.0, 1.0);
                    CGContextFillRect(
                        context,
                        CGRect {
                            x: 0.0,
                            y: 0.0,
                            width: w as f64,
                            height: h as f64,
                        },
                    );
                }
                CGContextSetInterpolationQuality(context, INTERPOLATION_MEDIUM);
                // CoreGraphics counts from the bottom.
                let rect = CGRect {
                    x,
                    y: h as f64 - y - ch,
                    width: cw,
                    height: ch,
                };
                CGContextDrawImage(context, rect, image);
                CGContextRelease(context);
                drawn = true;
            }
            IOSurfaceUnlock(raw, 0, std::ptr::null_mut());
        }
        CGImageRelease(image);
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4×2 JPEG, red: ffmpeg -f lavfi -i color=red:s=4x2 -frames:v 1 -f mjpeg -
    fn red() -> Vec<u8> {
        crate::cdp::base64(
            b"/9j/4AAQSkZJRgABAgAAAQABAAD//gAPTGF2YzYzLjEuMTAyAP/bAEMACAQEBAQEBQUFBQUFBgYGBgYGBgYGBgYGBgcHBwgICAcHBwYGBwcICAgICQkJCAgICAkJCgoKDAwLCw4ODhERFP/EAEwAAQEAAAAAAAAAAAAAAAAAAAAGAQEBAAAAAAAAAAAAAAAAAAAGBxABAAAAAAAAAAAAAAAAAAAAABEBAAAAAAAAAAAAAAAAAAAAAP/AABEIAAIABAMBIgACEQADEQD/2gAMAwEAAhEDEQA/AIsATX9//9k=",
        )
        .unwrap()
    }

    #[test]
    fn sizes_stay_in_peppers_limits() {
        assert_eq!(capped((2880, 1610)), (2880, 1610));
        let (w, h) = capped((10000, 5000));
        assert!(w <= MAX_SIDE && (w as u64 * h as u64) <= MAX_PIXELS);
        assert!(((w as f64 / h as f64) - 2.0).abs() < 0.01);
    }

    #[test]
    fn jpeg_sizes() {
        assert_eq!(jpeg_size(&red()), Some((4, 2)));
        assert_eq!(jpeg_size(b"not a jpeg"), None);
        assert_eq!(jpeg_size(&[0xff, 0xd8, 0xff]), None);
        assert_eq!(jpeg_size(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x00]), None);
    }

    #[test]
    fn a_jpeg_is_drawn_into_a_surface() {
        let surface = Surface::new(8, 8).unwrap();
        assert!(draw(&red(), &surface));
        assert!(!draw(b"not a jpeg", &surface));
    }

    #[test]
    fn the_graphics_chip_decodes_it() {
        // 64×64, blue: ffmpeg -f lavfi -i color=0x2050c0:s=64x64 -frames:v 1 -q:v 31 -f mjpeg -
        let blue = crate::cdp::base64(b"/9j/4AAQSkZJRgABAgAAAQABAAD//gAPTGF2YzYzLjEuMTAyAP/bAEMACD4+ST5JVVVVVVVVZF1kaGhoZGRkZGhoaHBwcIODg3BwcGhocHB8fIODj5OPh4eDh5OTm5uburqystnZ4P/////EAEwAAQEAAAAAAAAAAAAAAAAAAAAGAQEBAAAAAAAAAAAAAAAAAAAABhABAAAAAAAAAAAAAAAAAAAAABEBAAAAAAAAAAAAAAAAAAAAAP/AABEIAEAAQAMBIgACEQADEQD/2gAMAwEAAhEDEQA/AI0Bfp0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAB//Z").unwrap();
        assert_eq!(jpeg_size(&blue), Some((64, 64)));
        let surface = Surface::new(64, 64).unwrap();
        let mut decoder = Decoder::new((64, 64)).expect("VideoToolbox decodes JPEG here");
        let picture = decoder.decode(&blue).expect("a picture");
        assert!(picture.copy_into(&surface));
        assert!(!picture.copy_into(&Surface::new(8, 8).unwrap()));
        // Too small for it: never tried.
        assert!(Decoder::new((4, 2)).is_none());
        assert!(Decoder::new((64, 16)).is_none());
    }

    /// How long a frame takes to decode and get into a surface, each way:
    /// measured against mpv's in notes/agent.md.
    #[test]
    #[ignore = "a measurement: PEPPER_BENCH_JPEGS=<a folder of JPEGs> cargo test --release draw_speed -- --ignored --nocapture"]
    fn draw_speed() {
        let dir = std::env::var("PEPPER_BENCH_JPEGS").expect("PEPPER_BENCH_JPEGS");
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        files.sort();
        let frames: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(f).unwrap()).collect();
        let size = jpeg_size(&frames[0]).unwrap();
        let surface = Surface::new(size.0, size.1).unwrap();
        let mut decoder = Decoder::new(size).unwrap();
        let report = |name: &str, mut times: Vec<f64>| {
            times.sort_by(f64::total_cmp);
            let mean = times.iter().sum::<f64>() / times.len() as f64;
            println!(
                "{name} {}x{}: mean {mean:.2} ms, median {:.2}, p95 {:.2}, max {:.2} ({} frames)",
                size.0,
                size.1,
                times[times.len() / 2],
                times[times.len() * 95 / 100],
                times[times.len() - 1],
                times.len()
            );
        };
        let time = |f: &mut dyn FnMut(&[u8]) -> bool| -> Vec<f64> {
            frames
                .iter()
                .map(|jpeg| {
                    let started = Instant::now();
                    assert!(f(jpeg));
                    started.elapsed().as_secs_f64() * 1000.0
                })
                .collect()
        };
        report(
            "VideoToolbox",
            time(&mut |jpeg| decoder.decode(jpeg).is_some_and(|p| p.copy_into(&surface))),
        );
        report("ImageIO", time(&mut |jpeg| draw(jpeg, &surface)));
    }
}
