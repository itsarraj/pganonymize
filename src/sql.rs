//! SQL construction, kept pure and separate from execution so it can be
//! tested without a server.
//!
//! Table and column names come out of a config file, which means they are
//! *not* trustworthy input and can never be pasted straight into a statement.
//! Everything here goes through [`quote_ident`]; every value goes through a
//! bind parameter. Those are the only two ways user input reaches the server.

use std::fmt;

/// A possibly schema-qualified table name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    /// `users` or `public.users`. Split on the *first* dot: a name with more
    /// dots than that isn't something this tool tries to guess at, and the
    /// remainder just stays part of the table name (where it will be quoted,
    /// so it fails loudly at the server rather than silently hitting some
    /// other relation).
    pub fn parse(raw: &str) -> TableRef {
        match raw.split_once('.') {
            Some((schema, name)) => TableRef {
                schema: Some(schema.to_string()),
                name: name.to_string(),
            },
            None => TableRef {
                schema: None,
                name: raw.to_string(),
            },
        }
    }

    /// The same table name relocated into `schema` — used by `--into-schema`
    /// to build the anonymized copy's name from the source's.
    pub fn in_schema(&self, schema: &str) -> TableRef {
        TableRef {
            schema: Some(schema.to_string()),
            name: self.name.clone(),
        }
    }

    /// Quoted and ready to splice into a statement.
    pub fn quoted(&self) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote_ident(schema), quote_ident(&self.name)),
            None => quote_ident(&self.name),
        }
    }
}

impl fmt::Display for TableRef {
    /// Human-facing form, for log lines — not for SQL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{}.{}", schema, self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// Wraps an identifier in double quotes, doubling any quote inside it. This
/// is what stops a config file from being an injection vector, and it also
/// makes mixed-case and reserved-word names (`"user"`, `"Order"`) work.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub fn count_rows(table: &TableRef) -> String {
    format!("SELECT count(*) FROM {}", table.quoted())
}

pub fn select_distinct(table: &TableRef, column: &str) -> String {
    format!(
        "SELECT DISTINCT {col}::text FROM {tbl} WHERE {col} IS NOT NULL",
        col = quote_ident(column),
        tbl = table.quoted()
    )
}

/// One bulk update for the collapsing strategies. The replacement rides in as
/// `$1`, never as a literal. `WHERE ... IS NOT NULL` keeps the reported row
/// count honest: rows that were already NULL weren't anonymized by this.
pub fn update_constant(table: &TableRef, column: &str) -> String {
    format!(
        "UPDATE {tbl} SET {col} = $1 WHERE {col} IS NOT NULL",
        tbl = table.quoted(),
        col = quote_ident(column)
    )
}

pub fn update_to_null(table: &TableRef, column: &str) -> String {
    format!(
        "UPDATE {tbl} SET {col} = NULL WHERE {col} IS NOT NULL",
        tbl = table.quoted(),
        col = quote_ident(column)
    )
}

/// The session-local table the value mapping is staged in. Explicitly
/// `pg_temp`-qualified everywhere so it can never be confused with a real
/// table of the same name, whatever the connection's `search_path` says.
pub const MAP_TABLE: &str = "pg_temp.\"pganonymize_value_map\"";

pub fn create_map_table() -> String {
    // Created unqualified-but-TEMP (which lands it in pg_temp) and read back
    // through pg_temp. The primary key is not decoration: it's the index the
    // join below uses.
    "CREATE TEMP TABLE \"pganonymize_value_map\" \
     (old_value text PRIMARY KEY, new_value text NOT NULL)"
        .to_string()
}

pub fn drop_map_table() -> String {
    format!("DROP TABLE IF EXISTS {}", MAP_TABLE)
}

/// A multi-row `INSERT` staging `pair_count` mappings. Parameters are bound
/// in order, so pair *i* is `$(2i+1)` and `$(2i+2)`.
pub fn insert_map_rows(pair_count: usize) -> String {
    assert!(pair_count > 0, "insert_map_rows needs at least one pair");
    let tuples: Vec<String> = (0..pair_count)
        .map(|i| format!("(${}, ${})", i * 2 + 1, i * 2 + 2))
        .collect();
    format!(
        "INSERT INTO {map} (old_value, new_value) VALUES {tuples} \
         ON CONFLICT (old_value) DO NOTHING",
        map = MAP_TABLE,
        tuples = tuples.join(", ")
    )
}

/// The actual rewrite: **one** statement per column, joining the table
/// against the staged mapping.
///
/// One statement is not just a performance choice, it's a correctness one. If
/// the rewrite were split into batches of `WHERE col IN (...)`, a row already
/// rewritten by batch 1 could match an *original* value listed in batch 2 and
/// get rewritten twice — which is not far-fetched, since a prod users table
/// containing an `@example.com` test account could collide with a generated
/// one. A single `UPDATE` touches each row at most once by definition, so
/// batching happens on the inserts into the staging table instead, where it
/// is harmless.
///
/// Matching on the value rather than on a primary key also means this works
/// on tables that don't have one.
pub fn update_from_map(table: &TableRef, column: &str) -> String {
    format!(
        "UPDATE {tbl} AS t SET {col} = m.new_value \
         FROM {map} AS m WHERE t.{col}::text = m.old_value",
        tbl = table.quoted(),
        col = quote_ident(column),
        map = MAP_TABLE
    )
}

pub fn create_schema(schema: &str) -> String {
    format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema))
}

pub fn drop_table(table: &TableRef) -> String {
    format!("DROP TABLE IF EXISTS {}", table.quoted())
}

