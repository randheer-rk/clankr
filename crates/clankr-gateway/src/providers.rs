//! Provider adapters. Each maps a provider-neutral request onto one HTTP call and maps the reply
//! (and errors) back. Credentials go in headers only and never appear in errors.
//!
//! | provider            | chat                              | embeddings                 | credential (the secret's value)        |
//! |---------------------|-----------------------------------|----------------------------|----------------------------------------|
//! | openai-compatible   | `/chat/completions`               | `/embeddings`              | API key (bearer)                       |
//! | anthropic           | `/v1/messages`                    | none                       | API key (`x-api-key`)                  |
//! | azure-openai        | `/openai/deployments/{m}/chat/…`  | `…/embeddings`             | API key (`api-key`); model = deployment |
//! | gemini              | `/v1beta/models/{m}:generateContent` | `:batchEmbedContents`   | API key (`x-goog-api-key`)             |
//! | vertex              | same body, under the project      | same                       | service-account JSON (OAuth, RS256)    |
//! | bedrock             | `/model/{m}/converse` (SigV4)     | `/model/{m}/invoke` (Titan) | `KEY_ID:SECRET[:SESSION]`             |

use clankr_types::{ApiError, ErrorCode};
use serde_json::{Value, json};

use crate::{GenerateRequest, Provider, ProviderConnectionSpec, Usage, aws};

pub(crate) struct Reply {
    pub text: String,
    pub stop_reason: Option<String>,
    pub usage: Usage,
}

pub(crate) struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    pub input_tokens: i64,
}

/// The credential, already resolved: a key, or for Vertex an OAuth access token.
pub(crate) type Key<'a> = Option<&'a str>;

pub(crate) async fn call(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: Key<'_>, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let key = key.unwrap_or_default();
    match conn.provider {
        Provider::Mock => Ok(mock(model, req)),
        Provider::Replay => replay(conn, req),
        Provider::OpenaiCompatible => openai(http, &format!("{}/chat/completions", endpoint(conn, "https://api.openai.com/v1")), ("authorization", &format!("Bearer {key}")), model, req, max_out).await,
        Provider::AzureOpenai => openai(http, &azure_url(conn, model, "chat/completions"), ("api-key", key), model, req, max_out).await,
        Provider::Anthropic => anthropic(http, conn, model, key, req, max_out).await,
        Provider::Gemini => gemini(http, &format!("{}/v1beta/models/{model}:generateContent", endpoint(conn, "https://generativelanguage.googleapis.com")), ("x-goog-api-key", key), req, max_out).await,
        Provider::Vertex => gemini(http, &format!("{}:generateContent", vertex_model(conn, model)), ("authorization", &format!("Bearer {key}")), req, max_out).await,
        Provider::Bedrock => bedrock(http, conn, model, key, req, max_out).await,
    }
}

pub(crate) async fn embed(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: Key<'_>, inputs: &[String]) -> Result<Embedded, ApiError> {
    let key = key.unwrap_or_default();
    match conn.provider {
        Provider::Mock => Ok(mock_embed(inputs)),
        Provider::Replay | Provider::Anthropic => Err(ApiError::new(ErrorCode::UnsupportedCapability, format!("{:?} has no embeddings endpoint", conn.provider))),
        Provider::OpenaiCompatible => openai_embed(http, &format!("{}/embeddings", endpoint(conn, "https://api.openai.com/v1")), ("authorization", &format!("Bearer {key}")), model, inputs).await,
        Provider::AzureOpenai => openai_embed(http, &azure_url(conn, model, "embeddings"), ("api-key", key), model, inputs).await,
        Provider::Gemini => gemini_embed(http, &format!("{}/v1beta/models/{model}:batchEmbedContents", endpoint(conn, "https://generativelanguage.googleapis.com")), ("x-goog-api-key", key), &format!("models/{model}"), inputs).await,
        Provider::Vertex => gemini_embed(http, &format!("{}:batchEmbedContents", vertex_model(conn, model)), ("authorization", &format!("Bearer {key}")), &format!("models/{model}"), inputs).await,
        Provider::Bedrock => bedrock_embed(http, conn, model, key, inputs).await,
    }
}

fn tokens(s: &str) -> i64 {
    (s.len() as i64 / 4).max(1)
}

fn mock(model: &str, req: &GenerateRequest) -> Reply {
    let last = req.messages.iter().rev().find(|m| m.role == "user").map_or("", |m| m.content.as_str());
    let text = format!("mock({model}): {last}");
    let input = req.messages.iter().map(|m| tokens(&m.content)).sum::<i64>() + req.system.as_deref().map_or(0, tokens);
    Reply { usage: Usage { input_tokens: input, output_tokens: tokens(&text) }, text, stop_reason: Some("end_turn".into()) }
}

