//! A macOS display through ScreenCaptureKit (macOS 12.3 and later).
//!
//! Frames arrive on a queue of their own, each with the regions that changed
//! since the one before, in pixels. The newest frame is kept and the regions
//! gathered until the next capture, which reads only those regions out of it
//! into the picture kept from before, like DAMAGE on X11. Moved regions come
//! as changes: ScreenCaptureKit does not say where they came from.
use crate::Result;
use aad_platform::buffer::Target;
use aad_platform::damage::{Damage, Rect};
use aad_platform::{PixelFormat, Raw};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{CFArray, CFRetained};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSRect, NSString, NSValue};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoDirtyRects, SCStreamFrameInfoStatus, SCStreamOutput,
    SCStreamOutputType,
};
use std::ffi::c_void;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::Duration;

/// More regions than this between two captures are read as one whole frame.
const MOST_REGIONS: usize = 512;
const WAIT: Duration = Duration::from_secs(10);

/// What the capture queue hands over to the next capture.
#[derive(Default)]
struct Latest {
    frame: Option<CFRetained<CVPixelBuffer>>,
    /// Every region changed since the last capture.
    changed: Vec<Rect>,
    /// A frame said nothing of what changed: read it whole.
    whole: bool,
    stopped: Option<String>,
}

// SAFETY: a pixel buffer may be handed between threads (CoreVideo retains
// and releases atomically), and it is read only under the lock.
unsafe impl Send for Latest {}

fn lock(latest: &Mutex<Latest>) -> MutexGuard<'_, Latest> {
    latest.lock().unwrap_or_else(|e| e.into_inner())
}

define_class!(
    // SAFETY: NSObject has no rules for subclasses, and Output has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "AadAssistStreamOutput"]
    #[ivars = Arc<Mutex<Latest>>]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.take(buffer);
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            lock(self.ivars()).stopped = Some(describe(error));
        }
    }
);

impl Output {
    fn new(latest: Arc<Mutex<Latest>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(latest);
        unsafe { msg_send![super(this), init] }
    }

    /// Keeps a complete frame and what it changed. Idle frames (nothing
    /// changed) carry no picture and are passed over.
    fn take(&self, buffer: &CMSampleBuffer) {
        let Some(info) = frame_info(buffer) else {
            return;
        };
        // SAFETY: the keys exist wherever SCStream does.
        let status = unsafe { info.objectForKey(SCStreamFrameInfoStatus) }
            .and_then(|s| s.downcast::<NSNumber>().ok())
            .map(|s| s.integerValue());
        if status != Some(SCFrameStatus::Complete.0) {
            return;
        }
        let Some(image) = (unsafe { buffer.image_buffer() }) else {
            return;
        };
        let dirty = unsafe { info.objectForKey(SCStreamFrameInfoDirtyRects) }
            .and_then(|d| d.downcast::<NSArray>().ok());
        let mut latest = lock(self.ivars());
        latest.frame = Some(image);
        let before = latest.changed.len();
        match dirty {
            Some(dirty) => {
                for value in dirty.iter() {
                    match rect_of(&value) {
                        Some(rect) => latest.changed.extend(pixels(rect)),
                        // Not a rect as documented: read the frame whole
                        // rather than miss what changed.
                        None => {
                            trace(format_args!(
                                "dirty rect of class {:?}",
                                value.class().name()
                            ));
                            latest.whole = true;
                        }
                    }
                }
                if latest.changed.len() > MOST_REGIONS {
                    latest.whole = true;
                    latest.changed.clear();
                }
            }
            None => latest.whole = true,
        }
        trace(format_args!(
            "frame: {} dirty rects, whole {}",
            latest.changed.len().saturating_sub(before),
            latest.whole
        ));
    }
}

