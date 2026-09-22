use crate::{config::AppConfig, logging, models::{ActivitySnapshot, ChatMessage, ObservationDecision, StoredObservation}};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::time::Duration;


const GOOGLE_FAST_TIMEOUT_SECS: u64 = 12;
const CLOUDFLARE_TIMEOUT_SECS: u64 = 18;
const GOOGLE_CIRCUIT_SECONDS: i64 = 10 * 60;
static GOOGLE_UNHEALTHY_UNTIL: AtomicI64 = AtomicI64::new(0);

fn cloudflare_configured(cfg: &AppConfig) -> bool {
    !cfg.cloudflare_account_id.trim().is_empty()
        && !cfg.cloudflare_api_token.trim().is_empty()
        && !cfg.cloudflare_model.trim().is_empty()
}

fn google_circuit_open() -> bool {
    chrono::Utc::now().timestamp() < GOOGLE_UNHEALTHY_UNTIL.load(Ordering::Relaxed)
}

fn mark_google_unhealthy(reason: &str) {
    let until = chrono::Utc::now().timestamp() + GOOGLE_CIRCUIT_SECONDS;
    GOOGLE_UNHEALTHY_UNTIL.store(until, Ordering::Relaxed);
    logging::warn(&format!(
        "Google AI temporarily bypassed for {} minutes after infrastructure error: {}",
        GOOGLE_CIRCUIT_SECONDS / 60,
        reason
    ));
}

fn clear_google_circuit() {
    GOOGLE_UNHEALTHY_UNTIL.store(0, Ordering::Relaxed);
}

fn is_google_infrastructure_error(error: &anyhow::Error) -> bool {
    // `anyhow::Error::to_string()` only contains the outermost context. Network
    // errors are wrapped with labels such as "Gemini request to ...", so inspect
    // the full error chain or a timeout can be misclassified as a normal model
    // error and never reach the Cloudflare fallback.
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("http 500")
        || text.contains("http 502")
        || text.contains("http 503")
        || text.contains("http 504")
        || text.contains("http 408")
        || text.contains("timed out")
        || text.contains("timeout")
        || text.contains("error sending request")
        || text.contains("connection reset")
        || text.contains("connection refused")
        || text.contains("connect error")
}

