//! Ogg audio Windows can't decode: Opus (Telegram, WhatsApp and Signal voice messages, Discord recordings) and Vorbis.
//! Decoded packet by packet, so a long podcast never sits in memory whole, in pure Rust.

use std::fs::File;
use std::io::{BufReader, Read};

use anyhow::{Result, anyhow};

use super::resample::{FloatSource, MonoSource, RATE, To16k};
use crate::file_kinds::pathx;
use crate::paths::Log;
use crate::uni::lower_str;

pub fn can_try(path: &str) -> bool {
    matches!(lower_str(pathx::extension(path)).as_str(), ".ogg" | ".oga" | ".opus")
}

/// The sound as 16 kHz mono, or None when it's neither Opus nor Vorbis.
pub fn open(path: &str) -> Option<Box<dyn MonoSource>> {
    let mut magic = [0u8; 64];
    let n = File::open(path).and_then(|mut f| f.read(&mut magic)).unwrap_or(0);
    // The first Ogg page carries the codec's identification header.
    let head = &magic[..n];
    let has = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
    let result: Result<Box<dyn MonoSource>> = if has(b"OpusHead") {
        Opus::open(path).map(|o| Box::new(o) as Box<dyn MonoSource>)
    } else if has(b"vorbis") {
        Vorbis::open(path).map(|v| Box::new(To16k::new(v)) as Box<dyn MonoSource>)
    } else {
        return None;
    };
    match result {
        Ok(s) => Some(s),
        Err(e) => {
            Log::warn(format!("Ogg audio in {path}: {e}"));
            None
        }
    }
}

/// Ogg Opus, decoded straight to 16 kHz mono (Opus decodes to any rate and channel count asked for).
struct Opus {
    reader: ogg::PacketReader<BufReader<File>>,
    decoder: opus_decoder::OpusDecoder,
    pending: Vec<i16>,
    at: usize,
    /// Samples at the start that are the encoder's lead-in (OpusHead's pre-skip, at 16 kHz).
    skip: usize,
    done: bool,
}

impl Opus {
    fn open(path: &str) -> Result<Opus> {
        let mut reader = ogg::PacketReader::new(BufReader::new(File::open(path)?));
        let head = reader.read_packet()?.ok_or_else(|| anyhow!("no OpusHead"))?;
        if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
            return Err(anyhow!("not Ogg Opus"));
        }
        let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
        let _tags = reader.read_packet()?;
        let decoder = opus_decoder::OpusDecoder::new(RATE, 1).map_err(|e| anyhow!("{e:?}"))?;
        Ok(Opus { reader, decoder, pending: vec![], at: 0, skip: pre_skip / 3, done: false })
    }
}

impl MonoSource for Opus {
    fn read(&mut self, out: &mut [i16]) -> Result<usize> {
        let mut written = 0;
        while written < out.len() {
            if self.at >= self.pending.len() {
                if self.done {
                    break;
                }
                let packet = match self.reader.read_packet() {
                    Ok(Some(p)) => p,
                    _ => {
                        self.done = true;
                        break;
                    }
                };
                let mut pcm = vec![0i16; self.decoder.max_frame_size_per_channel().max(5760)];
                let n = match self.decoder.decode(&packet.data, &mut pcm, false) {
                    Ok(n) => n,
                    Err(_) => continue, // a broken packet: the next one goes on
                };
                pcm.truncate(n);
                let skip = self.skip.min(pcm.len());
                self.skip -= skip;
                self.pending = pcm.split_off(skip);
                self.at = 0;
                continue;
            }
            let n = (out.len() - written).min(self.pending.len() - self.at);
            out[written..written + n].copy_from_slice(&self.pending[self.at..self.at + n]);
            self.at += n;
            written += n;
        }
        Ok(written)
    }
}

/// Ogg Vorbis, at the file's own rate and channels.
struct Vorbis {
    reader: lewton::inside_ogg::OggStreamReader<BufReader<File>>,
    pending: Vec<i16>,
    at: usize,
    done: bool,
}

impl Vorbis {
    fn open(path: &str) -> Result<Vorbis> {
        let reader = lewton::inside_ogg::OggStreamReader::new(BufReader::new(File::open(path)?))?;
        Ok(Vorbis { reader, pending: vec![], at: 0, done: false })
    }
}

impl FloatSource for Vorbis {
    fn rate(&self) -> u32 {
        self.reader.ident_hdr.audio_sample_rate
    }

    fn channels(&self) -> u32 {
        self.reader.ident_hdr.audio_channels as u32
    }

    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let channels = self.channels().max(1) as usize;
        let room = out.len() - out.len() % channels;
        let mut written = 0;
        while written < room {
            if self.at >= self.pending.len() {
                if self.done {
                    break;
                }
                match self.reader.read_dec_packet_itl() {
                    Ok(Some(p)) => {
                        self.pending = p;
                        self.at = 0;
                    }
                    _ => self.done = true,
                }
                continue;
            }
            let n = (room - written).min(self.pending.len() - self.at);
            for (o, s) in out[written..written + n].iter_mut().zip(&self.pending[self.at..self.at + n]) {
                *o = *s as f32 / 32768.0;
            }
            self.at += n;
            written += n;
        }
        Ok(written)
    }
}
