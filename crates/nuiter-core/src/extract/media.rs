//! Audio is decoded with Media Foundation (MP3, AAC/M4A, WMA, FLAC, WAV… and the soundtrack of videos) or, for Ogg,
//! in Rust; brought to 16 kHz mono and cut into 30-second windows (as much as the model hears of one input) that
//! overlap by a few seconds, so a match points to a moment and a sentence across a window's end is whole in the next.
//! Video frames are read with Media Foundation: one per shot.

use anyhow::Result;

use super::image::{self, OcrMode};
use super::mf::{AudioSamples, NO_SUCH_STREAM, VideoFrames};
use super::ogg_audio;
use super::resample::{FloatSource, MonoSource, RATE, To16k};
use super::{ExtractedItem, Sink, chunk_kind};
use crate::cancel::Cancel;
use crate::paths::Log;
use crate::runtime::EmbedInput;

pub const SAMPLE_RATE: u32 = RATE;
pub const WINDOW_SECONDS: f64 = 30.0;
pub const HOP_SECONDS: f64 = 26.0;
pub const MAX_AUDIO_SECONDS: f64 = 4.0 * 3600.0;

/// A video frame is looked at this often (longer videos less often: at most `MAX_SAMPLES`).
pub const SAMPLE_SECONDS: f64 = 2.0;
pub const MAX_SAMPLES: f64 = 1800.0;
/// Within one shot, a frame is kept at least this often.
pub const MAX_GAP_SECONDS: f64 = 30.0;

/// A 100 ms stretch louder than this RMS (about −48 dBFS) is sound, not silence or hiss.
const ACTIVE_RMS: f64 = 0.004;
/// A window with less sound than this gets no vector.
const MIN_ACTIVE_SECONDS: f64 = 0.5;
/// A recording shorter than this is a click; a window needs this much audio no earlier window had.
const MIN_FILE_SECONDS: f64 = 0.4;
const MIN_NEW_SECONDS: f64 = 3.0;

impl FloatSource for AudioSamples {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        AudioSamples::read(self, out)
    }
}

/// The file's sound as 16 kHz mono, or None when it has none.
fn open_audio(path: &str) -> Option<Box<dyn MonoSource>> {
    match AudioSamples::open(path) {
        Ok(a) => Some(Box::new(To16k::new(a))),
        Err(e) if e.code().0 == NO_SUCH_STREAM => {
            Log::info(format!("No sound track in {path}"));
            None
        }
        Err(e) if ogg_audio::can_try(path) => {
            let ogg = ogg_audio::open(path);
            if ogg.is_none() {
                Log::warn(format!("No decodable audio in {path}: {}", e.message()));
            }
            ogg
        }
        Err(e) => {
            Log::warn(format!("No decodable audio in {path}: {}", e.message()));
            None
        }
    }
}

pub fn audio(path: &str, cancel: &Cancel, sink: Sink<'_>) -> Result<()> {
    let Some(mut source) = open_audio(path) else { return Ok(()) };
    let window = (WINDOW_SECONDS * SAMPLE_RATE as f64) as usize;
    let hop = (HOP_SECONDS * SAMPLE_RATE as f64) as usize;
    let mut buffer = vec![0i16; window];
    let mut filled = 0;
    let mut fresh = 0; // samples of the window no earlier window had
    let mut start = 0.0;
    let mut eof = false;
    while !eof && start < MAX_AUDIO_SECONDS {
        cancel.check()?;
        while filled < window {
            // A decoder that fails partway leaves what was heard before it (as video frames do).
            let n = match source.read(&mut buffer[filled..]) {
                Ok(n) => n,
                Err(e) => {
                    Log::warn(format!("Sound of {path} ends at {}: {e}", timestamp(start + filled as f64 / SAMPLE_RATE as f64)));
                    0
                }
            };
            if n == 0 {
                eof = true;
                break;
            }
            filled += n;
            fresh += n;
        }
        // A window needs enough new sound to say something new: a short file whole, a tail a few seconds.
        let minimum = if start == 0.0 { MIN_FILE_SECONDS } else { MIN_NEW_SECONDS };
        if fresh as f64 / SAMPLE_RATE as f64 >= minimum && active_seconds(&buffer[..filled]) >= MIN_ACTIVE_SECONDS {
            sink(ExtractedItem::media(chunk_kind::AUDIO, EmbedInput::Audio(wav(&buffer[..filled])), Some(timestamp(start)), Some(start)))?;
        }
        if eof {
            break;
        }
        // Slide on by a hop: the end of this window starts the next.
        buffer.copy_within(hop..window, 0);
        filled = window - hop;
        fresh = 0;
        start += HOP_SECONDS;
    }
    Ok(())
}

