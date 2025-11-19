// src/main.rs

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

mod home_assistant;
use home_assistant::*; // extract_entity_id, send_control_signal

use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::{HeaderValue, Method},
    routing::post,
    Json, Router,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;
use tokio::{
    net::TcpListener,
    sync::Mutex,
    task::JoinHandle,
    time::{interval, sleep},
};

const OPERATIONS: &str = include_str!("operations.graphql");

#[derive(Clone)]
struct AppState {
    client: Client,
    graphql_url: String,
    hourly_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    ha_base_url: String,
    ha_api_key: Arc<Mutex<Option<String>>>,
}

// Body for /set-ha-api-key
#[derive(Deserialize)]
struct HaApiKeyRequest {
    api_key: String,
}

// ---------------- GraphQL helpers ----------------

async fn graphql_request(
    client: &Client,
    graphql_url: &str,
    query_document: &str,
    operation_name: Option<&str>,
    variables: Value,
) -> Result<Value> {
    let mut payload = json!({
        "query": query_document,
        "variables": variables,
    });

    if let Some(name) = operation_name {
        payload["operationName"] = json!(name);
    }

    let resp = client
        .post(graphql_url)
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;

    Ok(resp.json::<Value>().await?)
}

async fn save_model(client: &Client, graphql_url: &str) -> Result<()> {
    let resp = graphql_request(client, graphql_url, OPERATIONS, Some("SaveModel"), json!({})).await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!("GraphQL errors in SaveModel: {errors:#?}"));
    }

    let message = resp
        .pointer("/data/saveModel/message")
        .and_then(|v| v.as_str());

    if let Some(msg) = message {
        return Err(anyhow!("saveModel failed: {msg}"));
    }

    println!("[saveModel] OK");
    Ok(())
}

async fn start_optimization(client: &Client, graphql_url: &str) -> Result<i64> {
    let resp =
        graphql_request(client, graphql_url, OPERATIONS, Some("StartOptimization"), json!({}))
            .await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!("GraphQL errors in StartOptimization: {errors:#?}"));
    }

    let job_id = resp
        .pointer("/data/startOptimization")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("startOptimization did not return a valid jobId"))?;

    println!("[startOptimization] jobId = {job_id}");
    Ok(job_id)
}

async fn wait_for_job(
    client: &Client,
    graphql_url: &str,
    job_id: i64,
) -> Result<(String, Option<String>)> {
    let poll_interval = Duration::from_secs(2);
    let max_attempts = 300; // ~10 minutes

    for attempt in 1..=max_attempts {
        let resp = graphql_request(
            client,
            graphql_url,
            OPERATIONS,
            Some("JobStatus"),
            json!({ "jobId": job_id }),
        )
        .await?;

        if let Some(errors) = resp.get("errors") {
            return Err(anyhow!(
                "GraphQL errors in JobStatus for job {job_id}: {errors:#?}"
            ));
        }

        let state = resp
            .pointer("/data/jobStatus/state")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();

        let message = resp
            .pointer("/data/jobStatus/message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        println!(
            "[jobStatus] attempt {attempt}, jobId={job_id}, state={state}, message={:?}",
            message
        );

        if state == "FINISHED" || state == "FAILED" {
            return Ok((state, message));
        }

        sleep(poll_interval).await;
    }

    Err(anyhow!(
        "Timeout waiting for job {job_id} to finish (reached {max_attempts} attempts)"
    ))
}

async fn get_optimization_outcome(
    client: &Client,
    graphql_url: &str,
    job_id: i64,
) -> Result<Option<Value>> {
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("GetJobOutcome"),
        json!({ "jobId": job_id }),
    )
    .await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!(
            "GraphQL errors in GetJobOutcome for job {job_id}: {errors:#?}"
        ));
    }

    let outcome = resp
        .pointer("/data/jobOutcome")
        .ok_or_else(|| anyhow!("jobOutcome missing in response"))?;

    let typename = outcome
        .pointer("/__typename")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");

    if typename != "OptimizationOutcome" {
        println!(
            "[jobOutcome] jobId={job_id}, unexpected __typename={typename}, ignoring"
        );
        return Ok(None);
    }

    let control_signals = outcome.pointer("/controlSignals").cloned();
    Ok(control_signals)
}

