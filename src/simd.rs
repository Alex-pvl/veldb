//! Векторные ядра.
//!
//! Целевая платформа — aarch64, где NEON входит в базовый ISA: рантайм-детекта не нужно,
//! хватает `cfg(target_arch)`. Явные интринсики держим только там, где это горячий путь
//! KNN-поиска (`dot`, `l2_sq`); остальное — обычные циклы, LLVM их и так разворачивает
//! в `addv`/`fadd` не хуже ручного кода.

/// Число `f32`, обрабатываемых за итерацию основного цикла: 4 регистра по 4 полосы.
const LANES: usize = 16;

/// Скалярное произведение. `a` и `b` должны быть одной длины.
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON — часть базового aarch64, длины сверены выше.
        unsafe { dot_neon(a, b) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        dot_scalar(a, b)
    }
}

/// Квадрат евклидова расстояния. Корень не берём: для сортировки top-k он не нужен.
#[inline]
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: см. `dot`.
        unsafe { l2_sq_neon(a, b) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        l2_sq_scalar(a, b)
    }
}

/// Косинусное расстояние: `1 - cos(a,b)`. Меньше — ближе, как у l2.
#[inline]
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot_ab, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot_ab += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = (na * nb).sqrt();
    if denom == 0.0 {
        // Нулевой вектор не имеет направления: считаем максимально далёким,
        // иначе он «выигрывает» любой поиск с расстоянием 0.
        return 1.0;
    }
    1.0 - dot_ab / denom
}

pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// Нормализация на месте. Нулевой вектор оставляем как есть.
pub fn normalize(a: &mut [f32]) {
    let n = norm(a);
    if n > 0.0 {
        let inv = 1.0 / n;
        for x in a.iter_mut() {
            *x *= inv;
        }
    }
}

pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn l2_sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::LANES;
    use std::arch::aarch64::*;

    /// # Safety
    /// `a.len() == b.len()`. NEON доступен на любом aarch64.
    #[target_feature(enable = "neon")]
    pub unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        // Четыре независимых аккумулятора: FMA на M-серии имеет латентность ~4 такта,
        // одного аккумулятора не хватает, чтобы забить конвейер.
        let mut acc = [vdupq_n_f32(0.0); 4];
        let mut i = 0;
        while i + LANES <= n {
            for (k, a_k) in acc.iter_mut().enumerate() {
                let off = i + k * 4;
                *a_k = vfmaq_f32(*a_k, vld1q_f32(pa.add(off)), vld1q_f32(pb.add(off)));
            }
            i += LANES;
        }
        let mut sum = vaddvq_f32(vaddq_f32(
            vaddq_f32(acc[0], acc[1]),
            vaddq_f32(acc[2], acc[3]),
        ));
        while i < n {
            sum += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        sum
    }

    /// # Safety
    /// См. `dot_neon`.
    #[target_feature(enable = "neon")]
    pub unsafe fn l2_sq_neon(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = [vdupq_n_f32(0.0); 4];
        let mut i = 0;
        while i + LANES <= n {
            for (k, a_k) in acc.iter_mut().enumerate() {
                let off = i + k * 4;
                let d = vsubq_f32(vld1q_f32(pa.add(off)), vld1q_f32(pb.add(off)));
                *a_k = vfmaq_f32(*a_k, d, d);
            }
            i += LANES;
        }
        let mut sum = vaddvq_f32(vaddq_f32(
            vaddq_f32(acc[0], acc[1]),
            vaddq_f32(acc[2], acc[3]),
        ));
        while i < n {
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

#[cfg(target_arch = "aarch64")]
pub use neon::{dot_neon, l2_sq_neon};

// --- агрегаты по колонкам ---------------------------------------------------
// ponytail: обычные циклы. Проверено на godbolt — LLVM выдаёт те же ld1/add/addv,
// что и ручные интринсики. Переписываем, если профиль скажет иначе.

/// Сумма с насыщением вместо переполнения: агрегат по 100M строк не должен
/// молча уходить в отрицательные числа.
pub fn sum_i64(v: &[i64]) -> i64 {
    v.iter().fold(0i64, |a, &b| a.saturating_add(b))
}

pub fn sum_f64(v: &[f64]) -> f64 {
    v.iter().sum()
}

pub fn min_max_i64(v: &[i64]) -> Option<(i64, i64)> {
    let mut it = v.iter().copied();
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), x| (lo.min(x), hi.max(x))))
}

pub fn min_max_f64(v: &[f64]) -> Option<(f64, f64)> {
    let mut it = v.iter().copied().filter(|x| !x.is_nan());
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), x| (lo.min(x), hi.max(x))))
}
