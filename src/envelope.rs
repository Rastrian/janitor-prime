//! Versioned JSON envelope — the single stdout contract (PLAN §1.6, R12).
//!
//! `stdout` is 100% JSON from day one: every user-facing message, including
//! `--version` and clap usage errors, flows through `Envelope`; human- and
//! machine-readable alike. Diagnostics go to `stderr` via `tracing`, never to
//! stdout.
//!
//! The shape is frozen from F0 (agents bake formats into prompts; retrofit
//! would be a breaking change — eval-2 §6.9):
//!
//! ```json
//! { "schema": "janitor.v1", "ok": true, "data": { … } }
//! { "schema": "janitor.v1", "ok": false,
//!   "error": { "code": "E_…", "message": "…", "details": { … } } }
//! ```

use serde::Serialize;
use serde_json::Value;
use std::io::Write;

use crate::error::{JanitorError, OutputError};

/// Envelope schema identifier. Bumping this is a breaking change (PLAN §7).
pub const SCHEMA: &str = "janitor.v1";

/// Exit codes (PLAN §1.6): 0 success / 2 usage / 1 operational.
/// Usage errors are produced only at the clap boundary (`crate::cli`);
/// every `JanitorError` is operational.
pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_OPERATIONAL: u8 = 1;

/// The success/error envelope around every stdout message.
///
/// `data` and `error` are mutually exclusive; each is omitted (not `null`)
/// when absent so consumers can rely on exact key sets.
#[derive(Debug, Serialize)]
pub struct Envelope<T> {
    pub schema: &'static str,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl<T: Serialize> Envelope<T> {
    /// A success envelope carrying `data`.
    pub fn ok(data: T) -> Self {
        Self {
            schema: SCHEMA,
            ok: true,
            data: Some(data),
            error: None,
        }
    }
}

impl Envelope<Value> {
    /// An error envelope with a stable `E_*` code (see `crate::error`).
    pub fn error(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self {
            schema: SCHEMA,
            ok: false,
            data: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
                details,
            }),
        }
    }
}

/// The `error` body of a failure envelope.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Stable, documented error code (e.g. `E_POINTER_DANGLING`).
    pub code: String,
    /// Human-readable one-liner; structured context belongs in `details`.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// One `From<JanitorError>` impl maps the whole taxonomy onto the envelope
/// (PLAN §1.7).
impl From<JanitorError> for Envelope<Value> {
    fn from(err: JanitorError) -> Self {
        Self::error(err.code(), err.to_string(), err.details())
    }
}

/// Serialize `envelope` and write it (one line, trailing `\n`) to stdout.
///
/// `pretty` selects multi-line JSON for humans; stdout stays 100% JSON either
/// way (`--pretty` is an affordance, not a format switch).
pub fn emit_stdout<T: Serialize>(envelope: &Envelope<T>, pretty: bool) -> Result<(), OutputError> {
    let json = if pretty {
        serde_json::to_string_pretty(envelope)
    } else {
        serde_json::to_string(envelope)
    }
    .map_err(|err| OutputError::Serialization(err.to_string()))?;

    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(json.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|err| OutputError::Io(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EditError, MemoryError};
    use serde_json::json;

    #[derive(Serialize)]
    struct Version {
        version: &'static str,
        grammars: &'static [&'static str],
    }

    #[test]
    fn success_envelope_is_exact_shape() -> Result<(), serde_json::Error> {
        let env = Envelope::ok(Version {
            version: "0.1.0",
            grammars: &["rust"],
        });
        let text = serde_json::to_string(&env)?;
        assert_eq!(
            text,
            r#"{"schema":"janitor.v1","ok":true,"data":{"version":"0.1.0","grammars":["rust"]}}"#
        );
        Ok(())
    }

    #[test]
    fn error_envelope_omits_details_when_none() -> Result<(), serde_json::Error> {
        let env = Envelope::<Value>::error("E_NAP_PENDING", "pending", None);
        let text = serde_json::to_string(&env)?;
        assert_eq!(
            text,
            r#"{"schema":"janitor.v1","ok":false,"error":{"code":"E_NAP_PENDING","message":"pending"}}"#
        );
        Ok(())
    }

    #[test]
    fn error_envelope_carries_details() -> Result<(), serde_json::Error> {
        let env =
            Envelope::<Value>::error("E_NAP_PENDING", "pending", Some(json!({"slot": "0-1"})));
        let text = serde_json::to_string(&env)?;
        assert_eq!(
            text,
            r#"{"schema":"janitor.v1","ok":false,"error":{"code":"E_NAP_PENDING","message":"pending","details":{"slot":"0-1"}}}"#
        );
        Ok(())
    }

    #[test]
    fn envelope_round_trips_through_value() -> Result<(), serde_json::Error> {
        let env = Envelope::ok(Version {
            version: "0.1.0",
            grammars: &["rust", "python"],
        });
        let value = serde_json::to_value(&env)?;
        let text = serde_json::to_string(&value)?;
        let reparsed: Value = serde_json::from_str(&text)?;
        assert_eq!(reparsed, value);
        assert_eq!(reparsed["schema"], SCHEMA);
        assert_eq!(reparsed["ok"], true);
        assert_eq!(reparsed["data"]["grammars"][1], "python");
        Ok(())
    }

    #[test]
    fn janitor_error_maps_onto_the_envelope() {
        let err: JanitorError = MemoryError::PointerDangling {
            path: "src/vfs/overlay.rs".into(),
            span: (120, 480),
        }
        .into();
        let env: Envelope<Value> = err.into();
        assert!(!env.ok);
        let body = env.error.expect("error envelope must carry an error body");
        assert_eq!(body.code, "E_POINTER_DANGLING");
        assert_eq!(
            body.details,
            Some(json!({"path": "src/vfs/overlay.rs", "span": [120, 480]}))
        );
    }

    #[test]
    fn stale_view_maps_onto_the_envelope() {
        let err: JanitorError = EditError::StaleView {
            path: "src/x.rs".into(),
            expected_sha: "deadbeef".into(),
        }
        .into();
        let env: Envelope<Value> = err.into();
        let code = env
            .error
            .expect("error envelope must carry an error body")
            .code;
        assert_eq!(code, "E_STALE_VIEW");
    }
}
