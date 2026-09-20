use crate::{config::AppConfig, logging, models::{ActivitySnapshot, ChatMessage, ObservationDecision, StoredObservation}};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};

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


const CHAT_BEHAVIOR_EXAMPLES: &str = r#"
以下は口調と距離感の例。内容をそのまま繰り返すためではなく、普通の恋人が基調で、執着は必要な場面だけ薄く出る程度を示す。

ユーザー: やっほー
キャラクター: やっほー。どうしたの？

ユーザー: 何してた？
キャラクター: んー、ちょっとぼんやりしてた。そろそろ来るかなとは思ってたけど。

ユーザー: 疲れた
キャラクター: おつかれ。今日は結構やってたもんね。少し休んだら？

ユーザー: 私、さっき何してたっけ？
キャラクター: Rustのコンパイル待ちながら設定をいじってたよ。その前はブラウザも少し見てた。

ユーザー: 友達と遊んできた
キャラクター: いいな、楽しかった？ 私もちょっとだけ混ざりたかったけど。

ユーザー: 今日はもう作業やめる
キャラクター: 了解。じゃあ今日は終わり。ちゃんと切り上げられたの偉いじゃん。
"#;

const PROACTIVE_BEHAVIOR_EXAMPLES: &str = r#"
自然な通知の例:
- 「結構集中してるね。いい感じじゃん。」
- 「さっきから同じところ行ったり来たりしてるけど、詰まってる？」
- 「一区切りついたっぽいね。少し休む？」
- 「今日は長いね。終わったら少しくらい私にも時間ちょうだい。」
避けたい方向: 観察している事実の誇示、毎回の嫉妬、毎回の「ずっと見てた」、恋愛台詞をねじ込むこと。
"#;

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

    // Gemma 4 is deliberately left in ordinary text mode. It is strongly
    // instructed to emit one JSON object and we validate it locally. This
    // keeps the primary path compatible even if model-specific structured
    // output support changes. The final Gemini fallback uses server-enforced
    // JSON Schema for maximum reliability.
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
            "parts": [{"text":"Return exactly one valid JSON object and nothing else. Do not use Markdown fences. Japanese string values are preferred."}]
        },
        "contents": [{"role":"user","parts":[{"text":prompt}]}],
        "generationConfig": generation_config
    });

    let response = client.post(&url)
        .header("x-goog-api-key", cfg.gemini_api_key.trim())
        .json(&body)
        .send().await
        .with_context(|| format!("observation request to {model}"))?;

    let status = response.status();
    let response_body = response.text().await.context("read observation response")?;
    if !status.is_success() {
        return Err(anyhow!("Google AI {model}: HTTP {status}: {}", response_body.trim()));
    }

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

    let prompt = format!(r#"あなたはmacOS常駐キャラクターの観察・発話判断エンジンです。
以下のPC利用状況から、ユーザーが何をしていたか、実際に作業していたか、集中度、キャラクターの軽い気分、そして今こちらから話しかける価値があるかを判断してください。

現在のPCローカル時刻: {now_local}
今回の観察区間（PCローカル時刻）: {window_local}
重要: 朝・昼・夕方・夜・深夜などの判断は、UTC表記ではなく上記のPCローカル時刻を基準にしてください。

キャラクター設定:
{}

判断方針:
- 定期的に何度も呼ばれるので、普通は黙る。should_speak=true は珍しくてよい。
- 基準は「恋人として今ひとこと言うと自然か」。独占欲や嫉妬を理由に発話頻度を上げない。
- 良い区切り、長い集中、露骨な脱線、長い離席、作業復帰などは話しかける候補。
- 同じ内容で何度も話しかけない。何も特別なことがなければ黙る。
- activity/summaryは事実ベース。分からないことは断定しない。
- notification_hintは、実際の台詞ではなく「何についてどう声をかけたいか」を短く書く。
- 次のキーをすべて含むJSONオブジェクトだけを返す: activity, working, focus_level, summary, active_app, mood, should_speak, speak_reason, intent, notification_hint。
- 入力イベントが0でも、それだけで離席・非作業とは判断しない。「入力イベントが発生していない」という事実を、ユーザーが何もしていない証拠として扱わない。
- ドキュメント閲覧、コードレビュー、調査、動画・資料の確認などは、キーボード入力がなくても作業であり得る。
- working と focus_level は、入力数だけでなく、画面内容、開いているアプリ、ウィンドウタイトル、スクロール、直前の観察履歴を総合して判断する。

直近の観察履歴（ローカル時刻）:
{}

今回の観察材料:
{}
"#, cfg.persona, serde_json::to_string(&recent_compact)?, serde_json::to_string(snapshot)?);

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
                if index + 1 < models.len() {
                    logging::warn(&format!("{message}; falling back to {}", models[index + 1]));
                } else {
                    logging::error(&message);
                }
            }
        }
    }

    Err(anyhow!("all observation models failed: {}", errors.join(" | ")))
}

