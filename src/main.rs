// src/main.rs

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

mod home_assistant;
use home_assistant::*; // extract_entity_id, send_control_signal

use anyhow::{anyhow, Result};
use axum::{
    extract::{State, Path},
    http::{Method, StatusCode},
    routing::{get, post},
    Json, Router, response::IntoResponse
};
use reqwest::Client;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tokio::{
    net::TcpListener,
    sync::Mutex,
    task::JoinHandle,
    time::{interval, sleep},
};
use std::path::Path as StdPath;

const OPERATIONS: &str = include_str!("operations.graphql");
const CONTROL_SIGNAL_STEP: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
struct AppState {
    client: Client,
    graphql_url: String,
    optimization_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    optimization_cycle: Arc<Mutex<()>>,
    control_signal_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    weather_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    price_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    prices_latest: Arc<Mutex<Option<Value>>>,
    prices_error_latest: Arc<Mutex<Option<String>>>,
    control_signals_latest: Arc<Mutex<Option<Value>>>,
    ha_base_url: String,
    ha_token: String,
    weather_latest: Arc<Mutex<Option<Value>>>,
    weather_error_latest: Arc<Mutex<Option<String>>>,
}

async fn spa_fallback() -> impl IntoResponse {
    axum::response::Html(
        std::fs::read_to_string("/web/index.html")
            .unwrap_or_else(|_| "<h1>UI not found</h1><p>/web/index.html missing</p>".to_string()),
    )
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

async fn graphql_proxy_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    match state.client.post(&state.graphql_url).json(&payload).send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.json::<Value>().await.unwrap_or_else(|_| {
                json!({
                    "errors": [{ "message": "Hertta returned a non-JSON response" }]
                })
            });
            (status, Json(body))
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "errors": [{ "message": err.to_string() }]
            })),
        ),
    }
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

