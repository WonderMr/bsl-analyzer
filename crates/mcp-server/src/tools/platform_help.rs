//! What a tool that needs the platform help answers while the server is still loading it.
//!
//! The application loads the help in the background (a first extraction of an installed
//! platform's help takes minutes, far past the time a client waits for `initialize`), so a
//! call can arrive before the help is in. Such a call waits a moment — a cached corpus is in
//! within about a second — and then answers with its tool's retry envelope marked
//! `phase: platform_help`, rather than holding the call for the whole extraction or
//! answering from facts that are not the configured ones.

use std::time::Duration;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::diagnostics_state::{DiagnosticsState, StatusReport};

/// What holds a retry envelope back. Its only value names the platform help.
#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
pub(crate) enum LoadingPhase {
    #[serde(rename = "platform_help")]
    PlatformHelp,
}

/// The wire value of [`LoadingPhase::PlatformHelp`].
pub(crate) const PHASE: &str = "platform_help";

/// The sentence beside the machine fields of every such envelope.
pub(crate) const DETAIL: &str = "platform help is still loading (a first start extracts it \
                                 from the installed platform, which takes minutes); retry \
                                 shortly";

/// When to ask again.
pub(crate) const RETRY_AFTER_MS: u64 = 5_000;

/// How long a call waits for a load in flight before it answers `loading`: enough for a
/// cached corpus, far short of an extraction.
const GRACE: Duration = Duration::from_secs(5);

/// `Ok(None)` when the platform help serves, at once or within a short wait; `loading()`
/// when it is still being loaded after that; the cancellation error when the client
/// cancelled meanwhile, since a cancelled call is never handed a body.
pub(crate) async fn loading_answer(
    ct: &CancellationToken,
    loading: impl FnOnce() -> CallToolResult,
) -> Result<Option<CallToolResult>, McpError> {
    if !bsl_platform::platform_help_pending() {
        return Ok(None);
    }
    // The wait blocks a thread, so it takes one of the blocking pool's: an async worker
    // held here would stop serving every other call on it.
    let wait = tokio::task::spawn_blocking(|| bsl_platform::wait_for_platform_help(Some(GRACE)));
    let waited = tokio::select! {
        biased;
        () = ct.cancelled() => None,
        waited = wait => Some(waited.unwrap_or(false)),
    };
    let ready = match waited {
        Some(waited) => waited || !bsl_platform::platform_help_pending(),
        None => return Err(McpError::internal_error("request cancelled", None)),
    };
    Ok((!ready).then(loading))
}

/// [`loading_answer`] for a tool that reads the resident analysis database: its envelope is
/// rendered from the resident's lifecycle as it is when the wait gives up.
pub(crate) async fn resident_loading_answer(
    ct: &CancellationToken,
    diag: &DiagnosticsState,
    schema_version: Option<&str>,
) -> Result<Option<CallToolResult>, McpError> {
    loading_answer(ct, || resident_loading(&diag.status_report(), schema_version)).await
}

/// The retry envelope of a tool that reads the resident analysis database, while the help
/// loads: the resident's own lifecycle snapshot, marked with the phase that holds the answer
/// back. The resident may well be ready; every answer it gives reads the help, so `status`
/// is `loading` whatever the resident's own state. `schema_version` is the tool's own, when
/// its envelope carries one.
pub(crate) fn resident_loading(
    report: &StatusReport,
    schema_version: Option<&str>,
) -> CallToolResult {
    let mut body = super::resident::loading_body(report, DETAIL);
    body["status"] = json!("loading");
    body["phase"] = json!(PHASE);
    if let Some(version) = schema_version {
        body["schema_version"] = json!(version);
    }
    super::response::structured(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn report(state: &'static str) -> StatusReport {
        StatusReport {
            state,
            generation: 2,
            files: None,
            unread_files: None,
            reload: "none",
            error: None,
            elapsed_ms: None,
            failed_age_ms: None,
            attempts: 0,
            retry_after_ms: None,
            watch: None,
        }
    }

    fn validate(schema: &serde_json::Map<String, Value>, body: &Value) {
        let schema = Value::Object(schema.clone());
        let validator = jsonschema::validator_for(&schema).expect("the published schema compiles");
        if let Err(error) = validator.validate(body) {
            panic!("{body} is not a body the tool publishes: {error}");
        }
    }

    /// The enum and the constant name the same wire value: the schema is built from one,
    /// the bodies from the other.
    #[test]
    fn the_phase_constant_is_the_wire_value_of_the_phase() {
        assert_eq!(serde_json::to_value(LoadingPhase::PlatformHelp).unwrap(), json!(PHASE));
    }

    #[test]
    fn syntax_help_answers_loading_under_its_published_schema() {
        let result = crate::tools::platform::syntax_help_loading();
        let body = result.structured_content.expect("structured");
        assert_eq!(body["status"], "loading");
        assert_eq!(body["phase"], PHASE);
        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["retry_after_ms"], RETRY_AFTER_MS);
        assert!(body.get("kind").is_none(), "a loading answer is no card: {body}");
        validate(&crate::tools::platform::syntax_help_output_schema(), &body);
    }

    #[test]
    fn platform_search_actions_answer_not_ready_under_their_published_schemas() {
        use crate::indexing::{Indexing, Kind, State, Target};

        for schema in [
            crate::tools::search::search_output_schema(),
            crate::tools::search::reference_search_output_schema(),
        ] {
            let listing = crate::tools::search::platform_help_not_ready("list_platform");
            let body = listing.structured_content.expect("structured");
            assert_eq!(body["status"], "not_ready");
            assert_eq!(body["phase"], PHASE);
            validate(&schema, &body);
            for action in ["find_docs", "search_docs"] {
                let mut result = crate::tools::search::platform_help_not_ready(action);
                // A docs answer leaves the handler with its indexing telemetry attached.
                let indexing = Indexing::single(Target::new(Kind::Reference, State::Ready, None));
                indexing.attach(&mut result);
                let body = result.structured_content.expect("structured");
                assert_eq!(body["status"], "not_ready");
                assert_eq!(body["phase"], PHASE);
                validate(&schema, &body);
            }
        }

        // Only the workspace profile serves `search_code`.
        let mut result = crate::tools::search::platform_help_not_ready("search_code");
        let indexing = Indexing::single(Target::new(Kind::Lexical, State::Ready, None));
        indexing.attach(&mut result);
        let body = result.structured_content.expect("structured");
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["phase"], PHASE);
        assert_eq!(body["schema_version"], "8");
        validate(&crate::tools::search::search_output_schema(), &body);
    }

    #[test]
    fn resident_tools_answer_loading_under_their_published_schemas() {
        use crate::tools::diagnostics::DiagnosticsResponseSchema;
        use rmcp::handler::server::tool::schema_for_type;

        for state in ["ready", "loading", "failed"] {
            let symbol = resident_loading(&report(state), Some("1"));
            let body = symbol.structured_content.expect("structured");
            assert_eq!(body["status"], "loading", "{state}: {body}");
            assert_eq!(body["phase"], PHASE);
            assert_eq!(body["state"], state);
            validate(&crate::tools::symbol_info::symbol_info_output_schema(), &body);
            validate(&crate::tools::references::references_output_schema(), &body);

            let diagnostics = resident_loading(&report(state), None);
            let body = diagnostics.structured_content.expect("structured");
            assert!(body.get("schema_version").is_none(), "{body}");
            validate(&schema_for_type::<DiagnosticsResponseSchema>(), &body);
        }
    }
}
