//! Vectors: normalising, dot products, and float16 storage (half the size; ranking is unaffected).

use half::f16;
use half::slice::HalfFloatSliceExt;

pub fn normalize(v: &mut [f32]) {
    let n = dot(v, v).sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// The dot product, with AVX2+FMA when the CPU has them.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
            // SAFETY: the features were just detected.
            return unsafe { dot_avx2(&a[..n], &b[..n]) };
        }
    }
    dot_scalar(&a[..n], &b[..n])
}

fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    // Eight running sums: the compiler turns this into SSE, and it rounds like the wide version does.
    let mut acc = [0f32; 8];
    let chunks = a.len() / 8;
    for i in 0..chunks {
        for k in 0..8 {
            acc[k] += a[i * 8 + k] * b[i * 8 + k];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for i in chunks * 8..a.len() {
        sum += a[i] * b[i];
    }
    sum
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len();
    let mut s0 = _mm256_setzero_ps();
    let mut s1 = _mm256_setzero_ps();
    let mut i = 0;
    // SAFETY: every load stays within the slices (i + 16 <= n, then i + 8 <= n).
    unsafe {
        while i + 16 <= n {
            s0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), s0);
            s1 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 8)), _mm256_loadu_ps(b.as_ptr().add(i + 8)), s1);
            i += 16;
        }
        while i + 8 <= n {
            s0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), _mm256_loadu_ps(b.as_ptr().add(i)), s0);
            i += 8;
        }
        let s = _mm256_add_ps(s0, s1);
        let lo = _mm256_castps256_ps128(s);
        let hi = _mm256_extractf128_ps(s, 1);
        let q = _mm_add_ps(lo, hi);
        let d = _mm_add_ps(q, _mm_movehl_ps(q, q));
        let e = _mm_add_ss(d, _mm_shuffle_ps(d, d, 0x55));
        let mut sum = _mm_cvtss_f32(e);
        while i < n {
            sum += a[i] * b[i];
            i += 1;
        }
        sum
    }
}

/// The dot product of a float32 vector and a float16 one, as `dot(a, b as float32)` gives it to the bit: float16 →
/// float32 is exact, and the sums are taken in the same order. `fast` comes from `has_f16c()`, asked once per search.
#[inline]
pub fn dot_half(a: &[f32], b: &[f16], fast: bool) -> f32 {
    let n = a.len().min(b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if fast {
            // SAFETY: the caller checked the features (`has_f16c`).
            return unsafe { dot_half_avx2(&a[..n], &b[..n]) };
        }
    }
    let _ = fast;
    let mut row = [0f32; 1024];
    if n <= row.len() {
        b[..n].convert_to_f32_slice(&mut row[..n]);
        return dot(&a[..n], &row[..n]);
    }
    dot(&a[..n], &b[..n].iter().map(|h| h.to_f32()).collect::<Vec<_>>())
}

/// Whether `dot_half` can take float16 lanes straight from memory (AVX2, FMA and F16C).
pub fn has_f16c() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") && std::arch::is_x86_feature_detected!("f16c")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma,f16c")]
unsafe fn dot_half_avx2(a: &[f32], b: &[f16]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len();
    let bp = b.as_ptr().cast::<u16>();
    let mut s0 = _mm256_setzero_ps();
    let mut s1 = _mm256_setzero_ps();
    let mut i = 0;
    // SAFETY: every load stays within the slices (i + 16 <= n, then i + 8 <= n); 8 float16 are 16 bytes.
    unsafe {
        let half8 = |at: usize| _mm256_cvtph_ps(_mm_loadu_si128(bp.add(at).cast()));
        while i + 16 <= n {
            s0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), half8(i), s0);
            s1 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 8)), half8(i + 8), s1);
            i += 16;
        }
        while i + 8 <= n {
            s0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), half8(i), s0);
            i += 8;
        }
        let s = _mm256_add_ps(s0, s1);
        let lo = _mm256_castps256_ps128(s);
        let hi = _mm256_extractf128_ps(s, 1);
        let q = _mm_add_ps(lo, hi);
        let d = _mm_add_ps(q, _mm_movehl_ps(q, q));
        let e = _mm_add_ss(d, _mm_shuffle_ps(d, d, 0x55));
        let mut sum = _mm_cvtss_f32(e);
        while i < n {
            sum += a[i] * b[i].to_f32();
            i += 1;
        }
        sum
    }
}

/// float32 → float16.
pub fn to_half(v: &[f32]) -> Vec<f16> {
    let mut h = vec![f16::ZERO; v.len()];
    h.convert_from_f32_slice(v);
    h
}

/// The bytes stored in the index: float16, little-endian.
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    half_to_blob(&to_half(v))
}

pub fn half_to_blob(h: &[f16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(h.len() * 2);
    for x in h {
        out.extend_from_slice(&x.to_bits().to_le_bytes());
    }
    out
}

pub fn blob_to_half(blob: &[u8]) -> Vec<f16> {
    blob.as_chunks::<2>().0.iter().map(|b| f16::from_bits(u16::from_le_bytes([b[0], b[1]]))).collect()
}

pub fn from_blob(blob: &[u8]) -> Vec<f32> {
    let h = blob_to_half(blob);
    let mut v = vec![0f32; h.len()];
    h.convert_to_f32_slice(&mut v);
    v
}

/// Standard base64 (the reference vectors and the stored reference are kept this way).
pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for c in bytes.chunks(4) {
        let pad = c.iter().rev().take_while(|&&b| b == b'=').count();
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n <<= 6;
            if i < 4 - pad {
                n |= val(b)?;
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_matches_scalar_and_base64_roundtrips() {
        let a: Vec<f32> = (0..771).map(|i| (i as f32 * 0.37).sin()).collect();
        let b: Vec<f32> = (0..771).map(|i| (i as f32 * 0.11).cos()).collect();
        assert!((dot(&a, &b) - dot_scalar(&a, &b)).abs() < 1e-3);
        for n in 0..7 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn dot_half_is_dot_of_the_float32_vector() {
        let a: Vec<f32> = (0..771).map(|i| (i as f32 * 0.37).sin()).collect();
        let b = to_half(&(0..771).map(|i| (i as f32 * 0.11).cos()).collect::<Vec<_>>());
        let mut wide = vec![0f32; b.len()];
        b.convert_to_f32_slice(&mut wide);
        let expected = dot(&a, &wide);
        assert_eq!(dot_half(&a, &b, false).to_bits(), expected.to_bits());
        if has_f16c() {
            assert_eq!(dot_half(&a, &b, true).to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn vectors_roundtrip_through_float16() {
        let mut v: Vec<f32> = (0..768).map(|i| ((i * 7919) % 101) as f32 / 101.0).collect();
        normalize(&mut v);
        let back = from_blob(&to_blob(&v));
        assert!(dot(&v, &back) > 0.999);
    }
}
