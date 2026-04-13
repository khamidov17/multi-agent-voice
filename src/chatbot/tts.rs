//! Text-to-speech using XTTS.
//!
//! Generates voice audio from text using Coqui XTTS v2.
//! Requires running the XTTS server: `python scripts/xtts_server.py`

use std::process::Command;

use serde::Deserialize;
use tracing::{debug, info, warn};

/// Response from /v1/references/list endpoint.
#[derive(Debug, Deserialize)]
struct ListReferencesResponse {
    success: bool,
    reference_ids: Vec<String>,
}

/// TTS client for XTTS server.
pub struct TtsClient {
    endpoint: String,
    client: reqwest::Client,
}

impl TtsClient {
    /// Create a new TTS client.
    ///
    /// `endpoint` should be the base URL of the XTTS server,
    /// e.g., "http://localhost:8880"
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: reqwest::Client::new(),
        }
    }

    /// Get list of available voice reference IDs from Fish Speech.
    pub async fn list_voices(&self) -> Vec<String> {
        match self
            .client
            .get(format!("{}/v1/references/list", self.endpoint))
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success()
                    && let Ok(resp) = response.json::<ListReferencesResponse>().await
                {
                    if resp.success {
                        return resp.reference_ids;
                    }
                    warn!("Voice list API returned success=false");
                } else {
                    warn!("Failed to parse voice list response");
                }
                vec![]
            }
            Err(e) => {
                warn!("Failed to fetch voice list: {}", e);
                vec![]
            }
        }
    }

    /// Generate speech from text.
    ///
    /// Returns OGG Opus audio data suitable for Telegram voice messages.
    /// The `voice` parameter specifies the reference voice ID (default: "p231").
    pub async fn synthesize(&self, text: &str, voice: Option<&str>) -> Result<Vec<u8>, String> {
        let preview: String = text.chars().take(50).collect();
        info!("TTS: \"{}\"", preview);

        // Default voice (uses XTTS built-in "Ana Florence" if no reference)
        let reference_id = voice.unwrap_or("default");

        // Call XTTS server endpoint
        let response = self
            .client
            .post(format!("{}/v1/tts", self.endpoint))
            .json(&serde_json::json!({
                "text": text,
                "format": "wav",
                "reference_id": reference_id
            }))
            .send()
            .await
            .map_err(|e| format!("TTS request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("TTS error {}: {}", status, body));
        }

        let wav_data = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read TTS response: {e}"))?;

        debug!("Got {} bytes of WAV audio", wav_data.len());

        // Convert WAV to OGG Opus for Telegram
        let ogg_data = convert_wav_to_ogg(&wav_data)?;

        info!("Generated {} bytes of voice audio", ogg_data.len());
        Ok(ogg_data)
    }
}

/// Convert WAV audio to OGG Opus format for Telegram voice messages.
fn convert_wav_to_ogg(wav_data: &[u8]) -> Result<Vec<u8>, String> {
    // Write WAV to temp file
    let temp_dir = std::env::temp_dir();
    let input_path = temp_dir.join(format!("tts_input_{}.wav", std::process::id()));
    let output_path = temp_dir.join(format!("tts_output_{}.ogg", std::process::id()));

    std::fs::write(&input_path, wav_data).map_err(|e| format!("Failed to write temp WAV: {e}"))?;

    // Convert using ffmpeg with 300ms silence padding at start
    // (Telegram cuts off the first ~200ms when playing voice messages)
    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono",
            "-i",
            input_path.to_str().unwrap(),
            "-filter_complex",
            "[0]atrim=0:0.3[silence];[silence][1:a]concat=n=2:v=0:a=1",
            "-c:a",
            "libopus",
            "-b:a",
            "64k",
            output_path.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    // Clean up input
    let _ = std::fs::remove_file(&input_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&output_path);
        return Err(format!("ffmpeg conversion failed: {}", stderr));
    }

    // Read output
    let ogg_data =
        std::fs::read(&output_path).map_err(|e| format!("Failed to read OGG output: {e}"))?;

    // Clean up output
    let _ = std::fs::remove_file(&output_path);

    debug!(
        "Converted WAV ({} bytes) to OGG ({} bytes)",
        wav_data.len(),
        ogg_data.len()
    );
    Ok(ogg_data)
}

/// TTS client using Gemini API (gemini-2.5-flash-preview-tts model).
pub struct GeminiTtsClient {
    api_key: String,
    client: reqwest::Client,
}