/// Deterministic 8-dimensional vectors: equal texts embed equally, which is all tests need.
fn mock_embed(inputs: &[String]) -> Embedded {
    use sha2::Digest;
    let vectors = inputs
        .iter()
        .map(|s| sha2::Sha256::digest(s.as_bytes()).iter().take(8).map(|b| (*b as f32 - 128.0) / 128.0).collect())
        .collect();
    Embedded { vectors, input_tokens: inputs.iter().map(|s| tokens(s)).sum() }
}

fn replay(conn: &ProviderConnectionSpec, req: &GenerateRequest) -> Result<Reply, ApiError> {
    let hash = crate::prompt_sha256(req);
    let text: String = req.system.iter().chain(req.messages.iter().map(|m| &m.content)).cloned().collect::<Vec<_>>().join("\n");
    let e = conn
        .script
        .iter()
        .find(|e| e.prompt_sha256.as_deref() == Some(hash.as_str()))
        .or_else(|| conn.script.iter().find(|e| e.prompt_sha256.is_none() && e.contains.as_deref().is_some_and(|c| text.contains(c))))
        .ok_or_else(|| ApiError::new(ErrorCode::InvalidSpec, format!("replay: no recorded response for this prompt (promptSha256 {hash}); the agent's prompt changed or the trace is incomplete")))?;
    Ok(Reply {
        text: e.reply.clone(),
        stop_reason: e.stop_reason.clone().or_else(|| Some("end_turn".into())),
        usage: Usage { input_tokens: if e.input_tokens > 0 { e.input_tokens } else { tokens(&text) }, output_tokens: if e.output_tokens > 0 { e.output_tokens } else { tokens(&e.reply) } },
    })
}

fn endpoint(conn: &ProviderConnectionSpec, default: &str) -> String {
    let e = if conn.endpoint.is_empty() { default } else { &conn.endpoint };
    e.trim_end_matches('/').to_owned()
}

fn azure_url(conn: &ProviderConnectionSpec, deployment: &str, op: &str) -> String {
    format!("{}/openai/deployments/{deployment}/{op}?api-version=2024-10-21", endpoint(conn, ""))
}

fn vertex_model(conn: &ProviderConnectionSpec, model: &str) -> String {
    let base = endpoint(conn, &format!("https://{}-aiplatform.googleapis.com", conn.region));
    format!("{base}/v1/projects/{}/locations/{}/publishers/google/models/{model}", conn.project, conn.region)
}

async fn send(rb: reqwest::RequestBuilder, provider: &str) -> Result<Value, ApiError> {
    let resp = rb.send().await.map_err(|e| {
        if e.is_timeout() {
            // the provider may have done (and billed) the work; we can't know (PRV-08)
            ApiError::new(ErrorCode::OutcomeUnknown, format!("{provider}: request timed out"))
        } else {
            ApiError::new(ErrorCode::ProviderUnavailable, format!("{provider}: {}", e.without_url()))
        }
    })?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    // every provider puts a human-readable message somewhere near error.message
    let msg: String = body["error"]["message"].as_str().or(body["message"].as_str()).or(body["Message"].as_str()).unwrap_or("no error message").chars().take(300).collect();
    let code = match status.as_u16() {
        401 | 403 => ErrorCode::Forbidden,
        400 | 404 | 413 | 422 => ErrorCode::InvalidSpec,
        429 => ErrorCode::QuotaExceeded,
        _ => ErrorCode::ProviderUnavailable,
    };
    Err(ApiError::new(code, format!("{provider} returned {status}: {msg}")))
}

async fn openai(http: &reqwest::Client, url: &str, auth: (&str, &str), model: &str, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let mut messages: Vec<Value> = req.system.iter().map(|s| json!({"role": "system", "content": s})).collect();
    messages.extend(req.messages.iter().map(|m| json!({"role": m.role, "content": m.content})));
    let body = json!({"model": model, "messages": messages, "max_tokens": max_out});
    let r = send(http.post(url).header(auth.0, auth.1).json(&body), "openai-compatible").await?;
    let choice = &r["choices"][0];
    Ok(Reply {
        text: choice["message"]["content"].as_str().unwrap_or_default().to_owned(),
        stop_reason: choice["finish_reason"].as_str().map(str::to_owned),
        usage: Usage {
            input_tokens: r["usage"]["prompt_tokens"].as_i64().unwrap_or(0),
            output_tokens: r["usage"]["completion_tokens"].as_i64().unwrap_or(0),
        },
    })
}

