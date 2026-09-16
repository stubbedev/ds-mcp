//! End-to-end smoke test: spawn the real binary, speak JSON-RPC over stdio
//! against a throwaway sqlite file. No docker needed, runs in `cargo test`.

// clippy.toml's allow-*-in-tests only covers `#[test]` functions; the harness
// helpers below are plain fns, and a failed unwrap there is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn call(id: u64, tool: &str, args: &Value) -> String {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": tool, "arguments": args}
    })
    .to_string()
}

/// Run the server over stdio as a well-behaved client: send one request,
/// wait for its response, then send the next. (The server handles concurrent
/// requests concurrently, so firing them all at once would race.)
fn run_session(config: &str, requests: &[String]) -> Vec<Value> {
    let mut session = Session::start(config);
    let mut responses = Vec::new();
    for r in requests {
        responses.push(session.round_trip(r));
    }
    session.close();
    responses
}

/// One live stdio session against a freshly spawned server. Use when a
/// request depends on an earlier response (e.g. a `read_more` id); otherwise
/// prefer `run_session`.
struct Session {
    stdin: std::process::ChildStdin,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
    child: std::process::Child,
}

impl Session {
    fn start(config: &str) -> Self {
        static SESSION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.json");
        std::fs::write(&cfg_path, config).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_ds-mcp"))
            .args(["serve", "--config"])
            .arg(&cfg_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut lines = std::io::BufRead::lines(stdout);
        let send = |stdin: &mut std::process::ChildStdin, req: &str| {
            stdin.write_all(req.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
        };
        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "e2e", "version": "0"}}
            })
            .to_string(),
        );
        let recv =
            |lines: &mut std::io::Lines<std::io::BufReader<std::process::ChildStdout>>| -> Value {
                loop {
                    let line = lines.next().expect("server closed stdout").unwrap();
                    if !line.trim().is_empty() {
                        return serde_json::from_str(&line).expect("stdout line is JSON");
                    }
                }
            };
        recv(&mut lines);
        send(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        );
        Self {
            stdin,
            lines,
            child,
        }
    }

    /// Send one request line, return its response.
    fn round_trip(&mut self, req: &str) -> Value {
        self.stdin.write_all(req.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        loop {
            let line = self.lines.next().expect("server closed stdout").unwrap();
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).expect("stdout line is JSON");
            }
        }
    }

    /// EOF ends the session; the server exits when the client disconnects.
    fn close(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().unwrap().success());
    }
}

/// Extract the text content of the tool result with the given id, plus its
/// isError flag.
fn tool_result(responses: &[Value], id: u64) -> (String, bool) {
    let resp = responses
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no response with id {id}: {responses:?}"));
    let result = &resp["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let is_error = result["isError"].as_bool().unwrap_or(false);
    (text, is_error)
}

#[test]
fn duckdb_end_to_end() {
    let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-duck-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("demo.duckdb");
    let _ = std::fs::remove_file(&db);
    let db = db.to_str().unwrap();

    let config = json!({
        "sources": {"duck": {"engine": "duckdb", "path": db}}
    })
    .to_string();

    let responses = run_session(
        &config,
        &[
            call(
                1,
                "execute",
                &json!({"source": "duck",
                "query": "CREATE TABLE widgets(id INTEGER, name TEXT, price DECIMAL(8,2), added DATE)"}),
            ),
            call(
                2,
                "execute",
                &json!({"source": "duck",
                "query": "INSERT INTO widgets VALUES (1, 'sprocket', 9.95, DATE '2026-01-02')"}),
            ),
            call(
                3,
                "query",
                &json!({"source": "duck",
                "query": "SELECT id, name, price, added FROM widgets"}),
            ),
            call(4, "schema", &json!({"source": "duck"})),
            call(
                5,
                "query",
                &json!({"source": "duck", "query": "DROP TABLE widgets"}),
            ),
        ],
    );

    let (text, is_error) = tool_result(&responses, 3);
    assert!(!is_error, "select failed: {text}");
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["rows"][0][1], json!("sprocket"), "{text}");
    assert_eq!(rs["rows"][0][2], json!("9.95"), "{text}");
    assert_eq!(rs["rows"][0][3], json!("2026-01-02"), "{text}");

    let (text, is_error) = tool_result(&responses, 4);
    assert!(!is_error && text.contains("widgets"), "{text}");

    let (_, is_error) = tool_result(&responses, 5);
    assert!(is_error, "DROP via query must be rejected");
}

