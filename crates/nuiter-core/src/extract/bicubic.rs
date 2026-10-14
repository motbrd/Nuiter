//! Bicubic resampling with the result of Pillow's `Image.resize(size, BICUBIC)`, which the model's reference image
//! processor uses. Pillow's method: Keys' cubic with a = −0.5, widened by the scale factor when shrinking so every
//! source pixel is averaged in, weights normalised per output pixel, one axis at a time (width first), each pass
//! rounded to bytes. The reference tests hold it to Pillow's own output. Works on BGRA; alpha comes out opaque.

pub fn bgra(source: &[u8], width: usize, height: usize, to_width: usize, to_height: usize) -> Vec<u8> {
    let mut pixels: Option<Vec<u8>> = None;
    if to_width != width {
        let mut next = vec![0u8; to_width * height * 4];
        // Along each row: pixels 4 bytes apart, rows width*4 apart.
        pass(source, &mut next, &Kernel::new(width, to_width), height, width * 4, to_width * 4, 4, 3);
        pixels = Some(next);
    }
    if to_height != height {
        let src = pixels.as_deref().unwrap_or(source);
        let mut next = vec![0u8; to_width * to_height * 4];
        // Along each column: pixels a row apart; every byte of a row is its own column.
        pass(src, &mut next, &Kernel::new(height, to_height), 1, 0, 0, to_width * 4, to_width * 4);
        pixels = Some(next);
    }
    let mut out = pixels.unwrap_or_else(|| source.to_vec());
    for a in out.iter_mut().skip(3).step_by(4) {
        *a = 255;
    }
    out
}

/// One axis's weights: for output pixel o, the source pixels `start[o] .. start[o] + count[o]` with
/// `weight[o * width ..]`. Pillow's coefficients: centre (o + ½)·scale, support 2·max(scale, 1).
struct Kernel {
    start: Vec<usize>,
    count: Vec<usize>,
    weight: Vec<f32>,
    width: usize,
}

impl Kernel {
    fn new(in_size: usize, out_size: usize) -> Kernel {
        let scale = in_size as f64 / out_size as f64;
        let widen = scale.max(1.0);
        let support = 2.0 * widen;
        let width = support.ceil() as usize * 2 + 1;
        let mut k = Kernel { start: vec![0; out_size], count: vec![0; out_size], weight: vec![0.0; out_size * width], width };
        let mut raw = vec![0f64; width];
        for o in 0..out_size {
            let centre = (o as f64 + 0.5) * scale;
            let from = ((centre - support + 0.5) as i64).max(0) as usize;
            let to = ((centre + support + 0.5) as i64).min(in_size as i64) as usize;
            let n = to.saturating_sub(from).min(width);
            let mut total = 0.0;
            for (j, r) in raw[..n].iter_mut().enumerate() {
                *r = keys((from as f64 + j as f64 - centre + 0.5) / widen);
                total += *r;
            }
            for (w, &r) in k.weight[o * width..o * width + n].iter_mut().zip(&raw[..n]) {
                *w = (if total != 0.0 { r / total } else { r }) as f32;
            }
            k.start[o] = from;
            k.count[o] = n;
        }
        k
    }
}

fn keys(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

/// Resamples along one axis. For each of `lines` (rows for the horizontal pass; one for the vertical, which treats
/// all bytes of a row at once), output pixel o is Σ weight·source[start + k], where consecutive source pixels are
/// `step` bytes apart and each pixel has `channels` bytes to blend.
#[allow(clippy::too_many_arguments)]
fn pass(src: &[u8], dst: &mut [u8], kernel: &Kernel, lines: usize, in_line: usize, out_line: usize, step: usize, channels: usize) {
    let mut acc = vec![0f32; channels];
    for line in 0..lines {
        for o in 0..kernel.start.len() {
            acc.iter_mut().for_each(|a| *a = 0.0);
            let mut at = line * in_line + kernel.start[o] * step;
            for k in 0..kernel.count[o] {
                let w = kernel.weight[o * kernel.width + k];
                let px = &src[at..at + channels];
                for (a, &s) in acc.iter_mut().zip(px) {
                    *a += s as f32 * w;
                }
                at += step;
            }
            let put = line * out_line + o * step;
            for c in 0..channels {
                dst[put + c] = (acc[c] + 0.5).floor().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_flat_colour() {
        for (w, h, tw, th) in [(4, 4, 2, 2), (5, 3, 2, 7)] {
            let mut src = vec![0u8; w * h * 4];
            for p in src.as_chunks_mut::<4>().0 {
                *p = [10, 120, 250, 255];
            }
            let o = bgra(&src, w, h, tw, th);
            assert_eq!(o.len(), tw * th * 4);
            for p in o.as_chunks::<4>().0 {
                assert_eq!(*p, [10, 120, 250, 255]);
            }
        }
    }

    /// A reference PNG as RGBA (an opaque alpha added to RGB).
    fn read_png(path: &std::path::Path) -> (Vec<u8>, usize, usize) {
        let mut d = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut r = d.read_info().unwrap();
        let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        let (w, h) = (info.width as usize, info.height as usize);
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
            png::ColorType::Rgb => buf[..w * h * 3].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::Grayscale => buf[..w * h].iter().flat_map(|&g| [g, g, g, 255]).collect(),
            other => panic!("{path:?}: {other:?}"),
        };
        (rgba, w, h)
    }

    /// Pillow's own bicubic output for the reference pictures (scripts/make-reference.py), to within one level: floats
    /// here, fixed point in Pillow.
    #[test]
    fn matches_pillows_bicubic() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("reference");
        let cases: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("resize").join("cases.json")).unwrap()).unwrap();
        let cases = cases.as_array().unwrap();
        assert!(!cases.is_empty());
        for c in cases {
            let (src, w, h) = read_png(&dir.join(c["input"].as_str().unwrap()));
            let (tw, th) = (c["width"].as_u64().unwrap() as usize, c["height"].as_u64().unwrap() as usize);
            let (expected, ew, eh) = read_png(&dir.join(c["expected"].as_str().unwrap()));
            assert_eq!((ew, eh), (tw, th));
            let got = bgra(&src, w, h, tw, th);
            let worst = got.iter().zip(&expected).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= 1, "{}: {worst} levels off", c["expected"]);
        }
    }
}