async fn openai_embed(http: &reqwest::Client, url: &str, auth: (&str, &str), model: &str, inputs: &[String]) -> Result<Embedded, ApiError> {
    let r = send(http.post(url).header(auth.0, auth.1).json(&json!({"model": model, "input": inputs})), "openai-compatible").await?;
    let vectors = r["data"].as_array().into_iter().flatten().map(|d| d["embedding"].as_array().into_iter().flatten().filter_map(Value::as_f64).map(|f| f as f32).collect()).collect();
    Ok(Embedded { vectors, input_tokens: r["usage"]["prompt_tokens"].as_i64().unwrap_or_else(|| inputs.iter().map(|s| tokens(s)).sum()) })
}

/// Anthropic Messages API: `POST /v1/messages`, `x-api-key` + `anthropic-version` headers.
/// System text goes in the top-level `system` field, not in `messages`.
async fn anthropic(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: &str, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let mut system: Vec<&str> = req.system.iter().map(String::as_str).collect();
    system.extend(req.messages.iter().filter(|m| m.role == "system").map(|m| m.content.as_str()));
    let messages: Vec<Value> = req.messages.iter().filter(|m| m.role != "system").map(|m| json!({"role": m.role, "content": m.content})).collect();
    let mut body = json!({"model": model, "max_tokens": max_out, "messages": messages});
    if !system.is_empty() {
        body["system"] = system.join("\n\n").into();
    }
    let url = format!("{}/v1/messages", endpoint(conn, "https://api.anthropic.com"));
    let r = send(http.post(url).header("x-api-key", key).header("anthropic-version", "2023-06-01").json(&body), "anthropic").await?;
    // content is a list of blocks; concatenate the text ones. stop_reason may be "refusal".
    let text = r["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<String>();
    Ok(Reply {
        text,
        stop_reason: r["stop_reason"].as_str().map(str::to_owned),
        usage: Usage { input_tokens: r["usage"]["input_tokens"].as_i64().unwrap_or(0), output_tokens: r["usage"]["output_tokens"].as_i64().unwrap_or(0) },
    })
}

/// Gemini's `generateContent`, shared by the Gemini API (key) and Vertex (OAuth): roles are
/// `user`/`model`, system text is `systemInstruction`.
async fn gemini(http: &reqwest::Client, url: &str, auth: (&str, &str), req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let mut system: Vec<&str> = req.system.iter().map(String::as_str).collect();
    system.extend(req.messages.iter().filter(|m| m.role == "system").map(|m| m.content.as_str()));
    let contents: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| json!({"role": if m.role == "assistant" { "model" } else { "user" }, "parts": [{"text": m.content}]}))
        .collect();
    let mut body = json!({"contents": contents, "generationConfig": {"maxOutputTokens": max_out}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
    }
    let r = send(http.post(url).header(auth.0, auth.1).json(&body), "gemini").await?;
    let c = &r["candidates"][0];
    let text = c["content"]["parts"].as_array().into_iter().flatten().filter_map(|p| p["text"].as_str()).collect::<String>();
    Ok(Reply {
        text,
        stop_reason: c["finishReason"].as_str().map(|s| s.to_ascii_lowercase()),
        usage: Usage { input_tokens: r["usageMetadata"]["promptTokenCount"].as_i64().unwrap_or(0), output_tokens: r["usageMetadata"]["candidatesTokenCount"].as_i64().unwrap_or(0) },
    })
}

async fn gemini_embed(http: &reqwest::Client, url: &str, auth: (&str, &str), model: &str, inputs: &[String]) -> Result<Embedded, ApiError> {
    let requests: Vec<Value> = inputs.iter().map(|t| json!({"model": model, "content": {"parts": [{"text": t}]}})).collect();
    let r = send(http.post(url).header(auth.0, auth.1).json(&json!({"requests": requests})), "gemini").await?;
    let vectors = r["embeddings"].as_array().into_iter().flatten().map(|e| e["values"].as_array().into_iter().flatten().filter_map(Value::as_f64).map(|f| f as f32).collect()).collect();
    // no usage in the reply: charged by estimate
    Ok(Embedded { vectors, input_tokens: inputs.iter().map(|s| tokens(s)).sum() })
}

fn bedrock_creds(key: &str) -> Result<aws::Credentials, ApiError> {
    aws::Credentials::parse(key).ok_or_else(|| ApiError::new(ErrorCode::InvalidSpec, "bedrock: the secret must be ACCESS_KEY_ID:SECRET_ACCESS_KEY[:SESSION_TOKEN]"))
}

/// A SigV4-signed POST to the Bedrock runtime.
async fn bedrock_post(http: &reqwest::Client, conn: &ProviderConnectionSpec, creds: &aws::Credentials, path: &str, body: &Value) -> Result<Value, ApiError> {
    let base = endpoint(conn, &format!("https://bedrock-runtime.{}.amazonaws.com", conn.region));
    let url = reqwest::Url::parse(&format!("{base}{path}")).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("bedrock endpoint: {e}")))?;
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let bytes = body.to_string().into_bytes();
    // services other than S3 canonicalize an already-encoded path by encoding it once more
    let canonical: String = path.split('/').map(aws::encode_segment).collect::<Vec<_>>().join("/");
    let req = aws::Request { method: "POST", host: &host, path: &canonical, query: "", headers: vec![("content-type".into(), "application/json".into())], body: &bytes };
    let mut rb = http.post(url.clone()).header("content-type", "application/json").body(bytes.clone());
    for (k, v) in aws::sign(&req, creds, &conn.region, "bedrock", &aws::now()) {
        rb = rb.header(k, v);
    }
    send(rb, "bedrock").await
}

