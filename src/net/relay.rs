//! What every loopback translator shares: the per-session token check, the
//! pooled upstream call that watches the reader and cancels what it abandons,
//! and the streaming relay that decides the reply before any byte is written.
//!
//! The Anthropic shim (Claude Code) and the Responses shim (Codex) differ only
//! in what they translate. Everything here is the part that must behave the
//! same for both: a request the editor left is cancelled the same way, a
//! pre-stream 429 keeps its status the same way (#83), and a slow prefill gets
//! the same keepalives.

use crate::account::cancel_worker::{Bearer, CancelResult, CancelWorker};
use crate::account::console::CancelOutcome;
use crate::config::cli_paths::state_dir;
use crate::io::output::status_line;
use crate::net::http1::{LivenessProbe, ResponseHead, ResponseWriter, ServerRequest};
use crate::net::loopback_auth::constant_time_equals;
use crate::net::upstream_call::{self, WatchedCall, WatchedResult};
use crate::net::upstream_pool::{retry_on_fresh_connection, UpstreamPool};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------
// shim.log
// ---------------------------------------------------------------------

/// Now, as `%Y-%m-%dT%H:%M:%SZ` (`util::format_utc`).
pub fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    crate::util::format_utc(i64::try_from(now.as_secs()).unwrap_or(i64::MAX))
}

/// One timestamped line into shim.log; the error logger below and the
/// abandon path share it. Never the editor's terminal, which the wrapped
/// tool owns.
pub fn shim_log(line: &str) {
    let dir = state_dir();
    if dir.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/shim.log"))
    else {
        return;
    };
    use std::io::Write;
    let _ = writeln!(file, "{} {}", utc_timestamp(), line);
}

/// Appends one line about a failed upstream call to a log file, best effort.
///
/// A file, not stderr: the wrapped tool owns the terminal, and a line printed
/// into its TUI corrupts the display -- which is why a real error used to
/// vanish into a blind "API error, retrying" with nowhere to look. The
/// upstream response body is recorded; the bearer token never is (it is only
/// ever on the request, never echoed here).
pub fn log_upstream_error(model: &str, streaming: bool, status: i32, body: &[u8]) {
    let cap = body.len().min(2000);
    let mut snippet = body[..cap].to_vec();
    for byte in snippet.iter_mut() {
        if *byte == b'\n' || *byte == b'\r' {
            *byte = b' ';
        }
    }
    let snippet = String::from_utf8_lossy(&snippet);
    shim_log(&format!(
        "model={} stream={} status={} body={}",
        model,
        if streaming { 1 } else { 0 },
        status,
        snippet
    ));
}

/// Split "http://host:port/v1" into the host root and the path prefix the
/// HTTP layer wants separately.
pub fn split_base_url(base_url: &str) -> Option<(String, String)> {
    let scheme = if base_url.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    if !base_url.starts_with(scheme) {
        return None;
    }
    let (origin, prefix) = match base_url[scheme.len()..].find('/') {
        None => (base_url.to_string(), String::new()),
        Some(idx) => {
            let slash = scheme.len() + idx;
            (base_url[..slash].to_string(), base_url[slash..].to_string())
        }
    };
    if origin.is_empty() {
        None
    } else {
        Some((origin, prefix))
    }
}

// ---------------------------------------------------------------------
// The session token
// ---------------------------------------------------------------------

// The tokens the wrapped tool presents, from either header a client may send
// one in: x-api-key: <t> (Claude Code's ANTHROPIC_API_KEY, or a reader's
// apiKeyHelper) and Authorization: Bearer <t> (ANTHROPIC_AUTH_TOKEN, or an
// OpenAI client's key). Both can arrive at once, a helper's key beside
// wally's bearer token.
pub fn presented_tokens(request: &ServerRequest) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(value) = request.header("x-api-key") {
        tokens.push(value.to_string());
    }
    if let Some(authorization) = request.header("Authorization") {
        const BEARER: &str = "Bearer ";
        if let Some(rest) = authorization.strip_prefix(BEARER) {
            tokens.push(rest.to_string());
        }
    }
    tokens
}

