// src/main.rs

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

mod home_assistant;
use home_assistant::*; // extract_entity_id, send_control_signal

use anyhow::{anyhow, Result};
use axum::{
    extract::{State, Path},
    http::Method,
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

#[derive(Clone)]
struct AppState {
    client: Client,
    graphql_url: String,
    optimization_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    weather_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    price_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    prices_latest: Arc<Mutex<Option<Value>>>,
    ha_base_url: String,
    ha_token: String,
    weather_latest: Arc<Mutex<Option<Value>>>,
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
        // New AppState: token is always available (read from env in main, passed into AppState)
        let api_key = &state.ha_token;
        let base_url = &state.ha_base_url;

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
                        base_url,
                        api_key,
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
        // -----------------------------------
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
        return Json(json!({ "status": "stopped" }));
    }

    Json(json!({ "status": "not_running" }))
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

    let (final_state, message) = wait_for_job(client, graphql_url, job_id).await?;
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
    } else {
        println!("[weatherOutcome] jobId={job_id}, no WeatherForecastOutcome");
    }

    println!("=== Weather fetch cycle complete ===");
    Ok(())
}

async fn hourly_weather_loop(state: Arc<AppState>) {
    // First fetch right away
    if let Err(e) = run_weather_cycle(&state).await {
        eprintln!("[weather hourly] first weather fetch failed: {e:?}");
    }

    let mut ticker = interval(Duration::from_secs(60 * 60));
    loop {
        ticker.tick().await;
        if let Err(e) = run_weather_cycle(&state).await {
            eprintln!("[weather hourly] weather fetch failed: {e:?}");
        }
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
        Json(json!({
            "status": "no_data",
        }))
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
    let (final_state, message) = wait_for_job(client, graphql_url, job_id).await?;
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
    } else {
        println!("[priceOutcome] jobId={job_id}, no ElectricityPriceOutcome");
    }

    println!("=== Electricity price fetch cycle complete ===");
    Ok(())
}

async fn hourly_price_loop(state: Arc<AppState>) {
    // First fetch right away
    if let Err(e) = run_price_cycle(&state).await {
        eprintln!("[price hourly] first price fetch failed: {e:?}");
    }

    let mut ticker = interval(Duration::from_secs(60 * 60));
    loop {
        ticker.tick().await;
        if let Err(e) = run_price_cycle(&state).await {
            eprintln!("[price hourly] price fetch failed: {e:?}");
        }
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
        Json(json!({
            "status": "no_data",
        }))
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
        weather_task: Arc::new(Mutex::new(None)),
        price_task: Arc::new(Mutex::new(None)),
        prices_latest: Arc::new(Mutex::new(None)),
        ha_base_url,
        ha_token,
        weather_latest: Arc::new(Mutex::new(None)),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let mut app = Router::new()
        .route("/start-hourly-optimization", post(start_hourly_handler))
        .route("/stop-hourly-optimization", post(stop_hourly_handler))
        .route("/weather", post(get_weather_handler))
        .route("/start-weather", post(start_weather_handler))
        .route("/prices", post(get_prices_handler))
        .route("/start-prices", post(start_price_handler))
        .route("/ha-state/{entity_id}", get(ha_state_handler))
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