/// Bedrock's Converse API: one shape for every model family it hosts.
async fn bedrock(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: &str, req: &GenerateRequest, max_out: u32) -> Result<Reply, ApiError> {
    let creds = bedrock_creds(key)?;
    let mut system: Vec<&str> = req.system.iter().map(String::as_str).collect();
    system.extend(req.messages.iter().filter(|m| m.role == "system").map(|m| m.content.as_str()));
    let messages: Vec<Value> = req.messages.iter().filter(|m| m.role != "system").map(|m| json!({"role": m.role, "content": [{"text": m.content}]})).collect();
    let mut body = json!({"messages": messages, "inferenceConfig": {"maxTokens": max_out}});
    if !system.is_empty() {
        body["system"] = json!(system.iter().map(|s| json!({"text": s})).collect::<Vec<_>>());
    }
    let r = bedrock_post(http, conn, &creds, &format!("/model/{}/converse", aws::encode_segment(model)), &body).await?;
    let text = r["output"]["message"]["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<String>();
    Ok(Reply {
        text,
        stop_reason: r["stopReason"].as_str().map(str::to_owned),
        usage: Usage { input_tokens: r["usage"]["inputTokens"].as_i64().unwrap_or(0), output_tokens: r["usage"]["outputTokens"].as_i64().unwrap_or(0) },
    })
}

/// Titan embeddings (`amazon.titan-embed-*`): one `invoke` per input.
// ponytail: Titan only; Cohere embed on Bedrock has a different body (texts, input_type) when someone needs it
async fn bedrock_embed(http: &reqwest::Client, conn: &ProviderConnectionSpec, model: &str, key: &str, inputs: &[String]) -> Result<Embedded, ApiError> {
    let creds = bedrock_creds(key)?;
    let mut out = Embedded { vectors: vec![], input_tokens: 0 };
    for text in inputs {
        let r = bedrock_post(http, conn, &creds, &format!("/model/{}/invoke", aws::encode_segment(model)), &json!({"inputText": text})).await?;
        out.vectors.push(r["embedding"].as_array().into_iter().flatten().filter_map(Value::as_f64).map(|f| f as f32).collect());
        out.input_tokens += r["inputTextTokenCount"].as_i64().unwrap_or_else(|| tokens(text));
    }
    Ok(out)
}

/// Vertex: a service account's JSON becomes a short-lived access token (JWT bearer grant).
pub(crate) async fn vertex_token(http: &reqwest::Client, sa_json: &str) -> Result<(String, std::time::Duration), ApiError> {
    let sa: Value = serde_json::from_str(sa_json).map_err(|_| ApiError::new(ErrorCode::InvalidSpec, "vertex: the secret must be a service account JSON key"))?;
    let (email, pem, token_uri) = (sa["client_email"].as_str().unwrap_or_default(), sa["private_key"].as_str().unwrap_or_default(), sa["token_uri"].as_str().unwrap_or("https://oauth2.googleapis.com/token"));
    if email.is_empty() || pem.is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidSpec, "vertex: service account JSON needs client_email and private_key"));
    }
    let now = chrono::Utc::now().timestamp();
    let claims = json!({"iss": email, "scope": "https://www.googleapis.com/auth/cloud-platform", "aud": token_uri, "iat": now, "exp": now + 3600});
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(|e| ApiError::new(ErrorCode::InvalidSpec, format!("vertex: private_key: {e}")))?;
    let jwt = jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &claims, &key).map_err(|e| ApiError::new(ErrorCode::ProviderUnavailable, format!("vertex: sign: {e}")))?;
    let form = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}");
    let r = send(http.post(token_uri).header("content-type", "application/x-www-form-urlencoded").body(form), "vertex oauth").await?;
    let token = r["access_token"].as_str().ok_or_else(|| ApiError::new(ErrorCode::ProviderUnavailable, "vertex oauth: no access_token"))?.to_owned();
    let ttl = r["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
    Ok((token, std::time::Duration::from_secs(ttl)))
}
