use rand::prelude::*;
use veldb::simd;

fn rand_vec(rng: &mut impl Rng, n: usize) -> Vec<f32> {
    (0..n).map(|_| rng.random_range(-1.0f32..1.0)).collect()
}

/// f32-сложение не ассоциативно, поэтому NEON и скаляр совпадают не побитово.
/// Проверяем относительную ошибку — она должна быть на уровне единиц ULP.
fn assert_close(a: f32, b: f32, ctx: &str) {
    let scale = a.abs().max(b.abs()).max(1.0);
    assert!((a - b).abs() / scale < 1e-5, "{ctx}: {a} vs {b}");
}

#[test]
fn neon_matches_scalar_on_all_tail_lengths() {
    let mut rng = StdRng::seed_from_u64(42);
    // Ядро идёт по 16 полос; ломается обычно именно хвост, поэтому берём все длины 0..80.
    for n in 0..80 {
        let (a, b) = (rand_vec(&mut rng, n), rand_vec(&mut rng, n));
        assert_close(
            simd::dot(&a, &b),
            simd::dot_scalar(&a, &b),
            &format!("dot n={n}"),
        );
        assert_close(
            simd::l2_sq(&a, &b),
            simd::l2_sq_scalar(&a, &b),
            &format!("l2 n={n}"),
        );
    }
}

#[test]
fn neon_matches_scalar_on_realistic_dims() {
    let mut rng = StdRng::seed_from_u64(7);
    for dim in [128, 384, 768, 1024, 1536] {
        let (a, b) = (rand_vec(&mut rng, dim), rand_vec(&mut rng, dim));
        assert_close(
            simd::dot(&a, &b),
            simd::dot_scalar(&a, &b),
            &format!("dot dim={dim}"),
        );
        assert_close(
            simd::l2_sq(&a, &b),
            simd::l2_sq_scalar(&a, &b),
            &format!("l2 dim={dim}"),
        );
    }
}

#[test]
fn dot_and_l2_known_values() {
    let a = [1.0, 2.0, 3.0];
    let b = [4.0, 5.0, 6.0];
    assert_eq!(simd::dot(&a, &b), 32.0);
    assert_eq!(simd::l2_sq(&a, &b), 27.0);
    assert_eq!(simd::l2_sq(&a, &a), 0.0);
}

#[test]
fn cosine_distance_edges() {
    let a = [1.0, 0.0, 0.0];
    assert_close(simd::cosine_distance(&a, &a), 0.0, "сам с собой");
    assert_close(
        simd::cosine_distance(&a, &[2.0, 0.0, 0.0]),
        0.0,
        "коллинеарные",
    );
    assert_close(
        simd::cosine_distance(&a, &[0.0, 1.0, 0.0]),
        1.0,
        "ортогональные",
    );
    assert_close(
        simd::cosine_distance(&a, &[-1.0, 0.0, 0.0]),
        2.0,
        "противоположные",
    );
    // Нулевой вектор не должен притворяться идеальным совпадением.
    assert_eq!(simd::cosine_distance(&a, &[0.0, 0.0, 0.0]), 1.0);
}

#[test]
fn normalize_is_idempotent_and_safe_on_zero() {
    let mut v = vec![3.0, 4.0, 0.0];
    simd::normalize(&mut v);
    assert_close(simd::norm(&v), 1.0, "норма после нормализации");
    simd::normalize(&mut v);
    assert_close(simd::norm(&v), 1.0, "повторная нормализация");

    let mut z = vec![0.0, 0.0];
    simd::normalize(&mut z);
    assert_eq!(z, vec![0.0, 0.0]);
}

#[test]
fn sum_i64_saturates_instead_of_wrapping() {
    assert_eq!(simd::sum_i64(&[1, 2, 3]), 6);
    assert_eq!(simd::sum_i64(&[]), 0);
    assert_eq!(simd::sum_i64(&[i64::MAX, i64::MAX]), i64::MAX);
    assert_eq!(simd::sum_i64(&[i64::MIN, i64::MIN]), i64::MIN);
}

#[test]
fn min_max_ignores_nan_and_handles_empty() {
    assert_eq!(simd::min_max_i64(&[]), None);
    assert_eq!(simd::min_max_i64(&[5, -2, 9]), Some((-2, 9)));
    assert_eq!(simd::min_max_f64(&[f64::NAN, 1.0, -1.0]), Some((-1.0, 1.0)));
    assert_eq!(simd::min_max_f64(&[f64::NAN]), None);
}
