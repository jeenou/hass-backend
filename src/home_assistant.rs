use std::collections::HashSet;

use reqwest::Client;

/// Domains that are allowed to be controlled.
static VALID_DOMAINS: &[&str] = &["switch", "light", "climate", "number", "fan", "cover"];

/// Extract a Home Assistant `entity_id` from a raw control signal name.  This
/// mirrors the heuristic used in the JavaScript `extractEntityId` function.
pub fn extract_entity_id(name: &str) -> String {
    if !name.contains('.') {
        return String::new();
    }
    // Prefer simple case: if the name contains `_electricitygrid`, cut before it.
    let marker = "_electricitygrid";
    if let Some(idx) = name.find(marker) {
        let entity_id = &name[..idx];
        let domain = entity_id.split('.').next().unwrap_or("");
        if VALID_DOMAINS.contains(&domain) {
            return entity_id.to_string();
        }
        return String::new();
    }
    // General case: accumulate parts until the domain appears twice
    let parts: Vec<&str> = name.split('_').collect();
    let first_dot = name.find('.').unwrap_or(0);
    let domain = &name[..first_dot];
    let mut entity_parts: Vec<&str> = Vec::new();
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

/// Send a single control signal to Home Assistant.  Depending on the domain,
/// this will call `turn_on`, `turn_off` or `set_temperature`.
pub async fn send_control_signal(
    client: &Client,
    base_url: &str,
    api_key: &str,
    entity_id: &str,
    value: f64,
) -> Result<(), reqwest::Error> {
    let domain = entity_id
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();
    // Determine the service and payload
    let (service, mut payload) = match domain.as_str() {
        "switch" | "light" => {
            let service = if value > 0.0 { "turn_on" } else { "turn_off" };
            (service.to_string(), serde_json::json!({ "entity_id": entity_id }))
        }
        "climate" => {
            let mut p = serde_json::json!({ "entity_id": entity_id });
            p["temperature"] = serde_json::json!(value);
            ("set_temperature".to_string(), p)
        }
        _ => {
            let service = if value > 0.0 { "turn_on" } else { "turn_off" };
            (service.to_string(), serde_json::json!({ "entity_id": entity_id }))
        }
    };
    // Compose the URL and send the request
    let url = format!("{}/services/{}/{}", base_url.trim_end_matches('/'), domain, service);
    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await?;
    if let Err(e) = resp.error_for_status_ref() {
        eprintln!("Error sending control signal to {}: {}", entity_id, e);
    }
    Ok(())
}