#[test]
fn sqlite_end_to_end() {
    let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("demo.db");
    // An empty file is a valid sqlite database.
    std::fs::write(&db, b"").unwrap();
    let db = db.to_str().unwrap();

    let config = json!({
        "sources": {
            "demo": {"engine": "sqlite", "path": db},
            "demo_ro": {"engine": "sqlite", "path": db, "readonly": true,
                        "description": "read-only view of demo"}
        }
    })
    .to_string();

    let responses = run_session(
        &config,
        &[
            call(
                1,
                "execute",
                &json!({"source": "demo",
                "query": "CREATE TABLE IF NOT EXISTS widgets(id INTEGER PRIMARY KEY, name TEXT)"}),
            ),
            call(
                2,
                "execute",
                &json!({"source": "demo",
                "query": "INSERT INTO widgets(name) VALUES ('sprocket')"}),
            ),
            call(
                3,
                "query",
                &json!({"source": "demo",
                "query": "SELECT id, name FROM widgets"}),
            ),
            call(
                4,
                "query",
                &json!({"source": "demo", "query": "DROP TABLE widgets"}),
            ),
            call(
                5,
                "execute",
                &json!({"source": "demo_ro",
                "query": "INSERT INTO widgets(name) VALUES ('nope')"}),
            ),
            call(6, "list_sources", &json!({})),
            call(7, "schema", &json!({"source": "demo"})),
            call(
                8,
                "query",
                &json!({"source": "missing", "query": "SELECT 1"}),
            ),
        ],
    );

    let (text, is_error) = tool_result(&responses, 1);
    assert!(!is_error, "create table failed: {text}");

    let (text, is_error) = tool_result(&responses, 2);
    assert!(!is_error, "insert failed: {text}");
    assert!(text.contains("\"rows_affected\":1"), "{text}");

    let (text, is_error) = tool_result(&responses, 3);
    assert!(!is_error, "select failed: {text}");
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["row_count"], json!(1), "{text}");
    assert_eq!(rs["rows"][0][1], json!("sprocket"), "{text}");
    assert_eq!(rs["truncated"], json!(false), "{text}");

    let (text, is_error) = tool_result(&responses, 4);
    assert!(is_error, "DROP via query must be rejected");
    assert!(text.contains("execute"), "{text}");

    let (text, is_error) = tool_result(&responses, 5);
    assert!(is_error, "write on readonly source must be rejected");
    assert!(text.contains("read-only"), "{text}");

    let (text, is_error) = tool_result(&responses, 6);
    assert!(!is_error, "{text}");
    let sources: Value = serde_json::from_str(&text).unwrap();
    let names: Vec<_> = sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["demo", "demo_ro"], "{text}");

    let (text, is_error) = tool_result(&responses, 7);
    assert!(!is_error, "{text}");
    assert!(text.contains("widgets"), "{text}");

    let (text, is_error) = tool_result(&responses, 8);
    assert!(is_error);
    assert!(
        text.contains("unknown source") && text.contains("demo"),
        "{text}"
    );
}

/// A value too large for one result is truncated in place with a marker
/// whose embedded arguments the model can pass straight back to `read_more`
/// to page through the rest.
#[test]
fn read_more_pages_through_truncated_cells() {
    let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-more-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("more.db");
    // An empty file is a valid sqlite database (the server does not create
    // missing sqlite files).
    std::fs::write(&db, b"").unwrap();
    let db = db.to_str().unwrap();

    let config = json!({
        "sources": {"db": {"engine": "sqlite", "path": db}}
    })
    .to_string();

    let mut session = Session::start(&config);
    let tool = |session: &mut Session, id: u64, name: &str, args: &Value| {
        let resp = session.round_trip(&call(id, name, args));
        let result = &resp["result"];
        (
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            result["isError"].as_bool().unwrap_or(false),
        )
    };

    let (text, is_error) = tool(
        &mut session,
        1,
        "execute",
        &json!({"source": "db",
        "query": "CREATE TABLE big(id INTEGER, blob TEXT)"}),
    );
    assert!(!is_error, "create failed: {text}");
    // 6000 a's + 6000 b's: one cell, two read_more pages after the prefix.
    let (text, is_error) = tool(
        &mut session,
        2,
        "execute",
        &json!({"source": "db",
        "query": format!("INSERT INTO big VALUES (1, '{}{}')",
            "a".repeat(6000), "b".repeat(6000))}),
    );
    assert!(!is_error, "insert failed: {text}");
    let (text, is_error) = tool(
        &mut session,
        3,
        "query",
        &json!({"source": "db",
        "query": "SELECT blob FROM big"}),
    );
    assert!(!is_error, "select failed: {text}");

    // Parse the outer JSON first: the marker rides inside a cell, so its
    // quotes are escaped in the serialized text but real once decoded.
    let rs: Value = serde_json::from_str(&text).unwrap();
    let cell = rs["rows"][0][0].as_str().expect("cell is a string");
    let start = cell
        .find("read_more ")
        .unwrap_or_else(|| panic!("marker missing: {cell}"));
    let args: Value = {
        let tail = &cell[start + "read_more ".len()..];
        let end = tail.find(']').expect("marker closes");
        serde_json::from_str(&tail[..end]).expect("marker args are JSON")
    };

    // Page one: the marker's own arguments, passed straight back.
    let (text, is_error) = tool(
        &mut session,
        4,
        "read_more",
        &json!({
            "id": args["id"], "pointer": args["ptr"], "offset": args["off"],
        }),
    );
    assert!(!is_error, "read_more failed: {text}");
    let page: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(page["offset"], json!(512));
    assert_eq!(page["total"], json!(12000));
    assert_eq!(page["has_more"], json!(true));
    let value = page["value"].as_str().unwrap();
    assert_eq!(value.chars().count(), 4096);
    assert!(value.starts_with('a'));

    // Page two crosses the a/b boundary the marker promised to reach.
    let (text, is_error) = tool(
        &mut session,
        5,
        "read_more",
        &json!({
            "id": args["id"], "pointer": args["ptr"], "offset": 512 + 4096,
        }),
    );
    assert!(!is_error, "read_more failed: {text}");
    let page: Value = serde_json::from_str(&text).unwrap();
    let value = page["value"].as_str().unwrap();
    assert!(value.starts_with('a') && value.ends_with('b'), "{text}");
    session.close();

    // Ids do not survive into a new server process.
    let mut stranger = Session::start(&config);
    let (text, is_error) = tool(&mut stranger, 6, "read_more", &json!({"id": args["id"]}));
    assert!(is_error);
    assert!(text.contains("unknown or evicted"), "{text}");
    stranger.close();
}

