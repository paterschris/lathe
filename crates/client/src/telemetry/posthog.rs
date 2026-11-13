use anyhow::Result;
use http_client::{AsyncBody, HttpRequestExt as _, Method, Request};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use telemetry_events::{Event, EventRequestBody};

// This is the public, write-only token for Lathe's shared Default project.
const PROJECT_TOKEN: &str = "phc_kZU8WvMv2uAACoHYtidP4w3NnRQyCa3XQYJEmdTekzQH";
const CAPTURE_URL: &str = "https://us.i.posthog.com/batch/";

fn event_name(name: &str) -> Option<&'static str> {
    match name {
        "App First Opened" | "App First Opened For Release Channel" | "App Opened" => {
            Some("desktop_app_opened")
        }
        "Agent Thread Started" => Some("desktop_thread_started"),
        "Agent Message Sent" => Some("desktop_message_sent"),
        "Agent Turn Completed" => Some("desktop_agent_turn_completed"),
        "Agent Tool Call Authorized" => Some("desktop_tool_authorized"),
        "Agent Panel Error Shown" => Some("desktop_agent_error_shown"),
        _ => None,
    }
}

fn batch(body: &EventRequestBody) -> Option<Value> {
    let installation_id = body.installation_id.as_deref()?;
    if installation_id.is_empty() {
        return None;
    }
    // Namespace the anonymous installation ID so it cannot collide with another app's users.
    let distinct_id = format!(
        "lathe-desktop:{:x}",
        Sha256::digest(installation_id.as_bytes())
    );
    let environment = if cfg!(debug_assertions) {
        "development"
    } else {
        "production"
    };
    let mut first_open_seen = false;
    let events: Vec<Value> = body
        .events
        .iter()
        .filter_map(|wrapper| {
            let Event::Flexible(event) = &wrapper.event;
            let name = event_name(&event.event_type)?;
            // The initial launch emits two upstream first-open events for the same launch.
            if name == "desktop_app_opened" && std::mem::replace(&mut first_open_seen, true) {
                return None;
            }
            // Rebuild properties instead of forwarding upstream payloads, which may contain content.
            Some(json!({
                "event": name,
                "properties": {
                    "distinct_id": distinct_id,
                    "$process_person_profile": false,
                    "$geoip_disable": true,
                    "product": "lathe",
                    "app_name": "lathe-desktop",
                    "environment": environment,
                    "app_version": body.app_version,
                    "os": body.os_name,
                    "architecture": body.architecture,
                },
            }))
        })
        .collect();
    if events.is_empty() {
        return None;
    }
    Some(json!({ "token": PROJECT_TOKEN, "batch": events }))
}

pub(super) fn build_request(body: &EventRequestBody) -> Result<Option<Request<AsyncBody>>> {
    let Some(batch) = batch(body) else {
        return Ok(None);
    };
    Ok(Some(
        Request::builder()
            .method(Method::POST)
            .uri(CAPTURE_URL)
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(10))
            .body(serde_json::to_vec(&batch)?.into())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry_events::{EventWrapper, FlexibleEvent};

    fn body(names: &[&str]) -> EventRequestBody {
        EventRequestBody {
            system_id: Some("private-system".into()),
            installation_id: Some("private-installation".into()),
            session_id: Some("private-session".into()),
            metrics_id: Some("private-account".into()),
            is_staff: None,
            app_version: "1.0.0".into(),
            os_name: "macOS".into(),
            os_version: None,
            architecture: "aarch64".into(),
            release_channel: None,
            events: names
                .iter()
                .map(|name| EventWrapper {
                    signed_in: true,
                    milliseconds_since_first_event: 0,
                    event: Event::Flexible(FlexibleEvent {
                        event_type: name.to_string(),
                        event_properties: [("prompt".into(), json!("private-content"))].into(),
                    }),
                })
                .collect(),
        }
    }

    #[test]
    fn only_exports_allowed_event_names_and_sanitized_properties() {
        let result = batch(&body(&["Agent Message Sent", "Unknown Event"]))
            .expect("an allowed event should produce a batch");
        let events = result["batch"]
            .as_array()
            .expect("batch should be an array");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "desktop_message_sent");
        let properties = &events[0]["properties"];
        assert_eq!(properties["product"], "lathe");
        assert_eq!(properties["app_name"], "lathe-desktop");
        assert_eq!(properties["$process_person_profile"], false);
        assert!(
            properties["distinct_id"]
                .as_str()
                .expect("distinct ID")
                .starts_with("lathe-desktop:")
        );
        assert!(!result.to_string().contains("private-"));
        assert!(properties.get("prompt").is_none());
    }

    #[test]
    fn skips_missing_identity_and_unlisted_events_and_deduplicates_first_open() {
        let mut missing_identity = body(&["App Opened"]);
        missing_identity.installation_id = None;
        assert!(batch(&missing_identity).is_none());
        assert!(batch(&body(&["Unknown Event"])).is_none());
        let result = batch(&body(&[
            "App First Opened",
            "App First Opened For Release Channel",
        ]))
        .expect("first launch should produce a batch");
        assert_eq!(result["batch"].as_array().expect("events").len(), 1);
    }
}
