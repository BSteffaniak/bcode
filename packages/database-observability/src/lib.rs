#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Centralized, value-blind instrumentation for Switchy database operations.

use async_trait::async_trait;
use bcode_metrics::{DatabaseMetrics, DatabaseOperation};
use std::sync::Arc;
use std::time::Instant;
use switchy::database::query::{
    DeleteStatement, InsertStatement, SelectQuery, UpdateStatement, UpsertMultiStatement,
    UpsertStatement,
};
use switchy::database::schema;
use switchy::database::{
    Database, DatabaseError, DatabaseTransaction, DatabaseValue, Row, Savepoint,
};

fn elapsed_ms(started: Option<Instant>) -> u64 {
    started.map_or(0, |started| {
        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
    })
}

/// A database decorator that records stable operation metadata without SQL or values.
#[derive(Debug)]
pub struct ObservedDatabase {
    inner: Arc<Box<dyn Database>>,
    metrics: DatabaseMetrics,
}

#[derive(Debug)]
struct ObservedTransaction {
    inner: Box<dyn DatabaseTransaction>,
    metrics: DatabaseMetrics,
}

impl ObservedTransaction {
    fn record<T>(
        &self,
        operation: DatabaseOperation,
        table: Option<&str>,
        started: Option<Instant>,
        result: &Result<T, DatabaseError>,
    ) {
        self.metrics.record(
            operation,
            table,
            "active",
            result.is_ok(),
            elapsed_ms(started),
        );
    }
}

struct ObservedSavepoint {
    inner: Box<dyn Savepoint>,
    metrics: DatabaseMetrics,
}

impl ObservedDatabase {
    /// Wrap a database with centralized observability.
    #[must_use]
    pub fn new(
        inner: Box<dyn Database>,
        metrics: bcode_metrics::MetricsRegistry,
        role: impl Into<String>,
        backend: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            metrics: DatabaseMetrics::new(metrics, role, backend),
        }
    }

    fn record<T>(
        &self,
        operation: DatabaseOperation,
        table: Option<&str>,
        started: Option<Instant>,
        result: &Result<T, DatabaseError>,
    ) {
        self.metrics.record(
            operation,
            table,
            "none",
            result.is_ok(),
            elapsed_ms(started),
        );
    }
}

#[async_trait]
impl Database for ObservedDatabase {
    async fn query(&self, query: &SelectQuery<'_>) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query(query).await;
        self.record(
            DatabaseOperation::Select,
            Some(query.table_name),
            started,
            &result,
        );
        result
    }

