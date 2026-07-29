//! Shared fixtures for the installer tests: a loopback HTTP server and a
//! miniature catalog entry — mirrors `local_rag_models`' own
//! `tests/support/mod.rs`, adapted for a single-file GGUF entry instead of
//! the ONNX three-file set.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use local_rag_generate::{AssetFetcher, AssetFile, FetchError, GeneratorCatalogEntry};

/// The fixture model's id — deliberately not the real one, so a test can
/// never be confused with an installation of the shipped default.
pub const FIXTURE_MODEL_ID: &str = "fixture-generator-model";

/// The fixture file's contents.
pub const FIXTURE_FILE: (&str, &str, &[u8]) = (
    "model.gguf",
    "model.gguf",
    b"fixture gguf weights, byte for byte",
);

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Build a catalog entry for the fixture file served by `source`.
pub fn fixture_entry(source: &str) -> GeneratorCatalogEntry {
    let (relative, source_path, bytes) = FIXTURE_FILE;
    let file = AssetFile {
        relative_path: relative,
        source_path,
        size: bytes.len() as u64,
        sha256: leak(local_rag_core::hash::sha256_hex(bytes)),
    };
    GeneratorCatalogEntry {
        model_id: FIXTURE_MODEL_ID,
        source: leak(source.to_string()),
        revision: "rev-0",
        license: "Fixture Terms of Use",
        license_url: "https://example.invalid/terms",
        context_length: 4_096,
        files: Box::leak(vec![file].into_boxed_slice()),
        raw_chat_template_override: None,
    }
}

pub fn fixture_bytes() -> &'static [u8] {
    FIXTURE_FILE.2
}

/// A minimal HTTP server on `127.0.0.1:0` serving the fixture asset.
pub struct FixtureServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");

        let mut initial = HashMap::new();
        let (_, source_path, bytes) = FIXTURE_FILE;
        initial.insert(format!("/repo/resolve/rev-0/{source_path}"), bytes.to_vec());

        let bodies = Arc::new(Mutex::new(initial));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = {
            let bodies = Arc::clone(&bodies);
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { return };
                    serve_one(stream, &bodies, &requests);
                }
            })
        };

        FixtureServer {
            addr,
            bodies,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn source_url(&self) -> String {
        format!("http://{}/repo", self.addr)
    }

    pub fn entry(&self) -> GeneratorCatalogEntry {
        fixture_entry(&self.source_url())
    }

    pub fn corrupt(&self, body: &[u8]) {
        let (_, source_path, _) = FIXTURE_FILE;
        self.bodies
            .lock()
            .expect("bodies mutex")
            .insert(format!("/repo/resolve/rev-0/{source_path}"), body.to_vec());
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests mutex").clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests mutex").len()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_one(
    mut stream: TcpStream,
    bodies: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    requests.lock().expect("requests mutex").push(path.clone());

    let body = bodies.lock().expect("bodies mutex").get(&path).cloned();
    let response = match body {
        Some(bytes) => {
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            head.extend_from_slice(&bytes);
            head
        }
        None => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// A fetcher that fails the test if it is ever called.
#[derive(Debug, Default)]
pub struct ForbiddenFetcher;

impl AssetFetcher for ForbiddenFetcher {
    fn fetch(&self, url: &str, _sink: &mut dyn Write) -> Result<u64, FetchError> {
        panic!("the installer reached the network for {url} when it must not have");
    }
}