/// The request carries this session's token in either header. Each is
/// compared in constant time; a reader's own key in x-api-key (an
/// apiKeyHelper in their settings) beside wally's bearer token is not a
/// refusal.
pub fn presents_token(request: &ServerRequest, token: &str) -> bool {
    presented_tokens(request)
        .iter()
        .any(|presented| constant_time_equals(presented, token))
}

// ---------------------------------------------------------------------
// The upstream
// ---------------------------------------------------------------------

/// Where a translator sends its requests, and how it behaves while one is out.
pub struct Upstream {
    /// The path the endpoint's routes hang off ("/v1", or a load balancer
    /// prefix ahead of it).
    pub prefix: String,
    /// Never logged.
    pub api_key: String,
    /// Upstream connections, kept open across requests (#80).
    pub pool: Arc<UpstreamPool>,
    pub verbose: bool,
    /// The word `--verbose` lines start with: which translator said it.
    pub log_tag: &'static str,
    /// Tells an in-flight watch to stop waiting for an id when the translator
    /// is shutting down.
    pub stopping: AtomicBool,
    /// Where a request the editor abandoned is cancelled by name (#81): the
    /// session's control plane, or nothing for a local server. The worker
    /// sends the cancels off the request path; `drain` waits for them.
    pub cancels: Option<CancelWorker>,
    /// Sent on every upstream request: which harness this translator serves,
    /// in the User-Agent and, when the contract can name it, `X-RA-Harness`.
    pub headers: Vec<(String, String)>,
}

/// The cancel worker for a session's control plane, or None for a local
/// server (no console, or no key), which has nothing to tell.
pub fn cancel_worker(console_url: &str, api_key: &str) -> Option<CancelWorker> {
    if console_url.is_empty() || api_key.is_empty() {
        return None;
    }
    // Three seconds per cancel: fire-and-forget, and the bound on how long an
    // exiting wrapper waits for the last one to go out.
    let bearer_value = api_key.to_string(); // fixed for the session
    let bearer: Bearer = Arc::new(move || bearer_value.clone());
    let on_result: CancelResult = Arc::new(|id: &str, outcome: CancelOutcome, error: &str| {
        let word = match outcome {
            CancelOutcome::Cancelled => "202",
            CancelOutcome::NotFound => "404",
            CancelOutcome::Failed => "failed",
        };
        let suffix = if error.is_empty() {
            String::new()
        } else {
            format!(" error={error}")
        };
        shim_log(&format!("cancel id={id} result={word}{suffix}"));
    });
    Some(CancelWorker::new(
        console_url,
        bearer,
        3000,
        on_result,
        None,
    ))
}

impl Upstream {
    /// Stops waiting on in-flight ids, then sends whatever cancels are still
    /// queued -- bounded by 3s per queued cancel, typically one. Called once the
    /// server has stopped, so the last abandon's cancel goes out before the
    /// process does.
    pub fn begin_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub fn drain_cancels(&self) {
        if let Some(cancels) = self.cancels.as_ref() {
            if cancels.pending() > 0 {
                status_line("telling the model endpoint to stop the abandoned request");
            }
            let _ = cancels.stop();
        }
    }

    /// What the abandon path does once the id is known (or known to be
    /// unknowable): a shim.log line the person can find, and the cancel itself
    /// through the worker -- never on this thread, which is the upstream watch
    /// or the response handler.
    fn on_abandoned(&self, streaming: bool, request_id: &str, status: i32, during_prefill: bool) {
        let line = format!(
            "abandoned during={} id={} status={} stream={}",
            if during_prefill { "prefill" } else { "stream" },
            if request_id.is_empty() {
                "unknown"
            } else {
                request_id
            },
            status,
            if streaming { "1" } else { "0" }
        );
        if request_id.is_empty() {
            shim_log(&format!("{line} cancel=none(no-id)"));
            return;
        }
        let Some(cancels) = self.cancels.as_ref() else {
            // No worker: a local server, which has no console to tell.
            shim_log(&format!("{line} cancel=skipped(local)"));
            return;
        };
        shim_log(&format!("{line} cancel=queued"));
        cancels.enqueue(request_id);
    }

