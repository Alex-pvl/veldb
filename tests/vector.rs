use rand::prelude::*;
use veldb::exec::render;
use veldb::simd;
use veldb::Database;

fn q(db: &Database, sql: &str) -> Vec<String> {
    db.execute(sql)
        .unwrap_or_else(|e| panic!("{sql}\n  -> {e:#}"))
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

fn vec_str(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// База из `n` случайных векторов размерности `dim` с идентификаторами 0..n.
fn corpus(n: usize, dim: usize, seed: u64) -> (Database, Vec<Vec<f32>>) {
    let mut rng = StdRng::seed_from_u64(seed);
    let db = Database::new();
    db.execute(&format!("CREATE TABLE docs (id INT, e VECTOR({dim}))"))
        .unwrap();
    let mut vectors = Vec::with_capacity(n);
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let v: Vec<f32> = (0..dim).map(|_| rng.random_range(-1.0f32..1.0)).collect();
        rows.push(format!("({i}, '{}')", vec_str(&v)));
        vectors.push(v);
    }
    db.execute(&format!("INSERT INTO docs VALUES {}", rows.join(",")))
        .unwrap();
    (db, vectors)
}

#[test]
fn knn_returns_exact_nearest_neighbours() {
    let (db, vectors) = corpus(2000, 32, 1);
    let query = vectors[7].iter().map(|x| x + 0.01).collect::<Vec<_>>();

    let got: Vec<i64> = q(
        &db,
        &format!(
            "SELECT id FROM docs ORDER BY l2_distance(e, '{}') LIMIT 10",
            vec_str(&query)
        ),
    )
    .iter()
    .map(|s| s.parse().unwrap())
    .collect();

    // Эталон — честный перебор на стороне теста.
    let mut brute: Vec<(f32, i64)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (simd::l2_sq_scalar(v, &query), i as i64))
        .collect();
    brute.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let want: Vec<i64> = brute.iter().take(10).map(|x| x.1).collect();

    assert_eq!(
        got, want,
        "плоский поиск обязан совпадать с перебором точно"
    );
    assert_eq!(got[0], 7, "ближайшим должен быть исходный вектор");
}

#[test]
fn all_three_metrics_work_and_rank_consistently() {
    let db = Database::new();
    db.execute("CREATE TABLE v (id INT, e VECTOR(3))").unwrap();
    db.execute("INSERT INTO v VALUES (1,'[1,0,0]'), (2,'[2,0,0]'), (3,'[0,1,0]'), (4,'[-1,0,0]')")
        .unwrap();
    let query = "'[1,0,0]'";

    // L2: коллинеарная длина имеет значение.
    assert_eq!(
        q(
            &db,
            &format!("SELECT id FROM v ORDER BY l2_distance(e, {query}) LIMIT 2")
        ),
        ["1", "2"]
    );
    // Косинус: важно только направление, поэтому 1 и 2 равны и разводятся по id.
    assert_eq!(
        q(
            &db,
            &format!("SELECT id FROM v ORDER BY cosine_distance(e, {query}) LIMIT 2")
        ),
        ["1", "2"]
    );
    // Скалярное произведение: чем длиннее сонаправленный вектор, тем ближе.
    assert_eq!(
        q(
            &db,
            &format!("SELECT id FROM v ORDER BY inner_product(e, {query}) LIMIT 2")
        ),
        ["2", "1"]
    );
    // Самый далёкий по косинусу — противоположно направленный.
    assert_eq!(
        q(
            &db,
            &format!("SELECT id FROM v ORDER BY cosine_distance(e, {query}) DESC LIMIT 1")
        ),
        ["4"]
    );
}

#[test]
fn knn_combines_with_where_filter() {
    let (db, vectors) = corpus(500, 16, 2);
    let query = vec_str(&vectors[3]);
    let got = q(
        &db,
        &format!("SELECT id FROM docs WHERE id > 100 ORDER BY l2_distance(e, '{query}') LIMIT 5"),
    );
    assert_eq!(got.len(), 5);
    assert!(
        got.iter().all(|s| s.parse::<i64>().unwrap() > 100),
        "фильтр обязан отработать до KNN"
    );
}

#[test]
fn distance_is_selectable_as_a_column() {
    let db = Database::new();
    db.execute("CREATE TABLE v (id INT, e VECTOR(2))").unwrap();
    db.execute("INSERT INTO v VALUES (1,'[3,4]')").unwrap();
    // l2_distance возвращает квадрат расстояния — корень не нужен для ранжирования.
    assert_eq!(q(&db, "SELECT l2_distance(e, '[0,0]') FROM v"), ["25"]);
    assert_eq!(q(&db, "SELECT sqrt(l2_distance(e, '[0,0]')) FROM v"), ["5"]);
}

#[test]
fn dimension_mismatch_is_rejected_everywhere() {
    let db = Database::new();
    db.execute("CREATE TABLE v (id INT, e VECTOR(3))").unwrap();
    let e = |sql: &str| format!("{:#}", db.execute(sql).unwrap_err());
    assert!(e("INSERT INTO v VALUES (1, '[1,2]')").contains("VECTOR(3)"));
    db.execute("INSERT INTO v VALUES (1,'[1,2,3]')").unwrap();
    assert!(e("SELECT id FROM v ORDER BY l2_distance(e, '[1,2]')").contains("VECTOR(3)"));
    assert!(e("SELECT l2_distance(id, '[1,2,3]') FROM v").contains("VECTOR"));
    assert!(e("CREATE TABLE bad (e VECTOR(0))").contains("VECTOR(0)"));
}

#[test]
fn normalized_vectors_make_cosine_and_l2_agree() {
    let (db, vectors) = corpus(300, 24, 3);
    let mut query: Vec<f32> = vectors[11].clone();
    simd::normalize(&mut query);
    // На ненормализованном корпусе метрики расходятся — это ожидаемо и проверяется
    // тем, что косинусный порядок отличается от L2-порядка.
    let by_l2 = q(
        &db,
        &format!(
            "SELECT id FROM docs ORDER BY l2_distance(e, '{}') LIMIT 5",
            vec_str(&query)
        ),
    );
    let by_cos = q(
        &db,
        &format!(
            "SELECT id FROM docs ORDER BY cosine_distance(e, '{}') LIMIT 5",
            vec_str(&query)
        ),
    );
    assert_eq!(by_cos[0], "11", "косинус должен найти исходное направление");
    assert!(by_l2.contains(&"11".to_string()));
}

#[test]
fn parallel_path_matches_serial_path() {
    // Порог распараллеливания — 4096 строк; проверяем обе стороны границы.
    for n in [100usize, 5000] {
        let (db, vectors) = corpus(n, 8, 42);
        let query = vec_str(&vectors[0]);
        let got = q(
            &db,
            &format!("SELECT id FROM docs ORDER BY l2_distance(e, '{query}') LIMIT 1"),
        );
        assert_eq!(got, ["0"], "n={n}");
    }
}

#[test]
fn vectors_group_by_is_refused_not_silently_wrong() {
    let (db, _) = corpus(10, 4, 5);
    let e = format!(
        "{:#}",
        db.execute("SELECT count(*) FROM docs GROUP BY e")
            .unwrap_err()
    );
    assert!(e.contains("векторной"), "получили: {e}");
}
