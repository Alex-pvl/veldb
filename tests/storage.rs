use veldb::{Column, DataType, Schema, StrColumn, Table, Value};

fn schema() -> Schema {
    Schema::new(vec![
        ("id", DataType::I64),
        ("price", DataType::F64),
        ("active", DataType::Bool),
        ("title", DataType::Str),
        ("emb", DataType::Vector(3)),
    ])
}

fn row(id: i64) -> Vec<Value> {
    vec![
        Value::I64(id),
        Value::F64(id as f64 * 1.5),
        Value::Bool(id % 2 == 0),
        Value::Str(format!("товар {id}")),
        Value::Vector(vec![id as f32, 0.0, -1.0]),
    ]
}

#[test]
fn round_trip_all_types() {
    let mut t = Table::new("t", schema()).unwrap();
    for i in 0..100 {
        t.insert(&row(i)).unwrap();
    }
    assert_eq!(t.nrows(), 100);
    for i in 0..100 {
        assert_eq!(t.row(i), row(i as i64), "строка {i} не совпала");
    }
}

#[test]
fn insert_rejects_wrong_type_without_partial_write() {
    let mut t = Table::new("t", schema()).unwrap();
    t.insert(&row(1)).unwrap();
    let mut bad = row(2);
    bad[3] = Value::I64(7); // в TEXT-колонку летит число
    assert!(t.insert(&bad).is_err());
    // Ключевое: отвергнутая строка не оставила «хвостов» в первых колонках.
    assert_eq!(t.nrows(), 1);
    for c in t.columns() {
        assert_eq!(c.len(), 1);
    }
}

#[test]
fn insert_rejects_wrong_arity_and_vector_dim() {
    let mut t = Table::new("t", schema()).unwrap();
    assert!(t.insert(&row(1)[..3]).is_err());
    let mut bad = row(1);
    bad[4] = Value::Vector(vec![1.0, 2.0]);
    assert!(t.insert(&bad).is_err());
    assert_eq!(t.nrows(), 0);
}

#[test]
fn int_literal_widens_into_float_column() {
    let mut t = Table::new("t", Schema::new(vec![("x", DataType::F64)])).unwrap();
    t.insert(&[Value::I64(7)]).unwrap();
    assert_eq!(t.row(0), vec![Value::F64(7.0)]);
}

#[test]
fn schema_rejects_broken_definitions() {
    assert!(Table::new("t", Schema::new(vec![])).is_err());
    assert!(Table::new(
        "t",
        Schema::new(vec![("a", DataType::I64), ("A", DataType::F64)])
    )
    .is_err());
    assert!(Table::new("t", Schema::new(vec![("v", DataType::Vector(0))])).is_err());
}

#[test]
fn column_lookup_is_case_insensitive() {
    let t = Table::new("t", schema()).unwrap();
    assert!(t.column("PRICE").is_some());
    assert!(t.column("Price").is_some());
    assert!(t.column("nope").is_none());
}

#[test]
fn str_column_handles_empty_and_unicode() {
    let mut c = StrColumn::new();
    for s in ["", "привет", "a", "", "🐢🐢"] {
        c.push(s);
    }
    assert_eq!(c.len(), 5);
    assert_eq!(c.get(0), "");
    assert_eq!(c.get(1), "привет");
    assert_eq!(c.get(4), "🐢🐢");
    assert_eq!(
        c.iter().collect::<Vec<_>>(),
        vec!["", "привет", "a", "", "🐢🐢"]
    );
}

#[test]
fn vector_slice_is_zero_copy_and_aligned_to_rows() {
    let mut c = Column::empty(DataType::Vector(4));
    c.push(&Value::Vector(vec![1.0, 2.0, 3.0, 4.0])).unwrap();
    c.push(&Value::Vector(vec![5.0, 6.0, 7.0, 8.0])).unwrap();
    assert_eq!(c.len(), 2);
    assert_eq!(c.vector_at(1).unwrap(), &[5.0, 6.0, 7.0, 8.0]);
    assert!(Column::empty(DataType::I64).vector_at(0).is_none());
}

#[test]
fn bytes_used_tracks_actual_payload() {
    let mut t = Table::new("t", Schema::new(vec![("x", DataType::I64)])).unwrap();
    let before = t.bytes_used();
    t.insert_many(&(0..1000).map(|i| vec![Value::I64(i)]).collect::<Vec<_>>())
        .unwrap();
    assert_eq!(t.bytes_used() - before, 8000);
}
