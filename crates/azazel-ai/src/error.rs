//! The error type shared by providers and the router.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum AiError {
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited")]
    RateLimit {
        /// Serialised as seconds.
        #[serde(default, with = "opt_secs")]
        retry_after: Option<Duration>,
    },
    /// The account's usage allowance is used up. Asking again on the same account does not help
    /// before `resets_at` (Unix seconds), but another account or model may answer.
    #[error("usage limit reached: {message}")]
    Quota {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resets_at: Option<u64>,
        message: String,
    },
    #[error("provider overloaded")]
    Overloaded,
    #[error("the request exceeds the model's context window")]
    ContextOverflow,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    /// The provider sent nothing for `secs` seconds, before the reply began or in the middle of
    /// it. The attempt was given up; what had arrived of the reply is not kept.
    #[error("no answer for {secs} s")]
    Stalled { secs: u64 },
    #[error("aborted")]
    Aborted,
}

impl AiError {
    /// Whether the same model may succeed if the request is sent again.
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::Network(_)
            | AiError::Overloaded
            | AiError::RateLimit { .. }
            | AiError::Stalled { .. } => true,
            AiError::Server { status, .. } => *status >= 500,
            AiError::Auth(_)
            | AiError::Quota { .. }
            | AiError::ContextOverflow
            | AiError::InvalidRequest(_)
            | AiError::Aborted => false,
        }
    }
}

mod opt_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        value.map(|d| d.as_secs_f64()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let secs = Option::<f64>::deserialize(d)?;
        Ok(secs
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_shape_is_kind_plus_detail() {
        let cases = [
            (AiError::Overloaded, serde_json::json!({"kind": "overloaded"})),
            (
                AiError::Auth("no key".into()),
                serde_json::json!({"kind": "auth", "detail": "no key"}),
            ),
            (
                AiError::RateLimit {
                    retry_after: Some(Duration::from_millis(1500)),
                },
                serde_json::json!({"kind": "rate_limit", "detail": {"retry_after": 1.5}}),
            ),
            (
                AiError::Server {
                    status: 502,
                    body: "bad gateway".into(),
                },
                serde_json::json!({"kind": "server", "detail": {"status": 502, "body": "bad gateway"}}),
            ),
            (
                AiError::Quota {
                    resets_at: Some(1_800_000_000),
                    message: "weekly limit".into(),
                },
                serde_json::json!({"kind": "quota", "detail": {"resets_at": 1_800_000_000u64, "message": "weekly limit"}}),
            ),
            (
                AiError::Stalled { secs: 20 },
                serde_json::json!({"kind": "stalled", "detail": {"secs": 20}}),
            ),
        ];
        for (error, json) in cases {
            assert_eq!(serde_json::to_value(&error).unwrap(), json);
            assert_eq!(serde_json::from_value::<AiError>(json).unwrap(), error);
        }
    }

    #[test]
    fn retryable_covers_transient_failures_only() {
        assert!(AiError::Network("reset".into()).is_retryable());
        assert!(AiError::Stalled { secs: 20 }.is_retryable());
        assert!(
            AiError::Server {
                status: 503,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(
            !AiError::Server {
                status: 404,
                body: String::new()
            }
            .is_retryable()
        );
        assert!(!AiError::ContextOverflow.is_retryable());
        assert!(!AiError::Auth(String::new()).is_retryable());
        assert!(
            !AiError::Quota {
                resets_at: None,
                message: String::new()
            }
            .is_retryable()
        );
    }
}
