# pganonymize

Anonymizes the PII in a **real, existing** Postgres database so a
prod-shaped dataset can be used in staging. Not a fake-data generator —
it rewrites the rows that are already there, in place, keeping the row
count, the non-PII columns and the join behaviour exactly as they were.
Python has `pganonymizer` for this; Rust has nothing, and the Rust
answer to "we need realistic staging data" is currently either a
hand-rolled `UPDATE users SET email = 'x' || id || '@test.com'` script
(which destroys every join that went through an email) or shipping a raw
prod dump to staging and hoping.

## Usage

```bash
pganonymize plan  --config staging.toml            # read-only: shows what would change
pganonymize apply --config staging.toml --yes      # rewrite in place
pganonymize apply --config staging.toml --yes --into-schema anon   # rewrite a copy, leave the original alone
```

`apply` without `--yes` refuses to do anything. Config is TOML:

```toml
dsn  = "postgres://postgres@localhost:5432/staging_copy"
seed = "2026-staging-refresh"   # the salt; see "Pick a seed" below

[[rules]]
table = "users"
column = "email"
strategy = "fake_email"

[[rules]]
table = "users"
column = "full_name"
strategy = "fake_name"

[[rules]]
table = "users"
column = "phone"
strategy = "fake_phone"

[[rules]]
table = "users"
column = "ssn"
strategy = "hash"

[[rules]]
table = "users"
column = "support_note"
strategy = "redact"

[[rules]]
table = "orders"
column = "contact_email"     # a denormalized copy of users.email
strategy = "fake_email"

[[rules]]
table = "orders"
column = "shipping_note"
strategy = "null"
```

`--dsn` and `--seed` override the file. `table` may be schema-qualified
(`public.users`) or not, in which case it resolves through the
connection's `search_path`, same as in `psql`. `dsn` accepts either DSN
form the driver does, including libpq's
`host=/var/run/postgresql port=5432 user=postgres dbname=x`.

`plan` connects, reads, and writes nothing — it reports the distinct
value count per column and a few real `original -> anonymized` pairs so
you can see what you're about to do. (It prints real PII to your
terminal, on purpose. Don't paste that into a ticket.)

## Strategies

| strategy     | result                                    |
| ------------ | ----------------------------------------- |
| `fake_email` | `ibrahim.kowalski28@example.org` — always an RFC 2606 reserved domain, so staging can never actually mail a real person |
| `fake_name`  | `Tomas Sokolov` |
| `fake_phone` | `+1-802-555-0158` — the `555-01xx` block, permanently unassigned in every NANP area code |
| `hash`       | 64-char lowercase SHA-256 hex |
| `redact`     | the literal `REDACTED` |
| `null`       | SQL `NULL` |

## What "preserving referential integrity" means here

The mapping is a *function of the value*, not a random draw per row:
`sha256(seed, strategy, original)` picks the replacement. Two
consequences, both of which are the entire point:

- The same original value becomes the same anonymized value **everywhere
  it appears**, including across tables. `users.email` and
  `orders.contact_email` both anonymized with `fake_email` still join to
  each other afterwards, and still pick out the same rows.
- The same config and seed run next month against a fresh prod copy
  produces the **same** staging values, so a bookmarked staging URL or a
  test fixture that refers to `ibrahim.kowalski28@example.org` keeps
  working.

The table and column are deliberately *not* part of the key; the
strategy is. So two columns you join on must be given the same strategy
or they'll stop agreeing. `redact` and `null` collapse every value into
one, which destroys the column as a join key — that's what they're for,
just don't point them at something a foreign key travels through.

Foreign keys on *non*-anonymized columns (the usual
`orders.user_id -> users.id`) are untouched: this never rewrites a
column you didn't name, and the rewrite is value-matched rather than
key-matched, so it doesn't need a primary key at all.

### Pick a seed

The default seed is a constant in the source. With it, the mapping is
public: anyone holding the anonymized database and a list of candidate
emails can hash each one and recover who is who. Set `seed` to something
secret and keep it wherever you keep secrets — that's what turns the
mapping from *obfuscated* into *not recoverable without the seed*.

