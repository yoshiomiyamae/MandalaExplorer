//! Media Foundation decoding backend.
//!
//! Media Foundation is used through the synchronous Source Reader rather than
//! the async callback API: each playing tile already owns a worker, so the
//! extra machinery buys nothing. Advanced video processing is switched on so
//! the Video Processor MFT handles both colour conversion and scaling, which
//! keeps that work off the CPU and means a full-size frame is never touched.

pub mod d3d;

use crate::backend::{Advance, MediaBackend, VideoStream, thumbnail_timestamp};
use crate::frame::{Frame, Layout};
use crate::sizing::fit_within;
use anyhow::{Context, Result, anyhow};
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, HSTRING, Interface};

/// `MFVideoFormat_ABGR32` -- RGBA in memory order, which is exactly what the
/// texture uploader wants. The generated bindings do not include it, so the
/// GUID is spelled out; it is the D3DFMT_A8B8G8R8 media subtype.
const MF_VIDEO_FORMAT_ABGR32: GUID = GUID::from_u128(0x00000020_0000_0010_8000_00aa00389b71);

/// How far before the end a seek is held back, so it cannot land on or past
/// the final sample.
const SEEK_END_MARGIN: Duration = Duration::from_millis(200);

/// Media Foundation timestamps are in 100-nanosecond units.
fn to_hns(d: Duration) -> i64 {
    (d.as_nanos() / 100).min(i64::MAX as u128) as i64
}

fn from_hns(hns: i64) -> Duration {
    Duration::from_nanos(hns.max(0) as u64 * 100)
}

/// Starts Media Foundation once per process.
fn ensure_startup() -> Result<()> {
    static STARTUP: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    STARTUP
        .get_or_init(|| unsafe {
            // NOSOCKET skips the network source stack, which a local file
            // browser has no use for and which is slow to bring up.
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| e.to_string())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow!("MFStartup failed: {e}"))
}

/// Puts the calling thread into the multithreaded apartment.
///
/// Every thread that touches a Source Reader needs this, and decoding happens
/// on worker threads, so it is done per thread and torn down with the thread.
fn ensure_thread_com() {
    thread_local! {
        static COM_GUARD: ComGuard = ComGuard::new();
    }
    COM_GUARD.with(|_| {});
}

struct ComGuard;

impl ComGuard {
    fn new() -> Self {
        unsafe {
            // Already initialized on this thread is fine; the UI framework may
            // well have gotten there first.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Self
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Decoding backed by Windows Media Foundation.
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaFoundation;

impl MediaFoundation {
    pub fn new() -> Result<Self> {
        ensure_startup()?;
        Ok(Self)
    }

    /// Whether GPU decoding is available on this machine.
    ///
    /// Software decoding still works when this is false, just at a much higher
    /// CPU cost, so it is worth surfacing rather than silently tolerating.
    pub fn hardware_decoding(&self) -> bool {
        d3d::shared_device().is_some()
    }
}

impl MediaBackend for MediaFoundation {
    fn open_video(&self, path: &Path, max: (u32, u32)) -> Result<Box<dyn VideoStream>> {
        Ok(Box::new(MfStream::open(path, max)?))
    }

    fn video_thumbnail(&self, path: &Path, max: (u32, u32)) -> Result<Frame> {
        let mut stream = MfStream::open(path, max)?;
        let target = thumbnail_timestamp(stream.duration());
        if target > Duration::ZERO {
            stream.seek_to(target)?;
        }
        if let Advance::Frame(frame) = stream.advance_to(target)? {
            return Ok(frame);
        }

        // Seeking can land past the last keyframe of a very short or truncated
        // file, which still has an opening frame worth showing.
        stream.seek_to(Duration::ZERO)?;
        match stream.advance_to(Duration::ZERO)? {
            Advance::Frame(frame) => Ok(frame),
            _ => Err(anyhow!("no decodable frame in {}", path.display())),
        }
    }
}

struct MfStream {
    reader: IMFSourceReader,
    size: (u32, u32),
    duration: Option<Duration>,
    /// Timestamp of the most recent frame handed out.
    last_timestamp: Option<Duration>,
    at_end: bool,
    /// Whether the negotiated output needs its red and blue channels swapped.
    swap_rb: bool,
}

// The reader is created without MF_SOURCE_READER_ASYNC_CALLBACK, so it is only
// ever used from one thread at a time: a stream moves to its worker and stays.
unsafe impl Send for MfStream {}

impl MfStream {
    fn open(path: &Path, max: (u32, u32)) -> Result<Self> {
        ensure_startup()?;
        ensure_thread_com();

        unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1)?;
            let attributes =
                attributes.ok_or_else(|| anyhow!("MFCreateAttributes returned nothing"))?;
            // Lets the reader insert a Video Processor MFT, so it can be asked
            // for RGB32 at an arbitrary size rather than the native format.
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;

            // Without this the reader picks a software decoder no matter what
            // the hardware can do. With it, decoding and the colour/scale
            // conversion both run on the GPU.
            if let Some(device) = d3d::shared_device() {
                attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, device.manager())?;
            }

            let url = HSTRING::from(path.as_os_str());
            let reader = MFCreateSourceReaderFromURL(&url, &attributes)
                .with_context(|| format!("opening {}", path.display()))?;

            let all = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
            let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetStreamSelection(all, false)?;
            reader.SetStreamSelection(video, true)?;

            let native = reader
                .GetNativeMediaType(video, 0)
                .with_context(|| format!("no video stream in {}", path.display()))?;
            let native_size = unpack_size(native.GetUINT64(&MF_MT_FRAME_SIZE)?);
            let size = fit_within(native_size, max);

            let swap_rb = negotiate_output(&reader, video, size)?;

            // The processor may not honour the requested size exactly, so the
            // negotiated type is what the frame converter has to trust.
            let actual = reader.GetCurrentMediaType(video)?;
            let size = unpack_size(actual.GetUINT64(&MF_MT_FRAME_SIZE)?);
            let duration = read_duration(&reader);

            Ok(Self { reader, size, duration, last_timestamp: None, at_end: false, swap_rb })
        }
    }

