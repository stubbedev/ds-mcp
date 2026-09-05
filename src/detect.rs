//! Value detectors: the half of PII a column name never tells you.
//!
//! `SELECT note FROM tickets` has an innocent column name and an email address
//! in the text. A Redis `GET session:1` is a bare string. A Mongo document
//! invents its own keys. Name globs cannot see any of that, so these scan the
//! values themselves and mask the matched span, leaving the surrounding text
//! intact.
//!
//! Every detector that can be checked is checked (Luhn for cards, mod-97 for
//! IBANs) rather than matched by shape alone — a false positive here silently
//! destroys real data, so shape-only patterns are kept narrow enough to be
//! worth their noise (`ssn` and `phone` want separators; a bare run of digits
//! is left alone).

use regex::{Regex, RegexSet};

/// One named detector. `validate` rejects shape matches that fail the
/// format's own check digit.
pub struct Detector {
    pub name: &'static str,
    pattern: &'static str,
    validate: Option<fn(&str) -> bool>,
}

const DETECTORS: &[Detector] = &[
    Detector {
        name: "email",
        pattern: r"(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}\b",
        validate: None,
    },
    Detector {
        // Real card groupings only (contiguous, 4-4-4-4, Amex 4-6-5). An
        // "any 13-19 digits with any separators" pattern also swallows phone
        // lists, id runs and the digits inside a longer account number.
        name: "credit_card",
        pattern: r"\b(?:\d{13,19}|\d{4}[ \-]\d{4}[ \-]\d{4}[ \-]\d{1,4}|\d{4}[ \-]\d{6}[ \-]\d{5})\b",
        validate: Some(luhn),
    },
    Detector {
        name: "iban",
        pattern: r"(?i)\b[a-z]{2}\d{2}(?:[ ]?[a-z0-9]{4}){2,7}(?:[ ]?[a-z0-9]{1,3})?\b",
        validate: Some(iban),
    },
    Detector {
        // Separators required: a bare 9-digit run is far more often an id.
        name: "ssn",
        pattern: r"\b\d{3}[ \-]\d{2}[ \-]\d{4}\b",
        validate: Some(us_ssn),
    },
    Detector {
        // E.164 only — an international prefix is what makes this specific
        // enough to act on.
        name: "phone",
        pattern: r"\+\d[\d \-().]{7,17}\d",
        validate: Some(phone),
    },
    Detector {
        name: "jwt",
        pattern: r"\beyJ[A-Za-z0-9_\-]{4,}\.[A-Za-z0-9_\-]{4,}\.[A-Za-z0-9_\-]{4,}",
        validate: None,
    },
    Detector {
        name: "private_key",
        pattern: r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----",
        validate: None,
    },
    Detector {
        name: "aws_key",
        pattern: r"\b(?:AKIA|ASIA|AIDA|AROA)[0-9A-Z]{16}\b",
        validate: None,
    },
];

/// Every detector name, for config validation and the docs.
pub fn names() -> impl Iterator<Item = &'static str> {
    DETECTORS.iter().map(|d| d.name)
}

pub fn is_known(name: &str) -> bool {
    names().any(|n| n == name)
}

struct Compiled {
    set: RegexSet,
    regexes: Vec<Regex>,
}

fn compiled() -> &'static Compiled {
    static COMPILED: std::sync::LazyLock<Compiled> = std::sync::LazyLock::new(|| Compiled {
        // Patterns are compile-time constants in this file: a bad one is a
        // bug, not a config error.
        set: RegexSet::new(DETECTORS.iter().map(|d| d.pattern)).expect("detector patterns compile"),
        regexes: DETECTORS
            .iter()
            .map(|d| Regex::new(d.pattern).expect("detector patterns compile"))
            .collect(),
    });
    &COMPILED
}

/// Byte spans in `s` that hold a detected value, merged and in order.
/// `enabled` is None for "all detectors".
pub fn spans(s: &str, enabled: Option<&[String]>) -> Vec<(usize, usize)> {
    let on = |d: &Detector| enabled.is_none_or(|list| list.iter().any(|n| n == d.name));
    let c = compiled();
    // One pass over the string rejects the overwhelming majority of cells
    // before any span-finding runs.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for i in &c.set.matches(s) {
        let d = &DETECTORS[i];
        if !on(d) {
            continue;
        }
        for m in c.regexes[i].find_iter(s) {
            if d.validate.is_none_or(|check| check(m.as_str())) {
                spans.push((m.start(), m.end()));
            }
        }
    }
    merge(spans)
}