## Safety

- One transaction for the whole run. A rule that fails halfway rolls the
  entire run back, because a half-anonymized database is worse than an
  un-anonymized one — it looks safe.
- Row counts are compared before and after, inside that transaction,
  and a mismatch aborts the commit.
- Table and column names from the config are quoted as identifiers and
  every value is a bind parameter, so a config file can't inject SQL.
- `--into-schema` refuses to run if the target resolves to the source
  table itself.

## Status: built and verified live against a real throwaway Postgres

- **42 unit tests** (`cargo test --lib`), none needing a database:
  config parsing (every documented strategy name round-trips; an unknown
  strategy errors with the valid ones listed; a *misspelled key* like
  `colum` is rejected rather than silently ignored; two rules on one
  column rejected; the same column name in two tables allowed); the
  mapping (determinism for the same value, agreement between two
  separately constructed `Mapper`s with the same seed, divergence under
  a different seed, 2000 distinct inputs staying >95% distinct on
  output, `null` producing `None` rather than the string `"NULL"`, and a
  check that **no output ever contains its input** — the catastrophic
  bug where a "fake" value passes part of the original through); the
  generators (reserved domains only, the `555-01xx` block only, and that
  each generator actually varies with the digest rather than returning
  one value forever); and SQL construction (identifier quoting defusing
  a `email"; DROP TABLE users; --` column name, the staging table always
  reached through `pg_temp`, parameter numbering, and that the rewrite
  is one statement with no inlined values).
- **Live run against a real Postgres** (`initdb`'d into a throwaway data
  directory, started on port 55472 with `-h ''` so it had no TCP
  listener at all, deleted afterwards): a `users` table (8 rows:
  `email`, `full_name`, `phone`, `ssn`, `support_note`, plus non-PII
  `plan`, `created_at`, `short_code`) and an `orders` table (12 rows)
  with a real `user_id integer REFERENCES users(id)` foreign key and a
  denormalized `contact_email` duplicating `users.email`. All six
  strategies exercised in one run. Verified by reading the database back
  with `psql`, not by trusting the tool's own report:
  - **PII gone.** `alice.nguyen@corp.example` became
    `ibrahim.kowalski28@example.org`, `Alice Nguyen` became
    `Tomas Sokolov`, `+1-415-555-9182` became `+1-802-555-0158`,
    `123-45-6789` became
    `ec480da0bbfbee774673c7eccd71e13123a0917016bf1eb40f227616b862e5d2`,
    `called about billing on the 3rd` became `REDACTED`, and
    `leave with front desk` became NULL. Counting rows still holding an
    *exact* original value: 0 emails, 0 names, 0 phone numbers, 0
    support notes, 0 shipping notes.
  - **Non-PII byte-identical.** `md5(string_agg(id|plan|created_at|short_code))`
    over `users` was `e52a2487a565af0dd91a1959f8d7fc26` before and
    `e52a2487a565af0dd91a1959f8d7fc26` after; over `orders`
    (`id|user_id|amount_cents|placed_at`)
    `f37e11b0d05863da9cc93c9e9b7d6ac2` both times.
  - **Row counts unchanged**: 8 and 12, before and after. A NULL
    `phone` (user 3) was left NULL rather than having a fake invented
    for it, and `support_note`s that were already NULL were not
    redacted into a non-NULL `REDACTED`.
  - **Same value -> same replacement, everywhere.** Users 1 and 6 shared
    the phone `+1-415-555-9182`; afterwards both hold
    `+1-802-555-0158`. Alice's address appears in `users.email` and on
    two rows of `orders.contact_email`; all three became
    `ibrahim.kowalski28@example.org`. The value join
    `orders JOIN users ON u.email = o.contact_email` matched **12 of 12
    rows before and 12 of 12 after**, and picked the same user as the
    foreign key did on all 12 both times.
  - **Foreign key intact.** `orders JOIN users ON u.id = o.user_id`
    returned 12 rows after, with 0 orphans, and
    `orders_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id)` is
    still present in `pg_constraint`.
  - **Run-to-run determinism**: two independent `apply --into-schema`
    runs with the same seed, into two different schemas, produced
    byte-identical output (compared by md5 over all five anonymized
    columns).
  - **Rollback on failure, checked for real.** A config pointing `hash`
    at a `varchar(8)` column failed with
    `ERROR: value too long for type character varying(8)` *after* it had
    already reported "8 rows rewritten" for the preceding `email` rule —
    and the md5 of the PII columns afterwards was still the
    pre-run `2a766b749d5c47633af737fcbb9c43cd`. Nothing committed.
  - `plan` left the PII md5 unchanged (it really is read-only);
    `apply` without `--yes` exited 1 and wrote nothing;
    `--into-schema anon` left `public.users` fully original (8 rows
    still `@corp.example`) while `anon.users` was fully anonymized; and
    pointing `--into-schema anon` at a source table already in `anon`
    was refused instead of dropping the source.
