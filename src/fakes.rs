//! Faker-style replacement values, but driven by a digest instead of an RNG.
//!
//! Every generator here takes the 32-byte digest of the *original* value and
//! derives the fake from it, so the same input always yields the same output
//! without carrying any state around. That's the whole trick behind keeping
//! foreign keys and joins intact: two rows that shared an email before share
//! the same fake email after, because both derivations start from the same
//! bytes.

const FIRST_NAMES: &[&str] = &[
    "Aaliyah", "Adrian", "Amara", "Anders", "Bianca", "Callum", "Camila", "Casey", "Dmitri",
    "Elena", "Emeka", "Farrah", "Felix", "Gabriel", "Hana", "Ibrahim", "Imani", "Jonas", "Kenji",
    "Lena", "Lucas", "Marisol", "Nadia", "Noor", "Oscar", "Priya", "Rafael", "Rosa", "Soren",
    "Tomas", "Yara", "Zoe",
];

const LAST_NAMES: &[&str] = &[
    "Abara",
    "Almeida",
    "Bauer",
    "Bergstrom",
    "Caldwell",
    "Chen",
    "Delacroix",
    "Eriksen",
    "Fontaine",
    "Garrido",
    "Haddad",
    "Ibrahim",
    "Jansen",
    "Kowalski",
    "Laurent",
    "Moreau",
    "Nakamura",
    "Novak",
    "Okonkwo",
    "Pereira",
    "Quintero",
    "Reyes",
    "Santoro",
    "Sokolov",
    "Tanaka",
    "Ueda",
    "Vasquez",
    "Whitfield",
    "Xiong",
    "Yilmaz",
    "Zamora",
    "Zheng",
];

/// RFC 2606 / RFC 6761 reserved names. Mail to these can never be delivered,
/// which is the point: a staging system that "helpfully" sends a welcome
/// email to its anonymized user table must not be able to reach a real
/// person's inbox.
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net", "example.edu"];

/// Real, assigned NANP area codes — the fictional part of the generated
/// number is the `555-01xx` exchange, not the area code, so these look
/// plausible without being dialable.
const AREA_CODES: &[&str] = &[
    "201", "202", "206", "212", "213", "303", "305", "312", "404", "415", "470", "503", "512",
    "602", "617", "646", "702", "713", "737", "802", "808", "816", "857", "917",
];

/// Reads 8 bytes of the digest, starting at `word`, as a u64. Four disjoint
/// words fit in a SHA-256 digest, which is enough for every generator here.
fn word(digest: &[u8; 32], index: usize) -> u64 {
    let start = index * 8;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[start..start + 8]);
    u64::from_be_bytes(bytes)
}

fn pick<'a>(list: &[&'a str], value: u64) -> &'a str {
    list[(value % list.len() as u64) as usize]
}

pub fn fake_name(digest: &[u8; 32]) -> String {
    format!(
        "{} {}",
        pick(FIRST_NAMES, word(digest, 0)),
        pick(LAST_NAMES, word(digest, 1))
    )
}

pub fn fake_email(digest: &[u8; 32]) -> String {
    // The two-digit suffix is what keeps distinct inputs from colliding onto
    // one address too often: 32 x 32 x 100 x 4 combinations, not 32 x 32.
    format!(
        "{}.{}{:02}@{}",
        pick(FIRST_NAMES, word(digest, 0)).to_lowercase(),
        pick(LAST_NAMES, word(digest, 1)).to_lowercase(),
        word(digest, 2) % 100,
        pick(DOMAINS, word(digest, 3))
    )
}

/// `+1-NNN-555-01NN`. 555-0100 through 555-0199 is reserved for fiction in
/// *every* NANP area code, so varying the area code keeps the output
/// unroutable while giving it a realistic shape — and, more importantly,
/// 2400 possible values instead of 100. The first version of this held the
/// area code fixed, and a live run on eight users produced one number three
/// times; distinct phone numbers collapsing into each other quietly changes
/// join cardinality, which is the exact thing this tool is supposed to
/// preserve.
pub fn fake_phone(digest: &[u8; 32]) -> String {
    format!(
        "+1-{}-555-01{:02}",
        pick(AREA_CODES, word(digest, 1)),
        word(digest, 0) % 100
    )
}