async fn set_optimization_timeline(client: &Client, graphql_url: &str) -> Result<()> {
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("SetOptimizationTimeline"),
        json!({}),
    )
    .await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!(
            "GraphQL errors in SetOptimizationTimeline: {errors:#?}"
        ));
    }

    let validation_errors = resp
        .pointer("/data/updateTimeLine/errors")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow!("SetOptimizationTimeline returned an invalid response"))?;

    if !validation_errors.is_empty() {
        return Err(anyhow!(
            "SetOptimizationTimeline validation errors: {validation_errors:#?}"
        ));
    }

    println!("[timeline] 12-hour horizon with 15-minute steps");
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
    max_attempts: u32,
) -> Result<(String, Option<String>)> {
    let poll_interval = Duration::from_secs(2);

    println!("[jobStatus] waiting for jobId={job_id}, maxAttempts={max_attempts}");

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

        if state == "FINISHED" || state == "FAILED" {
            println!(
                "[jobStatus] jobId={job_id}, state={state}, attempts={attempt}, message={:?}",
                message
            );
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

    Ok(Some(outcome.clone()))
}

async fn schedule_control_signals(state: &AppState, signals: &Value) {
    let Some(arr) = signals.as_array() else {
        return;
    };

    let mut device_signals = std::collections::HashMap::<String, Vec<f64>>::new();
    for sig in arr {
        let Some(signal_name) = sig.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(values) = sig.get("signal").and_then(|value| value.as_array()) else {
            continue;
        };
        if !signal_name.contains("_electricitygrid_") {
            continue;
        }

        let entity_id = extract_entity_id(signal_name);
        if entity_id.is_empty() {
            continue;
        }

        let target = device_signals.entry(entity_id).or_default();
        if target.len() < values.len() {
            target.resize(values.len(), 0.0);
        }
        for (index, value) in values.iter().enumerate() {
            if let Some(value) = value.as_f64() {
                target[index] += value;
            }
        }
    }

    let mut task_guard = state.control_signal_task.lock().await;
    if let Some(handle) = task_guard.take() {
        handle.abort();
    }
    if device_signals.is_empty() {
        return;
    }

    let client = state.client.clone();
    let base_url = state.ha_base_url.clone();
    let api_key = state.ha_token.clone();
    *task_guard = Some(tokio::spawn(async move {
        let step_count = device_signals
            .values()
            .map(|values| values.len())
            .max()
            .unwrap_or(0);

        for step in 0..step_count {
            for (entity_id, values) in &device_signals {
                let value = values.get(step).copied().unwrap_or(0.0);
                println!(
                    "[HA schedule] step={step}, raw={value}, binary={}, entity={entity_id}",
                    if value > 1e-6 { "ON" } else { "OFF" }
                );
                if let Err(error) =
                    send_control_signal(&client, &base_url, &api_key, entity_id, value).await
                {
                    eprintln!("[HA schedule] error sending to {entity_id}: {error}");
                }
            }

            if step + 1 < step_count {
                sleep(CONTROL_SIGNAL_STEP).await;
            }
        }
    }));
}

/// One full optimisation cycle + send signals to HA.
async fn run_one_cycle(state: &AppState) -> Result<()> {
    let _cycle_guard = state.optimization_cycle.lock().await;
    println!("=== Starting optimisation cycle ===");

    let client = &state.client;
    let graphql_url = &state.graphql_url;

    set_optimization_timeline(client, graphql_url).await?;
    save_model(client, graphql_url).await?;

    let job_id = start_optimization(client, graphql_url).await?;

    // The first Julia/Predicer run on slower hardware can take considerably
    // longer than forecast jobs, especially while package code is warming up.
    let (final_state, message) = wait_for_job(client, graphql_url, job_id, 1800).await?;
    if final_state != "FINISHED" {
        return Err(anyhow!(
            "Job {job_id} ended in state {final_state}, message={message:?}"
        ));
    }

    let optimization_outcome = get_optimization_outcome(client, graphql_url, job_id).await?;

    if let Some(outcome) = optimization_outcome {
        let cs = outcome
            .pointer("/controlSignals")
            .cloned()
            .unwrap_or_else(|| json!([]));
        println!(
            "[jobOutcome] jobId={job_id}, received {} control signals",
            cs.as_array().map(|a| a.len()).unwrap_or(0)
        );

        if cs.as_array().is_some_and(|signals| !signals.is_empty()) {
            let mut guard = state.control_signals_latest.lock().await;
            *guard = Some(outcome);
        }

        schedule_control_signals(state, &cs).await;
    } else {
        println!("[jobOutcome] jobId={job_id}, no OptimizationOutcome/controlSignals");
    }

    println!("=== Optimisation cycle complete ===");
    Ok(())
}

// ---------------- Hourly loop + HTTP handlers ----------------

// GET /ha-state/:entity_id  -> fetch state of any Home Assistant entity
async fn ha_state_handler(
    State(state): State<Arc<AppState>>,
    Path(entity_id): Path<String>,
) -> Json<Value> {
    let api_key = &state.ha_token;
    let base_url = &state.ha_base_url;

    match fetch_entity_state(&state.client, base_url, api_key, &entity_id).await {
        Ok(state_json) => Json(json!({
            "status": "ok",
            "entity_id": entity_id,
            "data": state_json
        })),
        Err(e) => {
            eprintln!("[HA] error fetching state for {}: {}", entity_id, e);
            Json(json!({
                "status": "error",
                "entity_id": entity_id,
                "message": e.to_string()
            }))
        }
    }
}

async fn ha_api_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let url = format!("{}/", state.ha_base_url.trim_end_matches('/'));

    match state
        .client
        .get(url)
        .bearer_auth(&state.ha_token)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.json::<Value>().await.unwrap_or_else(|_| {
                json!({
                    "message": "Home Assistant returned a non-JSON response"
                })
            });

            (status, Json(body))
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "message": err.to_string()
            })),
        ),
    }
}

async fn ha_states_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let url = format!("{}/states", state.ha_base_url.trim_end_matches('/'));

    match state
        .client
        .get(url)
        .bearer_auth(&state.ha_token)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.json::<Value>().await.unwrap_or_else(|_| {
                json!({
                    "message": "Home Assistant returned a non-JSON response"
                })
            });

            (status, Json(body))
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "message": err.to_string()
            })),
        ),
    }
}

