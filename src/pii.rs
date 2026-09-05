//! Outbound redaction. `readonly` protects the database from the model; this
//! protects the data subjects from the client: matching columns/fields are
//! masked after the engine has answered and before the result reaches the
//! transcript (and whatever the client logs or ships upstream).
//!
//! Filtering is on the way out only, so a `WHERE email = '...'` still works
//! and server-side aggregates over a redacted column come back untouched.
//! Blocking columns from predicates too is a bigger change (it needs sqlguard)
//! and deliberately not done here.

use std::fmt::Write as _;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::config::{Pii, PiiMode};
use crate::source::ResultSet;

pub const REDACTED: &str = "[redacted]";

/// One call's redaction scope: the source's rules plus the relations the
/// payload touched, which is what `table.column` patterns match against.
#[derive(Clone, Copy)]
pub struct Filter<'a> {
    columns: &'a [String],
    /// Value detectors to run; None = all of them. See `crate::detect`.
    values: Option<&'a [String]>,
    mode: PiiMode,
    tables: &'a [String],
}

impl<'a> Filter<'a> {
    /// None when the source has no `pii` block, or has `"pii": false`.
    pub fn new(pii: Option<&'a Pii>, tables: &'a [String]) -> Option<Self> {
        let (columns, values, mode) = pii?.resolve()?;
        Some(Self {
            columns,
            values,
            mode,
            tables,
        })
    }

    /// Does this column/field name match — on its own, or qualified by one of
    /// the relations this call touched?
    fn matches(&self, name: &str) -> bool {
        self.columns.iter().any(|p| {
            if p.contains('.') {
                self.tables
                    .iter()
                    .any(|t| glob_ci(p, &format!("{t}.{name}")))
            } else {
                glob_ci(p, name)
            }
        })
    }

    /// Redact a serialized tool result in place.
    pub fn apply(&self, v: &mut Value) {
        match v {
            Value::Array(items) => items.iter_mut().for_each(|i| self.apply(i)),
            Value::Object(map) => {
                if !self.apply_table(map) {
                    self.apply_fields(map);
                }
            }
            // A value the column name said nothing about. Only the detected
            // span is masked, so the rest of a free-text field survives.
            Value::String(s) => {
                if let Some(masked) = self.scan(s) {
                    *v = Value::String(masked);
                }
            }
            _ => {}
        }
    }

    /// Mask every detected value inside a string, or None if it holds none.
    fn scan(&self, s: &str) -> Option<String> {
        if self.values.is_some_and(<[String]>::is_empty) {
            return None;
        }
        let spans = crate::detect::spans(s, self.values);
        if spans.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(s.len());
        let mut cursor = 0;
        for (start, end) in spans {
            out.push_str(&s[cursor..start]);
            // `drop` cannot remove half a value, so a detected span inside a
            // larger string is redacted (or hashed) in place. Writing to a
            // String cannot fail.
            match self.mode {
                PiiMode::Hash => {
                    let digest = hash_prefix(&Value::String(s[start..end].into()));
                    let _ = write!(out, "sha256:{digest}");
                }
                PiiMode::Redact | PiiMode::Drop => out.push_str(REDACTED),
            }
            cursor = end;
        }
        out.push_str(&s[cursor..]);
        Some(out)
    }

    /// `schema(table)` returns a table's columns as *rows*, so the name globs
    /// can say up front which of them come back masked — the model stops
    /// spending queries to find out.
    pub fn mark_described_columns(&self, rs: &mut ResultSet) {
        // Each engine names the column-name column differently.
        let Some(idx) = rs.columns.iter().position(|c| {
            matches!(
                c.to_ascii_lowercase().as_str(),
                "field" | "column_name" | "name"
            )
        }) else {
            return;
        };
        rs.columns.push("pii".into());
        for row in &mut rs.rows {
            let hit = row
                .get(idx)
                .and_then(Value::as_str)
                .is_some_and(|name| self.matches(name));
            row.push(Value::Bool(hit));
        }
    }

    /// `ResultSet`'s wire shape — `{"columns": [names], "rows": [[values]]}`.
    /// The names live in `columns`, so matching has to be positional: the row
    /// cells are values and must never be matched as field names. Returns
    /// false (leaving the object to `apply_fields`) when this isn't one.
    fn apply_table(&self, map: &mut Map<String, Value>) -> bool {
        let Some(Value::Array(cols)) = map.get("columns") else {
            return false;
        };
        if !map.get("rows").is_some_and(Value::is_array) {
            return false;
        }
        let hits: Vec<bool> = cols
            .iter()
            .map(|c| c.as_str().is_some_and(|n| self.matches(n)))
            .collect();
        let drop = self.mode == PiiMode::Drop;
        if let Some(Value::Array(rows)) = map.get_mut("rows") {
            for row in rows.iter_mut() {
                let Value::Array(cells) = row else { continue };
                for (i, cell) in cells.iter_mut().enumerate() {
                    match hits.get(i) {
                        Some(true) => {
                            if !drop {
                                mask(cell, self.mode);
                            }
                        }
                        // A JSON/JSONB cell can carry sensitive keys of its own.
                        _ => self.apply(cell),
                    }
                }
                if drop {
                    drop_hits(cells, &hits);
                }
            }
        }
        if drop && let Some(Value::Array(cols)) = map.get_mut("columns") {
            drop_hits(cols, &hits);
        }
        true
    }

