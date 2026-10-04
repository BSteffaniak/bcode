use super::*;

#[tokio::test]
async fn occupancy_initialization_preserves_existing_rows_and_uses_filtered_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let db = crate::db_connection::init_turso_local_with_retry(&temp.path().join("schema.db"))
        .await
        .unwrap();
    db.create_table("events")
        .column(schema_column("event_seq", DataType::Int, false))
        .primary_key("event_seq")
        .column(schema_column("event_type", DataType::Text, false))
        .execute(&*db)
        .await
        .unwrap();
    db.create_table("projection_checkpoints")
        .column(schema_column("projection_name", DataType::Text, false))
        .primary_key("projection_name")
        .column(schema_column("last_event_seq", DataType::Int, false))
        .column(schema_column("projection_version", DataType::Int, false))
        .column(schema_column("updated_at_ms", DataType::Int, false))
        .execute(&*db)
        .await
        .unwrap();
    for (seq, kind) in [
        (2, "model_changed"),
        (8, "context_compacted"),
        (13, "other"),
    ] {
        db.insert("events")
            .value("event_seq", seq)
            .value("event_type", kind)
            .execute(&*db)
            .await
            .unwrap();
    }
    InitializeOccupancy.execute(&*db).await.unwrap();
    let row = db
        .select("context_occupancy_projection")
        .execute_first(&*db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get("context_epoch"), Some(DatabaseValue::Int64(8)));
    let checkpoint = db
        .select("projection_checkpoints")
        .execute_first(&*db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint.get("last_event_seq"),
        Some(DatabaseValue::Int64(13))
    );
    db.update("context_occupancy_projection")
        .value("schema_version", 99)
        .execute(&*db)
        .await
        .unwrap();
    InitializeOccupancy.execute(&*db).await.unwrap();
    let row = db
        .select("context_occupancy_projection")
        .execute_first(&*db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get("schema_version"), Some(DatabaseValue::Int64(99)));
    db.close().await.unwrap();
}