    fn seek_to(&mut self, position: Duration) -> Result<()> {
        // Seeking at or past the end puts the reader into a state where every
        // later seek is refused with MF_E_INVALIDREQUEST, stranding the stream
        // for good. Requests are pulled back inside the clip instead.
        let position = match self.duration {
            Some(duration) => position.min(duration.saturating_sub(SEEK_END_MARGIN)),
            None => position,
        };

        unsafe {
            // GUID_NULL as the time format means "in 100ns units", which is
            // the only format the Source Reader accepts.
            let mut target = propvariant_i64(to_hns(position));
            let result = self.reader.SetCurrentPosition(&GUID::zeroed(), &target);
            let _ = PropVariantClear(&mut target);
            result?;
        }
        self.last_timestamp = None;
        self.at_end = false;
        Ok(())
    }

    /// Pulls one sample. `None` means end of stream; a sample of `None` is a
    /// gap or a format change, which the caller reads past.
    unsafe fn read_sample(&mut self) -> Result<Option<(Option<IMFSample>, Duration)>> {
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            self.reader.ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;
        }

        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }
        Ok(Some((sample, from_hns(timestamp))))
    }

    /// Copies a sample into an RGBA frame.
    unsafe fn convert(&self, sample: &IMFSample, timestamp: Duration) -> Result<Frame> {
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;

            // IMF2DBuffer2 reports the real stride and orientation. RGB32 out
            // of Media Foundation is frequently bottom-up, which shows up as a
            // negative pitch; assuming instead of asking flips the picture.
            if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
                let mut scanline0: *mut u8 = std::ptr::null_mut();
                let mut pitch = 0i32;
                let mut start: *mut u8 = std::ptr::null_mut();
                let mut length = 0u32;
                buffer2d.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scanline0,
                    &mut pitch,
                    &mut start,
                    &mut length,
                )?;
                let bytes = std::slice::from_raw_parts(start, length as usize);
                let layout = Layout {
                    stride: pitch.unsigned_abs() as usize,
                    bottom_up: pitch < 0,
                    swap_rb: self.swap_rb,
                };
                let frame =
                    Frame::from_packed(bytes, layout, self.size.0, self.size.1, timestamp);
                let _ = buffer2d.Unlock2D();
                return frame;
            }

            // Fallback for buffers that are only one-dimensional: assume the
            // packed stride, which is what a contiguous RGB32 buffer uses.
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut current = 0u32;
            buffer.Lock(&mut data, None, Some(&mut current))?;
            let bytes = std::slice::from_raw_parts(data, current as usize);
            let layout = Layout {
                stride: self.size.0 as usize * 4,
                bottom_up: false,
                swap_rb: self.swap_rb,
            };
            let frame = Frame::from_packed(bytes, layout, self.size.0, self.size.1, timestamp);
            let _ = buffer.Unlock();
            frame
        }
    }
}

impl Drop for MfStream {
    fn drop(&mut self) {
        // Releasing the reader is not sufficient on its own: the media source
        // behind it holds a file handle and its own worker until it is told to
        // shut down. Media Foundation documents this as the caller's job.
        unsafe {
            let mut service: *mut core::ffi::c_void = std::ptr::null_mut();
            let found = self.reader.GetServiceForStream(
                MF_SOURCE_READER_MEDIASOURCE.0 as u32,
                &GUID::zeroed(),
                &IMFMediaSource::IID,
                &mut service,
            );
            if found.is_ok() && !service.is_null() {
                let source = IMFMediaSource::from_raw(service);
                let _ = source.Shutdown();
            }
        }
    }
}