async fn set_ha_api_key_handler() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "message": "Use HASS_TOKEN when starting the container to set the Home Assistant token."
    }))
}

async fn hertta_health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let health_url = state.graphql_url.trim_end_matches("/graphql").to_string() + "/health";

    match state.client.get(health_url).send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_else(|_| String::new());

            (
                status,
                Json(json!({
                    "status": if status.is_success() { "ok" } else { "error" },
                    "body": text,
                })),
            )
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "status": "error",
                "message": err.to_string(),
            })),
        ),
    }
}

async fn hourly_optimization_loop(state: Arc<AppState>) {
    if let Err(e) = run_one_cycle(&state).await {
        eprintln!("[hourly] first optimisation failed: {e:?}");
    }

    let mut ticker = interval(Duration::from_secs(60 * 60));
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Err(e) = run_one_cycle(&state).await {
            eprintln!("[hourly] optimisation failed: {e:?}");
        }
    }
}

// POST /start-hourly-optimization
async fn start_hourly_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut guard = state.optimization_task.lock().await;

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
    let mut guard = state.optimization_task.lock().await;

    if let Some(handle) = guard.take() {
        handle.abort();
        if let Some(control_handle) = state.control_signal_task.lock().await.take() {
            control_handle.abort();
        }
        return Json(json!({ "status": "stopped" }));
    }

    Json(json!({ "status": "not_running" }))
}

// POST /refresh-optimization
async fn refresh_optimization_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let guard = state.optimization_task.lock().await;
    if guard.is_none() {
        return Json(json!({ "status": "not_running" }));
    }
    drop(guard);

    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = run_one_cycle(&state_clone).await {
            eprintln!("[refresh] optimisation failed: {e:?}");
        }
    });

    Json(json!({ "status": "refresh_started" }))
}

async fn get_control_signals_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let guard = state.control_signals_latest.lock().await;
    if let Some(control_signals) = &*guard {
        Json(json!({
            "status": "ok",
            "data": control_signals,
        }))
    } else {
        Json(json!({
            "status": "no_data",
        }))
    }
}

async fn start_weather_fetch(client: &Client, graphql_url: &str) -> Result<i64> {
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("StartWeatherForecastFetch"),
        json!({}),
    )
    .await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!("GraphQL errors in StartWeatherForecastFetch: {errors:#?}"));
    }

    let job_id = resp
        .pointer("/data/startWeatherForecastFetch")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("startWeatherForecastFetch did not return a valid jobId"))?;

    println!("[startWeatherFetch] jobId = {job_id}");
    Ok(job_id)
}

async fn get_weather_outcome(
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

    if typename != "WeatherForecastOutcome" {
        println!(
            "[weather jobOutcome] jobId={job_id}, unexpected __typename={typename}, ignoring"
        );
        return Ok(None);
    }

    Ok(Some(outcome.clone()))
}

async fn run_weather_cycle(state: &AppState) -> Result<()> {
    println!("=== Starting weather fetch cycle ===");

    let client = &state.client;
    let graphql_url = &state.graphql_url;


    let job_id = start_weather_fetch(client, graphql_url).await?;

    let (final_state, message) = wait_for_job(client, graphql_url, job_id, 300).await?;
    if final_state != "FINISHED" {
        return Err(anyhow!(
            "Weather job {job_id} ended in state {final_state}, message={message:?}"
        ));
    }

    // 3) Read outcome
    if let Some(outcome) = get_weather_outcome(client, graphql_url, job_id).await? {
        println!("[weatherOutcome] jobId={job_id}, updating latest weather");

        // Just store the raw WeatherForecastOutcome JSON; frontend will shape it
        let mut guard = state.weather_latest.lock().await;
        *guard = Some(outcome);
        *state.weather_error_latest.lock().await = None;
    } else {
        println!("[weatherOutcome] jobId={job_id}, no WeatherForecastOutcome");
    }

    println!("=== Weather fetch cycle complete ===");
    Ok(())
}