impl GeminiTtsClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Generate speech from text using Gemini TTS API.
    ///
    /// Returns OGG Opus audio data suitable for Telegram voice messages.
    /// `voice` can be any Gemini prebuilt voice name (e.g. "Kore", "Puck", "Charon").
    pub async fn synthesize(&self, text: &str, voice: Option<&str>) -> Result<Vec<u8>, String> {
        use base64::Engine;
        use serde::{Deserialize, Serialize};

        let preview: String = text.chars().take(50).collect();
        info!("Gemini TTS: \"{}\"", preview);

        // Gemini TTS fails on very short inputs — pad to at least 10 chars
        let padded;
        let text = if text.chars().count() < 10 {
            padded = format!("{}.", text);
            padded.as_str()
        } else {
            text
        };

        let voice_name = voice.unwrap_or("Kore");

        #[derive(Serialize)]
        struct Request {
            contents: Vec<Content>,
            #[serde(rename = "generationConfig")]
            generation_config: GenerationConfig,
        }
        #[derive(Serialize)]
        struct Content {
            parts: Vec<Part>,
        }
        #[derive(Serialize)]
        struct Part {
            text: String,
        }
        #[derive(Serialize)]
        struct GenerationConfig {
            #[serde(rename = "responseModalities")]
            response_modalities: Vec<String>,
            #[serde(rename = "speechConfig")]
            speech_config: SpeechConfig,
        }
        #[derive(Serialize)]
        struct SpeechConfig {
            #[serde(rename = "voiceConfig")]
            voice_config: VoiceConfig,
        }
        #[derive(Serialize)]
        struct VoiceConfig {
            #[serde(rename = "prebuiltVoiceConfig")]
            prebuilt_voice_config: PrebuiltVoiceConfig,
        }
        #[derive(Serialize)]
        struct PrebuiltVoiceConfig {
            #[serde(rename = "voiceName")]
            voice_name: String,
        }
        #[derive(Deserialize)]
        struct Response {
            candidates: Option<Vec<Candidate>>,
            error: Option<ApiError>,
        }
        #[derive(Deserialize)]
        struct Candidate {
            content: Option<CandidateContent>,
        }
        #[derive(Deserialize)]
        struct CandidateContent {
            parts: Vec<ResponsePart>,
        }
        #[derive(Deserialize)]
        struct ResponsePart {
            #[serde(rename = "inlineData")]
            inline_data: Option<InlineData>,
        }
        #[derive(Deserialize)]
        struct InlineData {
            #[serde(rename = "mimeType")]
            mime_type: String,
            data: String,
        }
        #[derive(Deserialize)]
        struct ApiError {
            message: String,
        }

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-preview-tts:generateContent?key={}",
            self.api_key
        );

        let request = Request {
            contents: vec![Content {
                parts: vec![Part {
                    text: text.to_string(),
                }],
            }],
            generation_config: GenerationConfig {
                response_modalities: vec!["AUDIO".to_string()],
                speech_config: SpeechConfig {
                    voice_config: VoiceConfig {
                        prebuilt_voice_config: PrebuiltVoiceConfig {
                            voice_name: voice_name.to_string(),
                        },
                    },
                },
            },
        };

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Gemini TTS request failed: {e}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read Gemini TTS response: {e}"))?;

        if !status.is_success() {
            return Err(format!("Gemini TTS API error {status}: {body}"));
        }

        let parsed: Response = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse Gemini TTS response: {e}"))?;

        if let Some(err) = parsed.error {
            return Err(format!("Gemini TTS error: {}", err.message));
        }

        let candidates = parsed
            .candidates
            .ok_or("No candidates in Gemini TTS response")?;
        let candidate = candidates.into_iter().next().ok_or("Empty candidates")?;
        let content = candidate.content.ok_or("No content in candidate")?;

        for part in content.parts {
            if let Some(inline_data) = part.inline_data {
                let audio_bytes = base64::engine::general_purpose::STANDARD
                    .decode(&inline_data.data)
                    .map_err(|e| format!("Failed to decode Gemini TTS audio: {e}"))?;

                info!(
                    "Gemini TTS: {} bytes, mime={}",
                    audio_bytes.len(),
                    inline_data.mime_type
                );

                // Gemini returns PCM audio (audio/L16 at 24000Hz) — convert to OGG Opus
                let ogg = convert_pcm_to_ogg(&audio_bytes, 24000)?;
                return Ok(ogg);
            }
        }

        Err("No audio data in Gemini TTS response".to_string())
    }
}

/// Convert raw 16-bit PCM audio to OGG Opus for Telegram voice messages.
fn convert_pcm_to_ogg(pcm_data: &[u8], sample_rate: u32) -> Result<Vec<u8>, String> {
    let temp_dir = std::env::temp_dir();
    let input_path = temp_dir.join(format!("gemini_tts_{}.pcm", std::process::id()));
    let output_path = temp_dir.join(format!("gemini_tts_{}.ogg", std::process::id()));

    std::fs::write(&input_path, pcm_data).map_err(|e| format!("Failed to write temp PCM: {e}"))?;

    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "s16le",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            "1",
            "-i",
            input_path.to_str().unwrap(),
            "-c:a",
            "libopus",
            "-b:a",
            "64k",
            output_path.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {e}"))?;

    let _ = std::fs::remove_file(&input_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&output_path);
        return Err(format!("ffmpeg PCM conversion failed: {}", stderr));
    }

    let ogg_data =
        std::fs::read(&output_path).map_err(|e| format!("Failed to read OGG output: {e}"))?;
    let _ = std::fs::remove_file(&output_path);

    debug!(
        "Converted PCM ({} bytes) to OGG ({} bytes)",
        pcm_data.len(),
        ogg_data.len()
    );
    Ok(ogg_data)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_tts_client_creation() {
        use super::TtsClient;
        let client = TtsClient::new("http://localhost:8880".to_string());
        assert_eq!(client.endpoint, "http://localhost:8880");
    }
}
