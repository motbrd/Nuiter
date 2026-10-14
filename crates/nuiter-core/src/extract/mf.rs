//! Media Foundation's Source Reader: video frames (upright RGB32, through its advanced video processing) and audio
//! (32-bit float at the file's own rate). Released as soon as a file is done: COM objects in Rust go when they're
//! dropped, so nothing waits for a collector (the .NET version leaked handles until finalizers ran).

use std::sync::Once;

use anyhow::{Result, anyhow};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING};

static STARTED: Once = Once::new();

fn startup() {
    STARTED.call_once(|| {
        // SAFETY: starts Media Foundation once for the process.
        unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        }
    });
}

const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const FIRST_AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
const MEDIA_SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
const END_OF_STREAM: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;
const READER_ERROR: u32 = MF_SOURCE_READERF_ERROR.0 as u32;
/// MF_E_INVALIDSTREAMNUMBER: the file has no stream of that kind (a video without sound).
pub const NO_SUCH_STREAM: i32 = 0xC00D36B3u32 as i32;

fn reader(path: &str, advanced_video: bool) -> Result<IMFSourceReader> {
    startup();
    // SAFETY: COM calls on owned interfaces.
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| anyhow!("no attributes"))?;
        if advanced_video {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        }
        Ok(MFCreateSourceReaderFromURL(&HSTRING::from(path), &attrs)?)
    }
}

fn duration_of(reader: &IMFSourceReader) -> f64 {
    // SAFETY: COM call on an owned interface; the PROPVARIANT owns its value.
    unsafe {
        reader
            .GetPresentationAttribute(MEDIA_SOURCE, &MF_PD_DURATION)
            .ok()
            .and_then(|pv: PROPVARIANT| u64::try_from(&pv).ok())
            .map(|d| d as f64 / 10_000_000.0)
            .unwrap_or(0.0)
    }
}

fn seek(reader: &IMFSourceReader, seconds: f64) -> Result<()> {
    let pv = PROPVARIANT::from((seconds * 10_000_000.0) as i64);
    // SAFETY: valid pointers for the call's duration.
    unsafe { Ok(reader.SetCurrentPosition(&GUID::zeroed(), &pv)?) }
}

/// One video's frames. One reader per video, one thread at a time.
pub struct VideoFrames {
    reader: IMFSourceReader,
    width: u32,
    height: u32,
    stride: i32,
    /// The video's length in seconds (0 when unknown).
    pub duration: f64,
}

impl VideoFrames {
    /// Opens a video; fails when it has no video stream Media Foundation can decode.
    pub fn open(path: &str) -> Result<VideoFrames> {
        let reader = reader(path, true)?;
        // SAFETY: COM calls on owned interfaces.
        unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            reader.SetCurrentMediaType(FIRST_VIDEO, None, &t)?;
            let current = reader.GetCurrentMediaType(FIRST_VIDEO)?;
            let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
            let (width, height) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((width * 4) as i32);
            let duration = duration_of(&reader);
            Ok(VideoFrames { reader, width, height, stride, duration })
        }
    }

    /// The frame at `seconds` (the first at or after it, from the keyframe before it) as top-down BGRA with opaque
    /// alpha, and its size; None past the end.
    pub fn frame_at(&self, seconds: f64) -> Result<Option<(Vec<u8>, u32, u32)>> {
        let target = (seconds * 10_000_000.0) as i64;
        seek(&self.reader, seconds)?;
        // Seeking lands on the keyframe before; decode forward to the moment (a phone video has one every second or two).
        for _ in 0..600 {
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: out pointers are valid for the call.
            unsafe {
                self.reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample))?;
            }
            if flags & READER_ERROR != 0 {
                return Err(anyhow!("the video decoder failed"));
            }
            let Some(sample) = sample else {
                if flags & END_OF_STREAM != 0 {
                    return Ok(None);
                }
                continue;
            };
            if timestamp + 340_000 < target {
                continue; // a frame (~1/30 s) short of the moment
            }
            return Ok(Some((self.to_bgra(&sample)?, self.width, self.height)));
        }
        Ok(None)
    }

    fn to_bgra(&self, sample: &IMFSample) -> Result<Vec<u8>> {
        let (w, h) = (self.width as usize, self.height as usize);
        let row = w * 4;
        let stride = if self.stride == 0 { row as i64 } else { self.stride as i64 };
        let mut out = vec![0u8; row * h];
        // SAFETY: the buffer is locked while read, and every row read is checked against its length.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data, length as usize);
            for y in 0..h {
                // A negative stride means bottom-up rows: the first row in memory is the last one on screen.
                let src = if stride > 0 { y as i64 * stride } else { (h - 1 - y) as i64 * -stride } as usize;
                if src + row > bytes.len() {
                    break;
                }
                out[y * row..(y + 1) * row].copy_from_slice(&bytes[src..src + row]);
            }
            buffer.Unlock()?;
        }
        // RGB32 is BGRX: whatever is in X, the picture is opaque.
        for a in out.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        Ok(out)
    }
}