/// PII redaction is applied to real tool output, on the way out of the
/// server — including a qualified `table.column` pattern, which needs the
/// relation parsed out of the SQL.
#[test]
fn pii_redaction_end_to_end() {
    let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-pii-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("pii.db");
    std::fs::write(&db, b"").unwrap();
    let db = db.to_str().unwrap();

    let config = json!({
        "sources": {
            "plain": {"engine": "sqlite", "path": db},
            "short": {"engine": "sqlite", "path": db, "pii": true},
            // Column patterns in isolation: value detectors off.
            "qualified": {"engine": "sqlite", "path": db,
                          "pii": {"columns": ["users.note"], "values": [], "mode": "drop"}}
        }
    })
    .to_string();

    let select = "SELECT id, email, note FROM users";
    let responses = run_session(
        &config,
        &[
            call(
                1,
                "execute",
                &json!({"source": "plain",
                "query": "CREATE TABLE users(id INTEGER, email TEXT, note TEXT)"}),
            ),
            call(
                2,
                "execute",
                &json!({"source": "plain",
                "query": "INSERT INTO users VALUES (1, 'a@b.co', 'hi'), (2, NULL, 'call +45 12 34 56 78 or z@y.co')"}),
            ),
            call(3, "query", &json!({"source": "plain", "query": select})),
            call(4, "query", &json!({"source": "short", "query": select})),
            call(5, "query", &json!({"source": "qualified", "query": select})),
            call(
                6,
                "query",
                &json!({"source": "qualified", "query": "SELECT note FROM (SELECT note FROM users) x"}),
            ),
            call(7, "list_sources", &json!({})),
            call(8, "schema", &json!({"source": "short", "table": "users"})),
        ],
    );

    // No `pii` block: values come through untouched.
    let (text, is_error) = tool_result(&responses, 3);
    assert!(!is_error, "select failed: {text}");
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["rows"][0][1], json!("a@b.co"), "{text}");

    // `"pii": true`: the default patterns catch `email`; NULL stays NULL and
    // the other columns are untouched.
    let (text, _) = tool_result(&responses, 4);
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["rows"][0][1], json!("[redacted]"), "{text}");
    assert_eq!(rs["rows"][1][1], json!(null), "{text}");
    assert_eq!(rs["rows"][0][0], json!(1), "{text}");
    assert_eq!(rs["rows"][0][2], json!("hi"), "{text}");

    // Qualified pattern + drop mode: `note` disappears, `email` (not in this
    // source's pattern list) stays.
    let (text, _) = tool_result(&responses, 5);
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["columns"], json!(["id", "email"]), "{text}");
    assert_eq!(rs["rows"][0], json!([1, "a@b.co"]), "{text}");

    // The relation is found through a subquery too.
    let (text, _) = tool_result(&responses, 6);
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["columns"], json!([]), "{text}");

    // list_sources tells the model which sources redact.
    let (text, _) = tool_result(&responses, 7);
    let listed: Value = serde_json::from_str(&text).unwrap();
    let flags: Vec<(&str, bool)> = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap(),
                s["pii"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert!(flags.contains(&("plain", false)), "{text}");
    assert!(flags.contains(&("short", true)), "{text}");

    // Value detectors reach a column no pattern names: `note` is innocent, its
    // contents are not, and only the detected spans are masked.
    let (text, _) = tool_result(&responses, 4);
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        rs["rows"][1][2],
        json!("call [redacted] or [redacted]"),
        "{text}"
    );

    // schema(table) says up front which columns come back masked.
    let (text, _) = tool_result(&responses, 8);
    let described: Value = serde_json::from_str(&text).unwrap();
    let cols = &described["columns"];
    let headers = cols["columns"].as_array().unwrap();
    let name_at = headers
        .iter()
        .position(|c| c == "name")
        .expect("sqlite describe has a `name` column");
    let pii_at = headers.len() - 1;
    assert_eq!(headers[pii_at], json!("pii"), "{text}");
    for row in cols["rows"].as_array().unwrap() {
        let flagged = row[pii_at] == json!(true);
        assert_eq!(flagged, row[name_at] == json!("email"), "{text}");
    }
}