- **A real bug this live run caught**: `fake_phone` originally emitted
  `+1-555-01NN` with a fixed area code — only 100 possible values. On
  eight users with six distinct phone numbers it produced
  `+1-555-0158` three times, collapsing two genuinely different numbers
  into one and quietly changing what a `GROUP BY phone` in staging would
  report. Fixed by varying the area code too (`555-01xx` is reserved in
  every NANP area code, so the output is still unroutable) for 2400
  values instead of 100; after the fix those six distinct numbers map to
  six distinct replacements. Locked down with
  `fake_phone_uses_more_than_one_area_code`.

**Not done / deliberately deferred**:

- **Dump files.** This only drives a live server. Anonymizing a `.sql`
  or custom-format dump directly would mean parsing `COPY` blocks and
  is a separate tool; the workable path today is `pg_restore` into a
  throwaway database, run this against it, and dump it back out.
- **Re-running in place is not idempotent**, and this is worth knowing
  before you wire it into a cron. A second in-place run treats the
  *fake* values as the new originals and maps them again —
  `ibrahim.kowalski28@example.org` became
  `nadia.laurent62@example.org` on a second pass, verified live. Safe
  and still deterministic, but the values move. Anonymize each fresh
  prod copy exactly once, or use `--into-schema`, which rebuilds the
  copy from the untouched source every time.
- **Text-shaped columns only.** Values are read as `::text` and written
  back as text, so `text`/`varchar`/`citext` work and anything requiring
  a different input syntax does not. Picking out one field inside a
  `jsonb` blob, or an element of an array column, isn't supported.
- **No pre-flight type check.** `plan` does not verify the column can
  hold the strategy's output — the `hash`-into-`varchar(8)` case above
  only failed at `apply` time. It failed *safely* (whole run rolled
  back), but `plan` should have caught it and didn't.
- **`--into-schema` copies with `CREATE TABLE ... AS TABLE ...`**, which
  brings the rows and column types but **not** constraints, indexes or
  defaults; confirmed live, `pg_constraint` lists nothing for the copied
  tables. The foreign key *values* stay consistent, the foreign key
  *constraint* is not recreated. Only the tables named in the config are
  copied, so the target schema is an anonymized slice, not a clone.
- **Scale is untested.** The design is one `UPDATE` per column joined
  against a temp table of the distinct values (rather than one statement
  per row, and rather than batched `IN` lists, which could rewrite an
  already-rewritten row twice), so it should behave, but the largest
  table it has actually run against is 12 rows. No benchmark, no
  progress reporting, no batching of the rewrite itself, and it takes a
  row-level write lock on every affected row for the duration of the
  transaction — don't point it at a live production table.
- **`fake_email` collision headroom**: 409,600 possible addresses, so
  birthday collisions start mattering somewhere in the tens of
  thousands of distinct values. Verified >95% distinct at 2,000 values;
  not characterized beyond that.
- **No allow-list / discovery mode.** It anonymizes exactly the columns
  you list. It won't find the PII column you forgot about, and it has no
  "fail if any column named `%email%` is unlisted" guard — that check
  would be a genuinely useful v2.