fn extract_cloudflare_chat_text(value: &Value) -> Option<String> {
    // Workers AI's OpenAI-compatible endpoint returns the standard
    // Chat Completions shape. Keep a couple of alternate pointers as a
    // defensive measure in case Cloudflare wraps the response.
    for pointer in [
        "/choices/0/message/content",
        "/result/choices/0/message/content",
        "/result/response",
        "/response",
    ] {
        let Some(content) = value.pointer(pointer) else { continue; };
        if let Some(text) = content.as_str() {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
        if let Some(parts) = content.as_array() {
            let text = parts.iter()
                .filter_map(|part| {
                    part.get("text").and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

async fn cloudflare_text(
    client: &Client,
    cfg: &AppConfig,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    if !cloudflare_configured(cfg) {
        return Err(anyhow!("Cloudflare Workers AI is not configured"));
    }
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/chat/completions",
        cfg.cloudflare_account_id.trim()
    );
    let body = json!({
        "model": cfg.cloudflare_model.trim(),
        "messages": [
            {"role":"system", "content": system},
            {"role":"user", "content": prompt}
        ],
        "temperature": temperature,
        "max_completion_tokens": max_tokens,
        "stream": false,
        // Gemma 4 has built-in thinking. For this app we want the token budget
        // spent on the actual answer/JSON, not hidden reasoning. With thinking
        // enabled, a small completion budget can legitimately end with empty
        // message.content even though the request itself succeeded.
        "chat_template_kwargs": {"enable_thinking": false},
        "options": {"rejectIfBusy": true}
    });
    let response = client.post(&url)
        .bearer_auth(cfg.cloudflare_api_token.trim())
        .timeout(Duration::from_secs(CLOUDFLARE_TIMEOUT_SECS))
        .json(&body)
        .send().await
        .context("Cloudflare Workers AI request")?;
    let status = response.status();
    let raw = response.text().await.context("read Cloudflare Workers AI response")?;
    if !status.is_success() {
        return Err(anyhow!("Cloudflare Workers AI: HTTP {status}: {}", raw.trim()));
    }
    let envelope: Value = serde_json::from_str(&raw).context("parse Cloudflare Workers AI response")?;
    if envelope.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(anyhow!("Cloudflare Workers AI returned failure: {}", raw.trim()));
    }
    extract_cloudflare_chat_text(&envelope).ok_or_else(|| {
        let finish_reason = envelope
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let content = envelope.pointer("/choices/0/message/content");
        let content_shape = match content {
            Some(Value::String(text)) => format!("string(len={})", text.chars().count()),
            Some(Value::Array(parts)) => format!("array(len={})", parts.len()),
            Some(Value::Null) => "null".to_string(),
            Some(other) => format!("{}", match other {
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::Object(_) => "object",
                _ => "other",
            }),
            None => "missing".to_string(),
        };
        let reasoning_len = envelope
            .pointer("/choices/0/message/reasoning_content")
            .and_then(Value::as_str)
            .map(|s| s.chars().count())
            .or_else(|| envelope
                .pointer("/choices/0/message/reasoning")
                .and_then(Value::as_str)
                .map(|s| s.chars().count()))
            .unwrap_or(0);
        anyhow!(
            "Cloudflare Workers AI returned no chat completion text (finish_reason={finish_reason}, content={content_shape}, reasoning_chars={reasoning_len})"
        )
    })
}

async fn observe_with_cloudflare(
    client: &Client,
    cfg: &AppConfig,
    prompt: &str,
) -> Result<ObservationDecision> {
    let text = cloudflare_text(
        client,
        cfg,
        &cfg.observation_system_prompt,
        prompt,
        420,
        0.25,
    ).await?;
    parse_observation_decision(&text).context("validate observation from Cloudflare Workers AI")
}

fn local_time_label_now() -> String {
    let now = Local::now();
    format!(
        "{} {} (24時間表記 {}, 12時間表記 {} {})",
        now.format("%Y-%m-%d"),
        now.format("%:z"),
        now.format("%H:%M:%S"),
        now.format("%p"),
        now.format("%I:%M:%S")
    )
}

fn localize_timestamp(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| {
            let local = dt.with_timezone(&Local);
            format!(
                "{} {} ({} {})",
                local.format("%Y-%m-%d %H:%M:%S"),
                local.format("%:z"),
                local.format("%p"),
                local.format("%I:%M:%S")
            )
        })
        .unwrap_or_else(|_| raw.to_string())
}

fn compact_messages_local(messages: &[ChatMessage]) -> Vec<Value> {
    messages.iter().map(|m| json!({
        "time_local": localize_timestamp(&m.created_at),
        "role": m.role,
        "text": m.text,
        "proactive": m.proactive,
    })).collect()
}


fn render_prompt_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut rendered = template.to_string();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{{{key}}}}}"), value);
    }
    rendered
}

fn parse_observation_decision(text: &str) -> Result<ObservationDecision> {
    let mut cleaned = text.trim();
    if let Some(rest) = cleaned.strip_prefix("```json") {
        cleaned = rest.trim();
    } else if let Some(rest) = cleaned.strip_prefix("```") {
        cleaned = rest.trim();
    }
    if let Some(rest) = cleaned.strip_suffix("```") {
        cleaned = rest.trim();
    }

    if let Ok(value) = serde_json::from_str::<ObservationDecision>(cleaned) {
        return Ok(value);
    }

    // Gemma can occasionally add a tiny preface/suffix even when explicitly
    // asked for JSON. Salvage exactly the outer JSON object, but never guess
    // missing fields: serde still validates the complete ObservationDecision.
    if let (Some(first), Some(last)) = (cleaned.find('{'), cleaned.rfind('}')) {
        if first < last {
            return serde_json::from_str::<ObservationDecision>(&cleaned[first..=last])
                .context("parse observation JSON object");
        }
    }
    Err(anyhow!("observation model returned invalid JSON"))
}

