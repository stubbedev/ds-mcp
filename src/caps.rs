//! Result capping with a way back: values too large for one tool result are
//! truncated in place, but the full (PII-filtered) value is kept in a small
//! per-process store and every truncation marker carries the arguments a
//! model needs to page through the rest via the `read_more` tool. Nothing is
//! ever silently lost — an oversized cell just becomes a follow-up call.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// Hard ceilings so one pathological payload (a multi-MB jsonb cell, a
/// `KEYS *` over a huge keyspace, a base64 blob) cannot flood the model's
/// context. Oversized strings keep a prefix; oversized arrays lose their
/// tail and gain a marker element saying how much was dropped.
pub const MAX_CELL_CHARS: usize = 4096;
pub const MAX_ARRAY_ITEMS: usize = 5000;
const CELL_PREFIX_CHARS: usize = 512;

/// Store limits: enough to keep the last few big results readable without
/// becoming a memory leak. FIFO eviction; a result bigger than the whole
/// budget is not kept (its markers say plain "truncated" with no id).
const MAX_ENTRIES: usize = 16;
const BYTE_BUDGET: usize = 64 << 20;

#[derive(Default)]
pub struct Store {
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, Value>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Store {
    /// Keep `value` for later `read_more` calls; None when it is too large
    /// to hold on to.
    pub fn insert(&self, value: Value) -> Option<String> {
        let bytes = approx_size(&value);
        if bytes > BYTE_BUDGET {
            return None;
        }
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.map.insert(id.clone(), value);
            inner.order.push_back(id.clone());
            inner.bytes += bytes;
            while inner.bytes > BYTE_BUDGET || inner.order.len() > MAX_ENTRIES {
                let Some(oldest) = inner.order.pop_front() else {
                    break;
                };
                if oldest == id {
                    // The newcomer is itself the oldest: keep it, drop the
                    // budget check for this round (it is under BYTE_BUDGET on
                    // its own).
                    inner.order.push_back(id.clone());
                    break;
                }
                if let Some(v) = inner.map.remove(&oldest) {
                    inner.bytes -= approx_size(&v);
                }
            }
            drop(inner);
        }
        Some(id)
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map
            .get(id)
            .cloned()
    }
}

fn approx_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len() + 8,
        Value::Number(_) => 24,
        Value::Bool(_) | Value::Null => 8,
        Value::Array(items) => 16 + items.iter().map(approx_size).sum::<usize>(),
        Value::Object(map) => {
            32 + map
                .iter()
                .map(|(k, v)| k.len() + approx_size(v))
                .sum::<usize>()
        }
    }
}

/// True when `v` is large enough anywhere that capping would truncate it.
fn needs_capping(v: &Value) -> bool {
    match v {
        Value::String(s) => s.chars().count() > MAX_CELL_CHARS,
        Value::Array(items) => items.len() > MAX_ARRAY_ITEMS || items.iter().any(needs_capping),
        Value::Object(map) => map.values().any(needs_capping),
        _ => false,
    }
}

/// Cap `v` for the model's context, embedding `read_more` continuation args
/// in every marker when a store is given. Returns true if anything was cut.
pub fn cap_for_context(v: &mut Value, store: Option<&Store>) -> bool {
    if !needs_capping(v) {
        return false;
    }
    let id = store.and_then(|s| s.insert(v.clone()));
    cap(v, "", id.as_deref());
    true
}

fn cap(v: &mut Value, pointer: &str, id: Option<&str>) {
    match v {
        Value::String(s) => {
            let total = s.chars().count();
            if total > MAX_CELL_CHARS {
                let prefix: String = s.chars().take(CELL_PREFIX_CHARS).collect();
                *s = format!(
                    "{prefix}{}",
                    marker(id, pointer, CELL_PREFIX_CHARS, total, "chars")
                );
            }
        }
        Value::Array(items) => {
            if items.len() > MAX_ARRAY_ITEMS {
                let dropped = items.len() - MAX_ARRAY_ITEMS;
                let tail = marker(id, pointer, MAX_ARRAY_ITEMS, dropped, "items");
                items.truncate(MAX_ARRAY_ITEMS);
                items.push(Value::String(tail));
            }
            for (i, item) in items.iter_mut().enumerate() {
                cap(item, &format!("{pointer}/{i}"), id);
            }
        }
        Value::Object(map) => {
            for (k, value) in map.iter_mut() {
                cap(value, &format!("{pointer}/{}", escape_key(k)), id);
            }
        }
        _ => {}
    }
}