    fn apply_fields(&self, map: &mut Map<String, Value>) {
        if self.mode == PiiMode::Drop {
            map.retain(|k, _| !self.matches(k));
        }
        for (k, v) in map.iter_mut() {
            if self.mode != PiiMode::Drop && self.matches(k) {
                mask(v, self.mode);
            } else {
                self.apply(v);
            }
        }
    }
}

/// Remove the positions flagged in `hits`, keeping the rest in order.
fn drop_hits(items: &mut Vec<Value>, hits: &[bool]) {
    let mut i = 0;
    items.retain(|_| {
        let keep = !hits.get(i).copied().unwrap_or(false);
        i += 1;
        keep
    });
}

/// NULL stays NULL: it carries nothing, and keeping it leaves the result
/// honest about which values were actually present.
fn mask(v: &mut Value, mode: PiiMode) {
    if v.is_null() {
        return;
    }
    *v = match mode {
        PiiMode::Hash => Value::String(format!("sha256:{}", hash_prefix(v))),
        // Drop never reaches here — the caller removes the key/column.
        PiiMode::Redact | PiiMode::Drop => Value::String(REDACTED.into()),
    };
}

/// First 8 bytes of the SHA-256 of the value's JSON form. Stable across calls
/// and processes, so equal values still group and join; it is a pseudonym,
/// not a secret — a short value stays guessable by anyone holding the hash.
fn hash_prefix(v: &Value) -> String {
    let bytes = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let digest = Sha256::digest(bytes.as_bytes());
    crate::source::hex(&digest[..8])
}