    /// Sends `body` upstream on a pooled connection (#80), watching the editor
    /// the whole time (#81), and once more on a fresh connection if the first
    /// went out on a stale keep-alive (see `retry_on_fresh_connection`) --
    /// never after the editor left: the stop that ended an abandoned call
    /// looks exactly like a stale connection to that rule, and re-sending the
    /// prompt for a reader that is gone is the waste this exists to end.
    /// `on_headers`, when set, receives the upstream response headers before
    /// any body byte, so a streaming caller can preserve a pre-stream failure
    /// status instead of a blind 200 (#83).
    #[allow(clippy::type_complexity)]
    pub fn post(
        &self,
        streaming: bool,
        path: &str,
        body: &[u8],
        mut receiver: Option<&mut dyn FnMut(&[u8]) -> bool>,
        reader_gone: &(dyn Fn() -> bool + Sync),
        mut on_headers: Option<&mut dyn FnMut(&ResponseHead)>,
    ) -> WatchedResult {
        let tag = self.log_tag;
        for attempt in 0..2 {
            let mut lease = self.pool.acquire(&self.api_key);
            if self.verbose {
                status_line(&format!(
                    "{tag}: upstream connection {}",
                    if lease.reused() { "reused" } else { "fresh" }
                ));
            }

            let receiver_box: Option<Box<dyn FnMut(&[u8]) -> bool + '_>> = receiver
                .as_mut()
                .map(|r| Box::new(&mut **r) as Box<dyn FnMut(&[u8]) -> bool + '_>);
            let on_headers_box: Option<Box<dyn FnMut(&ResponseHead) + '_>> = on_headers
                .as_mut()
                .map(|r| Box::new(&mut **r) as Box<dyn FnMut(&ResponseHead) + '_>);

            let call = WatchedCall {
                path: path.to_string(),
                body: body.to_vec(),
                content_type: "application/json".to_string(),
                headers: self.headers.clone(),
                receiver: receiver_box,
                reader_gone: Some(Box::new(reader_gone) as Box<dyn Fn() -> bool + Sync + '_>),
                on_headers: on_headers_box,
                stopping: Some(Box::new(|| self.stopping.load(Ordering::SeqCst))
                    as Box<dyn Fn() -> bool + Sync + '_>),
                on_abandoned: Some(Box::new(
                    move |id: &str, status: i32, during_prefill: bool| {
                        self.on_abandoned(streaming, id, status, during_prefill);
                    },
                )),
                ..Default::default()
            };

            let result = upstream_call::post_watched(&mut lease, call);
            if result.reply.is_ok() {
                // A complete reply, whatever its status, leaves the connection
                // clean; the lease goes back to the pool when it is destroyed.
                return result;
            }
            // No reply: the socket is in no state to reuse.
            lease.discard();
            if result.abandoned {
                if self.verbose {
                    status_line(&format!("{tag}: the editor left; upstream request dropped"));
                }
                return result;
            }
            let should_retry = attempt == 0
                && match &result.reply {
                    Err(error) => retry_on_fresh_connection(
                        *error,
                        false,
                        result.received_any,
                        lease.reused(),
                    ),
                    Ok(_) => false,
                };
            if should_retry {
                if self.verbose {
                    status_line(&format!(
                        "{tag}: upstream connection was stale; retrying once on a fresh one"
                    ));
                }
                continue;
            }
            return result;
        }
        unreachable!("post always returns within its 2 attempts")
    }
}

/// A `LivenessProbe` that treats "could not make one" as "the reader is
/// still there", so a failed clone never abandons a live request.
pub struct ReaderGoneProbe(Option<LivenessProbe>);

impl ReaderGoneProbe {
    pub fn new(stream: &TcpStream) -> Self {
        ReaderGoneProbe(LivenessProbe::new(stream).ok())
    }

    pub fn is_gone(&self) -> bool {
        self.0
            .as_ref()
            .map(|probe| probe.is_gone())
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------

/// Carries an upstream stream from the worker thread that runs the upstream
/// call to the loop that writes SSE frames to the editor.
///
/// A worker is needed because the upstream status is only known once its
/// headers arrive, and the pre-stream decision (commit a 200 event-stream,
/// or answer a 429/503 as a normal reply, #83) has to be made before any
/// event is written. The worker peeks the headers and hands whole transport
/// chunks across a single slot, which bounds read-ahead and keeps
/// backpressure on a long stream.
struct StreamPipeShared {
    headers_ready: bool,
    finished: bool,
    stopped: bool,
    successful: bool,
    abandoned: bool,
    status: i32,
    retry_after: String,
    chunk: Vec<u8>,
    error_body: Vec<u8>,
}

/// A local model can spend longer than an editor's idle timeout in prefill, so
/// `read` must not block indefinitely on the first transport chunk: it wakes
/// every second to let the caller send a harmless SSE keepalive comment while
/// it waits, instead of the editor reporting a network failure and retrying.
enum StreamReadResult {
    Chunk(Vec<u8>),
    KeepAlive,
    Finished,
}

struct StreamPipe {
    shared: Mutex<StreamPipeShared>,
    changed: Condvar,
}

impl StreamPipe {
    fn new() -> Self {
        StreamPipe {
            shared: Mutex::new(StreamPipeShared {
                headers_ready: false,
                finished: false,
                stopped: false,
                successful: false,
                abandoned: false,
                status: 0,
                retry_after: String::new(),
                chunk: Vec::new(),
                error_body: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    /// The shared state, taking a poisoned lock's inner value rather than
    /// panicking. A poison means a worker or route thread unwound mid-write, but
    /// the flags and chunk buffer are still coherent to read and set, and the
    /// drop guards below run *during* an unwind: a panic there would abort the
    /// whole process and drop every in-flight connection. Clearing the poison
    /// keeps the later condvar waits (which `.unwrap()` their lock result) from
    /// re-panicking on the same poison this already recovered from.
    fn lock(&self) -> std::sync::MutexGuard<'_, StreamPipeShared> {
        match self.shared.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                self.shared.clear_poison();
                poison.into_inner()
            }
        }
    }

    /// Blocks for the next transport chunk, waking every second to report a
    /// keepalive instead of the stream's first token, and `Finished` once the
    /// stream has ended with nothing left to hand over.
    fn read(&self) -> StreamReadResult {
        let guard = self.lock();
        let (mut guard, _timeout) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(1), |shared| {
                shared.chunk.is_empty() && !shared.finished
            })
            .unwrap();
        if guard.chunk.is_empty() && !guard.finished {
            return StreamReadResult::KeepAlive;
        }
        if guard.chunk.is_empty() {
            return StreamReadResult::Finished;
        }
        let next = std::mem::take(&mut guard.chunk);
        self.changed.notify_all();
        StreamReadResult::Chunk(next)
    }
}

/// Ends the pipe from the route-handling side no matter how `relay_stream`
/// returns -- normal completion, an early return because the reader is gone,
/// or a panic -- so a worker still blocked in its backpressure wait (nobody
/// left calling `StreamPipe::read`) is told to give up instead of hanging.
struct StopPipeOnDrop<'a>(&'a StreamPipe);

impl Drop for StopPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.lock();
        guard.stopped = true;
        self.0.changed.notify_all();
    }
}

/// Ends the pipe from the worker side no matter how the worker's body
/// returns, including a panic mid-call: without this, a panic would unwind
/// past the point that sets `finished`, leaving the route-handling thread's
/// `StreamPipe::read` (and the pre-stream headers-or-finished wait) blocked
/// forever.
struct FinishPipeOnDrop<'a>(&'a StreamPipe);

impl Drop for FinishPipeOnDrop<'_> {
    fn drop(&mut self) {
        let mut guard = self.0.lock();
        guard.finished = true;
        self.0.changed.notify_all();
    }
}

/// The upstream refused before streaming anything: everything a translator
/// needs to answer with a normal reply that keeps the status (#83).
pub struct PreStreamFailure {
    /// The editor left while the refusal was arriving; nothing written
    /// reaches anyone.
    pub abandoned: bool,
    /// The upstream status, 0 when there was no reply at all; what the log and
    /// the error translation see.
    pub raw_status: i32,
    /// What the editor sees: the upstream status, or 502 for no reply.
    pub client_status: i32,
    pub retry_after: String,
    /// The first 8 KB of the upstream's error body.
    pub body: Vec<u8>,
}

/// How a committed (200) stream ended.
pub enum StreamEnd {
    /// The upstream call failed after the stream began; `error_body` is what
    /// it said, if anything.
    Failed { error_body: Vec<u8> },
    /// Transport EOF. `incomplete_tail` is true when it cut a frame in half
    /// (a `data:` line with no blank line after it, or a partial line).
    Ended { incomplete_tail: bool },
}

/// What a translator does with an upstream chat-completions stream.
pub trait StreamSink {
    /// Before any byte of the response: write a complete non-2xx reply.
    fn pre_stream_failure(&mut self, writer: &mut ResponseWriter<'_>, failure: PreStreamFailure);
    /// One complete SSE `data:` payload (multi-line data joined with `\n`,
    /// raw bytes, `[DONE]` included); the events to write for it.
    fn on_payload(&mut self, payload: &[u8]) -> String;
    /// The upstream is done; the last events to write. Not called when the
    /// editor left, since nothing would reach it.
    fn on_end(&mut self, end: StreamEnd) -> String;
}

/// Splits SSE lines into complete `data:` payloads across arbitrary transport
/// chunks: CRLF and multi-line data fields are valid SSE too. Comments and
/// other fields are skipped.
#[derive(Default)]
pub struct SseFrames {
    pending: Vec<u8>,
    payload: Vec<u8>,
    has_data: bool,
}

impl SseFrames {
    /// Feeds `data` in, calling `on_payload` for each payload completed by a
    /// blank line. Stops early, returning false, once `on_payload` does.
    pub fn feed(&mut self, data: &[u8], on_payload: &mut dyn FnMut(&[u8]) -> bool) -> bool {
        self.pending.extend_from_slice(data);
        while let Some(split) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=split).collect();
            line.pop(); // drop the '\n' itself
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                if line.as_slice() == b"data" || line.starts_with(b"data:") {
                    let mut value = if line.as_slice() == b"data" {
                        Vec::new()
                    } else {
                        line[5..].to_vec()
                    };
                    if value.first() == Some(&b' ') {
                        value.remove(0);
                    }
                    if self.has_data {
                        self.payload.push(b'\n');
                    }
                    // Raw bytes, never lossily repaired: invalid UTF-8 must
                    // fail the translator's JSON parse, not get silently
                    // replaced with U+FFFD before parsing ever sees it.
                    self.payload.extend_from_slice(&value);
                    self.has_data = true;
                }
                continue;
            }
            if !self.has_data {
                continue; // comments/keepalives
            }
            self.has_data = false;
            let payload = std::mem::take(&mut self.payload);
            if !on_payload(&payload) {
                return false;
            }
        }
        true
    }

    /// Bytes are left that no blank line completed.
    pub fn incomplete(&self) -> bool {
        self.has_data || !self.pending.is_empty()
    }
}

