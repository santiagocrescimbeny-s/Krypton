//! Minimal scripted HTTP/1.1 server for network-free integration tests.
//!
//! Std-only on purpose: no extra dev-dependencies, so `Cargo.lock` and
//! `--locked` builds are unaffected. It binds to an ephemeral loopback port,
//! records every request (headers + decoded body) and answers through a
//! caller-supplied handler, which receives the zero-based request index so tests
//! can script sequences such as "503, 503, then 200".
//!
//! Supports `Content-Length` and `Transfer-Encoding: chunked` request bodies and
//! always replies with `Connection: close`.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A request as seen by the server.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.as_str())
    }

    pub fn body_contains(&self, needle: &[u8]) -> bool {
        !needle.is_empty()
            && self
                .body
                .windows(needle.len())
                .any(|window| window == needle)
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A canned response.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub delay: Duration,
}

impl MockResponse {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }

    pub fn raw(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            delay: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = dyn Fn(usize, &RecordedRequest) -> MockResponse + Send + Sync;

pub struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking handler thread must not hide the recorded requests.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MockServer {
    pub fn start<F>(handler: F) -> std::io::Result<Self>
    where
        F: Fn(usize, &RecordedRequest) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);

        let accept_thread = {
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let requests = Arc::clone(&requests);
                    let handler = Arc::clone(&handler);
                    thread::spawn(move || {
                        // I/O errors only mean the client went away (e.g. timeout tests).
                        let _ = serve_connection(&stream, &requests, handler.as_ref());
                    });
                }
            })
        };

        Ok(Self {
            addr,
            requests,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }

    /// Base URL such as `http://127.0.0.1:41234`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.requests).clone()
    }

    pub fn request_count(&self) -> usize {
        lock(&self.requests).len()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the blocking `accept` so the thread can observe the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(
    stream: &TcpStream,
    requests: &Mutex<Vec<RecordedRequest>>,
    handler: &Handler,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let Some(request) = read_request(stream)? else {
        return Ok(());
    };

    let index = {
        let mut guard = lock(requests);
        guard.push(request.clone());
        guard.len() - 1
    };
    let response = handler(index, &request);
    if !response.delay.is_zero() {
        thread::sleep(response.delay);
    }

    let mut writer = stream;
    write!(
        writer,
        "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.body.len()
    )?;
    writer.write_all(&response.body)?;
    writer.flush()
}

fn read_request(stream: &TcpStream) -> std::io::Result<Option<RecordedRequest>> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None); // connection opened and closed without a request
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };

    let body = if header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        read_chunked(&mut reader)?
    } else {
        let length = header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        body
    };

    Ok(Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    }))
}

fn read_chunked<R: BufRead>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size_text = size_line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Consume optional trailers up to the terminating blank line.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(body)
}