/// The marker a model sees where data was cut: `[truncated 5000 chars;
/// read_more {"id":"...","ptr":"/rows/0/1","off":512}]`. The embedded args
/// are exactly the `read_more` tool's arguments for the next page.
fn marker(id: Option<&str>, pointer: &str, offset: usize, total: usize, unit: &str) -> String {
    let note = match id {
        Some(id) => format!(
            "; read_more {}",
            serde_json::to_string(&serde_json::json!({
                "id": id,
                "ptr": pointer,
                "off": offset,
            }))
            .unwrap_or_default()
        ),
        None => String::new(),
    };
    format!("[truncated {total} {unit}{note}]")
}

fn escape_key(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Resolve an RFC 6901 JSON pointer against `root`.
fn resolve<'a>(root: &'a Value, pointer: &str) -> Result<&'a Value, String> {
    let mut current = root;
    if pointer.is_empty() {
        return Ok(current);
    }
    for token in pointer.trim_start_matches('/').split('/') {
        let token = token.replace("~1", "/").replace("~0", "~");
        current =
            match current {
                Value::Object(map) => map.get(&token).ok_or_else(|| {
                    format!("pointer {pointer:?}: no key {token:?} at this level")
                })?,
                Value::Array(items) => items
                    .get(token.parse::<usize>().map_err(|_| {
                        format!("pointer {pointer:?}: {token:?} is not an array index")
                    })?)
                    .ok_or_else(|| format!("pointer {pointer:?}: index {token} out of bounds"))?,
                _ => return Err(format!("pointer {pointer:?}: cannot descend into a scalar")),
            };
    }
    Ok(current)
}

/// One `read_more` page of a stored result: a window of chars (strings) or
/// items (arrays) at `pointer`, sized by the caller up to the page maximum.
pub fn read_window(
    root: &Value,
    pointer: &str,
    offset: usize,
    length: Option<usize>,
) -> Result<Value, String> {
    let target = resolve(root, pointer)?;
    match target {
        Value::String(s) => {
            let total = s.chars().count();
            let length = length.unwrap_or(MAX_CELL_CHARS).clamp(1, MAX_CELL_CHARS);
            let skip = offset.min(total);
            let taken = length.min(total - skip);
            let value: String = s.chars().skip(skip).take(taken).collect();
            Ok(window_result(&value.into(), skip, taken, total))
        }
        Value::Array(items) => {
            let total = items.len();
            let length = length.unwrap_or(1000).clamp(1, MAX_ARRAY_ITEMS);
            let skip = offset.min(total);
            let taken = length.min(total - skip);
            let value: Vec<Value> = items.iter().skip(skip).take(taken).cloned().collect();
            Ok(window_result(&value.into(), skip, taken, total))
        }
        other => Ok(serde_json::json!({ "value": other })),
    }
}

