//! Metadata and session regressions use dummy credentials and injected transports.
#[path = "common/mod.rs"]
mod common;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use wally::account::decisions_contract as contract;
use wally::account::{
    ConsoleClient, ConsoleSession, Credentials, HttpResponse, IdentityResult, Transport,
};

fn response(status: i32, id: &str, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        body: body.into(),
        headers: BTreeMap::from([
            ("x-request-id".into(), id.into()),
            ("retry-after".into(), "0".into()),
        ]),
    }
}

fn success(id: &str) -> HttpResponse {
    response(
        200,
        id,
        r#"{"object":"decisions","model":"eve","prompt_format_version":3,"answers":{},"usage":{"prompt_tokens":12,"total_tokens":12,"completion_tokens":0}}"#,
    )
}

fn client(replies: Vec<Result<HttpResponse, String>>) -> ConsoleClient {
    let replies = Mutex::new(VecDeque::from(replies));
    let transport: Transport = Arc::new(move |_| {
        replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request")
    });
    ConsoleClient::new(Some(transport))
}

fn request() -> contract::DecisionsRequest {
    contract::DecisionsRequest {
        model: "eve".into(),
        input: "ticket".into(),
        ..Default::default()
    }
}

#[test]
fn decisions_success_and_refusal_keep_status_and_header() {
    for status in [200, 400, 403, 429, 503] {
        let reply = if status == 200 {
            success("exact-header-id")
        } else {
            response(
                status,
                "exact-header-id",
                r#"{"error":{"code":"bad_request","message":"safe refusal","type":"invalid_request_error"}}"#,
            )
        };
        let (identity, result, message) = client(vec![Ok(reply)]).create_decisions(
            "https://api.example.test",
            "dummy-key",
            &request(),
            true,
            None,
        );
        assert_eq!(result.metadata.http_status, Some(status));
        assert_eq!(
            result.metadata.request_id.as_ref().unwrap().as_str(),
            "exact-header-id"
        );
        if status == 200 {
            assert_eq!(identity, IdentityResult::Ok);
            let output = result.metadata.success_json(&result.response);
            assert_eq!(output["usage"]["total_tokens"], 12);
        } else {
            assert_eq!(identity, IdentityResult::Failed);
            assert!(message.contains(&format!("HTTP {status}")));
            let output = result
                .metadata
                .failure_json(result.refusal.as_ref(), &message);
            assert_eq!(output["error"]["message"], "safe refusal");
        }
    }
}

#[test]
fn decisions_success_preserves_extensions_nulls_and_numeric_text() {
    let reply = response(
        200,
        "header-id",
        r#"{"object":"decisions","model":"eve","prompt_format_version":3,"answers":{},"usage":{"prompt_tokens":12,"total_tokens":12,"completion_tokens":0,"reasoning_tokens":null},"future":{"nullable":null,"number":1.234567890123456789e-100},"large":184467440737095516160,"request_id":"body-id","http_status":418}"#,
    );
    let (identity, result, _) = client(vec![Ok(reply)]).create_decisions(
        "https://api.example.test",
        "dummy-key",
        &request(),
        true,
        None,
    );
    assert_eq!(identity, IdentityResult::Ok);
    let output = result.success_json();
    assert!(output.contains("1.234567890123456789e-100"));
    assert!(output.contains("184467440737095516160"));
    assert_eq!(output.matches("\"request_id\"").count(), 1);
    assert_eq!(output.matches("\"http_status\"").count(), 1);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["request_id"], "header-id");
    assert_eq!(parsed["http_status"], 200);
    assert_eq!(
        parsed["usage"].get("reasoning_tokens"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        parsed["future"].get("nullable"),
        Some(&serde_json::Value::Null)
    );
    let typed = contract::DecisionsResponse::from_json(&parsed).unwrap();
    assert_eq!(typed, result.response);
}

#[test]
fn decisions_success_does_not_pass_through_echoed_credentials() {
    for body in [
        r#"{"object":"decisions","model":"eve","future":"dummy-key"}"#,
        r#"{"object":"decisions","model":"eve","future":"\u0064ummy-key"}"#,
    ] {
        let (identity, result, message) = client(vec![Ok(response(200, "header-id", body))])
            .create_decisions(
                "https://api.example.test",
                "dummy-key",
                &request(),
                true,
                None,
            );
        assert_eq!(identity, IdentityResult::Failed);
        assert_eq!(result.metadata.http_status, Some(200));
        let output = result
            .metadata
            .failure_json(result.refusal.as_ref(), &message)
            .to_string();
        assert!(!output.contains("dummy-key"));
        assert!(!output.contains("future"));
    }
}

#[test]
fn decisions_header_bounds_apply_to_success_and_refusal_without_truncation() {
    for status in [200, 400] {
        for id in [
            String::new(),
            "x".repeat(256),
            "x".repeat(257),
            "bad\nheader".into(),
        ] {
            let mut reply = success(&id);
            reply.status = status;
            let (_, result, _) = client(vec![Ok(reply)]).create_decisions(
                "https://api.example.test",
                "dummy-key",
                &request(),
                true,
                None,
            );
            assert_eq!(result.metadata.http_status, Some(status));
            if id.len() == 256 {
                assert_eq!(result.metadata.request_id.unwrap().as_str(), id);
            } else {
                assert!(result.metadata.request_id.is_none());
            }
        }
    }
}

