#![allow(missing_docs)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;

#[test]
fn responds_and_shuts_down() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut child = Command::new(env!("CARGO_BIN_EXE_git-ents-server"))
        .arg("--port")
        .arg(port.to_string())
        .arg("--max-requests")
        .arg("3")
        .spawn()
        .unwrap();

    for i in 0..3 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stream = loop {
            match TcpStream::connect(format!("127.0.0.1:{port}")) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("could not connect on request {i}: {e}"),
            }
        };
        stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "unexpected response: {response}");
    }

    let status = child.wait().unwrap();
    assert!(status.success());
}