async fn observe_with_google_model(
    client: &Client,
    cfg: &AppConfig,
    model: &str,
    prompt: &str,
    schema: &Value,
) -> Result<ObservationDecision> {
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", model);

    let mut generation_config = json!({
        "temperature": 0.35,
        "maxOutputTokens": 420
    });
    if model.starts_with("gemma-4") {
        generation_config["thinkingConfig"] = json!({"thinkingLevel":"minimal"});
    } else {
        generation_config["responseMimeType"] = json!("application/json");
        generation_config["responseJsonSchema"] = schema.clone();
    }

    let body = json!({
        "systemInstruction": {
            "parts": [{"text": cfg.observation_system_prompt.as_str()}]
        },
        "contents": [{"role":"user","parts":[{"text":prompt}]}],
        "generationConfig": generation_config
    });

    let response = client.post(&url)
        .header("x-goog-api-key", cfg.gemini_api_key.trim())
        .timeout(Duration::from_secs(GOOGLE_FAST_TIMEOUT_SECS))
        .json(&body)
        .send().await
        .with_context(|| format!("observation request to {model}"))?;

    let status = response.status();
    let response_body = response.text().await.context("read observation response")?;
    if !status.is_success() {
        return Err(anyhow!("Google AI {model}: HTTP {status}: {}", response_body.trim()));
    }

    clear_google_circuit();
    let res: Value = serde_json::from_str(&response_body)
        .with_context(|| format!("parse {model} response envelope"))?;
    let text = extract_gemini_text(&res).ok_or_else(|| {
        let finish_reason = res.pointer("/candidates/0/finishReason").and_then(Value::as_str).unwrap_or("unknown");
        anyhow!("Google AI {model} returned no text (finishReason={finish_reason})")
    })?;
    parse_observation_decision(&text).with_context(|| format!("validate observation from {model}"))
}

