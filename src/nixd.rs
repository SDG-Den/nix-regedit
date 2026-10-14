use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

fn nix_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Build the nixd `options.<name>.expr` for a target.
fn options_expr(flake_path: &str, target: &str) -> Option<String> {
    let f = nix_string(flake_path);
    let parts: Vec<&str> = target.split(':').collect();
    match parts.first().copied()? {
        "fp" => Some(format!("(builtins.getFlake {f}).debug.options")),
        "nixos" => {
            let host = nix_string(parts.get(1)?);
            Some(format!(
                "(builtins.getFlake {f}).nixosConfigurations.{host}.options"
            ))
        }
        "home" => {
            let user = nix_string(parts.get(1)?);
            Some(format!(
                "(builtins.getFlake {f}).homeConfigurations.{user}.options"
            ))
        }
        "hm" => {
            let host = nix_string(parts.get(1)?);
            Some(format!(
                "(builtins.getFlake {f}).nixosConfigurations.{host}.options.home-manager.users.type.getSubOptions []"
            ))
        }
        "darwin" => {
            let host = nix_string(parts.get(1)?);
            Some(format!(
                "(builtins.getFlake {f}).darwinConfigurations.{host}.options"
            ))
        }
        _ => None,
    }
}

fn write_message(stdin: &mut ChildStdin, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()
}

fn read_message(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "nixd closed the connection",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            length = rest.trim().parse().ok();
        }
    }
    let length = length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut buf = vec![0u8; length];
    reader.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn spawn_watchdog(mut child: Child, timeout: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        let _ = child.kill();
    });
}

fn handshake(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    config: &Value,
) -> Result<(), String> {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": Value::Null,
            "capabilities": {},
            "initializationOptions": Value::Null,
        }
    });
    write_message(stdin, &init).map_err(|e| e.to_string())?;

    loop {
        let msg = read_message(reader).map_err(|e| e.to_string())?;
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();

        if let Some(method) = method {
            if let Some(id) = id {
                let result = if method == "workspace/configuration" {
                    let count = msg
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map(|a| a.len())
                        .unwrap_or(0);
                    Value::Array(vec![config.clone(); count])
                } else {
                    Value::Null
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                write_message(stdin, &reply).map_err(|e| e.to_string())?;
            }
            continue;
        }

        if id.and_then(|v| v.as_i64()) == Some(1) {
            let done = json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
            write_message(stdin, &done).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
}

/// Ask nixd for hover information about an option path in the given flake.
pub fn query_hover(flake_path: &str, target: &str, option_path: &str) -> Result<String, String> {
    let expr = options_expr(flake_path, target)
        .ok_or_else(|| format!("no nixd option set for target {target}"))?;
    let config = json!({
        "nixpkgs": { "expr": "import <nixpkgs> { }" },
        "options": { "main": { "expr": expr } },
    });

    let mut child = Command::new("nixd")
        .arg("--config")
        .arg(config.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start nixd: {e}"))?;

    let mut stdin = child.stdin.take().ok_or("nixd has no stdin")?;
    let mut reader = BufReader::new(child.stdout.take().ok_or("nixd has no stdout")?);
    spawn_watchdog(child, Duration::from_secs(30));

    handshake(&mut stdin, &mut reader, &config)?;

    let uri = "file:///tmp/nix-regedit/synthetic.nix";
    let text = format!("{{ ... }}: {{\n  {option_path} = null;\n}}\n");
    // nixd resolves only the single attrpath segment under the cursor; hover the
    // leaf (e.g. `systemPackages`) so it reports the option's real type, not the
    // first segment (`environment`), which has no type.
    let last_len = option_path.rsplit('.').next().map(str::len).unwrap_or(0);
    let leaf_column = 2 + option_path.len() - last_len;
    let did_open = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": uri, "languageId": "nix", "version": 1, "text": text } }
    });
    write_message(&mut stdin, &did_open).map_err(|e| e.to_string())?;

    let hover = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": leaf_column }
        }
    });
    write_message(&mut stdin, &hover).map_err(|e| e.to_string())?;

    let result = loop {
        let msg = read_message(&mut reader).map_err(|e| e.to_string())?;
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();

        if method.is_some() {
            if let Some(id) = id {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                let _ = write_message(&mut stdin, &reply);
            }
            continue;
        }

        if id.and_then(|v| v.as_i64()) == Some(2) {
            break msg.get("result").cloned().unwrap_or(Value::Null);
        }
    };

    Ok(render_hover(&result))
}

fn render_hover(result: &Value) -> String {
    if result.is_null() {
        return String::new();
    }
    match result.get("contents") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("value").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