/// A dirty rect: a CGRect in an NSValue, as documented, or a CGRect's
/// dictionary form.
fn rect_of(value: &AnyObject) -> Option<NSRect> {
    if let Some(value) = value.downcast_ref::<NSValue>() {
        // SAFETY: rectValue is asked only of a value holding a CGRect.
        let kind = unsafe { std::ffi::CStr::from_ptr(value.objCType().as_ptr()) }.to_bytes();
        let rect = kind.starts_with(b"{CGRect") || kind.starts_with(b"{_NSRect");
        return rect.then(|| unsafe { value.rectValue() });
    }
    if value.downcast_ref::<NSDictionary>().is_some() {
        let mut rect = NSRect::default();
        let read = unsafe {
            CGRectMakeWithDictionaryRepresentation((value as *const AnyObject).cast(), &mut rect)
        };
        return read.then_some(rect);
    }
    None
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: *const c_void, rect: *mut NSRect) -> bool;
}

/// With AAD_ASSIST_TRACE set, what ScreenCaptureKit hands over goes to
/// stderr, to tell a capture that misses changes from one that gets none.
fn trace(what: std::fmt::Arguments<'_>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("AAD_ASSIST_TRACE").is_some()) {
        eprintln!("assist: screencapturekit {what}");
    }
}

/// The first sample's attachments: its status and dirty rects.
fn frame_info(buffer: &CMSampleBuffer) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    let array: CFRetained<CFArray> = unsafe { buffer.sample_attachments_array(false) }?;
    // SAFETY: CFArray and NSArray are the same object (toll-free bridged),
    // and the attachments are dictionaries keyed by strings.
    let array: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(CFRetained::as_ptr(&array).as_ptr() as *const _) };
    array.firstObject()
}

/// A rect in pixels, widened to whole pixels.
fn pixels(rect: NSRect) -> Option<Rect> {
    let left = rect.origin.x.floor().max(0.0);
    let top = rect.origin.y.floor().max(0.0);
    let right = (rect.origin.x + rect.size.width).ceil();
    let bottom = (rect.origin.y + rect.size.height).ceil();
    (right > left && bottom > top).then_some(Rect {
        x: left as u32,
        y: top as u32,
        w: (right - left) as u32,
        h: (bottom - top) as u32,
    })
}

fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

/// A finished frame stream from one display.
pub struct Stream {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    latest: Arc<Mutex<Latest>>,
    size: (u32, u32),
    frame: Option<Arc<Vec<u8>>>,
    spare: Option<Arc<Vec<u8>>>,
    stale: Vec<Rect>,
    refresh: bool,
}

impl Stream {
    /// Whether this macOS has ScreenCaptureKit (12.3 and later).
    pub fn available() -> bool {
        AnyClass::get(c"SCStream").is_some()
    }

