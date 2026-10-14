//! Vectors the pinned model makes on the CPU for a fixed text and a fixed picture. A GPU whose results differ from these
//! is computing wrong embeddings (some iGPU drivers return near-random vectors without a single error), so the worker
//! checks every GPU start against them and falls back to the CPU. Regenerate with `nuiter-cli reference` when the model
//! or the llama.cpp tag changes.

/// Below this the GPU's vectors are wrong, not just rounded differently (CPU vs GPU agree to ≥ 0.9998).
pub const MIN_COSINE: f32 = 0.99;

pub const TEXT: &str = "Nuiter reference vector: a small orange cat sleeping on a blue sofa.";

/// Float16 vectors, base64 (`vector_math::to_blob`), from llama.cpp b11577 (patched) on the CPU; empty: no check.
pub const TEXT_VECTOR: &str = "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";
pub const IMAGE_VECTOR: &str = "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";

/// A picture that is the same on every machine: colour bands and a disc, as a 24-bit PNG (the same pixels the .NET
/// version drew), at a 140-token size.
pub fn image() -> Vec<u8> {
    const SIDE: i32 = 384; // sides a multiple of 48 px, so mtmd takes it as it is
    let mut rgb = Vec::with_capacity((SIDE * SIDE * 3) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let (dx, dy) = (x - 230, y - 150);
            let px = if dx * dx + dy * dy < 70 * 70 {
                [240, 200, 30]
            } else {
                [(30 + y * 120 / SIDE) as u8, (90 + x * 100 / SIDE) as u8, (200 - y * 120 / SIDE) as u8]
            };
            rgb.extend_from_slice(&px);
        }
    }
    crate::extract::image::encode_png_rgb(&rgb, SIDE as u32, SIDE as u32).unwrap_or_default()
}