/// A recording's sound as interleaved 32-bit float at its own rate.
pub struct AudioSamples {
    reader: IMFSourceReader,
    pub rate: u32,
    pub channels: u32,
    pending: Vec<f32>,
    at: usize,
    done: bool,
    /// Reads in a row that gave no sample.
    empty: u32,
}

/// So many reads in a row without a sample end the sound (stream ticks mark gaps; a broken stream can give only those).
const MAX_EMPTY_READS: u32 = 10_000;

impl AudioSamples {
    /// Fails with `NO_SUCH_STREAM` (as the HRESULT) for a file without sound, or when nothing can decode it.
    pub fn open(path: &str) -> windows::core::Result<AudioSamples> {
        let reader = reader(path, false).map_err(|e| match e.downcast::<windows::core::Error>() {
            Ok(w) => w,
            Err(e) => windows::core::Error::new(windows::core::HRESULT(-1), e.to_string()),
        })?;
        // SAFETY: COM calls on owned interfaces.
        unsafe {
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(FIRST_AUDIO, true)?;
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            reader.SetCurrentMediaType(FIRST_AUDIO, None, &t)?;
            let current = reader.GetCurrentMediaType(FIRST_AUDIO)?;
            let rate = current.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let channels = current.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?.max(1);
            Ok(AudioSamples { reader, rate, channels, pending: vec![], at: 0, done: false, empty: 0 })
        }
    }

    /// Fills `out` with interleaved samples; returns how many (0 at the end).
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let mut written = 0;
        while written < out.len() {
            if self.at >= self.pending.len() {
                if self.done || !self.next()? {
                    break;
                }
                continue;
            }
            let n = (out.len() - written).min(self.pending.len() - self.at);
            out[written..written + n].copy_from_slice(&self.pending[self.at..self.at + n]);
            self.at += n;
            written += n;
        }
        Ok(written)
    }

    fn next(&mut self) -> Result<bool> {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        // SAFETY: out pointers are valid for the call; the buffer is locked while copied.
        unsafe {
            self.reader.ReadSample(FIRST_AUDIO, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            // After an error the reader must not be called again (for video, the next call hung): the sound ends here,
            // as it does for the frames.
            if flags & READER_ERROR != 0 {
                self.done = true;
                return Err(anyhow!("the audio decoder failed"));
            }
            if flags & END_OF_STREAM != 0 {
                self.done = true;
            }
            let Some(sample) = sample else {
                // A gap (a stream tick) has no sample; a stream that only ever gives those has nothing to read.
                self.empty += 1;
                if self.empty > MAX_EMPTY_READS {
                    self.done = true;
                }
                return Ok(!self.done);
            };
            self.empty = 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data as *const u8, length as usize);
            self.pending.clear();
            self.pending.extend(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            buffer.Unlock()?;
            self.at = 0;
        }
        Ok(true)
    }
}