    /// Streams a display at `width` x `height` pixels, at most 60 frames a
    /// second and only when something changed.
    pub fn open(display: u32, width: u32, height: u32) -> Result<Self> {
        let shared = shareable_display(display)?;
        let latest = Arc::new(Mutex::new(Latest::default()));
        let output = Output::new(latest.clone());
        let queue = DispatchQueue::new("aad.assist.capture", None);
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &shared,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            // The pointer is sent apart and drawn by the viewer.
            config.setShowsCursor(false);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setQueueDepth(4);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| describe(&e))?;
            stream
        };
        let (sent, started) = mpsc::channel();
        let done = RcBlock::new(move |error: *mut NSError| {
            let _ = sent.send(unsafe { error.as_ref() }.map(describe));
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
        match started.recv_timeout(WAIT) {
            Ok(None) => {}
            Ok(Some(error)) => return Err(format!("ScreenCaptureKit did not start: {error}")),
            Err(_) => return Err("ScreenCaptureKit did not start in time".into()),
        }
        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
            latest,
            size: (width, height),
            frame: None,
            spare: None,
            stale: Vec::new(),
            refresh: false,
        })
    }

    /// The next capture repeats the picture even when nothing changed.
    pub fn refresh(&mut self) {
        self.refresh = true;
    }

    /// The picture, if it changed (or a repeat was asked for), with what
    /// changed; `None` damage when the whole picture is new.
    pub fn capture(&mut self) -> Result<Option<Raw>> {
        let (image, changed, whole) = {
            let mut latest = lock(&self.latest);
            if let Some(why) = &latest.stopped {
                return Err(format!("ScreenCaptureKit stopped: {why}"));
            }
            (
                latest.frame.take(),
                std::mem::take(&mut latest.changed),
                std::mem::take(&mut latest.whole),
            )
        };
        let Some(image) = image else {
            return Ok(self.repeat());
        };
        if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA {
            return Err("ScreenCaptureKit sent pixels that are not BGRA".into());
        }
        let size = (
            CVPixelBufferGetWidth(&image) as u32,
            CVPixelBufferGetHeight(&image) as u32,
        );
        if size != self.size {
            self.size = size;
            self.frame = None;
            self.spare = None;
            self.stale.clear();
        }
        let new = self.frame.is_none() || whole;
        let rects: Vec<Rect> = if new {
            vec![Rect {
                x: 0,
                y: 0,
                w: size.0,
                h: size.1,
            }]
        } else {
            changed
                .into_iter()
                .filter_map(|r| r.clip(size.0, size.1))
                .collect()
        };
        if rects.is_empty() {
            return Ok(self.repeat());
        }
        let read_only = CVPixelBufferLockFlags::ReadOnly;
        if unsafe { CVPixelBufferLockBaseAddress(&image, read_only) } != 0 {
            return Err("the frame's pixels could not be read".into());
        }
        let stride = CVPixelBufferGetBytesPerRow(&image);
        let base = CVPixelBufferGetBaseAddress(&image) as *const u8;
        let width = size.0 as usize;
        if !base.is_null() && stride >= width * 4 {
            // SAFETY: the buffer is locked and holds `height` rows of `stride`.
            let bytes = unsafe { std::slice::from_raw_parts(base, stride * size.1 as usize) };
            let mut target = Target {
                frame: &mut self.frame,
                spare: &mut self.spare,
                stale: &mut self.stale,
                width,
                height: size.1 as usize,
                ready: false,
            };
            let pixels = target.pixels();
            for r in &rects {
                for y in r.y as usize..(r.y + r.h) as usize {
                    let from = &bytes[y * stride + r.x as usize * 4..][..r.w as usize * 4];
                    let to = (y * width + r.x as usize) * 4;
                    pixels[to..to + from.len()].copy_from_slice(from);
                    for alpha in pixels[to + 3..to + from.len()].iter_mut().step_by(4) {
                        *alpha = 255;
                    }
                }
            }
        }
        unsafe { CVPixelBufferUnlockBaseAddress(&image, read_only) };
        if base.is_null() || stride < width * 4 {
            return Err("the frame's pixels could not be read".into());
        }
        self.stale = rects.clone();
        self.refresh = false;
        Ok(Some(Raw {
            pixels: self.frame.clone().unwrap_or_default(),
            width: size.0,
            height: size.1,
            format: PixelFormat::Bgra,
            damage: (!new).then(|| Damage {
                moves: Vec::new(),
                dirty: rects,
            }),
        }))
    }

    fn repeat(&mut self) -> Option<Raw> {
        let again = std::mem::take(&mut self.refresh);
        self.frame.clone().filter(|_| again).map(|pixels| Raw {
            pixels,
            width: self.size.0,
            height: self.size.1,
            format: PixelFormat::Bgra,
            damage: Some(Damage::default()),
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// The display as ScreenCaptureKit offers it. Fails without the Screen
/// Recording permission.
fn shareable_display(display: u32) -> Result<Retained<SCDisplay>> {
    struct Found(Result<Retained<SCDisplay>>);
    // SAFETY: an SCDisplay only describes a display; it is read, not changed.
    unsafe impl Send for Found {}
    let (sent, found) = mpsc::channel();
    let done = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match (unsafe { content.as_ref() }, unsafe { error.as_ref() }) {
                (Some(content), _) => unsafe { content.displays() }
                    .iter()
                    .find(|d| unsafe { d.displayID() } == display)
                    .ok_or_else(|| format!("ScreenCaptureKit does not offer display {display}")),
                (None, Some(error)) => Err(describe(error)),
                (None, None) => Err("ScreenCaptureKit listed nothing to share".into()),
            };
            let _ = sent.send(Found(result));
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&done) };
    found
        .recv_timeout(WAIT)
        .map_err(|_| "ScreenCaptureKit did not list the displays in time".to_string())?
        .0
}