pub async fn observe_and_decide(client: &Client, cfg: &AppConfig, snapshot: &ActivitySnapshot, recent: &[StoredObservation]) -> Result<ObservationDecision> {
    if cfg.gemini_api_key.trim().is_empty() { return Err(anyhow!("Gemini API key is not configured")); }
    let recent_compact: Vec<Value> = recent.iter().rev().take(6).rev().map(|o| json!({
        "time_local": localize_timestamp(&o.created_at),
        "activity": o.decision.activity,
        "working": o.decision.working,
        "focus_level": o.decision.focus_level,
        "summary": o.decision.summary,
        "mood": o.decision.mood,
        "spoke": o.decision.should_speak
    })).collect();

    let window_local = format!(
        "{} 〜 {}",
        localize_timestamp(&snapshot.start_time),
        localize_timestamp(&snapshot.end_time)
    );
    let now_local = local_time_label_now();
    let recent_json = serde_json::to_string(&recent_compact)?;
    let snapshot_json = serde_json::to_string(snapshot)?;
    let prompt = render_prompt_template(
        &cfg.observation_prompt_template,
        &[
            ("persona", &cfg.persona),
            ("now_local", &now_local),
            ("window_local", &window_local),
            ("recent_observations", &recent_json),
            ("snapshot", &snapshot_json),
        ],
    );

    let schema = json!({
      "type":"object",
      "properties":{
        "activity":{"type":"string"},
        "working":{"type":"boolean"},
        "focus_level":{"type":"number","minimum":0,"maximum":1},
        "summary":{"type":"string"},
        "active_app":{"type":"string"},
        "mood":{"type":"string"},
        "should_speak":{"type":"boolean"},
        "speak_reason":{"type":"string"},
        "intent":{"type":"string"},
        "notification_hint":{"type":"string"}
      },
      "required":["activity","working","focus_level","summary","active_app","mood","should_speak","speak_reason","intent","notification_hint"],
      "additionalProperties":false
    });

    let models = [
        cfg.observer_primary_model.as_str(),
        cfg.observer_fallback_model.as_str(),
        cfg.observer_last_fallback_model.as_str(),
    ];
    let mut errors = Vec::new();
    let mut cloudflare_tried = false;

    if google_circuit_open() && cloudflare_configured(cfg) {
        logging::info("Google AI cooldown active; using Cloudflare Workers AI for observation");
        cloudflare_tried = true;
        match observe_with_cloudflare(client, cfg, &prompt).await {
            Ok(decision) => return Ok(decision),
            Err(e) => {
                let message = format!("Cloudflare observation fallback failed: {e:#}");
                logging::warn(&message);
                errors.push(message);
            }
        }
    }

    for (index, model) in models.iter().enumerate() {
        match observe_with_google_model(client, cfg, model, &prompt, &schema).await {
            Ok(decision) => {
                if index > 0 {
                    logging::info(&format!("observation fallback succeeded with {model}"));
                }
                return Ok(decision);
            }
            Err(e) => {
                let message = format!("observation model {model} failed: {e:#}");
                errors.push(message.clone());
                let infrastructure = is_google_infrastructure_error(&e);
                let rate_limited = is_rate_limit_error(&e);
                if infrastructure {
                    mark_google_unhealthy(&message);
                }
                if (infrastructure || rate_limited) && !cloudflare_tried && cloudflare_configured(cfg) {
                    cloudflare_tried = true;
                    logging::warn("switching observation to Cloudflare Workers AI");
                    match observe_with_cloudflare(client, cfg, &prompt).await {
                        Ok(decision) => {
                            logging::info("observation fallback succeeded with Cloudflare Workers AI");
                            return Ok(decision);
                        }
                        Err(cf_err) => {
                            let cf_message = format!("Cloudflare observation fallback failed: {cf_err:#}");
                            logging::warn(&cf_message);
                            errors.push(cf_message);
                        }
                    }
                }
                if index + 1 < models.len() {
                    logging::warn(&format!("{message}; falling back to {}", models[index + 1]));
                } else {
                    logging::error(&message);
                }
            }
        }
    }

    if !cloudflare_tried && cloudflare_configured(cfg) {
        match observe_with_cloudflare(client, cfg, &prompt).await {
            Ok(decision) => {
                logging::info("observation final fallback succeeded with Cloudflare Workers AI");
                return Ok(decision);
            }
            Err(e) => errors.push(format!("Cloudflare observation fallback failed: {e:#}")),
        }
    }

    Err(anyhow!("all observation models failed: {}", errors.join(" | ")))
}

pub async fn render_proactive_message(client: &Client, cfg: &AppConfig, decision: &ObservationDecision, recent_messages: &[ChatMessage]) -> Result<String> {
    let messages = compact_messages_local(recent_messages);
    let now_local = local_time_label_now();
    let messages_json = serde_json::to_string(&messages)?;
    let prompt = render_prompt_template(
        &cfg.proactive_prompt_template,
        &[
            ("persona", &cfg.persona),
            ("now_local", &now_local),
            ("decision_summary", &decision.summary),
            ("decision_intent", &decision.intent),
            ("notification_hint", &decision.notification_hint),
            ("recent_messages", &messages_json),
        ],
    );
    chat_text_with_fallback(client, cfg, &prompt, 240, 0.72).await
}

pub async fn chat(client: &Client, cfg: &AppConfig, user_text: &str, messages: &[ChatMessage], observations: &[StoredObservation]) -> Result<String> {
    let obs: Vec<Value> = observations.iter().rev().take(4).rev().map(|o| json!({
        "time_local":localize_timestamp(&o.created_at),
        "summary":o.decision.summary,
        "working":o.decision.working,
        "phone":o.snapshot.phone
    })).collect();
    let messages_local = compact_messages_local(messages);
    let now_local = local_time_label_now();
    let observations_json = serde_json::to_string(&obs)?;
    let messages_json = serde_json::to_string(&messages_local)?;
    let prompt = render_prompt_template(
        &cfg.chat_prompt_template,
        &[
            ("persona", &cfg.persona),
            ("now_local", &now_local),
            ("user_name", &cfg.user_name),
            ("companion_name", &cfg.companion_name),
            ("observations", &observations_json),
            ("recent_messages", &messages_json),
            ("user_text", user_text),
        ],
    );
    chat_text_with_fallback(client, cfg, &prompt, 520, 0.72).await
}