/// Frames for the model, one per shot. A frame is sampled every `SAMPLE_SECONDS` (further apart in videos over an
/// hour, to stay under `MAX_SAMPLES`); it starts a new shot when it differs enough from the last frame kept. A shot
/// that lasts gets another frame every `MAX_GAP_SECONDS` (a lecture's slides change without the picture changing much).
/// Returns why no frame could be read, when none could (the sound may still be read: it has a reader of its own).
pub fn video_frames(path: &str, ocr: bool, cancel: &Cancel, sink: Sink<'_>) -> Result<Option<String>> {
    let video = match VideoFrames::open(path) {
        Ok(v) => v,
        Err(e) => {
            Log::warn(format!("Cannot open video {path}: {e}"));
            return Ok(Some(format!("Windows can't open its video ({e})")));
        }
    };
    let total = video.duration;
    if total <= 0.0 {
        return Ok(None);
    }
    let mut read = 0usize;
    let step = SAMPLE_SECONDS.max(total / MAX_SAMPLES);
    let mut kept: Option<Looks> = None;
    let mut kept_at = f64::NEG_INFINITY;
    let mut t = 0.5f64.min(total / 2.0);
    while t < total {
        cancel.check()?;
        let at = t;
        t += step;
        let frame = match video.frame_at(at) {
            Ok(Some(f)) => f,
            Ok(None) => break, // past the end
            Err(e) => {
                // The decoder can't take this video (H.264 High 4:4:4 from video generators, a codec Windows lacks): the
                // reader stays broken, and the next seek's ReadSample hung until the 8-minute stall limit.
                Log::warn(format!("Frame {at:.0}s of {path}: {e}; no more frames are read"));
                if read == 0 {
                    return Ok(Some(format!("Windows can't decode its video ({e}); H.264 4:4:4 and some codecs aren't supported")));
                }
                break;
            }
        };
        read += 1;
        let (pixels, w, h) = frame;
        let mode = if ocr { OcrMode::Probe } else { OcrMode::None };
        let img = match image::from_bytes(&bmp(&pixels, w, h), mode, image::FRAME_TOKENS) {
            Ok(Some(img)) => img,
            Ok(None) => continue,
            Err(e) => {
                Log::warn(format!("Frame {at:.0}s of {path}: {e}"));
                continue;
            }
        };
        drop(pixels);
        let looks = Looks::of(&img.pixels, img.pixels_width as usize, img.pixels_height as usize);
        if let Some(k) = &kept
            && !looks.differs(k)
            && at - kept_at < MAX_GAP_SECONDS
        {
            continue;
        }
        kept = Some(looks);
        kept_at = at;
        sink(ExtractedItem::media(chunk_kind::FRAME, EmbedInput::Image(img.image), Some(timestamp(at)), Some(at)))?;
        if let Some(text) = img.ocr_text
            && text.chars().count() > 25
        {
            sink(ExtractedItem::text(chunk_kind::OCR, text, Some(timestamp(at)), Some(at)).keyword_only(true))?;
        }
    }
    Ok(None)
}

/// A top-down 32-bit BMP of BGRA pixels (what the image decoder takes for a video frame).
pub fn bmp(bgra: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(54 + bgra.len());
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&((54 + bgra.len()) as u32).to_le_bytes());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(-(h as i32)).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&[0u8; 24]);
    b.extend_from_slice(bgra);
    b
}

pub fn timestamp(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, s / 60 % 60, s % 60);
    if h >= 1 { format!("{h}:{m:02}:{sec:02}") } else { format!("{m}:{sec:02}") }
}

/// Seconds of the recording whose 100 ms frames are louder than `ACTIVE_RMS`.
fn active_seconds(pcm: &[i16]) -> f64 {
    let frame = (SAMPLE_RATE / 10) as usize;
    let samples = pcm.len();
    let mut active = 0usize;
    let mut f = 0;
    while f + frame <= samples || (f < samples && f == 0) {
        let n = frame.min(samples - f);
        let sum: f64 = pcm[f..f + n].iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
        if (sum / n as f64).sqrt() > ACTIVE_RMS {
            active += n;
        }
        f += frame;
    }
    active as f64 / SAMPLE_RATE as f64
}

/// 16 kHz mono 16-bit PCM as a WAV file.
pub fn wav(pcm: &[i16]) -> Vec<u8> {
    let data = pcm.len() * 2;
    let mut w = Vec::with_capacity(44 + data);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    w.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data as u32).to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// What a frame looks like, coarsely: its colours (a 4×4×4 RGB histogram, as shares) and its layout (the brightness of
/// a 16×9 grid). A cut changes the colours; a new slide or a pan keeps them but moves the layout.
pub struct Looks {
    colours: [f64; 64],
    grid: [f64; GRID_W * GRID_H],
}

