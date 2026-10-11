//! End-to-end test of --serve mode: socket round-trip, multi-line framing,
//! and kill-triggered shutdown with socket cleanup.

use exchange_organelles::wire;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn send_recv(stream: &mut UnixStream, reader: &mut BufReader<UnixStream>, req: &str) -> serde_json::Value {
    stream.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("read response");
    serde_json::from_str(&line).expect("response json")
}

#[test]
fn serve_roundtrip_and_kill() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock").to_string_lossy().into_owned();
    let sock2 = sock.clone();
    let handle = std::thread::spawn(move || wire::serve(&sock2));
    for _ in 0..100 {
        if std::path::Path::new(&sock).exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(std::path::Path::new(&sock).exists(), "socket not created");

    let mut stream = UnixStream::connect(&sock).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    // two requests on one connection: framing works
    let r1 = send_recv(
        &mut stream,
        &mut reader,
        r#"{"id": 42, "caller": "t", "capabilities": [], "codon": "ex", "op": "suspend", "input": {}}"#,
    );
    assert_eq!(r1["id"], 42);
    assert_eq!(r1["ok"], true);
    assert!(r1["receipt"].as_str().unwrap().starts_with("adapter-interim:sha256:"));

    let r2 = send_recv(
        &mut stream,
        &mut reader,
        r#"{"id": 43, "caller": "t", "capabilities": [], "codon": "zz", "op": "nope", "input": {}}"#,
    );
    assert_eq!(r2["ok"], false);
    assert_eq!(r2["error"]["verdict"], "rejected");

    // kill -> final receipt, server exits, socket cleaned up
    let r3 = send_recv(
        &mut stream,
        &mut reader,
        r#"{"id": 44, "caller": "t", "capabilities": [], "codon": "ex", "op": "kill", "input": {}}"#,
    );
    assert_eq!(r3["output"]["final"], true);
    handle.join().expect("serve thread").expect("serve io");
    assert!(!std::path::Path::new(&sock).exists(), "socket not cleaned up");
}

#[test]
fn serve_replaces_stale_socket() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("stale.sock").to_string_lossy().into_owned();
    std::fs::write(&sock, b"stale").unwrap();
    let sock2 = sock.clone();
    let handle = std::thread::spawn(move || wire::serve(&sock2));
    for _ in 0..100 {
        if UnixStream::connect(&sock).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // kill it via the socket
    let mut stream = UnixStream::connect(&sock).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    send_recv(
        &mut stream,
        &mut reader,
        r#"{"id": 1, "caller": "t", "capabilities": [], "codon": "ex", "op": "kill", "input": {}}"#,
    );
    handle.join().expect("serve thread").expect("serve io");
}
