//! End-to-end smoke test: spawn the real binary, speak JSON-RPC over stdio
//! against a throwaway sqlite file. No docker needed, runs in `cargo test`.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn call(id: u64, tool: &str, args: Value) -> String {
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
    let dir = std::env::temp_dir().join(format!("ds-mcp-e2e-{}", std::process::id()));
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
    let mut send = |req: &str| {
        stdin.write_all(req.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
    };
    let mut recv = || -> Value {
        loop {
            let line = lines.next().expect("server closed stdout").unwrap();
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).expect("stdout line is JSON");
            }
        }
    };

    send(
        &json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "e2e", "version": "0"}}
        })
        .to_string(),
    );
    recv();
    send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string());

    let mut responses = Vec::new();
    for r in requests {
        send(r);
        responses.push(recv());
    }
    // EOF ends the session; the server exits when the client disconnects.
    drop(stdin);
    assert!(child.wait().unwrap().success());
    responses
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
                "write_query",
                json!({"source": "demo",
                "sql": "CREATE TABLE IF NOT EXISTS widgets(id INTEGER PRIMARY KEY, name TEXT)"}),
            ),
            call(
                2,
                "write_query",
                json!({"source": "demo",
                "sql": "INSERT INTO widgets(name) VALUES ('sprocket')"}),
            ),
            call(
                3,
                "read_query",
                json!({"source": "demo",
                "sql": "SELECT id, name FROM widgets"}),
            ),
            call(
                4,
                "read_query",
                json!({"source": "demo", "sql": "DROP TABLE widgets"}),
            ),
            call(
                5,
                "write_query",
                json!({"source": "demo_ro",
                "sql": "INSERT INTO widgets(name) VALUES ('nope')"}),
            ),
            call(6, "list_sources", json!({})),
            call(7, "list_tables", json!({"source": "demo"})),
            call(
                8,
                "read_query",
                json!({"source": "missing", "sql": "SELECT 1"}),
            ),
        ],
    );

    let (text, is_error) = tool_result(&responses, 1);
    assert!(!is_error, "create table failed: {text}");

    let (text, is_error) = tool_result(&responses, 2);
    assert!(!is_error, "insert failed: {text}");
    assert!(text.contains("\"rows_affected\": 1"), "{text}");

    let (text, is_error) = tool_result(&responses, 3);
    assert!(!is_error, "select failed: {text}");
    let rs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(rs["row_count"], json!(1), "{text}");
    assert_eq!(rs["rows"][0][1], json!("sprocket"), "{text}");
    assert_eq!(rs["truncated"], json!(false), "{text}");

    let (text, is_error) = tool_result(&responses, 4);
    assert!(is_error, "DROP via read_query must be rejected");
    assert!(text.contains("write_query"), "{text}");

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