fn window_result(value: &Value, offset: usize, length: usize, total: usize) -> Value {
    serde_json::json!({
        "value": value,
        "offset": offset,
        "length": length,
        "total": total,
        "has_more": offset + length < total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn caps_embed_actionable_markers() {
        let store = Store::default();
        let mut v = json!({ "rows": [[ "x".repeat(9000) ]] });
        assert!(cap_for_context(&mut v, Some(&store)));
        let marker = v["rows"][0][0].as_str().expect("string");
        assert!(
            marker.contains("[truncated 9000 chars; read_more "),
            "{marker}"
        );
        let start = marker.find("read_more ").unwrap() + "read_more ".len();
        let args: Value = serde_json::from_str(&marker[start..marker.rfind(']').unwrap()])
            .expect("marker args are JSON");
        let id = args["id"].as_str().unwrap().to_string();
        let page = read_window(
            &store.get(&id).unwrap(),
            args["ptr"].as_str().unwrap(),
            args["off"].as_u64().unwrap().try_into().unwrap(),
            None,
        )
        .unwrap();
        // 9000 total, 512 already shown: the next page starts right after.
        assert_eq!(page["offset"], json!(512));
        assert_eq!(page["total"], json!(9000));
        assert_eq!(page["has_more"], json!(true));
        assert_eq!(page["value"].as_str().unwrap().chars().count(), 4096);
    }

    #[test]
    fn array_markers_point_at_the_dropped_tail() {
        let store = Store::default();
        let mut v = json!(
            (0..MAX_ARRAY_ITEMS + 3)
                .map(|i| format!("row-{i}"))
                .collect::<Vec<_>>()
        );
        assert!(cap_for_context(&mut v, Some(&store)));
        let items = v.as_array().unwrap();
        let marker = items.last().unwrap().as_str().unwrap();
        assert!(marker.contains("3 items"), "{marker}");
        let start = marker.find("read_more ").unwrap() + "read_more ".len();
        let args: Value = serde_json::from_str(&marker[start..marker.rfind(']').unwrap()]).unwrap();
        let page = read_window(
            &store.get(args["id"].as_str().unwrap()).unwrap(),
            "",
            args["off"].as_u64().unwrap().try_into().unwrap(),
            Some(2),
        )
        .unwrap();
        assert_eq!(page["value"], json!(["row-5000", "row-5001"]));
        assert_eq!(page["has_more"], json!(true));
    }

    #[test]
    fn small_payloads_pass_through_and_store_nothing() {
        let store = Store::default();
        let mut v = json!({ "ok": "short", "list": [1, 2, 3] });
        assert!(!cap_for_context(&mut v, Some(&store)));
        assert_eq!(v, json!({ "ok": "short", "list": [1, 2, 3] }));
        assert!(store.inner.lock().unwrap().map.is_empty());
    }

    #[test]
    fn pointers_use_rfc6901_escapes() {
        let root = json!({ "a/b": { "c~d": "z".repeat(9000) } });
        let store = Store::default();
        let mut v = root;
        cap_for_context(&mut v, Some(&store));
        let marker = v["a/b"]["c~d"].as_str().unwrap();
        let start = marker.find("read_more ").unwrap() + "read_more ".len();
        let args: Value = serde_json::from_str(&marker[start..marker.rfind(']').unwrap()]).unwrap();
        let page = read_window(
            &store.get(args["id"].as_str().unwrap()).unwrap(),
            args["ptr"].as_str().unwrap(),
            0,
            Some(1),
        )
        .unwrap();
        assert_eq!(page["value"], json!("z"));
    }

    #[test]
    fn eviction_is_fifo_and_reports_unknown_ids() {
        let store = Store::default();
        let big = "y".repeat(MAX_CELL_CHARS * 2);
        let first = store.insert(json!(big)).unwrap();
        for _ in 0..MAX_ENTRIES {
            store.insert(json!(big)).unwrap();
        }
        // `first` was pushed out by the newer entries.
        assert!(store.get(&first).is_none());
    }

    #[test]
    fn oversize_values_are_not_kept_but_still_capped() {
        let store = Store::default();
        let mut v = json!("w".repeat(BYTE_BUDGET + MAX_CELL_CHARS));
        assert!(cap_for_context(&mut v, Some(&store)));
        let s = v.as_str().unwrap();
        assert!(s.contains("[truncated") && !s.contains("read_more"), "{s}");
    }

    #[test]
    fn window_errors_name_the_pointer() {
        let root = json!({ "a": [1, 2] });
        assert!(read_window(&root, "/b", 0, None).is_err());
        assert!(read_window(&root, "/a/x", 0, None).is_err());
        assert!(read_window(&root, "/a/9", 0, None).is_err());
        // Root and plain scalars work too.
        let page = read_window(&root, "", 0, None).unwrap();
        assert_eq!(page["value"], json!({ "a": [1, 2] }));
    }
}
