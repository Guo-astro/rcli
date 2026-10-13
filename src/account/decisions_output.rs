//! CLI-only metadata around the pinned decisions body. This is not the
//! `/v1/decisions` service schema; local serving still uses that schema alone.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::value::RawValue;
use serde_json::Value;

use super::decisions_contract::{DecisionsResponse, OpenAIError, OpenAIErrorDetail};
use super::HttpResponse;

/// A service success checked by the generated binding. Business consumers use
/// the returned typed response; this original document is only for rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudDecisionsDocument(String);

impl CloudDecisionsDocument {
    pub(crate) fn parse(
        body: &str,
        access_token: &str,
    ) -> Result<(DecisionsResponse, Self), String> {
        let value: Value = serde_json::from_str(body)
            .map_err(|_| "console returned malformed JSON".to_string())?;
        if !value.is_object() {
            return Err("console returned a JSON value instead of an object".to_string());
        }
        // Canonicalizing for this check also detects unicode-escaped token
        // echoes. Do not return unsafe extension fields via raw passthrough.
        if !access_token.is_empty() && value.to_string().contains(access_token) {
            return Err("console returned an unsafe decisions response".to_string());
        }
        let response = DecisionsResponse::from_json(&value).map_err(|_| {
            "console returned a response that did not match the contract".to_string()
        })?;
        Ok((response, Self(body.to_string())))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionsRefusal {
    pub detail: OpenAIErrorDetail,
    /// Missing or future enum values must not be reported as the binding's
    /// default type. The message remains useful without this optional label.
    pub has_known_type: bool,
}

/// Bounds from the pinned OpenAPI `RequestId` schema (guarded by a contract test).
pub const REQUEST_ID_MIN_CHARS: usize = 1;
pub const REQUEST_ID_MAX_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn parse(value: &str) -> Option<Self> {
        let length = value.chars().count();
        ((REQUEST_ID_MIN_CHARS..=REQUEST_ID_MAX_CHARS).contains(&length)
            && !value.chars().any(char::is_control))
        .then(|| Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResponseMetadata {
    pub request_id: Option<RequestId>,
    pub http_status: Option<i32>,
}

impl ResponseMetadata {
    pub fn from_response(response: &HttpResponse) -> Self {
        Self {
            request_id: response
                .headers
                .get("x-request-id")
                .and_then(|id| RequestId::parse(id)),
            http_status: (100..=599)
                .contains(&response.status)
                .then_some(response.status),
        }
    }

    pub(crate) fn redact(&mut self, access_token: &str) {
        if !access_token.is_empty()
            && self
                .request_id
                .as_ref()
                .is_some_and(|id| id.as_str().contains(access_token))
        {
            self.request_id = None;
        }
    }

    fn attach(&self, mut body: Value) -> Value {
        // Both body constructors below are generated, typed objects. No opaque
        // response JSON or headers are forwarded into the CLI document.
        body.as_object_mut()
            .expect("typed decisions object")
            .extend(
                serde_json::to_value(self)
                    .expect("metadata serialization")
                    .as_object()
                    .expect("metadata object")
                    .clone(),
            );
        body
    }

    pub fn success_json(&self, response: &DecisionsResponse) -> Value {
        self.attach(response.to_json())
    }

    /// Preserve extension fields, explicit nulls and numeric lexemes after
    /// contract validation, while the response headers own CLI metadata.
    pub fn cloud_success_json(&self, document: &CloudDecisionsDocument) -> String {
        let mut fields: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(&document.0).expect("validated decisions object");
        fields.remove("request_id");
        fields.remove("http_status");
        #[derive(Serialize)]
        struct Success<'a> {
            #[serde(flatten)]
            body: &'a BTreeMap<String, Box<RawValue>>,
            #[serde(flatten)]
            metadata: &'a ResponseMetadata,
        }
        serde_json::to_string(&Success {
            body: &fields,
            metadata: self,
        })
        .expect("validated decisions serialization")
    }

    pub fn failure_json(&self, refusal: Option<&DecisionsRefusal>, message: &str) -> Value {
        match refusal {
            Some(error) => {
                let mut body = OpenAIError {
                    error: error.detail.clone(),
                }
                .to_json();
                if !error.has_known_type {
                    body["error"]
                        .as_object_mut()
                        .expect("typed error")
                        .remove("type");
                }
                self.attach(body)
            }
            None => {
                #[derive(Serialize)]
                struct Detail<'a> {
                    message: &'a str,
                }
                #[derive(Serialize)]
                struct Failure<'a> {
                    error: Detail<'a>,
                    #[serde(flatten)]
                    metadata: &'a ResponseMetadata,
                }
                serde_json::to_value(Failure {
                    error: Detail { message },
                    metadata: self,
                })
                .expect("failure serialization")
            }
        }
    }
}