/// Glob match: `*` is any run of characters, everything else literal,
/// case-insensitive. Iterative with backtracking, so a pattern full of stars
/// cannot blow the stack.
fn glob_ci(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            retry = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PiiRules;
    use serde_json::json;

    /// Column patterns only — the value detectors get their own tests below.
    fn rules(columns: &[&str], mode: PiiMode) -> Pii {
        Pii::Rules(PiiRules {
            columns: Some(columns.iter().map(|s| (*s).to_string()).collect()),
            values: Some(Vec::new()),
            mode,
        })
    }

    fn run(pii: &Pii, tables: &[String], mut v: Value) -> Value {
        Filter::new(Some(pii), tables).unwrap().apply(&mut v);
        v
    }

    #[test]
    fn glob_basics() {
        assert!(glob_ci("email", "EMail"));
        assert!(glob_ci("*_ssn", "user_ssn"));
        assert!(!glob_ci("*_ssn", "ssn_user"));
        assert!(glob_ci("*token*", "refresh_TOKEN_v2"));
        assert!(glob_ci("*", "anything"));
        assert!(glob_ci("a**b", "ab"));
        assert!(!glob_ci("a*b", "a"));
        assert!(glob_ci("users.address_*", "users.address_line1"));
    }

    #[test]
    fn redacts_result_set_columns_by_position() {
        let rs = json!({
            "columns": ["id", "email", "note"],
            "rows": [[1, "a@b.c", "hi"], [2, null, "yo"]],
            "row_count": 2,
            "truncated": false,
        });
        let out = run(&rules(&["email"], PiiMode::Redact), &[], rs);
        assert_eq!(out["rows"][0][1], json!(REDACTED));
        // NULL stays NULL; untouched columns keep their values.
        assert_eq!(out["rows"][1][1], json!(null));
        assert_eq!(out["rows"][0][0], json!(1));
        assert_eq!(out["rows"][0][2], json!("hi"));
    }

    #[test]
    fn drop_removes_column_and_cells() {
        let rs = json!({
            "columns": ["id", "email"],
            "rows": [[1, "a@b.c"]],
            "row_count": 1,
        });
        let out = run(&rules(&["email"], PiiMode::Drop), &[], rs);
        assert_eq!(out["columns"], json!(["id"]));
        assert_eq!(out["rows"], json!([[1]]));
    }

    #[test]
    fn hash_is_stable_and_equal_values_collide() {
        let rs = json!({
            "columns": ["email"],
            "rows": [["a@b.c"], ["a@b.c"], ["z@y.x"]],
        });
        let out = run(&rules(&["email"], PiiMode::Hash), &[], rs);
        let (a, b, c) = (&out["rows"][0][0], &out["rows"][1][0], &out["rows"][2][0]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.as_str().unwrap().starts_with("sha256:"));
        assert_ne!(a.as_str().unwrap(), "a@b.c");
    }

    #[test]
    fn redacts_document_fields_and_nested_json_cells() {
        // Mongo/Redis/REST shape: match on keys, at any depth.
        let doc = json!({"documents": [{"_id": 1, "user": {"email": "a@b.c", "age": 3}}]});
        let out = run(&rules(&["email"], PiiMode::Redact), &[], doc);
        assert_eq!(out["documents"][0]["user"]["email"], json!(REDACTED));
        assert_eq!(out["documents"][0]["user"]["age"], json!(3));

        // A JSONB cell inside a tabular result gets the same treatment.
        let rs = json!({"columns": ["id", "meta"], "rows": [[1, {"email": "a@b.c"}]]});
        let out = run(&rules(&["email"], PiiMode::Redact), &[], rs);
        assert_eq!(out["rows"][0][1]["email"], json!(REDACTED));
    }

    #[test]
    fn qualified_patterns_need_the_table() {
        let rs = json!({"columns": ["address"], "rows": [["Main St 1"]]});
        let pii = rules(&["users.address*"], PiiMode::Redact);
        let touched = vec!["users".to_string()];
        assert_eq!(
            run(&pii, &touched, rs.clone())["rows"][0][0],
            json!(REDACTED)
        );
        // Same column on another table is left alone.
        let elsewhere = vec!["warehouses".to_string()];
        assert_eq!(run(&pii, &elsewhere, rs)["rows"][0][0], json!("Main St 1"));
    }

    #[test]
    fn short_form_covers_the_obvious_columns() {
        let rs = json!({
            "columns": ["id", "email", "password_hash", "api_token", "street_address", "widget_count"],
            "rows": [[1, "a@b.c", "$2y$x", "tok", "Main St 1", 7]],
        });
        let out = run(&Pii::Enabled(true), &[], rs);
        for i in 1..=4 {
            assert_eq!(out["rows"][0][i], json!(REDACTED), "column {i}");
        }
        assert_eq!(out["rows"][0][0], json!(1));
        assert_eq!(out["rows"][0][5], json!(7));
    }

    #[test]
    fn value_detectors_catch_what_the_column_name_hides() {
        // `note` is an innocent column name; the value is not.
        let rs = json!({
            "columns": ["id", "note"],
            "rows": [[1, "chased a@b.co, card 4242 4242 4242 4242, rest is fine"]],
        });
        let out = run(&Pii::Enabled(true), &[], rs);
        assert_eq!(
            out["rows"][0][1],
            json!("chased [redacted], card [redacted], rest is fine"),
        );
        // Non-string cells and clean text are untouched.
        assert_eq!(out["rows"][0][0], json!(1));
    }

    #[test]
    fn value_detection_reaches_bare_and_nested_values() {
        // Redis returns a bare string; Mongo an arbitrary key.
        let out = run(&Pii::Enabled(true), &[], json!({"result": "a@b.co"}));
        assert_eq!(out["result"], json!("[redacted]"));
        let out = run(
            &Pii::Enabled(true),
            &[],
            json!({"d": [{"x": {"y": "a@b.co"}}]}),
        );
        assert_eq!(out["d"][0]["x"]["y"], json!("[redacted]"));
    }

    #[test]
    fn hash_mode_hashes_the_detected_span_only() {
        let out = run(
            &Pii::Rules(PiiRules {
                columns: Some(Vec::new()),
                values: None,
                mode: PiiMode::Hash,
            }),
            &[],
            json!({"result": "mail a@b.co now"}),
        );
        let s = out["result"].as_str().unwrap();
        assert!(s.starts_with("mail sha256:") && s.ends_with(" now"), "{s}");
        assert!(!s.contains("a@b.co"));
    }

    #[test]
    fn values_can_be_turned_off_leaving_column_names() {
        let rules = Pii::Rules(PiiRules {
            columns: Some(vec!["note".into()]),
            values: Some(Vec::new()),
            mode: PiiMode::Redact,
        });
        let out = run(
            &rules,
            &[],
            json!({"columns": ["other"], "rows": [["a@b.co"]]}),
        );
        assert_eq!(out["rows"][0][0], json!("a@b.co"));
    }

    #[test]
    fn describe_output_gains_a_pii_column() {
        let mut rs = ResultSet {
            columns: vec!["Field".into(), "Type".into()],
            rows: vec![
                vec![json!("id"), json!("int")],
                vec![json!("email"), json!("text")],
            ],
            row_count: 2,
            truncated: false,
        };
        Filter::new(Some(&Pii::Enabled(true)), &[])
            .unwrap()
            .mark_described_columns(&mut rs);
        assert_eq!(rs.columns.last().unwrap(), "pii");
        assert_eq!(rs.rows[0].last().unwrap(), &json!(false));
        assert_eq!(rs.rows[1].last().unwrap(), &json!(true));
    }

    #[test]
    fn no_filter_when_disabled() {
        assert!(Filter::new(None, &[]).is_none());
        assert!(Filter::new(Some(&Pii::Enabled(false)), &[]).is_none());
    }
}