async fn hourly_weather_loop(state: Arc<AppState>) {
    loop {
        if let Err(e) = run_weather_cycle(&state).await {
            eprintln!("[weather hourly] weather fetch failed: {e:?}");
            *state.weather_error_latest.lock().await = Some(e.to_string());
        }

        let has_weather_data = state.weather_latest.lock().await.is_some();
        let delay = if has_weather_data {
            Duration::from_secs(60 * 60)
        } else {
            Duration::from_secs(30)
        };
        tokio::time::sleep(delay).await;
    }
}

async fn start_weather_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut guard = state.weather_task.lock().await;

    if guard.is_some() {
        return Json(json!({ "status": "already_running" }));
    }

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        hourly_weather_loop(state_clone).await;
    });

    *guard = Some(handle);
    Json(json!({ "status": "started" }))
}


async fn get_weather_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let guard = state.weather_latest.lock().await;
    if let Some(outcome) = &*guard {
        // outcome is the WeatherForecastOutcome object:
        // { "__typename": "WeatherForecastOutcome", "time": [...], "temperature": [...] }
        Json(json!({
            "status": "ok",
            "data": outcome,
        }))
    } else {
        let error = state.weather_error_latest.lock().await.clone();
        match error {
            Some(message) => Json(json!({ "status": "error", "message": message })),
            None => Json(json!({ "status": "no_data" })),
        }
    }
}

async fn start_price_fetch(client: &Client, graphql_url: &str) -> Result<i64> {
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("StartElectricityPriceFetch"),
        json!({}),
    )
    .await?;

    if let Some(errors) = resp.get("errors") {
        return Err(anyhow!(
            "GraphQL errors in StartElectricityPriceFetch: {errors:#?}"
        ));
    }

    let job_id = resp
        .pointer("/data/startElectricityPriceFetch")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("startElectricityPriceFetch did not return a valid jobId"))?;

    println!("[startPriceFetch] jobId = {job_id}");
    Ok(job_id)
}

async fn get_price_outcome(
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
            "GraphQL errors in GetJobOutcome for price job {job_id}: {errors:#?}"
        ));
    }

    let outcome = resp
        .pointer("/data/jobOutcome")
        .ok_or_else(|| anyhow!("jobOutcome missing in response"))?;

    let typename = outcome
        .pointer("/__typename")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN");

    if typename != "ElectricityPriceOutcome" {
        println!(
            "[price jobOutcome] jobId={job_id}, unexpected __typename={typename}, ignoring"
        );
        return Ok(None);
    }

    Ok(Some(outcome.clone()))
}

async fn run_price_cycle(state: &AppState) -> Result<()> {
    println!("=== Starting electricity price fetch cycle ===");

    let client = &state.client;
    let graphql_url = &state.graphql_url;

    // 1) Start electricity price job
    let job_id = start_price_fetch(client, graphql_url).await?;

    // 2) Wait until FINISHED
    let (final_state, message) = wait_for_job(client, graphql_url, job_id, 300).await?;
    if final_state != "FINISHED" {
        return Err(anyhow!(
            "Electricity price job {job_id} ended in state {final_state}, message={message:?}"
        ));
    }

    // 3) Read outcome
    if let Some(outcome) = get_price_outcome(client, graphql_url, job_id).await? {
        println!("[priceOutcome] jobId={job_id}, updating latest prices");

        // Store the raw ElectricityPriceOutcome JSON
        let mut guard = state.prices_latest.lock().await;
        *guard = Some(outcome);
        *state.prices_error_latest.lock().await = None;
    } else {
        println!("[priceOutcome] jobId={job_id}, no ElectricityPriceOutcome");
    }

    println!("=== Electricity price fetch cycle complete ===");
    Ok(())
}

async fn hourly_price_loop(state: Arc<AppState>) {
    loop {
        if let Err(e) = run_price_cycle(&state).await {
            eprintln!("[price hourly] price fetch failed: {e:?}");
            *state.prices_error_latest.lock().await = Some(e.to_string());
        }

        let has_price_data = state.prices_latest.lock().await.is_some();
        let delay = if has_price_data {
            Duration::from_secs(60 * 60)
        } else {
            Duration::from_secs(30)
        };
        tokio::time::sleep(delay).await;
    }
}

