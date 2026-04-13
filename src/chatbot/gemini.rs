//! Gemini API client for image generation (Nano Banana) and music generation (Lyria).

use base64::Engine;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent";

pub struct GeminiClient {
    api_key: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct GenerateRequest {
    contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Part {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "inlineData")]
        inline_data: InlineDataInput,
    },
}

#[derive(Serialize)]
struct InlineDataInput {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "responseModalities")]
    response_modalities: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct GenerateResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
}

#[derive(Deserialize, Debug)]
struct ApiError {
    message: String,
}

#[derive(Deserialize, Debug)]
struct Candidate {
    content: Option<CandidateContent>,
}

#[derive(Deserialize, Debug)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Deserialize, Debug)]
struct ResponsePart {
    #[serde(rename = "inlineData")]
    inline_data: Option<InlineData>,
}

#[derive(Deserialize, Debug)]
struct InlineData {
    data: String,
}

pub struct GeneratedImage {
    pub data: Vec<u8>,
}

impl GeminiClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Generate an image from a text prompt.
    pub async fn generate_image(&self, prompt: &str) -> Result<GeneratedImage, String> {
        info!("🎨 Generating image: {}", prompt);

        let request = GenerateRequest {
            contents: vec![Content {
                parts: vec![Part::Text {
                    text: prompt.to_string(),
                }],
            }],
            generation_config: GenerationConfig {
                response_modalities: vec!["TEXT".to_string(), "IMAGE".to_string()],
            },
        };

        let url = format!("{}?key={}", GEMINI_API_URL, self.api_key);

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        debug!("Gemini response status: {status}");

        if !status.is_success() {
            return Err(format!("API error {status}: {body}"));
        }

        let parsed: GenerateResponse =
            serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

        if let Some(error) = parsed.error {
            return Err(format!("Gemini error: {}", error.message));
        }

        let candidates = parsed.candidates.ok_or("No candidates in response")?;
        let candidate = candidates.first().ok_or("Empty candidates array")?;
        let content = candidate
            .content
            .as_ref()
            .ok_or("No content in candidate")?;

        // Find the image part
        for part in &content.parts {
            if let Some(ref inline_data) = part.inline_data {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(&inline_data.data)
                    .map_err(|e| format!("Failed to decode base64: {e}"))?;

                info!("🎨 Image generated: {} bytes", data.len());

                return Ok(GeneratedImage { data });
            }
        }

        Err("No image in response".to_string())
    }

    /// Edit an existing image using a text prompt (Gemini image editing).
    /// `source_image` is raw bytes + mime type (e.g. "image/jpeg").
    pub async fn edit_image(
        &self,
        prompt: &str,
        source_image: &[u8],
        source_mime_type: &str,
    ) -> Result<GeneratedImage, String> {
        info!("🎨 Editing image with prompt: {}", prompt);

        let encoded = base64::engine::general_purpose::STANDARD.encode(source_image);

        let request = GenerateRequest {
            contents: vec![Content {
                parts: vec![
                    Part::Image {
                        inline_data: InlineDataInput {
                            mime_type: source_mime_type.to_string(),
                            data: encoded,
                        },
                    },
                    Part::Text {
                        text: prompt.to_string(),
                    },
                ],
            }],
            generation_config: GenerationConfig {
                response_modalities: vec!["TEXT".to_string(), "IMAGE".to_string()],
            },
        };

        let url = format!("{}?key={}", GEMINI_API_URL, self.api_key);

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        debug!("Gemini edit_image response status: {status}");

        if !status.is_success() {
            return Err(format!("API error {status}: {body}"));
        }

        let parsed: GenerateResponse =
            serde_json::from_str(&body).map_err(|e| format!("Failed to parse response: {e}"))?;

        if let Some(error) = parsed.error {
            return Err(format!("Gemini error: {}", error.message));
        }

        let candidates = parsed.candidates.ok_or("No candidates in response")?;
        let candidate = candidates.first().ok_or("Empty candidates array")?;
        let content = candidate
            .content
            .as_ref()
            .ok_or("No content in candidate")?;

        for part in &content.parts {
            if let Some(ref inline_data) = part.inline_data {
                let data = base64::engine::general_purpose::STANDARD
                    .decode(&inline_data.data)
                    .map_err(|e| format!("Failed to decode base64: {e}"))?;

                info!("🎨 Edited image: {} bytes", data.len());
                return Ok(GeneratedImage { data });
            }
        }

        Err("No image in response".to_string())
    }

    /// Generate music from a text prompt using Gemini Lyria.
    ///
    /// Delegates to scripts/lyria_music.py which handles the BidiGenerateMusicContent
    /// WebSocket protocol and PCM→OGG conversion via ffmpeg.
    /// Returns OGG Opus audio bytes suitable for sending via Telegram.
    pub async fn generate_music(&self, prompt: &str) -> Result<Vec<u8>, String> {
        info!("🎵 Generating music via lyria_music.py: {}", prompt);

        let output = tokio::time::timeout(
            tokio::time::Duration::from_secs(90),
            tokio::process::Command::new("python3")
                .args(["lyria_music.py", &self.api_key, prompt])
                .output(),
        )
        .await
        .map_err(|_| "lyria_music.py timed out after 90s".to_string())?
        .map_err(|e| format!("Failed to run lyria_music.py: {e}"))?;

        // Log all stderr lines for debugging
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            info!("🎵 [lyria.py] {}", line);
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "lyria_music.py failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.lines().last().unwrap_or("unknown error")
            ));
        }

        if output.stdout.is_empty() {
            return Err("lyria_music.py produced no audio output".to_string());
        }

        info!("🎵 Got {} bytes of OGG audio", output.stdout.len());
        Ok(output.stdout)
    }
}