/// Streams `body` to the upstream's `path` and relays the reply through
/// `sink`: a non-2xx before the first byte goes to `pre_stream_failure` as a
/// normal reply; otherwise a 200 event-stream is committed, each upstream
/// payload is translated as it arrives, a keepalive comment goes out every
/// second the upstream is silent, and `on_end` closes it.
pub fn relay_stream(
    upstream: &Upstream,
    stream: &TcpStream,
    path: &str,
    body: &[u8],
    writer: &mut ResponseWriter<'_>,
    sink: &mut dyn StreamSink,
) {
    let probe = ReaderGoneProbe::new(stream);
    let pipe = StreamPipe::new();

    std::thread::scope(|scope| {
        let _stop_on_drop = StopPipeOnDrop(&pipe);

        scope.spawn(|| {
            let _finish_on_drop = FinishPipeOnDrop(&pipe);
            let reader_gone = || probe.is_gone();
            let mut receiver = |data: &[u8]| -> bool {
                let mut guard = pipe.lock();
                if guard.status < 200 || guard.status >= 300 {
                    // A non-2xx body is the error body, kept bounded so a real
                    // (2xx) stream of any size costs only these few KB.
                    const CAP: usize = 8192;
                    if guard.error_body.len() < CAP {
                        let room = CAP - guard.error_body.len();
                        let take = data.len().min(room);
                        guard.error_body.extend_from_slice(&data[..take]);
                    }
                    return !guard.stopped;
                }
                // Backpressure: hold the next transport chunk until the
                // reading side has taken the last one.
                loop {
                    if guard.chunk.is_empty() || guard.stopped {
                        break;
                    }
                    guard = pipe.changed.wait(guard).unwrap();
                }
                if guard.stopped {
                    return false;
                }
                guard.chunk = data.to_vec();
                pipe.changed.notify_all();
                true
            };
            let mut on_headers = |head: &ResponseHead| {
                let mut guard = pipe.lock();
                guard.status = head.status;
                guard.retry_after = head.header("Retry-After").unwrap_or("").to_string();
                guard.headers_ready = true;
                pipe.changed.notify_all();
            };

            let result = upstream.post(
                true,
                path,
                body,
                Some(&mut receiver),
                &reader_gone,
                Some(&mut on_headers),
            );
            let mut guard = pipe.lock();
            guard.successful = matches!(&result.reply, Ok(r) if r.status >= 200 && r.status < 300);
            guard.abandoned = result.abandoned;
            // `_finish_on_drop`, dropped when this closure returns (however it
            // returns), sets `finished` and notifies.
        });

        // Pre-stream decision (#83): known before any event is written, so it
        // can be answered as a normal reply that keeps the status and any
        // Retry-After, rather than a 200 stream carrying an error.
        let mut guard = pipe.lock();
        loop {
            if guard.headers_ready || guard.finished {
                break;
            }
            guard = pipe.changed.wait(guard).unwrap();
        }
        if guard.status < 200 || guard.status >= 300 {
            loop {
                if guard.finished {
                    break;
                }
                guard = pipe.changed.wait(guard).unwrap();
            }
            let failure = PreStreamFailure {
                abandoned: guard.abandoned,
                raw_status: guard.status,
                client_status: if guard.status != 0 { guard.status } else { 502 },
                retry_after: guard.retry_after.clone(),
                body: guard.error_body.clone(),
            };
            drop(guard);
            sink.pre_stream_failure(writer, failure);
            return;
        }
        drop(guard);

        if writer
            .begin_chunked(200, &[("Content-Type", "text/event-stream")])
            .is_err()
        {
            return;
        }
        let mut frames = SseFrames::default();
        loop {
            let bytes = match pipe.read() {
                StreamReadResult::Finished => break,
                StreamReadResult::KeepAlive => {
                    if writer.write_chunk(b": keepalive\n\n").is_err() {
                        return;
                    }
                    continue;
                }
                StreamReadResult::Chunk(bytes) => bytes,
            };
            // Returns false once the editor is gone: stop pulling chunks.
            let mut write_payload = |payload: &[u8]| -> bool {
                let events = sink.on_payload(payload);
                events.is_empty() || writer.write_chunk(events.as_bytes()).is_ok()
            };
            if !frames.feed(&bytes, &mut write_payload) {
                return;
            }
        }

        let (abandoned, successful, error_body) = {
            let guard = pipe.lock();
            (guard.abandoned, guard.successful, guard.error_body.clone())
        };
        if abandoned {
            // The editor is gone; the cancel is on its way (or was named as
            // impossible). Nothing written here reaches anyone.
            let _ = writer.end_chunked();
            return;
        }
        let end = if successful {
            StreamEnd::Ended {
                incomplete_tail: frames.incomplete(),
            }
        } else {
            StreamEnd::Failed { error_body }
        };
        let closing = sink.on_end(end);
        if !closing.is_empty() {
            let _ = writer.write_chunk(closing.as_bytes());
        }
        let _ = writer.end_chunked();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_base_url_separates_origin_and_prefix() {
        assert_eq!(
            split_base_url("http://127.0.0.1:8080/v1"),
            Some(("http://127.0.0.1:8080".to_string(), "/v1".to_string()))
        );
        assert_eq!(
            split_base_url("https://api.example.test"),
            Some(("https://api.example.test".to_string(), String::new()))
        );
        assert_eq!(
            split_base_url("https://api.example.test/api-dev"),
            Some((
                "https://api.example.test".to_string(),
                "/api-dev".to_string()
            ))
        );
        assert_eq!(split_base_url("not-a-url"), None);
        assert_eq!(split_base_url(""), None);
    }

    #[test]
    fn utc_timestamp_has_the_expected_shape() {
        let stamp = utc_timestamp();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.as_bytes()[4], b'-');
        assert_eq!(stamp.as_bytes()[10], b'T');
    }

    fn request_with_header(name: &str, value: &str) -> ServerRequest {
        ServerRequest {
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            query: String::new(),
            headers: vec![(name.to_string(), value.to_string())],
            body: Vec::new(),
        }
    }

    #[test]
    fn presented_tokens_read_x_api_key() {
        let request = request_with_header("x-api-key", "secret");
        assert_eq!(presented_tokens(&request), ["secret"]);
    }

    #[test]
    fn presented_tokens_read_bearer_authorization() {
        let request = request_with_header("Authorization", "Bearer secret");
        assert_eq!(presented_tokens(&request), ["secret"]);
    }

    #[test]
    fn presented_tokens_are_empty_without_a_recognized_header() {
        let request = request_with_header("X-Other", "value");
        assert!(presented_tokens(&request).is_empty());
    }

    /// A reader's apiKeyHelper sends its key in x-api-key beside wally's
    /// bearer token: the request is still this session's (it got a 401 under a
    /// settings.json with a helper). Neither alone of a wrong token passes.
    #[test]
    fn either_header_may_carry_the_token() {
        let mut both = request_with_header("x-api-key", "their-helper-key");
        both.headers
            .push(("Authorization".to_string(), "Bearer secret".to_string()));
        assert!(presents_token(&both, "secret"));
        assert!(!presents_token(
            &request_with_header("x-api-key", "their-helper-key"),
            "secret"
        ));
        assert!(!presents_token(
            &request_with_header("Authorization", "Bearer wrong"),
            "secret"
        ));
    }

    fn frames_of(chunks: &[&[u8]]) -> (Vec<Vec<u8>>, bool) {
        let mut frames = SseFrames::default();
        let mut seen = Vec::new();
        for chunk in chunks {
            frames.feed(chunk, &mut |payload| {
                seen.push(payload.to_vec());
                true
            });
        }
        (seen, frames.incomplete())
    }

    #[test]
    fn sse_frames_join_across_transport_chunks_and_crlf() {
        let (seen, incomplete) =
            frames_of(&[b"data: {\"a\"", b":1}\r\n\r\n: ping\n\ndata: [DONE]\n\n"]);
        assert_eq!(seen, vec![b"{\"a\":1}".to_vec(), b"[DONE]".to_vec()]);
        assert!(!incomplete);
    }

    #[test]
    fn sse_frames_join_multi_line_data_with_a_newline() {
        let (seen, _) = frames_of(&[b"data: one\ndata:two\n\n"]);
        assert_eq!(seen, vec![b"one\ntwo".to_vec()]);
    }

    #[test]
    fn sse_frames_report_a_cut_frame_as_incomplete() {
        assert!(frames_of(&[b"data: {}\n"]).1);
        assert!(frames_of(&[b"data: {"]).1);
    }

    #[test]
    fn sse_frames_stop_when_the_callback_does() {
        let mut frames = SseFrames::default();
        let mut count = 0;
        let kept_going = frames.feed(b"data: 1\n\ndata: 2\n\n", &mut |_| {
            count += 1;
            false
        });
        assert!(!kept_going);
        assert_eq!(count, 1);
    }
}