// POST /start-prices
async fn start_price_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut guard = state.price_task.lock().await;

    if guard.is_some() {
        return Json(json!({ "status": "already_running" }));
    }

    let state_clone = state.clone();
    let handle = tokio::spawn(async move {
        hourly_price_loop(state_clone).await;
    });

    *guard = Some(handle);
    Json(json!({ "status": "started" }))
}

// POST /prices
async fn get_prices_handler(State(state): State<Arc<AppState>>) -> Json<Value> {
    let guard = state.prices_latest.lock().await;
    if let Some(outcome) = &*guard {
        // outcome is the ElectricityPriceOutcome object:
        // { "__typename": "ElectricityPriceOutcome", "time": [...], "price": [...] }
        Json(json!({
            "status": "ok",
            "data": outcome,
        }))
    } else {
        let error = state.prices_error_latest.lock().await.clone();
        match error {
            Some(message) => Json(json!({ "status": "error", "message": message })),
            None => Json(json!({ "status": "no_data" })),
        }
    }
}



// ---------------- main ----------------

#[tokio::main]
async fn main() -> Result<()> {
    let graphql_url = std::env::var("HERTTA_GRAPHQL_URL")
        .unwrap_or_else(|_| "http://localhost:3030/graphql".to_string());

    let ha_base_url = std::env::var("HASS_BASE_URL")
        .unwrap_or_else(|_| "http://supervisor/core/api".to_string());

    let ha_token = std::env::var("HASS_TOKEN")
        .map_err(|_| anyhow!("HASS_TOKEN not set (export it from run.sh)"))?;

    let client = Client::new();

    let state = Arc::new(AppState {
        client,
        graphql_url,
        optimization_task: Arc::new(Mutex::new(None)),
        optimization_cycle: Arc::new(Mutex::new(())),
        control_signal_task: Arc::new(Mutex::new(None)),
        weather_task: Arc::new(Mutex::new(None)),
        price_task: Arc::new(Mutex::new(None)),
        prices_latest: Arc::new(Mutex::new(None)),
        prices_error_latest: Arc::new(Mutex::new(None)),
        control_signals_latest: Arc::new(Mutex::new(None)),
        ha_base_url,
        ha_token,
        weather_latest: Arc::new(Mutex::new(None)),
        weather_error_latest: Arc::new(Mutex::new(None)),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let mut app = Router::new()
        .route("/graphql", post(graphql_proxy_handler))
        .route("/start-hourly-optimization", post(start_hourly_handler))
        .route("/stop-hourly-optimization", post(stop_hourly_handler))
        .route("/refresh-optimization", post(refresh_optimization_handler))
        .route("/control-signals", get(get_control_signals_handler))
        .route("/weather", post(get_weather_handler))
        .route("/start-weather", post(start_weather_handler))
        .route("/prices", post(get_prices_handler))
        .route("/start-prices", post(start_price_handler))
        .route("/ha-api", get(ha_api_handler))
        .route("/ha-state/{entity_id}", get(ha_state_handler))
        .route("/ha-states", get(ha_states_handler))
        .route("/set-ha-api-key", post(set_ha_api_key_handler))
        .route("/hertta-health", get(hertta_health_handler))
        .with_state(state)
        .layer(cors);

    // Serve CRA frontend from /web (copied by Dockerfile)
    let web_dir = "/web";
    if StdPath::new(web_dir).exists() {
        app = app.fallback_service(
            ServeDir::new(web_dir).fallback(axum::routing::get(spa_fallback)),
        );
        println!("[UI] Serving frontend from {web_dir}");
    } else {
        println!("[UI] {web_dir} not found; UI will not be served");
    }

    let addr: SocketAddr = "0.0.0.0:4001".parse().unwrap();
    println!("Hass backend listening on {addr}");

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow!(e))
}

