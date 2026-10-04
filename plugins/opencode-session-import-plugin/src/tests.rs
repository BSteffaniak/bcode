use super::*;
use futures::executor::block_on;
use switchy_database::{
    Database,
    schema::{Column, DataType},
};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

fn fixture(path: &Path) {
    let db = open_sqlite_rusqlite(
        path,
        &SqliteOpenOptions {
            access: SqliteAccess::ReadWriteCreate,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: false,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .unwrap();
    block_on(async {
        for (table, columns) in [
            (
                "session",
                vec![
                    ("id", DataType::Text),
                    ("title", DataType::Text),
                    ("directory", DataType::Text),
                    ("time_created", DataType::BigInt),
                    ("time_updated", DataType::BigInt),
                    ("time_archived", DataType::BigInt),
                ],
            ),
            (
                "message",
                vec![
                    ("id", DataType::Text),
                    ("session_id", DataType::Text),
                    ("time_created", DataType::BigInt),
                    ("data", DataType::Text),
                ],
            ),
            (
                "part",
                vec![
                    ("id", DataType::Text),
                    ("message_id", DataType::Text),
                    ("time_created", DataType::BigInt),
                    ("data", DataType::Text),
                ],
            ),
        ] {
            let mut query = db.create_table(table);
            for (name, data_type) in columns {
                query = query.column(Column {
                    name: name.into(),
                    data_type,
                    nullable: true,
                    auto_increment: false,
                    default: None,
                });
            }
            query.execute(&db).await.unwrap();
        }
        for (id, updated, archived) in [
            ("older", 10, None),
            ("newer", 20, None),
            ("archived", 30, Some(30)),
        ] {
            db.insert("session")
                .value("id", id)
                .value("title", " title ")
                .value("directory", "/project")
                .value("time_created", 1)
                .value("time_updated", updated)
                .value("time_archived", archived)
                .execute(&db)
                .await
                .unwrap();
        }
        for (id, session) in [("m1", "newer"), ("m2", "newer"), ("m3", "archived")] {
            db.insert("message")
                .value("id", id)
                .value("session_id", session)
                .value("time_created", 2)
                .value("data", r#"{"role":"user"}"#)
                .execute(&db)
                .await
                .unwrap();
        }
        db.insert("part")
            .value("id", "p1")
            .value("message_id", "m1")
            .value("time_created", 3)
            .value("data", r#"{"type":"text","text":"hello"}"#)
            .execute(&db)
            .await
            .unwrap();
        db.close().await.unwrap();
    });
}

#[test]
fn discovery_and_selected_load_preserve_source_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    fixture(&path);
    let before = std::fs::read(&path).unwrap();
    let summaries = discover_database_sessions(&path, None).unwrap();
    assert_eq!(
        summaries
            .iter()
            .map(|s| s.external_session_id.as_str())
            .collect::<Vec<_>>(),
        ["newer", "older"]
    );
    assert_eq!(summaries[0].message_count, Some(2));
    assert_eq!(summaries[1].message_count, Some(0));
    assert_eq!(summaries[0].title.as_deref(), Some("title"));
    let loaded = load_opencode_session(path.to_str().unwrap(), "newer").unwrap();
    assert!(!loaded.events.is_empty());
    assert_eq!(loaded.summary.message_count, Some(2));
    assert!(load_opencode_session(path.to_str().unwrap(), "missing").is_err());
    assert!(load_opencode_session(path.to_str().unwrap(), "archived").is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn missing_source_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    assert!(open_read_only(&path).is_err());
    assert!(!path.exists());
}
