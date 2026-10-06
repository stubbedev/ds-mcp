//! End-to-end check of per-workspace sources over the HTTP transport: the
//! server's own cwd and global config say nothing about a client, so a
//! client on MCP 2026-07-28 (no `roots/list`) names its workspace via the
//! `X-Repo-Root` header.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-mcp-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Kills the server when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn list_sources(client: &reqwest::Client, url: &str, root: Option<&str>) -> (String, bool) {
    let mut req = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "list_sources")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "list_sources", "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }));
    if let Some(root) = root {
        req = req.header("X-Repo-Root", root);
    }
    let body = req.send().await.unwrap().text().await.unwrap();
    // JSON or a single SSE event, depending on the server's response mode.
    let json = body
        .lines()
        .find_map(|l| l.strip_prefix("data:"))
        .unwrap_or(&body)
        .trim();
    let resp: Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {body}"));
    let result = &resp["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (text, result["isError"].as_bool().unwrap_or(false))
}

#[tokio::test]
async fn http_workspace_config_from_header() {
    let workspace = scratch_dir("workspace");
    std::fs::write(
        workspace.join(".ds-mcp.json"),
        r#"{"sources":{"proj":{"engine":"sqlite","path":"proj.db"}}}"#,
    )
    .unwrap();
    let elsewhere = scratch_dir("cwd");

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ds-mcp"));
    cmd.args(["serve", "--transport", "http", "--http-addr", &addr])
        .current_dir(&elsewhere)
        .env("HOME", &elsewhere)
        .env("XDG_CONFIG_HOME", elsewhere.join(".config"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("DS_MCP_") {
            cmd.env_remove(key);
        }
    }
    let _server = Server(cmd.spawn().unwrap());

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..100 {
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "server did not come up on {addr}");
    let url = format!("{base}/mcp");

    let (text, is_error) = list_sources(&client, &url, Some(workspace.to_str().unwrap())).await;
    assert!(!is_error, "{text}");
    assert!(text.contains("\"proj\""), "{text}");

    // No header: the server's cwd is not the client's workspace, so nothing.
    let (text, is_error) = list_sources(&client, &url, None).await;
    assert!(is_error, "{text}");
    assert!(text.contains("no sources configured"), "{text}");
}
