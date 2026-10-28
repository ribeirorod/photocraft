//! `photocraft-cli embed`: the app with no window, driven over the token-authenticated control channel.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_photocraft-cli");
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn embed_serves_a_windowless_session_over_the_control_channel() {
    let dir = std::env::temp_dir().join(format!("photocraft-cli-embed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let token_file = dir.join("token");
    std::fs::write(&token_file, TOKEN).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let root = dir.to_str().unwrap();
    let mut child = Command::new(BIN)
        .args(["embed", "--control", &port.to_string(), "--control-token-file", token_file.to_str().unwrap(), "--size", "900x600"])
        .args(["--automation-read-root", root, "--automation-write-root", root])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let connect = || loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if start.elapsed() < Duration::from_secs(60) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("embed never listened: {e}"),
        }
    };
    let exchange = |stream: &std::net::TcpStream, lines: &mut std::io::Lines<BufReader<std::net::TcpStream>>, msg: Value| -> Value {
        writeln!(&*stream, "{msg}").unwrap();
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    // Without the token nothing runs.
    let s = connect();
    let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
    let r = exchange(&s, &mut lines, json!({"id": 1, "method": "ui.inspect"}));
    assert_eq!(r["ok"], false, "{r}");
    // With it, the session answers like the desktop app.
    let s = connect();
    let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
    assert_eq!(exchange(&s, &mut lines, json!({"id": 1, "method": "auth", "params": {"token": TOKEN}}))["ok"], true);
    let r = exchange(&s, &mut lines, json!({"id": 2, "method": "engine.execute", "params": {"command": "file.new", "params": {"width": 320, "height": 200}}}));
    assert_eq!(r["ok"], true, "{r}");
    let r = exchange(&s, &mut lines, json!({"id": 3, "method": "ui.screenshot", "params": {"path": "shot.png"}}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["id"], 3);
    assert_eq!((r["result"]["width"].as_u64(), r["result"]["height"].as_u64()), (Some(900), Some(600)));
    assert!(std::fs::metadata(dir.join("shot.png")).unwrap().len() > 1000);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(dir);
}
