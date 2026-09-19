use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use pganonymize::config::Config;
use pganonymize::db::{self, ColumnReport};
use pganonymize::mapping::Mapper;
use pganonymize::sql::TableRef;

#[derive(Parser)]
#[command(
    name = "pganonymize",
    version,
    about = "Anonymize PII in a real Postgres database, deterministically, so a prod-shaped dataset is safe to use in staging"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what would be rewritten. Reads only; writes nothing.
    Plan(CommonArgs),
    /// Rewrite the data. Requires --yes.
    Apply(ApplyArgs),
}

#[derive(Args)]
struct CommonArgs {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "pganonymize.toml")]
    config: PathBuf,
    /// Connection string, overriding the config's `dsn`.
    #[arg(long)]
    dsn: Option<String>,
    /// Seed for the value mapping, overriding the config's `seed`.
    #[arg(long)]
    seed: Option<String>,
}

#[derive(Args)]
struct ApplyArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Copy the configured tables into this schema and anonymize the copy,
    /// leaving the originals untouched.
    #[arg(long, value_name = "SCHEMA")]
    into_schema: Option<String>,
    /// Required. Without it, `apply` refuses to write anything.
    #[arg(long)]
    yes: bool,
}

fn load(common: &CommonArgs) -> Result<(Config, Mapper, String)> {
    let config = Config::load(&common.config)?;
    let dsn = common
        .dsn
        .clone()
        .or_else(|| config.dsn.clone())
        .context("no connection string: set `dsn` in the config file or pass --dsn")?;
    let seed = common.seed.clone().unwrap_or_else(|| config.seed.clone());
    let mapper = Mapper::new(seed);
    Ok((config, mapper, dsn))
}

fn print_column(report: &ColumnReport, sample_count: usize, rewritten: bool) {
    let tail = if rewritten {
        format!("{} rows rewritten", report.rows_updated)
    } else {
        format!("{} distinct values", report.distinct_values)
    };
    println!("  {:<20} {:<12} {}", report.column, report.strategy, tail);
    for (original, mapped) in report.samples.iter().take(sample_count) {
        println!("      {}  ->  {}", original, mapped);
    }
}

fn plan(args: &CommonArgs) -> Result<()> {
    let (config, mapper, dsn) = load(args)?;
    let mut client = db::connect(&dsn)?;

    println!(
        "plan: {} rule(s) across {} table(s), nothing will be written",
        config.rules.len(),
        config.tables().len()
    );
    for table_name in config.tables() {
        let table = TableRef::parse(table_name);
        let rows = db::row_count(&mut client, &table)?;
        println!("{} ({} rows)", table, rows);
        for rule in config.rules.iter().filter(|r| r.table == table_name) {
            let report = db::preview_rule(&mut client, &mapper, rule, &table)?;
            print_column(&report, 3, false);
        }
    }
    println!("run `pganonymize apply --yes` to write these changes");
    Ok(())
}

fn apply(args: &ApplyArgs) -> Result<()> {
    let (config, mapper, dsn) = load(&args.common)?;
    if !args.yes {
        bail!(
            "refusing to modify data without --yes. Run `pganonymize plan` first, \
             read what it says, then re-run with --yes"
        );
    }
    let mut client = db::connect(&dsn)?;

    // One transaction for the whole run: a partially anonymized database is
    // worse than an un-anonymized one, because it looks safe.
    let mut tx = client.transaction()?;

    let sources: Vec<TableRef> = config.tables().iter().map(|t| TableRef::parse(t)).collect();
    let mut before = BTreeMap::new();
    for source in &sources {
        before.insert(source.to_string(), db::row_count(&mut tx, source)?);
    }

    // Either rewrite in place, or rewrite a fresh copy in the target schema.
    let targets: Vec<(TableRef, TableRef)> = match &args.into_schema {
        Some(schema) => {
            let pairs = db::materialize_copies(&mut tx, schema, &sources)?;
            for (source, target) in &pairs {
                println!("copied {} -> {}", source, target);
            }
            pairs
        }
        None => sources.iter().map(|s| (s.clone(), s.clone())).collect(),
    };

    let mut reports = Vec::new();
    for (source, target) in &targets {
        println!("{} ({} rows)", target, before[&source.to_string()]);
        for rule in config
            .rules
            .iter()
            .filter(|r| r.table == source.to_string())
        {
            let report = db::apply_rule(&mut tx, &mapper, rule, target)?;
            print_column(&report, 1, true);
            reports.push(report);
        }
    }

    // The invariant this tool exists to hold: anonymizing must not add or
    // drop a row. Checked before the commit, so a violation rolls back.
    for (source, target) in &targets {
        let after = db::row_count(&mut tx, target)?;
        let was = before[&source.to_string()];
        if after != was {
            bail!(
                "row count of {} changed from {} to {} — rolling back, nothing was committed",
                target,
                was,
                after
            );
        }
    }

    tx.commit().context("committing the anonymization")?;
    println!(
        "committed: {} column(s) rewritten, every row count unchanged",
        reports.len()
    );
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan(args) => plan(&args),
        Command::Apply(args) => apply(&args),
    }
}
