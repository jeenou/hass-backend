// src/home_assistant.rs

use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Valid HA domains we support.
static VALID_DOMAINS: &[&str] = &["switch", "light", "climate", "number", "fan", "cover"];

/// Extract a Home Assistant `entity_id` from a raw control signal name.
/// Handles various naming formats used by your optimization model.
pub fn extract_entity_id(name: &str) -> String {
    if !name.contains('.') {
        return String::new();
    }

    let marker = "_electricitygrid";
    if let Some(idx) = name.find(marker) {
        let entity_id = &name[..idx];
        let domain = entity_id.split('.').next().unwrap_or("");
        if VALID_DOMAINS.contains(&domain) {
            return entity_id.to_string();
        }
        return String::new();
    }

    // General case: accumulate parts until domain appears twice.
    let parts: Vec<&str> = name.split('_').collect();
    let first_dot = name.find('.').unwrap_or(0);
    let domain = &name[..first_dot];
    let mut entity_parts = Vec::new();
    let mut seen_domain_count = 0;

    for part in parts {
        entity_parts.push(part);
        if part == domain || part.starts_with(&(domain.to_owned() + ".")) {
            seen_domain_count += 1;
            if seen_domain_count > 1 {
                entity_parts.pop();
                break;
            }
        }
    }

    let candidate = entity_parts.join("_");
    if !candidate.contains('.') {
        return String::new();
    }

    let cand_domain = candidate.split('.').next().unwrap_or("");
    if VALID_DOMAINS.contains(&cand_domain) {
        candidate
    } else {
        String::new()
    }
}

/// Fetch the current state of a Home Assistant entity by its entity_id.
/// Wraps the HA REST API: GET /api/states/{entity_id}
pub async fn fetch_entity_state(
    client: &Client,
    base_url: &str,
    api_key: &str,
    entity_id: &str,
) -> Result<Value, reqwest::Error> {
    let url = format!(
        "{}/states/{}",
        base_url.trim_end_matches('/'),
        entity_id
    );

    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await?
        .error_for_status()?; // propagate non-2xx as errors

    let body = resp.json::<Value>().await?;
    Ok(body)
}

/// Send a single control signal to Home Assistant using REST API.
pub async fn send_control_signal(
    client: &Client,
    base_url: &str,
    api_key: &str,
    entity_id: &str,
    value: f64,
) -> Result<(), reqwest::Error> {
    let domain = entity_id.split('.').next().unwrap_or("");

    let (service, payload) = match domain {
        "switch" | "light" => {
            let service = if value > 0.0 { "turn_on" } else { "turn_off" };
            (
                service.to_string(),
                json!({ "entity_id": entity_id }),
            )
        }
        "climate" => {
            (
                "set_temperature".to_string(),
                json!({
                    "entity_id": entity_id,
                    "temperature": value
                }),
            )
        }
        _ => {
            let service = if value > 0.0 { "turn_on" } else { "turn_off" };
            (
                service.to_string(),
                json!({ "entity_id": entity_id }),
            )
        }
    };

    let url = format!(
        "{}/services/{}/{}",
        base_url.trim_end_matches('/'),
        domain,
        service
    );

    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await?;

    if let Err(e) = resp.error_for_status_ref() {
        eprintln!("Home Assistant error for {}: {}", entity_id, e);
    }
    Ok(())
}

/// Sends all control signals (first hourly value only) to Home Assistant.
pub async fn send_control_signals_to_ha(
    client: &Client,
    base_url: &str,
    api_key: &str,
    control_signals: &serde_json::Value,
) {
    let Some(arr) = control_signals.as_array() else {
        return;
    };

    for sig in arr {
        let name = sig.get("name").and_then(|v| v.as_str());
        let values = sig.get("signal").and_then(|v| v.as_array());
        if name.is_none() || values.is_none() {
            continue;
        }

        let entity_id = extract_entity_id(name.unwrap());
        if entity_id.is_empty() {
            continue;
        }

        if let Some(first_value) = values.unwrap().get(0).and_then(|v| v.as_f64()) {
            println!("[HA] Sending {} → {}", entity_id, first_value);
            let _ = send_control_signal(client, base_url, api_key, &entity_id, first_value).await;
        }
    }
}