pub async fn diary(client: &Client, cfg: &AppConfig, date: &str, observations: &[StoredObservation], messages: &[ChatMessage]) -> Result<String> {
    let obs: Vec<Value> = observations.iter().map(|o| json!({
        "time_local":localize_timestamp(&o.created_at),
        "apps":o.snapshot.foreground_apps,
        "keyboard_events":o.snapshot.keyboard_events,
        "input_events":o.snapshot.input_events_total,
        "working":o.decision.working,
        "focus":o.decision.focus_level,
        "summary":o.decision.summary,
        "mood":o.decision.mood,
        "phone":o.snapshot.phone
    })).collect();
    let messages_local = compact_messages_local(messages);
    let now_local = local_time_label_now();
    let observations_json = serde_json::to_string(&obs)?;
    let messages_json = serde_json::to_string(&messages_local)?;
    let prompt = render_prompt_template(
        &cfg.diary_prompt_template,
        &[
            ("persona", &cfg.persona),
            ("now_local", &now_local),
            ("date", date),
            ("observations", &observations_json),
            ("recent_messages", &messages_json),
        ],
    );
    diary_text_with_fallback(client, cfg, &prompt, 2200, 0.88).await
}

fn extract_gemini_text(res: &Value) -> Option<String> {
    let parts = res.pointer("/candidates/0/content/parts")?.as_array()?;

    // Gemini 3 can return multiple parts and attach thought metadata/signatures.
    // Prefer all non-thought text parts rather than assuming parts[0] is the answer.
    let answer = parts.iter().filter_map(|part| {
        if part.get("thought").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }
        part.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
    }).collect::<Vec<_>>().join("");
    if !answer.trim().is_empty() {
        return Some(answer.trim().to_string());
    }

    // Defensive fallback for unusual responses: use any non-empty text part.
    let fallback = parts.iter().filter_map(|part| {
        part.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
    }).collect::<Vec<_>>().join("");
    if fallback.trim().is_empty() { None } else { Some(fallback.trim().to_string()) }
}

fn is_rate_limit_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("HTTP 429")
        || text.contains("RESOURCE_EXHAUSTED")
        || text.to_ascii_lowercase().contains("rate limit")
}

async fn gemini_text_once(
    client: &Client,
    cfg: &AppConfig,
    model: &str,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    if cfg.gemini_api_key.trim().is_empty() {
        return Err(anyhow!("Gemini API key is not configured"));
    }
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", model);
    let body = json!({
        "contents": [{"role":"user","parts":[{"text":prompt}]}],
        "generationConfig": {
            "temperature": temperature,
            "maxOutputTokens": max_tokens
        }
    });
    let response = client.post(&url)
        .header("x-goog-api-key", cfg.gemini_api_key.trim())
        .timeout(Duration::from_secs(GOOGLE_FAST_TIMEOUT_SECS))
        .json(&body)
        .send().await
        .with_context(|| format!("Gemini request to {model}"))?;
    let status = response.status();
    let response_body = response.text().await.context("read Gemini response")?;
    if !status.is_success() {
        return Err(anyhow!("Gemini API {model}: HTTP {status}: {}", response_body.trim()));
    }
    clear_google_circuit();
    let res: Value = serde_json::from_str(&response_body).context("parse Gemini response JSON")?;
    if let Some(text) = extract_gemini_text(&res) {
        return Ok(text);
    }
    let finish_reason = res.pointer("/candidates/0/finishReason").and_then(Value::as_str).unwrap_or("unknown");
    let block_reason = res.pointer("/promptFeedback/blockReason").and_then(Value::as_str).unwrap_or("none");
    Err(anyhow!("Gemini {model} returned no text (finishReason={finish_reason}, blockReason={block_reason})"))
}