/// `CREATE TABLE ... AS TABLE ...` copies the rows and the column types. It
/// does *not* copy constraints, indexes or defaults — see the README; the
/// foreign key *values* survive, the foreign key *constraint* is not
/// recreated on the copy.
pub fn copy_table(source: &TableRef, target: &TableRef) -> String {
    format!(
        "CREATE TABLE {} AS TABLE {}",
        target.quoted(),
        source.quoted()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unqualified_and_qualified_table_names_both_parse() {
        assert_eq!(
            TableRef::parse("users"),
            TableRef {
                schema: None,
                name: "users".into()
            }
        );
        assert_eq!(
            TableRef::parse("public.users"),
            TableRef {
                schema: Some("public".into()),
                name: "users".into()
            }
        );
    }

    #[test]
    fn quoted_form_quotes_both_halves() {
        assert_eq!(TableRef::parse("users").quoted(), "\"users\"");
        assert_eq!(
            TableRef::parse("public.users").quoted(),
            "\"public\".\"users\""
        );
    }

    #[test]
    fn display_form_is_plain_for_log_lines() {
        assert_eq!(TableRef::parse("public.users").to_string(), "public.users");
        assert_eq!(TableRef::parse("users").to_string(), "users");
    }

    #[test]
    fn in_schema_replaces_the_schema_and_keeps_the_table_name() {
        assert_eq!(
            TableRef::parse("public.users").in_schema("anon").quoted(),
            "\"anon\".\"users\""
        );
        assert_eq!(
            TableRef::parse("users").in_schema("anon").quoted(),
            "\"anon\".\"users\""
        );
    }

    #[test]
    fn identifier_quoting_defuses_an_injection_attempt_from_the_config() {
        // A config that tried to smuggle SQL through a column name.
        let hostile = "email\"; DROP TABLE users; --";
        let quoted = quote_ident(hostile);
        assert_eq!(quoted, "\"email\"\"; DROP TABLE users; --\"");
        // The injected quote is escaped, so the whole thing stays one
        // identifier: quotes come in a balanced pair around it.
        assert!(quoted.starts_with('"') && quoted.ends_with('"'));
        assert_eq!(quoted.matches('"').count() % 2, 0);
    }

    #[test]
    fn reserved_and_mixed_case_identifiers_survive_quoting() {
        assert_eq!(quote_ident("user"), "\"user\"");
        assert_eq!(quote_ident("Order"), "\"Order\"");
    }

    #[test]
    fn select_distinct_skips_nulls_and_casts_to_text() {
        assert_eq!(
            select_distinct(&TableRef::parse("public.users"), "email"),
            "SELECT DISTINCT \"email\"::text FROM \"public\".\"users\" \
             WHERE \"email\" IS NOT NULL"
        );
    }

    #[test]
    fn constant_update_binds_the_replacement_rather_than_inlining_it() {
        let sql = update_constant(&TableRef::parse("users"), "email");
        assert_eq!(
            sql,
            "UPDATE \"users\" SET \"email\" = $1 WHERE \"email\" IS NOT NULL"
        );
        assert!(!sql.contains("REDACTED"), "value must be a bind parameter");
    }

    #[test]
    fn null_update_only_touches_rows_that_were_not_already_null() {
        assert_eq!(
            update_to_null(&TableRef::parse("users"), "phone"),
            "UPDATE \"users\" SET \"phone\" = NULL WHERE \"phone\" IS NOT NULL"
        );
    }

    #[test]
    fn map_insert_numbers_its_parameters_in_pairs() {
        let sql = insert_map_rows(3);
        assert!(
            sql.contains("VALUES ($1, $2), ($3, $4), ($5, $6)"),
            "{}",
            sql
        );
        assert!(
            sql.contains("ON CONFLICT (old_value) DO NOTHING"),
            "{}",
            sql
        );
    }

    #[test]
    fn map_insert_for_a_single_pair_is_still_valid_shaped_sql() {
        let sql = insert_map_rows(1);
        assert!(sql.contains("VALUES ($1, $2)"), "{}", sql);
        assert!(!sql.contains("$3"));
    }

    #[test]
    fn the_rewrite_is_one_statement_joined_against_the_staging_table() {
        let sql = update_from_map(&TableRef::parse("public.users"), "email");
        assert_eq!(
            sql,
            "UPDATE \"public\".\"users\" AS t SET \"email\" = m.new_value \
             FROM pg_temp.\"pganonymize_value_map\" AS m \
             WHERE t.\"email\"::text = m.old_value"
        );
        // No value is inlined: the staging table carries them all.
        assert!(!sql.contains('$'));
    }

    #[test]
    fn the_staging_table_is_always_referenced_through_pg_temp() {
        // If any of these resolved through search_path instead, a real table
        // named pganonymize_value_map would be read or dropped by mistake.
        for sql in [drop_map_table(), insert_map_rows(1)] {
            assert!(sql.contains("pg_temp.\"pganonymize_value_map\""), "{}", sql);
        }
        assert!(create_map_table().starts_with("CREATE TEMP TABLE"));
    }

    #[test]
    fn schema_copy_statements() {
        assert_eq!(
            create_schema("anon"),
            "CREATE SCHEMA IF NOT EXISTS \"anon\""
        );
        let source = TableRef::parse("public.users");
        let target = source.in_schema("anon");
        assert_eq!(
            copy_table(&source, &target),
            "CREATE TABLE \"anon\".\"users\" AS TABLE \"public\".\"users\""
        );
        assert_eq!(
            drop_table(&target),
            "DROP TABLE IF EXISTS \"anon\".\"users\""
        );
    }
}
