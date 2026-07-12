#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Centralized, value-blind instrumentation for Switchy database operations.

use async_trait::async_trait;
use bcode_metrics::{DatabaseMetrics, DatabaseOperation};
use std::sync::Arc;
use std::time::Instant;
use switchy_database::query::{
    DeleteStatement, InsertStatement, SelectQuery, UpdateStatement, UpsertMultiStatement,
    UpsertStatement,
};
use switchy_database::schema;
use switchy_database::{
    Database, DatabaseError, DatabaseTransaction, DatabaseValue, Row, Savepoint,
};

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
        started: Instant,
        result: &Result<T, DatabaseError>,
    ) {
        self.metrics.record(
            operation,
            table,
            "active",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
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
        started: Instant,
        result: &Result<T, DatabaseError>,
    ) {
        self.metrics.record(
            operation,
            table,
            "none",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
    }
}

#[async_trait]
impl Database for ObservedDatabase {
    async fn query(&self, query: &SelectQuery<'_>) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.exec_raw(statement).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw(&self, query: &str) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.query_raw(query).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn exec_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<u64, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.exec_raw_params(query, params).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.query_raw_params(query, params).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn begin_transaction(&self) -> Result<Box<dyn DatabaseTransaction>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.list_tables().await;
        self.record(DatabaseOperation::ListTables, None, started, &result);
        result
    }
    async fn get_table_info(
        &self,
        table_name: &str,
    ) -> Result<Option<schema::TableInfo>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.trigger_close();
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn close(&self) -> Result<(), DatabaseError> {
        let started = Instant::now();
        let result = self.inner.close().await;
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn clear_connection_cache(&self) {
        let started = Instant::now();
        self.inner.clear_connection_cache().await;
        self.metrics.record(
            DatabaseOperation::ClearConnectionCache,
            None,
            "none",
            true,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
    }
}

#[async_trait]
impl Database for ObservedTransaction {
    async fn query(&self, query: &SelectQuery<'_>) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.exec_raw(statement).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw(&self, query: &str) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.query_raw(query).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn exec_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<u64, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.exec_raw_params(query, params).await;
        self.record(DatabaseOperation::RawExec, None, started, &result);
        result
    }

    async fn query_raw_params(
        &self,
        query: &str,
        params: &[DatabaseValue],
    ) -> Result<Vec<Row>, DatabaseError> {
        let started = Instant::now();
        let result = self.inner.query_raw_params(query, params).await;
        self.record(DatabaseOperation::RawQuery, None, started, &result);
        result
    }

    async fn begin_transaction(&self) -> Result<Box<dyn DatabaseTransaction>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.list_tables().await;
        self.record(DatabaseOperation::ListTables, None, started, &result);
        result
    }
    async fn get_table_info(
        &self,
        table_name: &str,
    ) -> Result<Option<schema::TableInfo>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
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
        let started = Instant::now();
        let result = self.inner.trigger_close();
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn close(&self) -> Result<(), DatabaseError> {
        let started = Instant::now();
        let result = self.inner.close().await;
        self.record(DatabaseOperation::Close, None, started, &result);
        result
    }
    async fn clear_connection_cache(&self) {
        let started = Instant::now();
        self.inner.clear_connection_cache().await;
        self.metrics.record(
            DatabaseOperation::ClearConnectionCache,
            None,
            "active",
            true,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
    }
}

#[async_trait]
impl DatabaseTransaction for ObservedTransaction {
    async fn commit(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = Instant::now();
        let result = inner.commit().await;
        metrics.record(
            DatabaseOperation::Commit,
            None,
            "active",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        result
    }

    async fn rollback(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = Instant::now();
        let result = inner.rollback().await;
        metrics.record(
            DatabaseOperation::Rollback,
            None,
            "active",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        result
    }

    async fn savepoint(&self, name: &str) -> Result<Box<dyn Savepoint>, DatabaseError> {
        let started = Instant::now();
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
        let started = Instant::now();
        let result = inner.release().await;
        metrics.record(
            DatabaseOperation::SavepointRelease,
            None,
            "active",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        result
    }

    async fn rollback_to(self: Box<Self>) -> Result<(), DatabaseError> {
        let Self { inner, metrics } = *self;
        let started = Instant::now();
        let result = inner.rollback_to().await;
        metrics.record(
            DatabaseOperation::SavepointRollback,
            None,
            "active",
            result.is_ok(),
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        result
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}
