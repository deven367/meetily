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

/// Request timeout for a single transcription call
const REQUEST_TIMEOUT_SECS: u64 = 60;

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

        let mut form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav).file_name("audio.wav"),
            )
            .text("model", self.model.clone())
            .text("response_format", "json");

        if let Some(lang) = &language {
            if !lang.is_empty() {
                form = form.text("language", lang.clone());
            }
        }

        debug!(
            "{}: uploading {:.2}s of audio to {} (model: {})",
            self.provider_label,
            audio.len() as f64 / 16000.0,
            self.endpoint,
            self.model
        );

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                TranscriptionError::EngineFailed(format!(
                    "{} request failed: {}",
                    self.provider_label, e
                ))
            })?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| TranscriptionError::EngineFailed(e.to_string()))?;

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
