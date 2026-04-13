//! Speech-to-text transcription using whisper-rs.
//!
//! Converts voice messages (OGG Opus from Telegram) to text.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tracing::{debug, info};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Whisper transcription engine.
#[derive(Clone)]
pub struct Whisper {
    ctx: Arc<WhisperContext>,
}

impl Whisper {
    /// Load a Whisper model from a .bin file.
    pub fn new(model_path: &Path) -> Result<Self, String> {
        info!("Loading Whisper model from {:?}", model_path);

        if !model_path.exists() {
            return Err(format!("Model file not found: {:?}", model_path));
        }

        let ctx = WhisperContext::new_with_params(
            model_path.to_str().ok_or("Invalid model path")?,
            WhisperContextParameters::default(),
        )
        .map_err(|e| format!("Failed to load Whisper model: {e}"))?;

        info!("Whisper model loaded successfully");
        Ok(Self { ctx: Arc::new(ctx) })
    }

    /// Transcribe audio data (OGG Opus format from Telegram).
    ///
    /// Converts to 16KHz mono PCM using ffmpeg, then runs Whisper.
    pub fn transcribe(&self, ogg_data: &[u8]) -> Result<String, String> {
        debug!("Transcribing {} bytes of audio", ogg_data.len());

        // Convert OGG to 16KHz mono f32 PCM using ffmpeg
        let pcm_data = convert_ogg_to_pcm(ogg_data)?;

        // Create state for this transcription
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| format!("Failed to create Whisper state: {e}"))?;

        // Configure parameters
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(None); // Auto-detect language (supports Uzbek, Russian, English)
        params.set_translate(false);
        params.set_no_timestamps(true);
        params.set_single_segment(false);

        // Run transcription
        state
            .full(params, &pcm_data)
            .map_err(|e| format!("Whisper transcription failed: {e}"))?;

        // Collect all segments
        let mut text = String::new();
        for segment in state.as_iter() {
            if let Ok(s) = segment.to_str() {
                text.push_str(s);
                text.push(' ');
            }
        }

        let text = text.trim().to_string();
        info!("Transcribed: \"{}\"", truncate(&text, 100));
        Ok(text)
    }
}

/// Convert OGG Opus audio to 16KHz mono f32 PCM samples using ffmpeg.
fn convert_ogg_to_pcm(ogg_data: &[u8]) -> Result<Vec<f32>, String> {
    // Create temp file for input (ffmpeg needs seekable input for OGG)
    let temp_dir = std::env::temp_dir();
    let input_path = temp_dir.join(format!("whisper_input_{}.ogg", std::process::id()));

    std::fs::write(&input_path, ogg_data)
        .map_err(|e| format!("Failed to write temp input: {e}"))?;

    // Run ffmpeg to convert to raw PCM
    // Output format: 16-bit signed little-endian, 16KHz, mono
    let output = Command::new("ffmpeg")
        .args([
            "-i",
            input_path.to_str().unwrap(),
            "-ar",
            "16000", // 16KHz sample rate
            "-ac",
            "1", // Mono
            "-f",
            "s16le", // 16-bit signed little-endian PCM
            "-acodec",
            "pcm_s16le",
            "-y",     // Overwrite
            "pipe:1", // Output to stdout
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    // Clean up temp file
    let _ = std::fs::remove_file(&input_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg failed: {}", stderr));
    }

    // Convert i16 samples to f32
    let samples: Vec<f32> = output
        .stdout
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / 32768.0
        })
        .collect();

    debug!("Converted to {} f32 samples", samples.len());
    Ok(samples)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated)
    }
}

/// OpenAI GPT-4o Transcribe speech-to-text.
///
/// Uses `gpt-4o-transcribe` — significantly better than whisper-1 for
/// multilingual audio, especially Uzbek, Russian, and English.
/// A prompt biases the model toward these three languages.
#[derive(Clone)]
pub struct OpenAITranscriber {
    api_key: String,
    client: reqwest::Client,
}