fn merge(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Luhn check digit, as every card scheme uses it.
fn luhn(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| match i % 2 {
            1 if *d > 4 => d * 2 - 9,
            1 => d * 2,
            _ => *d,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// ISO 13616 mod-97: move the first four characters to the end, map letters to
/// two-digit numbers, and the whole thing mod 97 must be 1.
fn iban(s: &str) -> bool {
    let compact: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if !(15..=34).contains(&compact.len()) {
        return false;
    }
    let (head, tail) = compact.split_at(4);
    let mut rem: u32 = 0;
    for c in tail.chars().chain(head.chars()) {
        rem = match c.to_digit(36) {
            Some(v) if v < 10 => (rem * 10 + v) % 97,
            Some(v) => (rem * 100 + v) % 97,
            None => return false,
        };
    }
    rem == 1
}

/// The area/group/serial blocks the SSA never issues.
fn us_ssn(s: &str) -> bool {
    let d: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if d.len() != 9 {
        return false;
    }
    let area = d[0] * 100 + d[1] * 10 + d[2];
    let group = d[3] * 10 + d[4];
    let serial = d[5] * 1000 + d[6] * 100 + d[7] * 10 + d[8];
    area != 0 && area != 666 && area < 900 && group != 0 && serial != 0
}

/// E.164 allows 15 digits at most, and no country code starts with 0.
fn phone(s: &str) -> bool {
    let digits = s.chars().filter(char::is_ascii_digit).count();
    (8..=15).contains(&digits) && s.chars().nth(1) != Some('0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(s: &str) -> Vec<&str> {
        spans(s, None).into_iter().map(|(a, b)| &s[a..b]).collect()
    }

    #[test]
    fn finds_values_inside_free_text() {
        assert_eq!(
            found("ping Alex at alex.b+x@example.co.uk about it"),
            vec!["alex.b+x@example.co.uk"]
        );
        assert_eq!(
            found("card 4242 4242 4242 4242 charged"),
            vec!["4242 4242 4242 4242"]
        );
        assert_eq!(
            found("iban DE89 3704 0044 0532 0130 00"),
            vec!["DE89 3704 0044 0532 0130 00"]
        );
        assert_eq!(found("ssn 219-09-9999"), vec!["219-09-9999"]);
        assert_eq!(found("call +45 12 34 56 78 now"), vec!["+45 12 34 56 78"]);
        assert_eq!(
            found("key AKIAIOSFODNN7EXAMPLE leaked"),
            vec!["AKIAIOSFODNN7EXAMPLE"]
        );
        assert_eq!(
            found("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdef").len(),
            1
        );
        assert_eq!(found("-----BEGIN RSA PRIVATE KEY-----").len(), 1);
    }

    #[test]
    fn check_digits_keep_lookalikes_out() {
        // Right shape, wrong Luhn / mod-97 / SSA block: left alone.
        assert!(found("order 4242 4242 4242 4243").is_empty());
        assert!(found("DE89 3704 0044 0532 0130 01").is_empty());
        assert!(found("666-09-9999").is_empty());
        assert!(found("219-09-0000").is_empty());
        // Bare digit runs are ids far more often than they are secrets.
        assert!(found("order 123456789 shipped").is_empty());
        assert!(found("v1.2.3 build 20260101").is_empty());
        // A local phone number without a country code stays.
        assert!(found("call 12345678").is_empty());
    }

    /// The strings a database is actually full of. A detector that fires here
    /// destroys real data on every query, which costs more than the leak it
    /// was guarding against.
    #[test]
    fn ordinary_values_survive() {
        for s in [
            "2026-09-05 14:32:11",
            "550e8400-e29b-41d4-a716-446655440000",
            "192.168.1.10:5432",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
            "SKU-4820-119",
            "Invoice 2026-000148 for 1.234,56 EUR",
            "https://example.com/orders/9912831",
            "lorem ipsum dolor sit amet",
            r#"{"qty": 3, "total": 4999}"#,
            "+++ diff header +++",
        ] {
            assert!(
                found(s).is_empty(),
                "false positive on {s:?}: {:?}",
                found(s)
            );
        }
    }

    #[test]
    fn overlapping_hits_merge_into_one_span() {
        let s = "a@b.co 4242424242424242";
        assert_eq!(spans(s, None).len(), 2);
        // A card inside an email-ish string must not produce nested spans.
        assert_eq!(
            merge(vec![(0, 5), (3, 9), (20, 22)]),
            vec![(0, 9), (20, 22)]
        );
    }

    #[test]
    fn detectors_can_be_selected() {
        let s = "a@b.co and +4512345678";
        let only_email = spans(s, Some(&["email".to_string()]));
        assert_eq!(only_email.len(), 1);
        assert!(spans(s, Some(&[])).is_empty());
    }
}
