//! Minimal blocking HTTP server exposing the hub as MJPEG (multipart/x-mixed-replace).
//!
//! This is the format OpenCV's FFmpeg backend (`cv::VideoCapture("http://...")`,
//! used by DFTFringe for URL sources) and WebKit `<img>` tags both understand.
//! One thread per client; frames are written straight from the shared buffer.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::hub::{FrameHub, Wait};

pub const BOUNDARY: &str = "frame";
pub const STREAM_PATH: &str = "/video_feed";
pub const SNAPSHOT_PATH: &str = "/snapshot.jpg";

const MAX_HEAD_BYTES: usize = 8192;

const INDEX_HTML: &str = "<!doctype html><title>livestream</title>\
<body style='margin:0;background:#111'>\
<img src='/video_feed' style='max-width:100vw;max-height:100vh;display:block;margin:auto'>";

#[derive(Clone, Copy, Debug)]
pub struct ServerOptions {
    /// While the hub is paused, the last frame is re-sent this often so
    /// clients (DFTFringe) do not time out and drop the connection.
    pub keepalive: Duration,
    /// How long `/snapshot.jpg` waits for a first frame before answering 503.
    pub snapshot_wait: Duration,
    pub write_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(2),
            snapshot_wait: Duration::from_secs(5),
            write_timeout: Duration::from_secs(30),
        }
    }
}

/// Multipart part header preceding a JPEG of `len` bytes.
pub fn part_header(len: usize) -> String {
    format!("--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {len}\r\n\r\n")
}

/// Path of a `GET` request line, without query string.
pub fn request_path(head: &str) -> Option<&str> {
    let mut parts = head.lines().next()?.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => target.split('?').next(),
        _ => None,
    }
}

/// Bind and serve in background threads; returns the bound address.
pub fn spawn(hub: Arc<FrameHub>, addr: impl ToSocketAddrs, options: ServerOptions) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    thread::Builder::new().name("http-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let hub = hub.clone();
            let _ = thread::Builder::new()
                .name("http-client".into())
                .spawn(move || handle(stream, &hub, options));
        }
    })?;
    Ok(local)
}

fn handle(mut stream: TcpStream, hub: &FrameHub, options: ServerOptions) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(options.write_timeout))?;
    let head = read_head(&mut stream)?;
    match request_path(&head) {
        Some(STREAM_PATH) => stream_frames(stream, hub, options.keepalive),
        Some(SNAPSHOT_PATH) => snapshot(stream, hub, options.snapshot_wait),
        Some("/") => respond(stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        Some(_) => respond(stream, "404 Not Found", "text/plain", b"not found"),
        None => respond(stream, "405 Method Not Allowed", "text/plain", b"GET only"),
    }
}

fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_HEAD_BYTES {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn respond(mut stream: TcpStream, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.0 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-cache, no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn snapshot(stream: TcpStream, hub: &FrameHub, wait: Duration) -> io::Result<()> {
    let frame = match hub.latest() {
        (_, Some(frame)) => Some(frame),
        (seq, None) => match hub.wait_newer(seq, wait) {
            Wait::Frame(_, frame) => Some(frame),
            Wait::Timeout | Wait::Closed => None,
        },
    };
    match frame {
        Some(frame) => respond(stream, "200 OK", "image/jpeg", &frame),
        None => respond(stream, "503 Service Unavailable", "text/plain", b"no frame available yet"),
    }
}

fn write_part(stream: &mut TcpStream, frame: &[u8]) -> io::Result<()> {
    stream.write_all(part_header(frame.len()).as_bytes())?;
    stream.write_all(frame)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn stream_frames(mut stream: TcpStream, hub: &FrameHub, keepalive: Duration) -> io::Result<()> {
    let _client = hub.client_guard();
    write!(
        stream,
        "HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\n\
         Cache-Control: no-cache, no-store\r\nPragma: no-cache\r\nConnection: close\r\n\r\n"
    )?;
    let (mut seq, first) = hub.latest();
    if let Some(frame) = first {
        write_part(&mut stream, &frame)?;
    }
    loop {
        match hub.wait_newer(seq, keepalive) {
            Wait::Frame(next, frame) => {
                seq = next;
                write_part(&mut stream, &frame)?;
            }
            Wait::Timeout => resend_if_paused(&mut stream, hub)?,
            Wait::Closed => return Ok(()),
        }
    }
}

fn resend_if_paused(stream: &mut TcpStream, hub: &FrameHub) -> io::Result<()> {
    match (hub.is_paused(), hub.latest()) {
        (true, (_, Some(frame))) => write_part(stream, &frame),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn options(keepalive_ms: u64) -> ServerOptions {
        ServerOptions {
            keepalive: Duration::from_millis(keepalive_ms),
            snapshot_wait: Duration::from_millis(100),
            ..ServerOptions::default()
        }
    }

    fn get(addr: SocketAddr, path: &str) -> TcpStream {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        s
    }

    fn read_until(s: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while !out.ends_with(needle) {
            assert_eq!(s.read(&mut byte).unwrap(), 1, "connection closed early");
            out.push(byte[0]);
        }
        out
    }

    fn read_part(s: &mut TcpStream) -> Vec<u8> {
        let header = String::from_utf8(read_until(s, b"\r\n\r\n")).unwrap();
        let len: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut body = vec![0; len + 2];
        s.read_exact(&mut body).unwrap();
        assert!(body.ends_with(b"\r\n"));
        body.truncate(len);
        body
    }

    fn read_all(mut s: TcpStream) -> String {
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn part_header_format() {
        assert_eq!(part_header(4), "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n");
    }

    #[test]
    fn request_path_parsing() {
        assert_eq!(request_path("GET /video_feed?x=1 HTTP/1.1\r\n"), Some("/video_feed"));
        assert_eq!(request_path("POST / HTTP/1.1\r\n"), None);
        assert_eq!(request_path(""), None);
    }

    #[test]
    fn stream_sends_latest_then_new_frames() {
        let hub = Arc::new(FrameHub::new());
        hub.publish(b"one".to_vec());
        let addr = spawn(hub.clone(), "127.0.0.1:0", options(2000)).unwrap();
        let mut s = get(addr, STREAM_PATH);
        let head = String::from_utf8(read_until(&mut s, b"\r\n\r\n")).unwrap();
        assert!(head.contains("multipart/x-mixed-replace; boundary=frame"));
        assert_eq!(read_part(&mut s), b"one");
        hub.publish(b"two".to_vec());
        assert_eq!(read_part(&mut s), b"two");
        assert_eq!(hub.clients(), 1);
    }

    #[test]
    fn paused_hub_resends_last_frame_as_keepalive() {
        let hub = Arc::new(FrameHub::new());
        hub.publish(b"still".to_vec());
        hub.set_paused(true);
        let addr = spawn(hub.clone(), "127.0.0.1:0", options(50)).unwrap();
        let mut s = get(addr, STREAM_PATH);
        read_until(&mut s, b"\r\n\r\n");
        for _ in 0..3 {
            assert_eq!(read_part(&mut s), b"still");
        }
    }

    #[test]
    fn playing_hub_does_not_resend() {
        let hub = Arc::new(FrameHub::new());
        hub.publish(b"once".to_vec());
        let addr = spawn(hub.clone(), "127.0.0.1:0", options(30)).unwrap();
        let mut s = get(addr, STREAM_PATH);
        read_until(&mut s, b"\r\n\r\n");
        assert_eq!(read_part(&mut s), b"once");
        s.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let err = s.read(&mut [0u8; 1]).unwrap_err();
        assert!(matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut));
    }

    #[test]
    fn snapshot_and_errors() {
        let hub = Arc::new(FrameHub::new());
        let addr = spawn(hub.clone(), "127.0.0.1:0", options(2000)).unwrap();
        assert!(read_all(get(addr, SNAPSHOT_PATH)).starts_with("HTTP/1.0 503"));
        hub.publish(b"jpeg".to_vec());
        let ok = read_all(get(addr, SNAPSHOT_PATH));
        assert!(ok.starts_with("HTTP/1.0 200"));
        assert!(ok.contains("Content-Type: image/jpeg"));
        assert!(ok.ends_with("\r\n\r\njpeg"));
        assert!(read_all(get(addr, "/nope")).starts_with("HTTP/1.0 404"));
        assert!(read_all(get(addr, "/")).contains("<img src='/video_feed'"));
    }
}
