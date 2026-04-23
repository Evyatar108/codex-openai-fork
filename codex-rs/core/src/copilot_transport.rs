// This factory lives in codex-core because codex-api already depends on
// codex-copilot, and moving it into codex-copilot would require codex-api
// types here and create a dependency cycle.

use std::sync::Arc;

use codex_api::CoreAuthProvider;
use codex_api::Provider as ApiProvider;
use codex_api::ResponsesClient;
use codex_client::HttpTransport;
use codex_copilot::payload::Initiator;
use codex_copilot::payload::normalize_payload;
use codex_copilot::payload::request_initiator;
use codex_copilot::payload::request_vision;
use http::HeaderMap;
use http::HeaderValue;
use serde_json::Value;

pub fn build_copilot_client<T: HttpTransport>(
    api_provider: ApiProvider,
    auth: CoreAuthProvider,
    transport: T,
) -> ResponsesClient<T, CoreAuthProvider> {
    ResponsesClient::new(transport, api_provider, auth)
        .with_pre_send_hook(Arc::new(apply_copilot_request_transforms))
}

fn apply_copilot_request_transforms(payload: &mut Value, headers: &mut HeaderMap) {
    normalize_payload(payload);

    let initiator = request_initiator(payload);
    headers.insert("x-initiator", HeaderValue::from_static(initiator.as_str()));
    headers.insert(
        "x-interaction-type",
        HeaderValue::from_static(match initiator {
            Initiator::Agent => "conversation-agent",
            Initiator::User => "conversation-user",
        }),
    );

    if request_vision(payload) {
        headers.insert("copilot-vision-request", HeaderValue::from_static("true"));
    }
}