    async fn query_first(&self, query: &SelectQuery<'_>) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_first(query).await;
        self.record(
            DatabaseOperation::Select,
            Some(query.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_update(
        &self,
        statement: &UpdateStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_update(statement).await;
        self.record(
            DatabaseOperation::Update,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_update_first(
        &self,
        statement: &UpdateStatement<'_>,
    ) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_update_first(statement).await;
        self.record(
            DatabaseOperation::Update,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_insert(&self, statement: &InsertStatement<'_>) -> Result<Row, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_insert(statement).await;
        self.record(
            DatabaseOperation::Insert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert(
        &self,
        statement: &UpsertStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert_first(
        &self,
        statement: &UpsertStatement<'_>,
    ) -> Result<Row, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert_first(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert_multi(
        &self,
        statement: &UpsertMultiStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert_multi(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_delete(
        &self,
        statement: &DeleteStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_delete(statement).await;
        self.record(
            DatabaseOperation::Delete,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_delete_first(
        &self,
        statement: &DeleteStatement<'_>,
    ) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_delete_first(statement).await;
        self.record(
            DatabaseOperation::Delete,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_raw(&self, statement: &str) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_raw(statement).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw(&self, query: &str) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_raw(query).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn exec_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<u64, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_raw_params(query, params).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_raw_params(query, params).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn begin_transaction(&self) -> Result<Box<dyn DatabaseTransaction>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.begin_transaction().await;
        self.record(DatabaseOperation::Begin, None, started, &result);
        result.map(|inner| {
            Box::new(ObservedTransaction {
                inner,
                metrics: self.metrics.clone(),
            }) as Box<dyn DatabaseTransaction>
        })
    }

    async fn exec_create_table(
        &self,
        statement: &schema::CreateTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_create_table(statement).await;
        self.record(
            DatabaseOperation::CreateTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_drop_table(
        &self,
        statement: &schema::DropTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_drop_table(statement).await;
        self.record(
            DatabaseOperation::DropTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_create_index(
        &self,
        statement: &schema::CreateIndexStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_create_index(statement).await;
        self.record(
            DatabaseOperation::CreateIndex,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_drop_index(
        &self,
        statement: &schema::DropIndexStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_drop_index(statement).await;
        self.record(
            DatabaseOperation::DropIndex,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_alter_table(
        &self,
        statement: &schema::AlterTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_alter_table(statement).await;
        self.record(
            DatabaseOperation::AlterTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn table_exists(&self, table_name: &str) -> Result<bool, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.table_exists(table_name).await;
        self.record(
            DatabaseOperation::TableExists,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn list_tables(&self) -> Result<Vec<String>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.list_tables().await;
        self.record(DatabaseOperation::ListTables, None, started, &result);
        result
    }
    async fn get_table_info(
        &self,
        table_name: &str,
    ) -> Result<Option<schema::TableInfo>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.get_table_info(table_name).await;
        self.record(
            DatabaseOperation::TableInfo,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn get_table_columns(
        &self,
        table_name: &str,
    ) -> Result<Vec<schema::ColumnInfo>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.get_table_columns(table_name).await;
        self.record(
            DatabaseOperation::TableColumns,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn column_exists(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<bool, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.column_exists(table_name, column_name).await;
        self.record(
            DatabaseOperation::ColumnExists,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    fn trigger_close(&self) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.trigger_close();
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn close(&self) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.close().await;
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn clear_connection_cache(&self) {
        let started = self.metrics.started_at();
        self.inner.clear_connection_cache().await;
        self.metrics.record(
            DatabaseOperation::ClearConnectionCache,
            None,
            "none",
            true,
            elapsed_ms(started),
        );
    }
}

#[async_trait]
impl Database for ObservedTransaction {
    async fn query(&self, query: &SelectQuery<'_>) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query(query).await;
        self.record(
            DatabaseOperation::Select,
            Some(query.table_name),
            started,
            &result,
        );
        result
    }

    async fn query_first(&self, query: &SelectQuery<'_>) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_first(query).await;
        self.record(
            DatabaseOperation::Select,
            Some(query.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_update(
        &self,
        statement: &UpdateStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_update(statement).await;
        self.record(
            DatabaseOperation::Update,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_update_first(
        &self,
        statement: &UpdateStatement<'_>,
    ) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_update_first(statement).await;
        self.record(
            DatabaseOperation::Update,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_insert(&self, statement: &InsertStatement<'_>) -> Result<Row, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_insert(statement).await;
        self.record(
            DatabaseOperation::Insert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert(
        &self,
        statement: &UpsertStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert_first(
        &self,
        statement: &UpsertStatement<'_>,
    ) -> Result<Row, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert_first(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_upsert_multi(
        &self,
        statement: &UpsertMultiStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_upsert_multi(statement).await;
        self.record(
            DatabaseOperation::Upsert,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_delete(
        &self,
        statement: &DeleteStatement<'_>,
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_delete(statement).await;
        self.record(
            DatabaseOperation::Delete,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_delete_first(
        &self,
        statement: &DeleteStatement<'_>,
    ) -> Result<Option<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_delete_first(statement).await;
        self.record(
            DatabaseOperation::Delete,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }

    async fn exec_raw(&self, statement: &str) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_raw(statement).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw(&self, query: &str) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_raw(query).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn exec_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<u64, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_raw_params(query, params).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.query_raw_params(query, params).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn begin_transaction(&self) -> Result<Box<dyn DatabaseTransaction>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.begin_transaction().await;
        self.record(DatabaseOperation::Begin, None, started, &result);
        result.map(|inner| {
            Box::new(Self {
                inner,
                metrics: self.metrics.clone(),
            }) as Box<dyn DatabaseTransaction>
        })
    }

    async fn exec_create_table(
        &self,
        statement: &schema::CreateTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_create_table(statement).await;
        self.record(
            DatabaseOperation::CreateTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_drop_table(
        &self,
        statement: &schema::DropTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_drop_table(statement).await;
        self.record(
            DatabaseOperation::DropTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_create_index(
        &self,
        statement: &schema::CreateIndexStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_create_index(statement).await;
        self.record(
            DatabaseOperation::CreateIndex,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_drop_index(
        &self,
        statement: &schema::DropIndexStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_drop_index(statement).await;
        self.record(
            DatabaseOperation::DropIndex,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn exec_alter_table(
        &self,
        statement: &schema::AlterTableStatement<'_>,
    ) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.exec_alter_table(statement).await;
        self.record(
            DatabaseOperation::AlterTable,
            Some(statement.table_name),
            started,
            &result,
        );
        result
    }
    async fn table_exists(&self, table_name: &str) -> Result<bool, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.table_exists(table_name).await;
        self.record(
            DatabaseOperation::TableExists,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn list_tables(&self) -> Result<Vec<String>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.list_tables().await;
        self.record(DatabaseOperation::ListTables, None, started, &result);
        result
    }
    async fn get_table_info(
        &self,
        table_name: &str,
    ) -> Result<Option<schema::TableInfo>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.get_table_info(table_name).await;
        self.record(
            DatabaseOperation::TableInfo,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn get_table_columns(
        &self,
        table_name: &str,
    ) -> Result<Vec<schema::ColumnInfo>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.get_table_columns(table_name).await;
        self.record(
            DatabaseOperation::TableColumns,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    async fn column_exists(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<bool, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.column_exists(table_name, column_name).await;
        self.record(
            DatabaseOperation::ColumnExists,
            Some(table_name),
            started,
            &result,
        );
        result
    }
    fn trigger_close(&self) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.trigger_close();
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn close(&self) -> Result<(), DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.close().await;
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn clear_connection_cache(&self) {
        let started = self.metrics.started_at();
        self.inner.clear_connection_cache().await;
        self.metrics.record(
            DatabaseOperation::ClearConnectionCache,
            None,
            "active",
            true,
            elapsed_ms(started),
        );
    }
}

#[async_trait]
impl DatabaseTransaction for ObservedTransaction {
    async fn commit(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = metrics.started_at();
        let result = inner.commit().await;
        metrics.record(
            DatabaseOperation::Commit,
            None,
            "active",
            result.is_ok(),
            elapsed_ms(started),
        );
        result
    }

    async fn rollback(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = metrics.started_at();
        let result = inner.rollback().await;
        metrics.record(
            DatabaseOperation::Rollback,
            None,
            "active",
            result.is_ok(),
            elapsed_ms(started),
        );
        result
    }

    async fn savepoint(&self, name: &str) -> Result<Box<dyn Savepoint>, DatabaseError> {
        let started = self.metrics.started_at();
        let result = self.inner.savepoint(name).await;
        self.record(DatabaseOperation::Savepoint, None, started, &result);
        result.map(|inner| {
            Box::new(ObservedSavepoint {
                inner,
                metrics: self.metrics.clone(),
            }) as Box<dyn Savepoint>
        })
    }

    async fn find_cascade_targets(
        &self,
        table_name: &str,
    ) -> Result<schema::DropPlan, DatabaseError> {
        self.inner.find_cascade_targets(table_name).await
    }

    async fn has_any_dependents(&self, table_name: &str) -> Result<bool, DatabaseError> {
        self.inner.has_any_dependents(table_name).await
    }

    async fn get_direct_dependents(
        &self,
        table_name: &str,
    ) -> Result<std::collections::BTreeSet<String>, DatabaseError> {
        self.inner.get_direct_dependents(table_name).await
    }
}

#[async_trait]
impl Savepoint for ObservedSavepoint {
    async fn release(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = metrics.started_at();
        let result = inner.release().await;
        metrics.record(
            DatabaseOperation::SavepointRelease,
            None,
            "active",
            result.is_ok(),
            elapsed_ms(started),
        );
        result
    }

    async fn rollback_to(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = metrics.started_at();
        let result = inner.rollback_to().await;
        metrics.record(
            DatabaseOperation::SavepointRollback,
            None,
            "active",
            result.is_ok(),
            elapsed_ms(started),
        );
        result
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_metrics::MetricsRegistry;
    use switchy::database::query::{delete, insert, select, update, upsert};
    use switchy::database::rusqlite::RusqliteDatabase;
    use switchy::database::schema::{
        Column, DataType, alter_table, create_index, create_table, drop_index, drop_table,
    };

    fn observed_with_registry(metrics: MetricsRegistry) -> ObservedDatabase {
        let connection = rusqlite::Connection::open_in_memory().expect("in-memory sqlite");
        let database = RusqliteDatabase::new(vec![Arc::new(switchy::unsync::sync::Mutex::new(
            connection,
        ))]);
        ObservedDatabase::new(Box::new(database), metrics, "test", "sqlite")
    }

    fn observed(metrics: &MetricsRegistry) -> ObservedDatabase {
        observed_with_registry(metrics.clone())
    }

    async fn run_benchmark_workload(database: &dyn Database, samples: u32) {
        database
            .exec_create_table(
                &create_table("benchmark_items")
                    .column(Column {
                        name: "id".to_owned(),
                        nullable: false,
                        auto_increment: false,
                        data_type: DataType::Int,
                        default: None,
                    })
                    .primary_key("id"),
            )
            .await
            .expect("benchmark table");
        for value in 0..samples {
            database
                .exec_insert(&insert("benchmark_items").value("id", i64::from(value)))
                .await
                .expect("benchmark insert");
            database
                .query(&select("benchmark_items").columns(&["id"]).limit(1))
                .await
                .expect("benchmark select");
            database
                .exec_delete(&delete("benchmark_items"))
                .await
                .expect("benchmark delete");
        }
    }

    fn direct_database() -> RusqliteDatabase {
        let connection = rusqlite::Connection::open_in_memory().expect("direct sqlite");
        RusqliteDatabase::new(vec![Arc::new(switchy::unsync::sync::Mutex::new(
            connection,
        ))])
    }

    async fn benchmark_elapsed(database: &dyn Database, samples: u32) -> u128 {
        let started = Instant::now();
        run_benchmark_workload(database, samples).await;
        started.elapsed().as_nanos()
    }

    fn median(values: &mut [u128]) -> u128 {
        values.sort_unstable();
        values[values.len() / 2]
    }

    #[tokio::test]
    #[ignore = "manual release benchmark"]
    async fn benchmark_instrumentation_overhead() {
        const SAMPLES: u32 = 2_000;
        const ROUNDS: usize = 9;
        let mut direct_rounds = Vec::with_capacity(ROUNDS);
        let mut disabled_rounds = Vec::with_capacity(ROUNDS);
        let mut enabled_rounds = Vec::with_capacity(ROUNDS);

        for round in 0..ROUNDS {
            let direct = direct_database();
            let disabled = observed_with_registry(MetricsRegistry::disabled());
            let enabled = observed_with_registry(MetricsRegistry::in_memory());
            match round % 3 {
                0 => {
                    direct_rounds.push(benchmark_elapsed(&direct, SAMPLES).await);
                    disabled_rounds.push(benchmark_elapsed(&disabled, SAMPLES).await);
                    enabled_rounds.push(benchmark_elapsed(&enabled, SAMPLES).await);
                }
                1 => {
                    enabled_rounds.push(benchmark_elapsed(&enabled, SAMPLES).await);
                    direct_rounds.push(benchmark_elapsed(&direct, SAMPLES).await);
                    disabled_rounds.push(benchmark_elapsed(&disabled, SAMPLES).await);
                }
                _ => {
                    disabled_rounds.push(benchmark_elapsed(&disabled, SAMPLES).await);
                    enabled_rounds.push(benchmark_elapsed(&enabled, SAMPLES).await);
                    direct_rounds.push(benchmark_elapsed(&direct, SAMPLES).await);
                }
            }
        }
        let direct_ns = median(&mut direct_rounds);
        let disabled_ns = median(&mut disabled_rounds);
        let enabled_ns = median(&mut enabled_rounds);

        let overhead_percent =
            |observed: u128| observed.saturating_sub(direct_ns).saturating_mul(10_000) / direct_ns;
        eprintln!(
            "database observability benchmark ({ROUNDS} median rounds x {SAMPLES} insert/select/delete cycles): direct={} ns/cycle, disabled={} ns/cycle ({}.{:02}%), enabled={} ns/cycle ({}.{:02}%)",
            direct_ns / u128::from(SAMPLES),
            disabled_ns / u128::from(SAMPLES),
            overhead_percent(disabled_ns) / 100,
            overhead_percent(disabled_ns) % 100,
            enabled_ns / u128::from(SAMPLES),
            overhead_percent(enabled_ns) / 100,
            overhead_percent(enabled_ns) % 100,
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn typed_raw_schema_and_transaction_operations_are_observed() {
        let metrics = MetricsRegistry::default();
        let database = observed(&metrics);
        database
            .exec_create_table(
                &create_table("items")
                    .column(Column {
                        name: "id".to_owned(),
                        nullable: false,
                        auto_increment: false,
                        data_type: DataType::Int,
                        default: None,
                    })
                    .column(Column {
                        name: "name".to_owned(),
                        nullable: false,
                        auto_increment: false,
                        data_type: DataType::Text,
                        default: None,
                    })
                    .primary_key("id"),
            )
            .await
            .expect("create table");
        database
            .exec_insert(&insert("items").value("id", 1).value("name", "first"))
            .await
            .expect("insert");
        database
            .exec_update(&update("items").value("name", "updated"))
            .await
            .expect("update");
        database
            .exec_upsert(
                &upsert("items")
                    .value("id", 1)
                    .value("name", "upserted")
                    .unique(&["id"]),
            )
            .await
            .expect("upsert");
        database
            .exec_insert(&insert("items").value("id", 3).value("name", "deleted"))
            .await
            .expect("insert deleted row");
        database
            .exec_delete(&delete("items").limit(1))
            .await
            .expect("delete");
        database
            .exec_create_index(&create_index("items_name_idx").table("items").column("name"))
            .await
            .expect("create index");
        assert!(database.table_exists("items").await.expect("table exists"));
        assert!(
            database
                .column_exists("items", "name")
                .await
                .expect("column exists")
        );
        assert!(
            !database
                .list_tables()
                .await
                .expect("list tables")
                .is_empty()
        );
        assert!(
            database
                .get_table_info("items")
                .await
                .expect("table info")
                .is_some()
        );
        assert!(
            !database
                .get_table_columns("items")
                .await
                .expect("table columns")
                .is_empty()
        );
        assert_eq!(
            database
                .query(&select("items"))
                .await
                .expect("select")
                .len(),
            1
        );
        database
            .query_raw("INVALID secret-user-value")
            .await
            .expect_err("invalid raw query should fail");
        database
            .exec_raw("INVALID raw-exec-secret")
            .await
            .expect_err("invalid raw exec should fail");
        database
            .exec_raw_params(
                "INVALID parameterized-exec-secret",
                &[DatabaseValue::String("exec-parameter-secret".to_owned())],
            )
            .await
            .expect_err("invalid parameterized raw exec should fail");
        database
            .query_raw_params(
                "INVALID parameterized-query-secret",
                &[DatabaseValue::String("parameter-value-secret".to_owned())],
            )
            .await
            .expect_err("invalid parameterized raw query should fail");
        let transaction = database.begin_transaction().await.expect("begin");
        transaction
            .query(&select("missing_table"))
            .await
            .expect_err("missing transaction table should fail");
        transaction
            .exec_insert(&insert("items").value("id", 2).value("name", "second"))
            .await
            .expect("transaction insert");
        let savepoint = transaction.savepoint("safe_name").await.expect("savepoint");
        savepoint.release().await.expect("release savepoint");
        transaction.commit().await.expect("commit");
        let transaction = database.begin_transaction().await.expect("begin rollback");
        let savepoint = transaction
            .savepoint("rollback_name")
            .await
            .expect("rollback savepoint");
        savepoint
            .rollback_to()
            .await
            .expect("rollback to savepoint");
        transaction.rollback().await.expect("rollback transaction");
        database
            .exec_drop_index(&drop_index("items_name_idx", "items"))
            .await
            .expect("drop index");
        database
            .exec_alter_table(&alter_table("items").add_column(
                "extra".to_owned(),
                DataType::Text,
                true,
                None,
            ))
            .await
            .expect("alter table");
        database
            .exec_create_table(&create_table("discarded").column(Column {
                name: "id".to_owned(),
                nullable: false,
                auto_increment: false,
                data_type: DataType::Int,
                default: None,
            }))
            .await
            .expect("create discarded table");
        database
            .exec_drop_table(&drop_table("discarded"))
            .await
            .expect("drop table");

        let report = metrics.report();
        assert!(
            report
                .snapshot
                .counters
                .get("database.operation.total")
                .is_some_and(|count| *count >= 28)
        );
        let descriptors = &report.descriptors;
        let aggregate = descriptors
            .get("database.operation.total")
            .expect("aggregate descriptor");
        assert_eq!(
            aggregate.label_keys,
            vec![
                "database_backend".to_owned(),
                "database_role".to_owned(),
                "operation".to_owned(),
                "outcome".to_owned(),
                "table".to_owned(),
                "transaction".to_owned(),
            ]
        );
        let serialized = serde_json::to_string(&report).expect("serialize report");
        assert!(!serialized.contains("secret-user-value"));
        assert!(!serialized.contains("raw-exec-secret"));
        assert!(!serialized.contains("parameterized-exec-secret"));
        assert!(!serialized.contains("exec-parameter-secret"));
        assert!(!serialized.contains("parameterized-query-secret"));
        assert!(!serialized.contains("parameter-value-secret"));
        assert!(!serialized.contains("second"));
        assert!(!serialized.contains("SELECT *"));
        assert!(serialized.contains("raw_query"));

        let timeline_events = report
            .events
            .iter()
            .filter(|event| event.name == "database.operation.timeline")
            .collect::<Vec<_>>();
        assert!(!timeline_events.is_empty());
        for event in &timeline_events {
            assert_eq!(
                event.labels.get("database_role").map(String::as_str),
                Some("test")
            );
            assert_eq!(
                event.labels.get("database_backend").map(String::as_str),
                Some("sqlite")
            );
            assert_eq!(
                event.labels.get("outcome").map(String::as_str),
                Some("error")
            );
            assert!(event.labels.contains_key("operation"));
            assert!(event.labels.contains_key("transaction"));
        }
        let raw = timeline_events
            .iter()
            .find(|event| event.labels.get("operation").map(String::as_str) == Some("raw_query"))
            .expect("raw query timeline event");
        assert_eq!(
            raw.labels.get("transaction").map(String::as_str),
            Some("none")
        );
        assert!(!raw.labels.contains_key("table"));
        let transaction_select = timeline_events
            .iter()
            .find(|event| {
                event.labels.get("operation").map(String::as_str) == Some("select")
                    && event.labels.get("transaction").map(String::as_str) == Some("active")
            })
            .expect("transaction select timeline event");
        assert_eq!(
            transaction_select.labels.get("table").map(String::as_str),
            Some("missing_table")
        );
    }
}