const GRID_W: usize = 16;
const GRID_H: usize = 9;
/// Colour histograms further apart than this (L1, 0–2) are different shots.
const COLOUR_CHANGE: f64 = 0.45;
/// Grids whose cells differ by this much on average (0–255) are different pictures.
const LAYOUT_CHANGE: f64 = 14.0;

impl Looks {
    /// From BGRA pixels, averaged down to 32×18.
    pub fn of(bgra: &[u8], w: usize, h: usize) -> Looks {
        let mut looks = Looks { colours: [0.0; 64], grid: [0.0; GRID_W * GRID_H] };
        let (sw, sh) = (GRID_W * 2, GRID_H * 2);
        for y in 0..sh {
            for x in 0..sw {
                let (x0, x1) = (x * w / sw, ((x + 1) * w / sw).max(x * w / sw + 1).min(w));
                let (y0, y1) = (y * h / sh, ((y + 1) * h / sh).max(y * h / sh + 1).min(h));
                let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
                for yy in y0..y1 {
                    for xx in x0..x1 {
                        let p = (yy * w + xx) * 4;
                        if p + 2 < bgra.len() {
                            b += bgra[p] as u64;
                            g += bgra[p + 1] as u64;
                            r += bgra[p + 2] as u64;
                            n += 1;
                        }
                    }
                }
                let n = n.max(1);
                let (r, g, b) = ((r / n) as u8, (g / n) as u8, (b / n) as u8);
                looks.colours[(r >> 6) as usize * 16 + (g >> 6) as usize * 4 + (b >> 6) as usize] +=
                    1.0 / (GRID_W * GRID_H * 4) as f64;
                looks.grid[y / 2 * GRID_W + x / 2] += (0.2126 * r as f64 + 0.7152 * g as f64 + 0.0722 * b as f64) / 4.0;
            }
        }
        looks
    }

    pub fn differs(&self, other: &Looks) -> bool {
        let colour: f64 = self.colours.iter().zip(&other.colours).map(|(a, b)| (a - b).abs()).sum();
        let layout: f64 = self.grid.iter().zip(&other.grid).map(|(a, b)| (a - b).abs()).sum();
        colour > COLOUR_CHANGE || layout / self.grid.len() as f64 > LAYOUT_CHANGE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: usize, h: usize, paint: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = paint(x, y);
                px[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        px
    }

    #[test]
    fn a_new_shot_is_told_by_its_colours_or_its_layout() {
        let sky = [135, 206, 235];
        let sand = [244, 164, 96];
        let beach = Looks::of(&picture(320, 180, |_, y| if y >= 120 { sand } else { sky }), 320, 180);
        let same = Looks::of(&picture(320, 180, |_, y| if y >= 122 { sand } else { sky }), 320, 180);
        let forest = Looks::of(&picture(320, 180, |_, _| [0, 100, 0]), 320, 180);
        let slide_a = Looks::of(&picture(320, 180, |x, y| if (20..140).contains(&x) && (20..160).contains(&y) { [0, 0, 0] } else { [255, 255, 255] }), 320, 180);
        let slide_b = Looks::of(&picture(320, 180, |x, y| if (180..300).contains(&x) && (20..160).contains(&y) { [0, 0, 0] } else { [255, 255, 255] }), 320, 180);
        assert!(!beach.differs(&same));
        assert!(beach.differs(&forest));
        assert!(slide_a.differs(&slide_b));
    }

    #[test]
    fn wav_header_is_valid() {
        let w = wav(&vec![0i16; 1600]);
        assert_eq!(w.len(), 44 + 3200);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 16000);
    }

    #[test]
    fn timestamps_read_like_a_player() {
        assert_eq!(timestamp(6.4), "0:06");
        assert_eq!(timestamp(3725.0), "1:02:05");
    }

    #[test]
    fn short_sounds_get_a_vector_and_silence_does_not() {
        fn tone(seconds: f64, amplitude: f64) -> Vec<i16> {
            (0..(seconds * 16000.0) as usize)
                .map(|i| (amplitude * 32767.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16000.0).sin()) as i16)
                .collect()
        }
        let dir = std::env::temp_dir().join(format!("nuiter-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let count = |name: &str, pcm: Vec<i16>| -> usize {
            let p = dir.join(name);
            std::fs::write(&p, wav(&pcm)).unwrap();
            let mut n = 0;
            audio(p.to_str().unwrap(), &Cancel::never(), &mut |_| {
                n += 1;
                Ok(())
            })
            .unwrap();
            n
        };
        crate::win::init_com_mta();
        assert_eq!(count("bark.wav", tone(0.8, 0.3)), 1);
        assert_eq!(count("hiss.wav", tone(5.0, 0.001)), 0);
        assert_eq!(count("click.wav", tone(0.2, 0.5)), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
