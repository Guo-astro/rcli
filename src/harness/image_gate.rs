//! A loopback forwarder in front of the hosted chat API.
//!
//! OpenCode, Hermes, OpenClaw and the DeepSeek harness post the whole session
//! themselves. The gateway counts every image in that one request, so pictures
//! from earlier turns block a new one. This gate drops the oldest earlier
//! images before the request is forwarded. MiMo takes none, so its images
//! become a note instead.

use crate::anthropic::translate::{self, MAX_IMAGES_PER_REQUEST};
use crate::net::http1::{ResponseWriter, Server, ServerRequest};
use std::io::Read;

/// `http://127.0.0.1:<port>/v1` forwarding to `upstream_origin` (no `/v1`).
/// The server lives until this process exits: the harness is a child, and
/// dropping the handle would stop the gate before the child sent anything.
/// `api_key` replaces whatever `Authorization` the harness sent: Hermes names
/// its key variable after the base URL's host, reads none for a loopback
/// address, and sends a placeholder bearer instead.
pub fn start(upstream_origin: &str, api_key: &str) -> Option<String> {
    let origin = upstream_origin.trim_end_matches('/').to_string();
    if origin.is_empty() {
        return None;
    }
    let api_key = api_key.to_string();
    let mut server = Server::new();
    server.not_found(move |request, writer| forward(&origin, &api_key, request, writer));
    let (handle, port) = server.bind_and_run("127.0.0.1").ok()?;
    std::mem::forget(handle);
    Some(format!("http://127.0.0.1:{port}/v1"))
}

fn rewrite(path: &str, body: &[u8]) -> Vec<u8> {
    if !path.ends_with("/chat/completions") {
        return body.to_vec();
    }
    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    if !parsed.is_object() {
        return body.to_vec();
    }
    let model = parsed
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if model.to_ascii_lowercase().contains("mimo") {
        parsed = translate::without_openai_images(parsed);
    } else {
        parsed = translate::within_openai_image_limit(parsed, MAX_IMAGES_PER_REQUEST);
    }
    serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec())
}

fn forward(origin: &str, api_key: &str, request: &ServerRequest, writer: &mut ResponseWriter<'_>) {
    let query = if request.query.is_empty() {
        String::new()
    } else {
        format!("?{}", request.query)
    };
    let url = format!("{origin}{}{query}", request.path);
    let body = rewrite(&request.path, &request.body);
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(None)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&url);
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
            || (!api_key.is_empty() && name.eq_ignore_ascii_case("authorization"))
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    if !api_key.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {api_key}"));
    }
    let http_request = match builder.body(body) {
        Ok(request) => request,
        Err(_) => {
            let _ = writer.send_full(502, &[("Content-Type", "text/plain")], b"bad gateway");
            return;
        }
    };
    let response = match agent.run(http_request) {
        Ok(response) => response,
        Err(error) => {
            crate::io::output::error_line(&format!("harness proxy failed: {error}"));
            let _ = writer.send_full(502, &[("Content-Type", "text/plain")], b"bad gateway");
            return;
        }
    };
    let status = response.status().as_u16() as i32;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let streaming = content_type.contains("text/event-stream");
    let mut reader = response.into_body().into_reader();
    if streaming {
        if writer
            .begin_chunked(status, &[("Content-Type", &content_type)])
            .is_err()
        {
            return;
        }
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_chunk(&buf[..n]).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
        let _ = writer.end_chunked();
    } else {
        let mut collected = Vec::new();
        if reader.read_to_end(&mut collected).is_err() {
            let _ = writer.send_full(502, &[("Content-Type", "text/plain")], b"bad gateway");
            return;
        }
        let _ = writer.send_full(status, &[("Content-Type", &content_type)], &collected);
    }
}
