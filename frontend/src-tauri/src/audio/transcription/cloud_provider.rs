// audio/transcription/cloud_provider.rs
//
// Cloud transcription provider for OpenAI-compatible /audio/transcriptions
// endpoints (OpenAI and OpenRouter). Audio is sent as 16 kHz mono PCM16 WAV
// via multipart/form-data; no local model is ever loaded.

use super::provider::{TranscriptionError, TranscriptionProvider, TranscriptResult};
use async_trait::async_trait;
use log::debug;

/// OpenAI audio transcription endpoint
pub const OPENAI_TRANSCRIPTION_URL: &str =
    "https://api.openai.com/v1/audio/transcriptions";

/// OpenRouter audio transcription endpoint (OpenAI-compatible)
pub const OPENROUTER_TRANSCRIPTION_URL: &str =
    "https://openrouter.ai/api/v1/audio/transcriptions";

/// Default models advertised per cloud provider
pub const DEFAULT_OPENAI_TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";
pub const DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL: &str = "openai/whisper-1";

/// Minimum audio length (100 ms at 16 kHz) before a cloud request is worthwhile
const MIN_CLOUD_SAMPLES: usize = 1600;

/// Upper bound for a single cloud request (OpenRouter limits uploads to 25 MB)
const MAX_CLOUD_SAMPLES: usize = 5 * 60 * 16000; // 5 minutes at 16 kHz

/// Request timeout for a single transcription call. Segments are up to 5
/// minutes of audio and remote processing (notably via a proxy like
/// OpenRouter) can legitimately take longer than a minute.
const REQUEST_TIMEOUT_SECS: u64 = 300;

/// Attempts per transcription call: one retry on transient transport
/// failures (connection reset, TLS error, timeout). Without it, a single
/// flaky VPN/WiFi/proxy hiccup on one segment aborts the whole import.
const MAX_ATTEMPTS: u32 = 2;
const RETRY_DELAY_MS: u64 = 2000;

/// Cloud transcription provider (OpenAI and OpenRouter)
pub struct CloudTranscriptionProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    provider_label: &'static str,
}

impl CloudTranscriptionProvider {
    /// Create a cloud provider for the given provider id ("openai" | "openrouter").
    /// Any unknown id is treated as OpenAI.
    pub fn new(provider: &str, api_key: String, model: String) -> Self {
        let (endpoint, label) = match provider {
            "openrouter" => (OPENROUTER_TRANSCRIPTION_URL, "OpenRouter"),
            _ => (OPENAI_TRANSCRIPTION_URL, "OpenAI"),
        };
        Self::with_endpoint(endpoint, api_key, model, label)
    }

    /// Shared constructor (also used by tests with a local endpoint)
    fn with_endpoint(endpoint: &str, api_key: String, model: String, label: &'static str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();

        debug!(
            "Creating {} cloud transcription provider (model: {})",
            label, model
        );

        Self {
            client,
            endpoint: endpoint.to_string(),
            api_key,
            model,
            provider_label: label,
        }
    }

    /// Default model id for a provider (used as a fallback when config is empty)
    pub fn default_model(provider: &str) -> String {
        match provider {
            "openrouter" => DEFAULT_OPENROUTER_TRANSCRIPTION_MODEL.to_string(),
            _ => DEFAULT_OPENAI_TRANSCRIPTION_MODEL.to_string(),
        }
    }

    /// Model ids currently offered to the user per provider.
    /// Kept in sync with the frontend model dropdown (TranscriptSettings).
    pub fn valid_models(provider: &str) -> Vec<&'static str> {
        match provider {
            "openai" => vec!["gpt-4o-transcribe", "gpt-4o-mini-transcribe", "whisper-1"],
            "openrouter" => vec![
                "openai/whisper-1",
                "openai/whisper-large-v3",
                "openai/gpt-4o-transcribe",
            ],
            _ => Vec::new(),
        }
    }

    /// Whether `model` is a currently valid model id for `provider`.
    pub fn is_valid_model(provider: &str, model: &str) -> bool {
        Self::valid_models(provider).iter().any(|m| *m == model.trim())
    }
}

/// Encode 16 kHz mono f32 samples as a 16-bit PCM WAV file (44-byte header).
fn encode_wav_pcm16(samples: &[f32]) -> Vec<u8> {
    let data_size = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_size);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16000u32.to_le_bytes()); // sample rate
    wav.extend_from_slice(&32000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_size as u32).to_le_bytes());

    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * i16::MAX as f32).round() as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }

    wav
}

/// Truncate a string for error messages (keeps first `max` chars)
fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Transport-level failure that a single retry can reasonably clear
/// (connection reset, TLS error, timeout). HTTP status errors are complete
/// responses, not transport failures — retrying them cannot help.
fn is_transient(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_body()
}

