//! Vector kernels: distance, normalization, and i8 quantization.
//!
//! The quantization is what makes the on-disk matrix a quarter of its f32 size,
//! which is the point — the brute-force scan it feeds is page-fault bound, not
//! compute bound.

use anny::metric::{Cosine, Metric};

/// Cosine distance (1 - cos). Lower is better; range [0, 2].
#[inline]
pub fn distance(a: &[f32], b: &[f32]) -> f32 {
    <Cosine as Metric<f32>>::distance(a, b)
}

/// Normalize to unit length in place (no-op for zero vectors).
pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in v {
            *x /= n;
        }
    }
}

/// Quantize a unit vector to i8 (x → round(x·127)). On normalized vectors
/// the i8·i8 dot preserves cosine ordering to within noise, and the 4×
/// smaller matrix is the point: the brute scan is page-fault/IO bound
/// (provenance: 188k faults, 0.45× CPU util on the f32 matrix).
pub fn quantize_i8(v: &[f32]) -> Vec<i8> {
    v.iter().map(|&x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

/// Distance for i8-quantized unit vectors: `1 − (a·b)/127²`, same
/// lower-is-better convention as [`distance`]. On unit vectors this is one
/// integer FMA stream instead of the three float passes cosine needs; the scan
/// it feeds was 72% of a warm kernel-corpus query.
///
/// `chunks_exact` is load-bearing, not style. The obvious form of this loop —
/// index `a[i + l]` under `while i + 8 <= n` — does not vectorize, because the
/// callers pass a query slice whose length LLVM cannot see (it is a `&[i8]`
/// parameter, not a `[i8; EMBED_DIM]`), so `n` stays dynamic, the eight
/// element accesses keep their bounds checks, and the loop is emitted as scalar
/// `ldursb`/`madd`. It shipped that way: the release binary contained no
/// `smull`/`sadalp` at all. `chunks_exact` hands LLVM fixed-size slices it can
/// prove in-bounds, which is the whole precondition. Measured over 1.5M×256
/// rows on 8 threads, the indexed form ran 23.9 ms against 4.35 ms here, and
/// `rank:brute` on the kernel corpus went 27 ms → 16 ms.
///
/// Hand-written NEON was measured too and is not worth it: 4.29 ms against this
/// 4.35 ms, because at 8 threads the scan is memory-bandwidth bound (~88 GB/s),
/// not compute bound. The one-instruction `sdot` would need nightly
/// (`vdotq_s32`, rust-lang#117224) for a win the memory system would eat.
///
/// The arithmetic is integer, so this is bit-identical to the scalar form —
/// changing it left all 114 snapshot cases byte-for-byte unchanged.
#[inline]
pub fn dot_distance_i8(a: &[i8], b: &[i8]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0i32; 16];
    let mut ca = a.chunks_exact(16);
    let mut cb = b.chunks_exact(16);
    for (x, y) in ca.by_ref().zip(cb.by_ref()) {
        for l in 0..16 {
            lanes[l] += x[l] as i32 * y[l] as i32;
        }
    }
    let mut d: i32 = lanes.iter().sum();
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        d += *x as i32 * *y as i32;
    }
    1.0 - d as f32 / (127.0 * 127.0)
}

/// A vector as the index would hand it back: normalized, quantized to i8, then
/// dequantized.
///
/// The cold path computes embeddings in f32 and never stores them, so its
/// diversity reranking used to compare vectors the warm path could not produce —
/// which made cold and warm results differ for reasons that had nothing to do
/// with the query. Putting cold vectors through the same lossy step makes the two
/// paths comparable, which is what "the index is a cache" has to mean.
pub fn as_stored(v: &[f32]) -> Vec<f32> {
    let mut owned = v.to_vec();
    normalize(&mut owned);
    dequantize_i8(&quantize_i8(&owned))
}

/// i8 back to the unit range it was quantized from.
pub fn dequantize_i8(v: &[i8]) -> Vec<f32> {
    v.iter().map(|&x| x as f32 / 127.0).collect()
}