impl OpenAITranscriber {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Transcribe OGG Opus audio. Auto-detects language (English, Russian, Uzbek).
    pub async fn transcribe(
        &self,
        ogg_data: &[u8],
        audio_duration_secs: u32,
    ) -> Result<String, String> {
        debug!(
            "OpenAI STT: {} bytes, {}s",
            ogg_data.len(),
            audio_duration_secs
        );
        use reqwest::multipart;

        let audio_part = multipart::Part::bytes(ogg_data.to_vec())
            .file_name("voice.ogg")
            .mime_str("audio/ogg")
            .map_err(|e| format!("MIME error: {e}"))?;

        // Prompt biases the model toward the expected languages and improves accuracy
        let form = multipart::Form::new()
            .text("model", "gpt-4o-transcribe")
            .text("response_format", "json")
            .text(
                "prompt",
                "The audio is a voice message in English, Russian, or Uzbek.",
            )
            .part("file", audio_part);

        let resp = self
            .client
            .post("https://api.openai.com/v1/audio/transcriptions")
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Response error: {e}"))?;

        if !status.is_success() {
            return Err(format!("OpenAI API error {status}: {body}"));
        }

        #[derive(serde::Deserialize)]
        struct OpenAIJsonResponse {
            text: String,
        }

        let parsed: OpenAIJsonResponse = serde_json::from_str(&body)
            .map_err(|e| format!("Parse error: {e} (body: {})", truncate(&body, 200)))?;

        let text = parsed.text.trim().to_string();
        if text.is_empty() {
            return Ok("Voice message not understood, please type instead".to_string());
        }

        info!(
            "OpenAI STT ({}s): \"{}\"",
            audio_duration_secs,
            truncate(&text, 100)
        );
        Ok(text)
    }
}

/// Groq-based speech-to-text transcription via HTTP API.
///
/// Sends OGG Opus audio directly to Groq Whisper — no local model required.
#[derive(Clone)]
pub struct GroqTranscriber {
    api_key: String,
    client: reqwest::Client,
}

impl GroqTranscriber {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Transcribe OGG Opus audio using a 2-pass strategy.
    ///
    /// Pass 1: auto-detect language.
    /// - If Kazakh (kk) detected → retry with language=uz (Uzbek often misidentified as Kazakh).
    /// - If text is short (<3 words) and audio is ≥3s → retry with language=uz (hallucination catch).
    /// - If final result is empty → return "Voice message not understood" hint.
    pub async fn transcribe(
        &self,
        ogg_data: &[u8],
        audio_duration_secs: u32,
    ) -> Result<String, String> {
        debug!(
            "Groq STT: {} bytes, {}s",
            ogg_data.len(),
            audio_duration_secs
        );

        // Pass 1: auto-detect
        let (text, lang) = self.transcribe_internal(ogg_data, None).await?;
        let detected = lang.as_deref().unwrap_or("");
        let word_count = text.split_whitespace().count();

        let needs_retry = detected == "kk" || (word_count < 3 && audio_duration_secs >= 3);

        if needs_retry {
            if detected == "kk" {
                info!("Groq detected Kazakh (kk), retrying with Uzbek (uz)");
            } else {
                info!(
                    "Groq: {} words for {}s audio, retrying with uz",
                    word_count, audio_duration_secs
                );
            }

            let (text2, _) = self.transcribe_internal(ogg_data, Some("uz")).await?;
            let text2 = text2.trim().to_string();

            if text2.is_empty() {
                return Ok("Voice message not understood, please type instead".to_string());
            }
            info!("Groq STT (uz retry): \"{}\"", truncate(&text2, 100));
            return Ok(text2);
        }

        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok("Voice message not understood, please type instead".to_string());
        }
        info!(
            "Groq STT ({}): \"{}\"",
            if detected.is_empty() {
                "auto"
            } else {
                detected
            },
            truncate(&text, 100)
        );
        Ok(text)
    }

    async fn transcribe_internal(
        &self,
        ogg_data: &[u8],
        language: Option<&str>,
    ) -> Result<(String, Option<String>), String> {
        use reqwest::multipart;

        let audio_part = multipart::Part::bytes(ogg_data.to_vec())
            .file_name("voice.ogg")
            .mime_str("audio/ogg")
            .map_err(|e| format!("MIME error: {e}"))?;

        let mut form = multipart::Form::new()
            .text("model", "whisper-large-v3-turbo")
            .text("response_format", "verbose_json")
            .part("file", audio_part);

        if let Some(lang) = language {
            form = form.text("language", lang.to_string());
        }

        let resp = self
            .client
            .post("https://api.groq.com/openai/v1/audio/transcriptions")
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| format!("Response error: {e}"))?;

        if !status.is_success() {
            return Err(format!("Groq API error {status}: {body}"));
        }

        #[derive(serde::Deserialize)]
        struct GroqVerboseResponse {
            text: String,
            language: Option<String>,
        }

        let parsed: GroqVerboseResponse =
            serde_json::from_str(&body).map_err(|e| format!("Parse error: {e}"))?;

        Ok((parsed.text.trim().to_string(), parsed.language))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello...");
    }
}
