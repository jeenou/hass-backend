// src/main.rs

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::time::interval;

const OPERATIONS: &str = include_str!("operations.graphql");

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
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("SaveModel"),
        json!({}),
    )
    .await?;

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
    let resp = graphql_request(
        client,
        graphql_url,
        OPERATIONS,
        Some("StartOptimization"),
        json!({}),
    )
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

        tokio::time::sleep(poll_interval).await;
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

async fn run_one_cycle(client: &Client, graphql_url: &str) -> Result<()> {
    println!("=== Starting optimisation cycle ===");

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

    } else {
        println!("[jobOutcome] jobId={job_id}, no OptimizationOutcome/controlSignals");
    }

    println!("=== Optimisation cycle complete ===");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {

    let graphql_url = std::env::var("HERTTA_GRAPHQL_URL")
        .unwrap_or_else(|_| "http://localhost:3030/graphql".to_string());

    let client = Client::new();

    if let Err(e) = run_one_cycle(&client, &graphql_url).await {
        eprintln!("Initial optimisation cycle failed: {e:?}");
    }

    let mut ticker = interval(Duration::from_secs(60 * 60));

    loop {
        ticker.tick().await;

        if let Err(e) = run_one_cycle(&client, &graphql_url).await {
            eprintln!("Hourly optimisation cycle failed: {e:?}");
        }
    }
}
