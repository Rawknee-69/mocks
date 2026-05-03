/// Context window compactor for the conversation history.
///
/// Nemotron Omni has a 256K token context window. At ~200 tokens per exchange
/// the risk is very low, but this compactor fires at 80% capacity (~205K tokens)
/// to prevent gradual hallucination creep as history grows.
///
/// When the threshold is exceeded:
/// - The last 2 exchanges are kept verbatim (recency matters most).
/// - All older exchanges are summarised in a single call to the LLM and replaced
///   by a compact `[summary]` exchange so history length stays bounded.
use crate::api::Provider;
use reqwest::Client;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// Nemotron Omni context window in tokens.
const CONTEXT_WINDOW_TOKENS: usize = 256_000;
/// Fire compaction at 80% of the context window.
const COMPACTION_THRESHOLD: usize = (CONTEXT_WINDOW_TOKENS as f64 * 0.80) as usize;
/// Number of recent exchanges to preserve verbatim.
const RECENCY_KEEP: usize = 2;

/// Rough char-to-token heuristic (4 chars ≈ 1 token).
fn estimate_tokens(text: &str) -> usize {
    (text.len() + 3) / 4
}

fn estimate_history_tokens(history: &VecDeque<(String, String)>) -> usize {
    history
        .iter()
        .map(|(user, assistant)| estimate_tokens(user) + estimate_tokens(assistant))
        .sum()
}

/// Checks whether the history exceeds the compaction threshold and, if so,
/// summarises older exchanges and replaces them with a single summary pair.
///
/// This is an async operation (it may call the LLM to summarise). Pass the
/// same `rate_limiter` used for the main pipeline to respect the RPM cap.
pub async fn maybe_compact(
    history: &mut VecDeque<(String, String)>,
    http_client: &Client,
    provider: &Provider,
    rate_limiter: Arc<Semaphore>,
) {
    let estimated = estimate_history_tokens(history);
    if estimated <= COMPACTION_THRESHOLD {
        return;
    }

    info!(
        "context compaction triggered: ~{} tokens (threshold {})",
        estimated, COMPACTION_THRESHOLD
    );

    if history.len() <= RECENCY_KEEP {
        // Nothing old enough to compact — just trim oldest entry and move on
        history.pop_front();
        return;
    }

    let split_at = history.len() - RECENCY_KEEP;
    let old_exchanges: Vec<(String, String)> = history.drain(..split_at).collect();
    // `history` now contains only the last RECENCY_KEEP exchanges

    let summary = summarise_exchanges(&old_exchanges, http_client, provider, rate_limiter).await;

    // Prepend the summary as a synthetic exchange so the LLM sees the context
    // as a single coherent system note rather than raw history fragments
    history.push_front((
        "[conversation summary]".into(),
        summary,
    ));

    info!(
        "compaction complete: history now has {} exchanges (~{} tokens)",
        history.len(),
        estimate_history_tokens(history),
    );
}

/// Calls the LLM to produce a dense factual summary of the given exchanges.
/// Returns a fallback string on any error so the pipeline is never blocked.
async fn summarise_exchanges(
    exchanges: &[(String, String)],
    http_client: &Client,
    provider: &Provider,
    rate_limiter: Arc<Semaphore>,
) -> String {
    let mut conversation_text = String::new();
    for (user, assistant) in exchanges {
        conversation_text.push_str("User: ");
        conversation_text.push_str(user);
        conversation_text.push('\n');
        conversation_text.push_str("Assistant: ");
        conversation_text.push_str(assistant);
        conversation_text.push('\n');
    }

    let prompt = format!(
        "Summarize these conversation exchanges in 3-5 sentences as a memory note for a \
         voice assistant. Be factual and dense. Do not include meta-commentary.\n\n{conversation_text}"
    );

    let _permit = match rate_limiter.acquire().await {
        Ok(p) => p,
        Err(_) => {
            warn!("context compactor: rate limiter closed, using fallback summary");
            return format!("[{} older exchanges omitted]", exchanges.len());
        }
    };

    match provider {
        Provider::OpenRouter { api_key, model } => {
            summarise_openrouter(http_client, api_key, model, &prompt, exchanges.len()).await
        }
        Provider::Gemini { api_key, model } => {
            summarise_gemini(http_client, api_key, model, &prompt, exchanges.len()).await
        }
    }
}

async fn summarise_openrouter(
    http_client: &Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    exchange_count: usize,
) -> String {
    let api_url = "https://openrouter.ai/api/v1/chat/completions";
    let request_body = json!({
        "model": model,
        "max_tokens": 256,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    let response = match http_client
        .post(api_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            warn!("context compactor: summarisation request failed: {err}");
            return format!("[{exchange_count} older exchanges omitted due to error]");
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        warn!("context compactor: summarisation API error {status}");
        return format!("[{exchange_count} older exchanges omitted]");
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            warn!("context compactor: failed to parse summarisation response: {err}");
            return format!("[{exchange_count} older exchanges omitted]");
        }
    };

    body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("[summary unavailable]")
        .trim()
        .to_string()
}

async fn summarise_gemini(
    http_client: &Client,
    api_key: &str,
    model: &str,
    prompt: &str,
    exchange_count: usize,
) -> String {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={api_key}"
    );
    let request_body = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": prompt }]
        }],
        "generationConfig": { "maxOutputTokens": 256 }
    });

    let response = match http_client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            warn!("context compactor (Gemini): summarisation request failed: {err}");
            return format!("[{exchange_count} older exchanges omitted due to error]");
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        warn!("context compactor (Gemini): summarisation API error {status}");
        return format!("[{exchange_count} older exchanges omitted]");
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(err) => {
            warn!("context compactor (Gemini): failed to parse summarisation response: {err}");
            return format!("[{exchange_count} older exchanges omitted]");
        }
    };

    body["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("[summary unavailable]")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_sanity() {
        // 400 chars → ~100 tokens
        let text = "a".repeat(400);
        assert_eq!(estimate_tokens(&text), 100);
    }

    #[test]
    fn history_below_threshold_is_not_flagged() {
        // 10 exchanges × 200 chars each ≈ 500 tokens — well below 205K
        let mut history: VecDeque<(String, String)> = VecDeque::new();
        for _ in 0..10 {
            history.push_back(("user message".into(), "assistant response".into()));
        }
        let tokens = estimate_history_tokens(&history);
        assert!(tokens < COMPACTION_THRESHOLD);
    }
}