#[test]
fn decisions_retry_and_exhaustion_use_only_final_attempt_metadata() {
    for status in [429, 503] {
        let (identity, result, _) = client(vec![
            Ok(response(status, "first", "{}")),
            Ok(success("last")),
        ])
        .create_decisions(
            "https://api.example.test",
            "dummy-key",
            &request(),
            false,
            None,
        );
        assert_eq!(identity, IdentityResult::Ok);
        assert_eq!(result.metadata.http_status, Some(200));
        assert_eq!(result.metadata.request_id.unwrap().as_str(), "last");
        let (identity, result, _) = client(
            (1..=3)
                .map(|i| Ok(response(status, &format!("attempt-{i}"), "{}")))
                .collect(),
        )
        .create_decisions(
            "https://api.example.test",
            "dummy-key",
            &request(),
            false,
            None,
        );
        assert_eq!(identity, IdentityResult::Failed);
        assert_eq!(result.metadata.http_status, Some(status));
        assert_eq!(result.metadata.request_id.unwrap().as_str(), "attempt-3");
    }
}

#[test]
fn decisions_invalid_body_keeps_headers_but_transport_failure_does_not() {
    for body in ["{", r#"{"usage":"wrong type"}"#] {
        let (identity, result, _) = client(vec![Ok(response(200, "received", body))])
            .create_decisions(
                "https://api.example.test",
                "dummy-key",
                &request(),
                true,
                None,
            );
        assert_eq!(identity, IdentityResult::Failed);
        assert_eq!(result.metadata.http_status, Some(200));
        assert_eq!(result.metadata.request_id.unwrap().as_str(), "received");
    }
    let (_, result, message) = client(vec![
        Ok(response(503, "earlier", "{}")),
        Err("offline".into()),
    ])
    .create_decisions(
        "https://api.example.test",
        "dummy-key",
        &request(),
        false,
        None,
    );
    let output = result.metadata.failure_json(None, &message);
    assert_eq!(output.get("http_status"), Some(&serde_json::Value::Null));
    assert_eq!(output.get("request_id"), Some(&serde_json::Value::Null));
}

#[test]
fn decisions_tolerates_unknown_error_enums_and_does_not_echo_secrets() {
    for (body, id, safe) in [
        (
            r#"{"error":{"code":"future-code","message":"safe message","type":"future-type"}}"#,
            "received",
            true,
        ),
        (
            r#"{"error":{"message":"dummy-key","type":"invalid_request_error"}}"#,
            "dummy-key",
            false,
        ),
        (
            r#"{"error":{"message":"safe message","param":"dummy-key","type":"invalid_request_error"}}"#,
            "received",
            false,
        ),
    ] {
        let (_, result, message) = client(vec![Ok(response(400, id, body))]).create_decisions(
            "https://api.example.test",
            "dummy-key",
            &request(),
            true,
            None,
        );
        let output = result
            .metadata
            .failure_json(result.refusal.as_ref(), &message)
            .to_string();
        assert!(!output.contains("dummy-key"));
        assert_eq!(result.refusal.is_some(), safe);
        assert_eq!(result.metadata.http_status, Some(400));
    }
}

#[test]
fn decisions_refresh_failure_keeps_decisions_metadata_not_auth_metadata() {
    let _lock = common::env_lock();
    let home = common::TempHome::new();
    let mut env = common::EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path());
    for refresh_status in [401, 503] {
        let client = client(vec![
            Ok(response(401, "decision-401", "{}")),
            Ok(response(refresh_status, "refresh-id", "{}")),
        ]);
        let credentials = Credentials {
            console_url: "https://api.example.test".into(),
            access_token: "dummy-key".into(),
            refresh_token: "dummy-refresh".into(),
            expires_at: 5000,
            ..Default::default()
        };
        let mut session = ConsoleSession::resume(client, credentials, 1000).unwrap();
        let error = session
            .call_preserving_failure(|client, url, token| {
                client.create_decisions(url, token, &request(), true, None)
            })
            .unwrap_err();
        assert_eq!(error.value.metadata.http_status, Some(401));
        assert_eq!(
            error.value.metadata.request_id.unwrap().as_str(),
            "decision-401"
        );
        assert!(!error.message.contains("dummy-key"));
    }
}

#[test]
fn decisions_second_401_keeps_second_attempt_metadata() {
    let _lock = common::env_lock();
    let home = common::TempHome::new();
    let mut env = common::EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", home.path());
    let client = client(vec![
        Ok(response(401, "first", "{}")),
        Ok(response(
            200,
            "refresh-id",
            r#"{"access_token":"new-dummy-key","refresh_token":"dummy-refresh","expires_in":3600}"#,
        )),
        Ok(response(401, "second", "{}")),
    ]);
    let credentials = Credentials {
        console_url: "https://api.example.test".into(),
        access_token: "dummy-key".into(),
        refresh_token: "dummy-refresh".into(),
        expires_at: 5000,
        ..Default::default()
    };
    let mut session = ConsoleSession::resume(client, credentials, 1000).unwrap();
    let error = session
        .call_preserving_failure(|client, url, token| {
            client.create_decisions(url, token, &request(), true, None)
        })
        .unwrap_err();
    assert_eq!(error.value.metadata.http_status, Some(401));
    assert_eq!(error.value.metadata.request_id.unwrap().as_str(), "second");
}
