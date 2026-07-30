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
/// integer FMA stream instead of the three float passes cosine needs, and it
/// autovectorizes; the scan it feeds was 72% of a warm kernel-corpus query.
#[inline]
pub fn dot_distance_i8(a: &[i8], b: &[i8]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0i32; 8];
    let mut i = 0;
    while i + 8 <= n {
        for l in 0..8 {
            lanes[l] += a[i + l] as i32 * b[i + l] as i32;
        }
        i += 8;
    }
    let mut d: i32 = lanes.iter().sum();
    while i < n {
        d += a[i] as i32 * b[i] as i32;
        i += 1;
    }
    1.0 - d as f32 / (127.0 * 127.0)
}
