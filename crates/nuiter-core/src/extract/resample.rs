//! Sound as the model hears it: 16 kHz mono. Channels are averaged, then a windowed-sinc low-pass resampler (cut-off
//! just under the new Nyquist frequency, 16 zero crossings each side, Blackman window) brings the rate to 16 kHz.

use anyhow::Result;

pub const RATE: u32 = 16_000;

/// Interleaved float samples at some rate.
pub trait FloatSource {
    fn rate(&self) -> u32;
    fn channels(&self) -> u32;
    /// Fills `out` with interleaved samples; 0 at the end.
    fn read(&mut self, out: &mut [f32]) -> Result<usize>;
}

/// 16 kHz mono 16-bit samples.
pub trait MonoSource {
    fn read(&mut self, out: &mut [i16]) -> Result<usize>;
}

pub struct To16k<S: FloatSource> {
    source: S,
    channels: usize,
    /// Input samples per output sample.
    step: f64,
    /// Cut-off, in cycles per input sample.
    cutoff: f64,
    half: usize,
    /// Mono input from `base` on.
    buf: Vec<f32>,
    base: usize,
    /// Position of the next output sample, in input samples.
    t: f64,
    ended: bool,
    /// Real (mono) input samples received.
    real: usize,
    scratch: Vec<f32>,
    passthrough: bool,
}

impl<S: FloatSource> To16k<S> {
    pub fn new(source: S) -> Self {
        let rate = source.rate().max(1) as f64;
        let channels = source.channels().max(1) as usize;
        let step = rate / RATE as f64;
        let cutoff = 0.5 * (1.0 / step).min(1.0) * 0.95;
        let half = (16.0 / (2.0 * cutoff)).ceil() as usize;
        Self {
            source,
            channels,
            step,
            cutoff,
            half,
            buf: vec![],
            base: 0,
            t: 0.0,
            ended: false,
            real: 0,
            scratch: vec![0.0; 8192 * channels],
            passthrough: source_is_16k(rate) && channels >= 1,
        }
    }

    fn fill(&mut self) -> Result<bool> {
        if self.ended {
            return Ok(false);
        }
        let n = self.source.read(&mut self.scratch)?;
        if n == 0 {
            self.ended = true;
            if !self.passthrough {
                // Zeros after the end, so the last samples get their full filter.
                self.buf.extend(std::iter::repeat_n(0.0, self.half + 1));
            }
            return Ok(true);
        }
        let c = self.channels;
        for frame in self.scratch[..n - n % c].chunks_exact(c) {
            self.buf.push(frame.iter().sum::<f32>() / c as f32);
            self.real += 1;
        }
        Ok(true)
    }

    fn sample_at(&self, t: f64) -> f32 {
        let centre = t.floor() as i64;
        let mut acc = 0f64;
        let mut wsum = 0f64;
        for k in (centre - self.half as i64 + 1)..=(centre + self.half as i64) {
            if k < 0 {
                continue;
            }
            let i = k as usize;
            if i < self.base || i - self.base >= self.buf.len() {
                continue;
            }
            let x = t - k as f64;
            let w = kernel(x, self.cutoff, self.half as f64);
            acc += self.buf[i - self.base] as f64 * w;
            wsum += w;
        }
        // Normalised by the weights' sum: a flat signal stays flat whatever the fraction.
        (if wsum.abs() > 1e-9 { acc / wsum } else { 0.0 }) as f32
    }
}

fn source_is_16k(rate: f64) -> bool {
    (rate - RATE as f64).abs() < 0.5
}

fn kernel(x: f64, cutoff: f64, half: f64) -> f64 {
    if x.abs() >= half {
        return 0.0;
    }
    let s = 2.0 * cutoff * x;
    let sinc = if s.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * s).sin() / (std::f64::consts::PI * s) };
    // Blackman over [-half, half].
    let n = (x + half) / (2.0 * half);
    let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * n).cos() + 0.08 * (4.0 * std::f64::consts::PI * n).cos();
    2.0 * cutoff * sinc * w
}

impl<S: FloatSource> MonoSource for To16k<S> {
    fn read(&mut self, out: &mut [i16]) -> Result<usize> {
        let mut written = 0;
        while written < out.len() {
            if self.ended && self.t >= self.real as f64 {
                break;
            }
            if self.passthrough {
                let i = self.t as usize;
                if i >= self.base + self.buf.len() {
                    if !self.fill()? {
                        break;
                    }
                    continue;
                }
                let start = i - self.base;
                let n = (self.buf.len() - start).min(out.len() - written);
                for (o, s) in out[written..written + n].iter_mut().zip(&self.buf[start..start + n]) {
                    *o = to_i16(*s);
                }
                written += n;
                self.t += n as f64;
                self.trim();
                continue;
            }
            // Enough input past t for the filter's right half?
            let need = self.t.floor() as usize + self.half + 1;
            if need > self.base + self.buf.len() && !self.ended {
                self.fill()?;
                continue;
            }
            out[written] = to_i16(self.sample_at(self.t));
            written += 1;
            self.t += self.step;
            if self.buf.len() > 65536 {
                self.trim();
            }
        }
        Ok(written)
    }
}

impl<S: FloatSource> To16k<S> {
    /// Drops input the filter will never look at again.
    fn trim(&mut self) {
        let keep_from = (self.t.floor() as usize).saturating_sub(self.half + 1);
        if keep_from > self.base {
            let drop = (keep_from - self.base).min(self.buf.len());
            self.buf.drain(..drop);
            self.base += drop;
        }
    }
}

fn to_i16(s: f32) -> i16 {
    (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tone {
        rate: u32,
        channels: u32,
        left: usize,
        n: usize,
    }

    impl FloatSource for Tone {
        fn rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u32 {
            self.channels
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize> {
            let frames = (out.len() / self.channels as usize).min(self.left);
            for f in 0..frames {
                let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * self.n as f32 / self.rate as f32).sin();
                for c in 0..self.channels as usize {
                    out[f * self.channels as usize + c] = v;
                }
                self.n += 1;
            }
            self.left -= frames;
            Ok(frames * self.channels as usize)
        }
    }

    #[test]
    fn a_tone_keeps_its_length_and_loudness() {
        for (rate, channels) in [(48_000, 2), (44_100, 1), (16_000, 1), (8_000, 1)] {
            let mut r = To16k::new(Tone { rate, channels, left: rate as usize, n: 0 });
            let mut out = vec![0i16; 40_000];
            let mut total = 0;
            loop {
                let n = r.read(&mut out[total..]).unwrap();
                if n == 0 {
                    break;
                }
                total += n;
            }
            assert!((total as i64 - 16_000).abs() <= 40, "{rate}: {total}");
            let rms = (out[1000..15000].iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum::<f64>() / 14000.0).sqrt();
            assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.02, "{rate}: rms {rms}");
        }
    }
}