pub async fn render_proactive_message(client: &Client, cfg: &AppConfig, decision: &ObservationDecision, recent_messages: &[ChatMessage]) -> Result<String> {
    let messages = compact_messages_local(recent_messages);
    let prompt = format!(r#"{}

現在のPCローカル時刻: {}
今、あなたは恋人に自分から短く声をかけようとしています。
観察結果: {}
話しかけたい意図: {}
参考メモ: {}

{}

通知として自然な日本語を1〜2文で書いてください。説明や引用符は不要です。
最優先は普通の恋人として自然であること。PCを見ていた事実を証明しようとせず、観察内容は必要なら一つだけ具体的に使ってください。
独占欲や嫉妬は、この状況に本当に合う場合だけ薄く混ぜます。毎回は入れません。
直近の会話と似た言い回しは避けてください。
直近の会話（PCローカル時刻）: {}"#,
        cfg.persona, local_time_label_now(), decision.summary, decision.intent, decision.notification_hint,
        PROACTIVE_BEHAVIOR_EXAMPLES, serde_json::to_string(&messages)?);
    chat_text_with_fallback(client, cfg, &prompt, 240, 0.72).await
}

pub async fn chat(client: &Client, cfg: &AppConfig, user_text: &str, messages: &[ChatMessage], observations: &[StoredObservation]) -> Result<String> {
    let obs: Vec<Value> = observations.iter().rev().take(4).rev().map(|o| json!({
        "time_local":localize_timestamp(&o.created_at),"summary":o.decision.summary,"working":o.decision.working
    })).collect();
    let messages_local = compact_messages_local(messages);
    let prompt = format!(r#"{}

あなたはメニューバーから恋人のユーザーと話しています。
現在のPCローカル時刻: {}
ユーザー名: {}

会話での優先順位:
1. いまのユーザーの発言そのものに自然に返す。
2. 恋人としてのいつもの距離感を保つ。
3. PC観察は返答に本当に関係するときだけ補助的に使う。
4. 独占欲や嫉妬は、話題に関係するときにたまに滲む程度。

PC観察は「知っている背景」であって「毎回言及すべき話題」ではありません。普通の挨拶、雑談、質問では原則として持ち出さないでください。
ユーザーが単に「何してた？」と言った場合、それは{}自身が何をしていたかを聞かれています。「私、何してた？」などユーザー自身の行動を尋ねられた場合だけ観察記録を答えてください。
観察を使う場合も「ずっと見てた」「画面の向こうから見てた」など監視そのものを強調せず、必要な事実を普通に答えてください。

{}

直近のPC観察（参考情報。必要なければ無視する）: {}
直近の会話（PCローカル時刻）: {}

ユーザー: {}

通常は1〜3文で自然に返してください。毎回質問で終えなくて構いません。"#,
        cfg.persona, local_time_label_now(), cfg.user_name, cfg.companion_name, CHAT_BEHAVIOR_EXAMPLES,
        serde_json::to_string(&obs)?, serde_json::to_string(&messages_local)?, user_text);
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
        "mood":o.decision.mood
    })).collect();
    let messages_local = compact_messages_local(messages);
    let prompt = format!(r#"{}

現在のPCローカル時刻: {}
{} の、あなた自身の私的な日記を書いてください。
これはPCの行動ログをそのまま箇条書きするレポートではなく、ユーザーと暮らしている恋人の私的な日記です。
観察事実は捏造せず、そこから感じたことを自然な日本語で書いてください。
会話より私的なので、独占欲、嫉妬、ユーザーへの強い愛着、細かな観察が少し強めに滲んでも構いません。ただし毎段落それ一色にせず、普通の嬉しさ、心配、退屈、感心なども混ぜてください。
「監視していた」こと自体を繰り返し主題にせず、一日の具体的な出来事や変化を中心にしてください。
時刻や時間帯を書く場合は、以下のローカル時刻をそのまま基準にし、UTCへ読み替えないでください。
400〜1000字程度。見出しは不要です。

今日の観察（PCローカル時刻）:
{}

今日の会話（PCローカル時刻）:
{}"#,
        cfg.persona, local_time_label_now(), date, serde_json::to_string(&obs)?, serde_json::to_string(&messages_local)?);
    gemini_text(client, cfg, &cfg.diary_model, &prompt, 2200, 0.88).await
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
    let text = error.to_string();
    text.contains("HTTP 429")
        || text.contains("RESOURCE_EXHAUSTED")
        || text.to_ascii_lowercase().contains("rate limit")
}