async fn chat_text_with_fallback(
    client: &Client,
    cfg: &AppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    // Fast/light model first for ordinary conversation. The larger Flash
    // models are only quota fallbacks; infrastructure failures jump straight
    // to Cloudflare instead of walking the Google list.
    const MODELS: [&str; 5] = [
        "gemini-3.5-flash-lite",
        "gemini-3.7-flash",
        "gemini-3.6-flash",
        "gemini-3.5-flash",
        "gemini-3.1-flash-lite",
    ];

    if google_circuit_open() && cloudflare_configured(cfg) {
        logging::info("Google AI cooldown active; using Cloudflare Workers AI for chat");
        match cloudflare_text(
            client, cfg,
            &cfg.cloudflare_chat_system_prompt,
            prompt, max_tokens, temperature,
        ).await {
            Ok(text) => return Ok(text),
            Err(e) => logging::warn(&format!("Cloudflare chat during Google cooldown failed: {e:#}; probing Google fallbacks")),
        }
    }

    let mut errors = Vec::new();
    let mut cloudflare_tried = false;
    for (index, model) in MODELS.iter().enumerate() {
        match gemini_text_once(client, cfg, model, prompt, max_tokens, temperature).await {
            Ok(text) => {
                if index > 0 { logging::info(&format!("chat fallback succeeded with {model}")); }
                return Ok(text);
            }
            Err(e) if is_rate_limit_error(&e) => {
                let message = format!("chat model {model} rate-limited: {e:#}");
                errors.push(message.clone());
                if index + 1 < MODELS.len() {
                    logging::warn(&format!("{message}; falling back to {}", MODELS[index + 1]));
                    continue;
                }
                logging::warn(&message);
            }
            Err(e) if is_google_infrastructure_error(&e) => {
                let message = format!("chat model {model} infrastructure failure: {e:#}");
                errors.push(message.clone());
                mark_google_unhealthy(&message);
                if cloudflare_configured(cfg) {
                    cloudflare_tried = true;
                    logging::warn("switching chat to Cloudflare Workers AI");
                    match cloudflare_text(
                        client, cfg,
                        &cfg.cloudflare_chat_system_prompt,
                        prompt, max_tokens, temperature,
                    ).await {
                        Ok(text) => {
                            logging::info("chat fallback succeeded with Cloudflare Workers AI");
                            return Ok(text);
                        }
                        Err(cf_err) => {
                            let cf_message = format!("Cloudflare chat fallback failed: {cf_err:#}");
                            logging::warn(&cf_message);
                            errors.push(cf_message);
                        }
                    }
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    if !cloudflare_tried && cloudflare_configured(cfg) {
        match cloudflare_text(
            client, cfg,
            &cfg.cloudflare_chat_system_prompt,
            prompt, max_tokens, temperature,
        ).await {
            Ok(text) => {
                logging::info("chat final fallback succeeded with Cloudflare Workers AI");
                return Ok(text);
            }
            Err(e) => errors.push(format!("Cloudflare chat fallback failed: {e:#}")),
        }
    }

    Err(anyhow!("all chat providers failed: {}", errors.join(" | ")))
}

async fn diary_text_with_fallback(
    client: &Client,
    cfg: &AppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    if google_circuit_open() && cloudflare_configured(cfg) {
        logging::info("Google AI cooldown active; using Cloudflare Workers AI for diary");
        return cloudflare_text(
            client, cfg,
            &cfg.cloudflare_diary_system_prompt,
            prompt, max_tokens, temperature,
        ).await;
    }
    match gemini_text_once(client, cfg, &cfg.diary_model, prompt, max_tokens, temperature).await {
        Ok(text) => Ok(text),
        Err(e) if (is_google_infrastructure_error(&e) || is_rate_limit_error(&e)) && cloudflare_configured(cfg) => {
            if is_google_infrastructure_error(&e) {
                mark_google_unhealthy(&format!("diary: {e:#}"));
            }
            logging::warn(&format!("diary Gemini failed ({e:#}); switching to Cloudflare Workers AI"));
            cloudflare_text(
                client, cfg,
                &cfg.cloudflare_diary_system_prompt,
                prompt, max_tokens, temperature,
            ).await
        }
        Err(e) => Err(e),
    }
}
