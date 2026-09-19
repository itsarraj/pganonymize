use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// What to replace a column's values with. The name in the config file is
/// the snake_case form of the variant (`fake_email`, `redact`, ...).
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// `casey.moreau41@example.net` — RFC 2606 reserved domains only, so a
    /// staging system that accidentally sends mail can't reach a real inbox.
    FakeEmail,
    /// `Casey Moreau`
    FakeName,
    /// `+1-555-0142` — the NANP range reserved for fiction.
    FakePhone,
    /// Every value becomes the literal `REDACTED`. Destroys distinctness on
    /// purpose; see `is_collapsing`.
    Redact,
    /// Lowercase hex SHA-256 of the value (64 chars). Keeps values distinct
    /// and joinable without keeping them readable.
    Hash,
    /// SQL `NULL`. Also collapsing, and needs a nullable column.
    Null,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::FakeEmail => "fake_email",
            Strategy::FakeName => "fake_name",
            Strategy::FakePhone => "fake_phone",
            Strategy::Redact => "redact",
            Strategy::Hash => "hash",
            Strategy::Null => "null",
        }
    }

    /// True when the strategy maps *every* input to the same output, so the
    /// tool can do one bulk `UPDATE` instead of reading the distinct values
    /// first. This is also the property that makes a column unusable as a
    /// join key afterwards, which is worth saying out loud in `plan` output.
    pub fn is_collapsing(self) -> bool {
        matches!(self, Strategy::Redact | Strategy::Null)
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad`, not `write_str`, so that a `{:<12}` in the CLI's output
        // actually lines the columns up.
        f.pad(self.name())
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Either `users` or `public.users`. Unqualified names resolve through
    /// the connection's `search_path`, exactly as they would in `psql`.
    pub table: String,
    pub column: String,
    pub strategy: Strategy,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Connection string. `--dsn` on the command line wins over this.
    pub dsn: Option<String>,
    /// Salt for the deterministic mapping. Two runs with the same seed
    /// produce the same fake values; two runs with different seeds don't.
    /// Optional, but see the README on why you want a secret one.
    #[serde(default = "default_seed")]
    pub seed: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_seed() -> String {
    "pganonymize".to_string()
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Config> {
        let config: Config = toml::from_str(toml_text).context("config file is not valid TOML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("in config file {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        if self.rules.is_empty() {
            bail!("config has no [[rules]] — nothing to anonymize");
        }
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for rule in &self.rules {
            if rule.table.trim().is_empty() || rule.column.trim().is_empty() {
                bail!("a rule has an empty table or column name");
            }
            if !seen.insert((rule.table.as_str(), rule.column.as_str())) {
                bail!(
                    "two rules both target {}.{} — a column can only have one strategy",
                    rule.table,
                    rule.column
                );
            }
        }
        Ok(())
    }

    /// The tables touched by the rules, in first-mention order (order matters
    /// only so that output is stable and diffable between runs).
    pub fn tables(&self) -> Vec<&str> {
        let mut tables = Vec::new();
        for rule in &self.rules {
            if !tables.contains(&rule.table.as_str()) {
                tables.push(rule.table.as_str());
            }
        }
        tables
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
dsn = "postgres://postgres@localhost:5432/staging"
seed = "2026-refresh"

[[rules]]
table = "users"
column = "email"
strategy = "fake_email"

[[rules]]
table = "users"
column = "phone"
strategy = "fake_phone"

[[rules]]
table = "orders"
column = "contact_email"
strategy = "fake_email"
"#;

    #[test]
    fn parses_a_realistic_config() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(
            config.dsn.as_deref(),
            Some("postgres://postgres@localhost:5432/staging")
        );
        assert_eq!(config.seed, "2026-refresh");
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.rules[0].table, "users");
        assert_eq!(config.rules[0].column, "email");
        assert_eq!(config.rules[0].strategy, Strategy::FakeEmail);
        assert_eq!(config.rules[2].strategy, Strategy::FakeEmail);
    }

    #[test]
    fn tables_are_deduplicated_in_first_mention_order() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.tables(), vec!["users", "orders"]);
    }

    #[test]
    fn seed_defaults_when_omitted_so_a_config_without_one_still_runs() {
        let config = Config::parse(
            r#"
[[rules]]
table = "users"
column = "email"
strategy = "redact"
"#,
        )
        .unwrap();
        assert_eq!(config.seed, "pganonymize");
        assert!(config.dsn.is_none());
    }

    #[test]
    fn every_documented_strategy_name_parses() {
        for (name, expected) in [
            ("fake_email", Strategy::FakeEmail),
            ("fake_name", Strategy::FakeName),
            ("fake_phone", Strategy::FakePhone),
            ("redact", Strategy::Redact),
            ("hash", Strategy::Hash),
            ("null", Strategy::Null),
        ] {
            let text = format!(
                "[[rules]]\ntable = \"t\"\ncolumn = \"c\"\nstrategy = \"{}\"\n",
                name
            );
            let config = Config::parse(&text).unwrap();
            assert_eq!(config.rules[0].strategy, expected, "for {}", name);
            assert_eq!(expected.name(), name, "round trip for {}", name);
        }
    }

    #[test]
    fn unknown_strategy_is_an_error_that_lists_the_valid_ones() {
        let err = Config::parse(
            r#"
[[rules]]
table = "users"
column = "email"
strategy = "fake_ssn"
"#,
        )
        .unwrap_err();
        let message = format!("{:#}", err);
        assert!(message.contains("fake_ssn"), "{}", message);
        assert!(message.contains("fake_email"), "{}", message);
    }

    #[test]
    fn a_config_with_no_rules_is_rejected_instead_of_silently_doing_nothing() {
        let err = Config::parse("dsn = \"postgres://localhost/x\"\n").unwrap_err();
        assert!(format!("{}", err).contains("no [[rules]]"));
    }

    #[test]
    fn two_rules_on_the_same_column_are_rejected() {
        let err = Config::parse(
            r#"
[[rules]]
table = "users"
column = "email"
strategy = "fake_email"

[[rules]]
table = "users"
column = "email"
strategy = "hash"
"#,
        )
        .unwrap_err();
        assert!(format!("{}", err).contains("users.email"));
    }

    #[test]
    fn same_column_name_in_two_different_tables_is_fine() {
        let config = Config::parse(
            r#"
[[rules]]
table = "users"
column = "email"
strategy = "fake_email"

[[rules]]
table = "invoices"
column = "email"
strategy = "fake_email"
"#,
        )
        .unwrap();
        assert_eq!(config.rules.len(), 2);
    }

    #[test]
    fn a_misspelled_key_is_rejected_rather_than_silently_ignored() {
        // `colum` instead of `column`: without deny_unknown_fields this would
        // fail much later, as a confusing "missing field" error or worse, a
        // rule that quietly targets the wrong thing.
        let err = Config::parse(
            r#"
[[rules]]
table = "users"
colum = "email"
strategy = "fake_email"
"#,
        )
        .unwrap_err();
        assert!(format!("{:#}", err).contains("colum"));
    }

    #[test]
    fn collapsing_strategies_are_exactly_redact_and_null() {
        assert!(Strategy::Redact.is_collapsing());
        assert!(Strategy::Null.is_collapsing());
        assert!(!Strategy::FakeEmail.is_collapsing());
        assert!(!Strategy::FakeName.is_collapsing());
        assert!(!Strategy::FakePhone.is_collapsing());
        assert!(!Strategy::Hash.is_collapsing());
    }
}