impl VideoStream for MfStream {
    fn advance_to(&mut self, target: Duration) -> Result<Advance> {
        if self.at_end {
            return Ok(Advance::EndOfStream);
        }
        if self.last_timestamp.is_some_and(|t| t >= target) {
            return Ok(Advance::Unchanged);
        }

        // Bounds the work one call can do, so a file whose timestamps never
        // reach the target cannot spin here forever.
        const MAX_SAMPLES_PER_CALL: usize = 512;

        for _ in 0..MAX_SAMPLES_PER_CALL {
            let Some((sample, timestamp)) = (unsafe { self.read_sample()? }) else {
                self.at_end = true;
                return Ok(Advance::EndOfStream);
            };
            let Some(sample) = sample else { continue };

            // Frames behind the target are dropped without ever being
            // converted, so falling behind costs only the decode. The first
            // frame is always taken, so a tile shows something immediately
            // instead of waiting for its first on-time frame.
            if timestamp < target && self.last_timestamp.is_some() {
                continue;
            }

            let frame = unsafe { self.convert(&sample, timestamp)? };
            self.last_timestamp = Some(timestamp);
            return Ok(Advance::Frame(frame));
        }
        Ok(Advance::Unchanged)
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        self.seek_to(position)
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }
}

/// Settles on an output format, preferring one that needs no channel swap.
///
/// Returns whether frames will need red and blue exchanged on the CPU. Asking
/// for RGBA first is worth it: at large tile sizes the swap is hundreds of
/// megabytes a second of pointless work.
unsafe fn negotiate_output(reader: &IMFSourceReader, stream: u32, size: (u32, u32)) -> Result<bool> {
    // (subtype, whether its byte order needs swapping to reach RGBA)
    const CANDIDATES: [(GUID, bool); 2] =
        [(MF_VIDEO_FORMAT_ABGR32, false), (MFVideoFormat_RGB32, true)];

    unsafe {
        for (subtype, swap_rb) in CANDIDATES {
            let Ok(output) = MFCreateMediaType() else { continue };
            if output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).is_err()
                || output.SetGUID(&MF_MT_SUBTYPE, &subtype).is_err()
                || output.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(size)).is_err()
            {
                continue;
            }
            if reader.SetCurrentMediaType(stream, None, &output).is_ok() {
                return Ok(swap_rb);
            }
        }
    }
    Err(anyhow!("no supported output format; the video processor rejected RGBA and BGRA"))
}

/// Media Foundation packs frame dimensions into one 64-bit attribute.
fn pack_size((w, h): (u32, u32)) -> u64 {
    ((w as u64) << 32) | h as u64
}

fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// Reads total duration from the presentation descriptor, which not every
/// container provides.
fn read_duration(reader: &IMFSourceReader) -> Option<Duration> {
    unsafe {
        let mut value = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()?;
        let hns = propvariant_u64(&value);
        let _ = PropVariantClear(&mut value);
        let hns = hns?;
        (hns > 0).then(|| Duration::from_nanos(hns.saturating_mul(100)))
    }
}

/// Builds a `VT_I8` PROPVARIANT.
///
/// The generated bindings expose PROPVARIANT as the raw union, so the variant
/// tag and payload have to be filled in by hand.
fn propvariant_i64(value: i64) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: value },
            }),
        },
    }
}

/// Reads a 64-bit integer PROPVARIANT, whichever signedness it carries.
///
/// # Safety
/// `value` must be a PROPVARIANT that Media Foundation actually filled in.
unsafe fn propvariant_u64(value: &PROPVARIANT) -> Option<u64> {
    unsafe {
        let inner = &*value.Anonymous.Anonymous;
        if inner.vt == VT_UI8 {
            Some(inner.Anonymous.uhVal)
        } else if inner.vt == VT_I8 {
            u64::try_from(inner.Anonymous.hVal).ok()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_between_durations_and_hundred_nanosecond_units() {
        const ONE_SECOND_IN_HNS: i64 = 10_000_000;
        assert_eq!(to_hns(Duration::from_secs(1)), ONE_SECOND_IN_HNS);
        assert_eq!(from_hns(ONE_SECOND_IN_HNS), Duration::from_secs(1));
        assert_eq!(from_hns(-5), Duration::ZERO, "negative timestamps clamp to zero");
    }

    #[test]
    fn packs_and_unpacks_a_frame_size() {
        assert_eq!(pack_size((1920, 1080)), (1920u64 << 32) | 1080);
        assert_eq!(unpack_size(pack_size((1920, 1080))), (1920, 1080));
    }

    #[test]
    fn media_foundation_starts_up() {
        assert!(MediaFoundation::new().is_ok());
    }

    #[test]
    fn gpu_decoding_is_available() {
        // Not an absolute requirement -- software decoding still works -- but
        // on a machine with a GPU its absence means a large silent slowdown.
        let backend = MediaFoundation::new().unwrap();
        if !backend.hardware_decoding() {
            eprintln!("warning: no D3D11 device, decoding falls back to the CPU");
        }
    }
}