#[async_trait]
impl TranscriptionProvider for CloudTranscriptionProvider {
    async fn transcribe(
        &self,
        audio: Vec<f32>,
        language: Option<String>,
    ) -> std::result::Result<TranscriptResult, TranscriptionError> {
        if audio.len() < MIN_CLOUD_SAMPLES {
            return Err(TranscriptionError::AudioTooShort {
                samples: audio.len(),
                minimum: MIN_CLOUD_SAMPLES,
            });
        }

        if audio.len() > MAX_CLOUD_SAMPLES {
            return Err(TranscriptionError::EngineFailed(format!(
                "Audio segment too long for cloud transcription ({} samples, max {})",
                audio.len(),
                MAX_CLOUD_SAMPLES
            )));
        }

        if self.api_key.is_empty() {
            return Err(TranscriptionError::EngineFailed(format!(
                "No {} API key configured. Set it in Settings → Transcription.",
                self.provider_label
            )));
        }

        let wav = encode_wav_pcm16(&audio);

        debug!(
            "{}: uploading {:.2}s of audio to {} (model: {})",
            self.provider_label,
            audio.len() as f64 / 16000.0,
            self.endpoint,
            self.model
        );

        let mut last_err = String::new();
        let mut body: Option<String> = None;
        let mut status: Option<reqwest::StatusCode> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                debug!(
                    "{}: retrying transcription ({}/{}) after: {}",
                    self.provider_label, attempt, MAX_ATTEMPTS, last_err
                );
            }

            // The request consumes the form, so it is rebuilt per attempt
            let mut form = reqwest::multipart::Form::new()
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(wav.clone()).file_name("audio.wav"),
                )
                .text("model", self.model.clone())
                .text("response_format", "json");
            if let Some(lang) = &language {
                if !lang.is_empty() {
                    form = form.text("language", lang.clone());
                }
            }

            let response = match self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .multipart(form)
                .send()
                .await
            {
                Ok(response) => response,
                Err(e) if is_transient(&e) => {
                    last_err = e.to_string();
                    continue;
                }
                Err(e) => {
                    return Err(TranscriptionError::EngineFailed(format!(
                        "{} request failed: {}",
                        self.provider_label, e
                    )));
                }
            };

            let resp_status = response.status();
            match response.text().await {
                Ok(text) => {
                    body = Some(text);
                    status = Some(resp_status);
                    break;
                }
                Err(e) if is_transient(&e) => {
                    last_err = e.to_string();
                    continue;
                }
                Err(e) => return Err(TranscriptionError::EngineFailed(e.to_string())),
            }
        }

        let (status, body) = match (status, body) {
            (Some(status), Some(body)) => (status, body),
            _ => {
                return Err(TranscriptionError::EngineFailed(format!(
                    "{} request failed after {} attempts: {} — check network/VPN/proxy/antivirus and try again",
                    self.provider_label, MAX_ATTEMPTS, last_err
                )))
            }
        };

        if !status.is_success() {
            return Err(TranscriptionError::EngineFailed(format!(
                "{} transcription failed ({}): {}",
                self.provider_label,
                status,
                truncate_for_log(&body, 300)
            )));
        }

        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| {
                TranscriptionError::EngineFailed(format!(
                    "Invalid JSON response from {}: {}",
                    self.provider_label, e
                ))
            })?;

        let text = parsed
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        Ok(TranscriptResult {
            text: text.trim().to_string(),
            confidence: None,
            is_partial: false,
        })
    }

    async fn is_model_loaded(&self) -> bool {
        !self.api_key.is_empty()
    }

    async fn get_current_model(&self) -> Option<String> {
        if self.api_key.is_empty() {
            return None;
        }
        Some(self.model.clone())
    }

    fn provider_name(&self) -> &'static str {
        self.provider_label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A transient transport failure (first connection dropped) must be
    /// retried instead of failing the call; the second attempt succeeds.
    #[tokio::test]
    async fn test_transient_transport_error_is_retried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Attempt 1: accept, then drop immediately -> transport error client-side
            let (_socket, _) = listener.accept().await.unwrap();
            // Attempt 2: drain headers + Content-Length bytes of body, then answer
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let headers_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let headers = String::from_utf8_lossy(&raw[..headers_end]).to_lowercase();
            let content_length = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body_have = raw.len().saturating_sub(headers_end);
            while body_have < content_length {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                body_have += n;
            }
            let resp = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 18\r\n\r\n{\"text\":\"retried\"}";
            socket.write_all(resp).await.unwrap();
        });

        let provider = CloudTranscriptionProvider::with_endpoint(
            &format!("http://{}", addr),
            "test-key".to_string(),
            "test-model".to_string(),
            "OpenAI",
        );

        // 125 ms of 16 kHz audio (above MIN_CLOUD_SAMPLES)
        let audio = vec![0.1f32; 2000];
        let result = provider.transcribe(audio, None).await;

        let result = result.expect("transient failure on first attempt must be retried, not propagated");
        assert_eq!(result.text, "retried");
    }
}
