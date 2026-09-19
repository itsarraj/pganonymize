//! Everything that talks to a server. Deliberately thin: the decisions live
//! in `config`, `mapping` and `sql`, which are all testable without one.

use anyhow::{bail, Context, Result};
use postgres::types::ToSql;
use postgres::{Client, GenericClient, NoTls};

use crate::config::{Rule, Strategy};
use crate::mapping::{Mapper, REDACTED};
use crate::sql::{self, TableRef};

/// Mappings staged per `INSERT`. 1000 pairs is 2000 bind parameters, well
/// under Postgres's 65535-parameter limit, and the staging is split only
/// because of that limit — the rewrite itself is always one statement.
const STAGE_BATCH: usize = 1000;

#[derive(Debug, Clone)]
pub struct ColumnReport {
    pub table: TableRef,
    pub column: String,
    pub strategy: Strategy,
    /// How many distinct non-NULL values the column held before the run.
    pub distinct_values: usize,
    /// Rows the `UPDATE` actually touched. Rows that were already NULL are
    /// not counted, because nothing about them was anonymized.
    pub rows_updated: u64,
    /// A few `original -> anonymized` pairs, for the operator to eyeball.
    pub samples: Vec<(String, String)>,
}

pub fn connect(dsn: &str) -> Result<Client> {
    Client::connect(dsn, NoTls).context("connecting to Postgres")
}

pub fn row_count<C: GenericClient>(client: &mut C, table: &TableRef) -> Result<i64> {
    let row = client
        .query_one(sql::count_rows(table).as_str(), &[])
        .with_context(|| format!("counting rows in {}", table))?;
    Ok(row.get(0))
}

/// Every distinct non-NULL value in the column, rendered as text.
pub fn distinct_values<C: GenericClient>(
    client: &mut C,
    table: &TableRef,
    column: &str,
) -> Result<Vec<String>> {
    let rows = client
        .query(sql::select_distinct(table, column).as_str(), &[])
        .with_context(|| format!("reading distinct values of {}.{}", table, column))?;
    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}

fn samples(
    mapper: &Mapper,
    strategy: Strategy,
    originals: &[String],
    n: usize,
) -> Vec<(String, String)> {
    originals
        .iter()
        .take(n)
        .map(|original| {
            let mapped = mapper
                .map(strategy, original)
                .unwrap_or_else(|| "NULL".to_string());
            (original.clone(), mapped)
        })
        .collect()
}

/// Read-only: works out what `apply` would do to one column, and writes
/// nothing. This is what `plan` runs.
pub fn preview_rule<C: GenericClient>(
    client: &mut C,
    mapper: &Mapper,
    rule: &Rule,
    table: &TableRef,
) -> Result<ColumnReport> {
    let originals = distinct_values(client, table, &rule.column)?;
    Ok(ColumnReport {
        table: table.clone(),
        column: rule.column.clone(),
        strategy: rule.strategy,
        distinct_values: originals.len(),
        rows_updated: 0,
        samples: samples(mapper, rule.strategy, &originals, 3),
    })
}

/// Rewrites one column. Caller is expected to be inside a transaction — an
/// anonymization run that fails halfway and commits anyway is the worst
/// possible outcome, since the half-anonymized result looks plausible.
pub fn apply_rule<C: GenericClient>(
    client: &mut C,
    mapper: &Mapper,
    rule: &Rule,
    table: &TableRef,
) -> Result<ColumnReport> {
    let column = rule.column.as_str();
    let strategy = rule.strategy;
    let originals = distinct_values(client, table, column)?;
    let mut report = ColumnReport {
        table: table.clone(),
        column: rule.column.clone(),
        strategy,
        distinct_values: originals.len(),
        rows_updated: 0,
        samples: samples(mapper, strategy, &originals, 3),
    };

    if strategy.is_collapsing() {
        // No per-value mapping to stage: one bulk statement does it.
        report.rows_updated = match strategy {
            Strategy::Null => client
                .execute(sql::update_to_null(table, column).as_str(), &[])
                .with_context(|| format!("nulling {}.{}", table, column))?,
            _ => client
                .execute(sql::update_constant(table, column).as_str(), &[&REDACTED])
                .with_context(|| format!("redacting {}.{}", table, column))?,
        };
        return Ok(report);
    }

    if originals.is_empty() {
        return Ok(report);
    }

    client
        .execute(sql::drop_map_table().as_str(), &[])
        .context("clearing the staging table")?;
    client
        .execute(sql::create_map_table().as_str(), &[])
        .context("creating the staging table")?;

    for chunk in originals.chunks(STAGE_BATCH) {
        let mut values: Vec<String> = Vec::with_capacity(chunk.len() * 2);
        for original in chunk {
            let mapped = mapper.map(strategy, original).expect(
                "a non-collapsing strategy always produces a value; \
                 NULL only comes from the `null` strategy, handled above",
            );
            values.push(original.clone());
            values.push(mapped);
        }
        let params: Vec<&(dyn ToSql + Sync)> =
            values.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
        client
            .execute(sql::insert_map_rows(chunk.len()).as_str(), &params)
            .with_context(|| format!("staging the mapping for {}.{}", table, column))?;
    }

    report.rows_updated = client
        .execute(sql::update_from_map(table, column).as_str(), &[])
        .with_context(|| {
            format!(
                "rewriting {}.{} (if this mentions a length or type error, the \
                 column can't hold the strategy's output — see the README)",
                table, column
            )
        })?;

    client
        .execute(sql::drop_map_table().as_str(), &[])
        .context("dropping the staging table")?;

    Ok(report)
}

/// True when both names resolve to the same relation (or both to nothing).
/// Used to refuse a `--into-schema` that would drop the very table it is
/// about to copy from.
fn same_relation<C: GenericClient>(client: &mut C, a: &TableRef, b: &TableRef) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT to_regclass($1) IS NOT DISTINCT FROM to_regclass($2)",
            &[&a.quoted(), &b.quoted()],
        )
        .context("checking whether source and target are the same table")?;
    Ok(row.get(0))
}

/// Copies each table into `schema`, replacing any previous copy, and returns
/// the `(source, copy)` pairs. Only the tables named in the config are
/// copied — the target schema is a partial, anonymized view, not a clone of
/// the database.
pub fn materialize_copies<C: GenericClient>(
    client: &mut C,
    schema: &str,
    sources: &[TableRef],
) -> Result<Vec<(TableRef, TableRef)>> {
    client
        .execute(sql::create_schema(schema).as_str(), &[])
        .with_context(|| format!("creating schema {}", schema))?;

    let mut pairs = Vec::new();
    for source in sources {
        let target = source.in_schema(schema);
        if same_relation(client, source, &target)? {
            bail!(
                "--into-schema {} would target {}, which is the source table itself",
                schema,
                target
            );
        }
        client
            .execute(sql::drop_table(&target).as_str(), &[])
            .with_context(|| format!("dropping the previous copy {}", target))?;
        client
            .execute(sql::copy_table(source, &target).as_str(), &[])
            .with_context(|| format!("copying {} to {}", source, target))?;
        pairs.push((source.clone(), target));
    }
    Ok(pairs)
}