/// One full optimisation cycle + send signals to HA.
async fn run_one_cycle(state: &AppState) -> Result<()> {
    println!("=== Starting optimisation cycle ===");

    let client = &state.client;
    let graphql_url = &state.graphql_url;

    save_model(client, graphql_url).await?;

    let job_id = start_optimization(client, graphql_url).await?;

    let (final_state, message) = wait_for_job(client, graphql_url, job_id).await?;
    if final_state != "FINISHED" {
        return Err(anyhow!(
            "Job {job_id} ended in state {final_state}, message={message:?}"
        ));
    }

    let control_signals = get_optimization_outcome(client, graphql_url, job_id).await?;

    if let Some(cs) = control_signals {
        println!(
            "[jobOutcome] jobId={job_id}, received {} control signals",
            cs.as_array().map(|a| a.len()).unwrap_or(0)
        );

        // ----- Send to Home Assistant -----
        let api_key_opt = {
            let guard = state.ha_api_key.lock().await;
            guard.clone()
        };

        if let Some(api_key) = api_key_opt {
            if let Some(arr) = cs.as_array() {
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
                        println!("[HA] Sending {first_value} to {entity_id}");
                        if let Err(e) = send_control_signal(
                            &state.client,
                            &state.ha_base_url,
                            &api_key,
                            &entity_id,
                            first_value,
                        )
                        .await
                        {
                            eprintln!("[HA] error sending to {}: {}", entity_id, e);
                        }
                    }
                }
            }
        } else {
            println!("[HA] No API key set; skipping sending control signals");
        }
        // -----------------------------------
    } else {
        println!("[jobOutcome] jobId={job_id}, no OptimizationOutcome/controlSignals");
    }

    println!("=== Optimisation cycle complete ===");
    Ok(())
}

// ---------------- Hourly loop + HTTP handlers ----------------

async fn hourly_optimization_loop(state: Arc<AppState>) {
    if let Err(e) = run_one_cycle(&state).await {
        eprintln!("[hourly] first optimisation failed: {e:?}");
    }

    let mut ticker = interval(Duration::from_secs(60 * 60));
    loop {
        ticker.tick().await;
        if let Err(e) = run_one_cycle(&state).await {
            eprintln!("[hourly] optimisation failed: {e:?}");
        }
    }
}

// POST /start-hourly-optimization
async fn start_hourly_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut guard = state.hourly_task.lock().await;

    if guard.is_some() {
        // Already running
        return Json(json!({ "status": "already_running" }));
    }

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        hourly_optimization_loop(state_clone).await;
    });

    *guard = Some(handle);
    Json(json!({ "status": "started" }))
}

// POST /stop-hourly-optimization
async fn stop_hourly_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut guard = state.hourly_task.lock().await;

    if let Some(handle) = guard.take() {
        handle.abort();
        return Json(json!({ "status": "stopped" }));
    }

    Json(json!({ "status": "not_running" }))
}

// POST /set-ha-api-key  { "api_key": "..." }
async fn set_ha_api_key_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<HaApiKeyRequest>,
) -> Json<Value> {
    let mut guard = state.ha_api_key.lock().await;
    *guard = Some(payload.api_key);
    println!("[HA] API key updated");
    Json(json!({ "status": "ok" }))
}

// ---------------- main ----------------

#[tokio::main]
async fn main() -> Result<()> {
    let graphql_url = std::env::var("HERTTA_GRAPHQL_URL")
        .unwrap_or_else(|_| "http://localhost:3030/graphql".to_string());

    // Home Assistant base URL (env or default)
    let ha_base_url = std::env::var("HASS_BASE_URL")
        .unwrap_or_else(|_| "http://192.168.1.110:8123/api".to_string());

    let client = Client::new();

    let state = Arc::new(AppState {
        client,
        graphql_url,
        hourly_task: Arc::new(Mutex::new(None)),
        ha_base_url,
        ha_api_key: Arc::new(Mutex::new(None)),
    });

    let cors = CorsLayer::new()
        .allow_origin(
            "http://localhost:3000"
                .parse::<HeaderValue>()
                .expect("invalid CORS origin"),
        )
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any);

    let app = Router::new()
        .route("/start-hourly-optimization", post(start_hourly_handler))
        .route("/stop-hourly-optimization", post(stop_hourly_handler))
        .route("/set-ha-api-key", post(set_ha_api_key_handler))
        .with_state(state)
        .layer(cors);

    let addr: SocketAddr = "0.0.0.0:4001".parse().unwrap();
    println!("Hass backend listening on {addr}");

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow!(e))
}
