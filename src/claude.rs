use serde::{Deserialize, Serialize};

pub struct Client {
    api_key: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    User,
    #[allow(dead_code)]
    Assistant,
}

#[derive(Debug, Clone, Copy)]
pub enum Model {
    Haiku,
    #[allow(dead_code)]
    Sonnet,
}

impl Model {
    fn as_str(&self) -> &'static str {
        match self {
            Model::Haiku => "claude-haiku-4-5-20251001",
            Model::Sonnet => "claude-sonnet-4-5-20250929",
        }
    }
}

#[derive(Serialize)]
struct ApiRequest {
    model: &'static str,
    max_tokens: u32,
    messages: Vec<ApiMessage>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

impl Client {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            http: reqwest::Client::new(),
        }
    }

    pub async fn message(
        &self,
        model: Model,
        messages: &[Message],
        max_tokens: u32,
    ) -> Result<String, Error> {
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .map(|m| ApiMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: m.content.clone(),
            })
            .collect();

        let request = ApiRequest {
            model: model.as_str(),
            max_tokens,
            messages: api_messages,
        };

        let response = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("{status}: {body}")));
        }

        let api_response: ApiResponse = response
            .json()
            .await
            .map_err(|e| Error::Parse(e.to_string()))?;

        api_response
            .content
            .first()
            .map(|c| c.text.clone())
            .ok_or(Error::Empty)
    }
}

#[derive(Debug)]
pub enum Error {
    Http(String),
    Api(String),
    Parse(String),
    Empty,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(e) => write!(f, "HTTP error: {e}"),
            Error::Api(e) => write!(f, "API error: {e}"),
            Error::Parse(e) => write!(f, "Parse error: {e}"),
            Error::Empty => write!(f, "Empty response"),
        }
    }
}

impl std::error::Error for Error {}