pub fn hex(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(n: u8) -> [u8; 32] {
        let mut d = [0u8; 32];
        for (i, slot) in d.iter_mut().enumerate() {
            *slot = n.wrapping_add(i as u8).wrapping_mul(37);
        }
        d
    }

    #[test]
    fn fake_email_has_a_plausible_shape_and_a_reserved_domain() {
        for n in 0..64u8 {
            let email = fake_email(&digest_of(n));
            assert_eq!(email.matches('@').count(), 1, "{}", email);
            let (local, domain) = email.split_once('@').unwrap();
            assert!(local.contains('.'), "{}", email);
            assert!(DOMAINS.contains(&domain), "{} is not reserved", email);
            assert_eq!(email, email.to_lowercase(), "{}", email);
            assert!(!local.is_empty());
        }
    }

    #[test]
    fn fake_name_is_two_capitalized_words() {
        for n in 0..64u8 {
            let name = fake_name(&digest_of(n));
            let parts: Vec<&str> = name.split(' ').collect();
            assert_eq!(parts.len(), 2, "{}", name);
            for part in parts {
                assert!(part.chars().next().unwrap().is_uppercase(), "{}", name);
            }
        }
    }

    #[test]
    fn fake_phone_stays_inside_the_fictional_555_0100_block() {
        for n in 0..64u8 {
            let phone = fake_phone(&digest_of(n));
            assert_eq!(phone.len(), 15, "{}", phone);
            let parts: Vec<&str> = phone.split('-').collect();
            assert_eq!(parts.len(), 4, "{}", phone);
            assert_eq!(parts[0], "+1");
            assert_eq!(parts[2], "555", "{}", phone);
            assert!(AREA_CODES.contains(&parts[1]), "{}", phone);
            // The reserved block is 555-0100..555-0199, i.e. the last four
            // digits always start "01".
            assert!(parts[3].starts_with("01"), "{}", phone);
            assert!(parts[3].parse::<u32>().unwrap() <= 199, "{}", phone);
        }
    }

    #[test]
    fn fake_phone_uses_more_than_one_area_code() {
        // The regression this guards: an earlier version pinned the area
        // code, leaving only 100 possible numbers, and a live run on eight
        // rows collided three ways.
        let phones: std::collections::HashSet<String> =
            (0..64u8).map(|n| fake_phone(&digest_of(n))).collect();
        assert!(phones.len() > 55, "only {} distinct phones", phones.len());
    }

    #[test]
    fn hex_is_64_lowercase_hex_chars() {
        let h = hex(&digest_of(9));
        assert_eq!(h.len(), 64);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn generators_use_the_whole_list_not_just_the_first_entry() {
        // A generator that accidentally ignored its digest would pass every
        // shape assertion above while producing one value forever.
        let names: std::collections::HashSet<String> =
            (0..64u8).map(|n| fake_name(&digest_of(n))).collect();
        assert!(names.len() > 20, "only {} distinct names", names.len());
        let emails: std::collections::HashSet<String> =
            (0..64u8).map(|n| fake_email(&digest_of(n))).collect();
        assert!(emails.len() > 20, "only {} distinct emails", emails.len());
    }

    #[test]
    fn word_reads_four_disjoint_slices_of_the_digest() {
        let mut digest = [0u8; 32];
        digest[7] = 1;
        digest[15] = 2;
        digest[23] = 3;
        digest[31] = 4;
        assert_eq!(word(&digest, 0), 1);
        assert_eq!(word(&digest, 1), 2);
        assert_eq!(word(&digest, 2), 3);
        assert_eq!(word(&digest, 3), 4);
    }
}
