//! The deterministic value mapping: original value -> anonymized value.
//!
//! The one property everything else depends on is that this is a *function*,
//! not a random draw. `alice@corp.com` must become the same fake address in
//! `users.email` and in `orders.contact_email`, and on the next run too, or a
//! staging database stops joining the way production does and every "works in
//! prod, breaks in staging" bug becomes unfalsifiable.

use sha2::{Digest, Sha256};

use crate::config::Strategy;
use crate::fakes;

/// What `redact` writes. Deliberately not per-value: the point of `redact` is
/// that the column carries no information at all afterwards.
pub const REDACTED: &str = "REDACTED";

pub struct Mapper {
    seed: String,
}

impl Mapper {
    pub fn new(seed: impl Into<String>) -> Mapper {
        Mapper { seed: seed.into() }
    }

    /// The anonymized form of `original`. `None` means SQL `NULL`.
    ///
    /// Note what is *not* in the key: the table and column. That's on
    /// purpose — it's exactly what makes the same value map identically
    /// across tables, which is what keeps a join on an anonymized column
    /// resolving. The strategy *is* in the key, so a column anonymized with
    /// `hash` will not agree with one anonymized with `fake_email`; if two
    /// columns are joined on, give them the same strategy.
    pub fn map(&self, strategy: Strategy, original: &str) -> Option<String> {
        match strategy {
            Strategy::Null => None,
            Strategy::Redact => Some(REDACTED.to_string()),
            Strategy::FakeEmail => Some(fakes::fake_email(&self.digest(strategy, original))),
            Strategy::FakeName => Some(fakes::fake_name(&self.digest(strategy, original))),
            Strategy::FakePhone => Some(fakes::fake_phone(&self.digest(strategy, original))),
            Strategy::Hash => Some(fakes::hex(&self.digest(strategy, original))),
        }
    }

    /// SHA-256 over seed, strategy name and value, each length-prefixed so
    /// that no two different triples can produce the same byte stream (a
    /// plain concatenation would let `seed="ab", value="c"` collide with
    /// `seed="a", value="bc"`).
    fn digest(&self, strategy: Strategy, original: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for part in [
            self.seed.as_bytes(),
            strategy.name().as_bytes(),
            original.as_bytes(),
        ] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        hasher.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[Strategy] = &[
        Strategy::FakeEmail,
        Strategy::FakeName,
        Strategy::FakePhone,
        Strategy::Redact,
        Strategy::Hash,
        Strategy::Null,
    ];

    #[test]
    fn the_same_value_always_maps_to_the_same_output() {
        let mapper = Mapper::new("seed-1");
        for strategy in ALL {
            let a = mapper.map(*strategy, "alice@corp.example");
            let b = mapper.map(*strategy, "alice@corp.example");
            assert_eq!(a, b, "{} is not deterministic", strategy);
        }
    }

    #[test]
    fn two_separately_constructed_mappers_with_the_same_seed_agree() {
        // The realistic version of "the same run": two processes, or one
        // process re-run next month against a fresh prod copy.
        let first = Mapper::new("2026-refresh");
        let second = Mapper::new("2026-refresh");
        assert_eq!(
            first.map(Strategy::FakeEmail, "bob@corp.example"),
            second.map(Strategy::FakeEmail, "bob@corp.example")
        );
    }

    #[test]
    fn a_different_seed_produces_a_different_mapping() {
        let a = Mapper::new("seed-a");
        let b = Mapper::new("seed-b");
        assert_ne!(
            a.map(Strategy::FakeEmail, "alice@corp.example"),
            b.map(Strategy::FakeEmail, "alice@corp.example")
        );
        assert_ne!(
            a.map(Strategy::Hash, "alice@corp.example"),
            b.map(Strategy::Hash, "alice@corp.example")
        );
    }

    #[test]
    fn distinct_inputs_stay_distinct_under_the_non_collapsing_strategies() {
        // Not a cryptographic claim — just that a 2000-value column doesn't
        // get squashed into a handful of addresses, which would quietly
        // change join cardinality in staging.
        let mapper = Mapper::new("seed-1");
        for strategy in [Strategy::FakeEmail, Strategy::Hash] {
            let outputs: std::collections::HashSet<Option<String>> = (0..2000)
                .map(|i| mapper.map(strategy, &format!("user{}@corp.example", i)))
                .collect();
            let ratio = outputs.len() as f64 / 2000.0;
            assert!(ratio > 0.95, "{} collapsed to {:.2}", strategy, ratio);
        }
    }

    #[test]
    fn redact_collapses_every_value_to_one_literal() {
        let mapper = Mapper::new("seed-1");
        assert_eq!(
            mapper.map(Strategy::Redact, "alice@corp.example"),
            Some("REDACTED".to_string())
        );
        assert_eq!(
            mapper.map(Strategy::Redact, "totally-different"),
            Some("REDACTED".to_string())
        );
    }

    #[test]
    fn null_maps_to_none_not_to_the_string_null() {
        let mapper = Mapper::new("seed-1");
        assert_eq!(mapper.map(Strategy::Null, "alice@corp.example"), None);
    }

    #[test]
    fn the_output_never_contains_the_input() {
        // The obvious catastrophic bug — a "fake" value that passes through
        // part of the original, e.g. keeping the local part of an address.
        let mapper = Mapper::new("seed-1");
        for value in [
            "alice",
            "alice@corp.example",
            "+1-415-555-9182",
            "Alice Nguyen",
        ] {
            for strategy in ALL {
                if let Some(mapped) = mapper.map(*strategy, value) {
                    assert!(
                        !mapped.to_lowercase().contains(&value.to_lowercase()),
                        "{} leaked {:?} into {:?}",
                        strategy,
                        value,
                        mapped
                    );
                }
            }
        }
    }

    #[test]
    fn the_same_value_under_two_strategies_does_not_accidentally_agree() {
        let mapper = Mapper::new("seed-1");
        assert_ne!(
            mapper.map(Strategy::FakeName, "alice@corp.example"),
            mapper.map(Strategy::FakeEmail, "alice@corp.example")
        );
    }

    #[test]
    fn hash_output_is_a_full_sha256_hex_digest() {
        let mapper = Mapper::new("seed-1");
        let hashed = mapper.map(Strategy::Hash, "alice@corp.example").unwrap();
        assert_eq!(hashed.len(), 64);
        assert!(hashed.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn length_prefixing_stops_seed_and_value_from_running_together() {
        // Without the length prefix these two would hash identical bytes.
        let a = Mapper::new("ab").map(Strategy::Hash, "c");
        let b = Mapper::new("a").map(Strategy::Hash, "bc");
        assert_ne!(a, b);
    }

    #[test]
    fn an_empty_string_is_mapped_like_any_other_value_not_skipped() {
        let mapper = Mapper::new("seed-1");
        let mapped = mapper.map(Strategy::FakeEmail, "").unwrap();
        assert!(mapped.contains('@'));
    }
}