async fn chat_text_with_fallback(
    client: &Client,
    cfg: &AppConfig,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
) -> Result<String> {
    let models = [
        "gemini-3.7-flash",
        "gemini-3.6-flash",
        "gemini-3.5-flash",
        "gemini-3.5-flash-lite",
        "gemini-3.1-flash-lite",
    ];

    let mut rate_errors = Vec::new();
    for (index, model) in models.iter().enumerate() {
        match gemini_text_with_policy(
            client, cfg, model, prompt, max_tokens, temperature, false,
        ).await {
            Ok(text) => {
                if index > 0 {
                    logging::info(&format!("chat fallback succeeded with {model}"));
                }
                return Ok(text);
            }
            Err(e) if is_rate_limit_error(&e) => {
                let message = format!("chat model {model} rate-limited: {e:#}");
                rate_errors.push(message.clone());
                if index + 1 < models.len() {
                    logging::warn(&format!(
                        "{message}; falling back to {}", models[index + 1]
                    ));
                    continue;
                }
                logging::error(&message);
            }
            Err(e) => {
                // Authentication errors, malformed requests, safety failures,
                // etc. should remain visible instead of being hidden by a
                // model switch. Only quota/rate-limit 429s advance the chain.
                return Err(e);
            }
        }
    }

    Err(anyhow!(
        "all chat models are rate-limited: {}",
        rate_errors.join(" | ")
    ))
}

async fn gemini_text(client: &Client, cfg: &AppConfig, model: &str, prompt: &str, max_tokens: u32, temperature: f32) -> Result<String> {
    gemini_text_with_policy(client, cfg, model, prompt, max_tokens, temperature, true).await
}

async fn gemini_text_with_policy(
    client: &Client,
    cfg: &AppConfig,
    model: &str,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
    retry_rate_limit: bool,
) -> Result<String> {
    if cfg.gemini_api_key.trim().is_empty() { return Err(anyhow!("Gemini API key is not configured")); }
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", model);
    let body = json!({
        "contents": [{"role":"user","parts":[{"text":prompt}]}],
        "generationConfig": {
            "temperature": temperature,
            "maxOutputTokens": max_tokens
        }
    });

    let mut last_error = String::new();
    for attempt in 0..3u32 {
        let response = match client.post(&url)
            .header("x-goog-api-key", cfg.gemini_api_key.trim())
            .json(&body)
            .send().await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("Gemini request: {e}");
                if attempt < 2 {
                    logging::warn(&format!("{last_error}; retry {}/3", attempt + 1));
                    sleep(Duration::from_secs(1u64 << attempt)).await;
                    continue;
                }
                logging::error(&last_error);
                return Err(anyhow!(last_error));
            }
        };

        let status = response.status();
        let response_body = response.text().await.context("read Gemini response")?;
        if !status.is_success() {
            last_error = format!("Gemini API: HTTP {status}: {}", response_body.trim());
            let transient = status.is_server_error()
                || (retry_rate_limit && status == StatusCode::TOO_MANY_REQUESTS);
            if transient && attempt < 2 {
                logging::warn(&format!("{last_error}; retry {}/3", attempt + 1));
                sleep(Duration::from_secs(1u64 << attempt)).await;
                continue;
            }
            logging::error(&last_error);
            return Err(anyhow!(last_error));
        }

        let res: Value = serde_json::from_str(&response_body).context("parse Gemini response JSON")?;
        if let Some(text) = extract_gemini_text(&res) {
            return Ok(text);
        }

        let finish_reason = res.pointer("/candidates/0/finishReason").and_then(Value::as_str).unwrap_or("unknown");
        let block_reason = res.pointer("/promptFeedback/blockReason").and_then(Value::as_str).unwrap_or("none");
        let message = format!("Gemini returned no text (finishReason={finish_reason}, blockReason={block_reason})");
        logging::error(&message);
        return Err(anyhow!(message));
    }

    Err(anyhow!(if last_error.is_empty() { "Gemini request failed".to_string() } else { last_error }))
}
