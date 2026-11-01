//! The Telegram Bot API, behind a trait so the bot logic runs against a
//! fake in tests.

use serde_json::Value;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;
use std::time::Duration;

/// Form parameters of an API call, in order; values are already strings
/// (numbers formatted, nested objects as JSON).
#[derive(Clone, Debug, Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key`, replacing an earlier value.
    pub fn set(mut self, key: &str, value: impl ToString) -> Self {
        let value = value.to_string();
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
        self
    }

    pub fn json(self, key: &str, value: &Value) -> Self {
        self.set(key, value)
    }

    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

#[derive(Debug)]
pub enum ApiError {
    /// Telegram answered and refused the call.
    Refused {
        method: String,
        code: i64,
        description: String,
        retry_after: Option<u64>,
        /// A group turned supergroup: its new identifier.
        migrate_to: Option<i64>,
    },
    /// No answer: network down, timeout, DNS, sleep.
    Network(String),
}

impl ApiError {
    /// The refusal's description mentions one of `words` (a deleted topic).
    pub fn mentions(&self, words: &[&str]) -> bool {
        match self {
            Self::Refused { description, .. } => {
                let lower = description.to_lowercase();
                words.iter().any(|word| lower.contains(word))
            }
            Self::Network(_) => false,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused {
                method,
                description,
                ..
            } => write!(f, "Telegram {method}: {description}"),
            Self::Network(reason) => write!(f, "Telegram unreachable: {reason}"),
        }
    }
}

impl std::error::Error for ApiError {}

pub trait Telegram {
    /// Calls `method`; with `document`, the file goes as a multipart upload.
    fn call(
        &self,
        method: &str,
        params: &Params,
        timeout: Duration,
        document: Option<&Path>,
    ) -> Result<Value, ApiError>;

    /// Downloads a file given by `getFile` to `target`.
    fn download(&self, file_path: &str, target: &Path) -> Result<(), ApiError>;
}

pub struct HttpTelegram {
    token: String,
    agent: ureq::Agent,
}

impl HttpTelegram {
    pub fn new(token: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { token, agent }
    }

    /// An error text never carries the token (it is in every URL).
    fn network(&self, error: impl fmt::Display) -> ApiError {
        ApiError::Network(error.to_string().replace(&self.token, "…"))
    }

    fn multipart(params: &Params, document: &Path) -> io::Result<(Vec<u8>, String)> {
        let boundary = format!("asp-telegram-{:016x}", rand_u64());
        let mut body = Vec::new();
        for (key, value) in params.pairs() {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        let name = document
            .file_name()
            .map(|name| name.to_string_lossy().replace('"', "'"))
            .unwrap_or_else(|| "file".to_string());
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"document\"; filename=\"{name}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&std::fs::read(document)?);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        Ok((body, format!("multipart/form-data; boundary={boundary}")))
    }
}

fn rand_u64() -> u64 {
    let mut bytes = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut file| io::Read::read_exact(&mut file, &mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        return nanos ^ u64::from(std::process::id());
    }
    u64::from_le_bytes(bytes)
}

impl Telegram for HttpTelegram {
    fn call(
        &self,
        method: &str,
        params: &Params,
        timeout: Duration,
        document: Option<&Path>,
    ) -> Result<Value, ApiError> {
        let url = format!("https://api.telegram.org/bot{}/{method}", self.token);
        let request = self
            .agent
            .post(&url)
            .config()
            .timeout_global(Some(timeout))
            .build();
        let response = match document {
            Some(file) => {
                let (body, kind) = Self::multipart(params, file).map_err(|e| self.network(e))?;
                request.header("Content-Type", kind).send(&body[..])
            }
            None => request.send_form(
                params
                    .pairs()
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            ),
        }
        .map_err(|e| self.network(e))?;
        let status = i64::from(response.status().as_u16());
        let raw = response
            .into_body()
            .read_to_string()
            .map_err(|e| self.network(e))?;
        let body: Value = serde_json::from_str(if raw.is_empty() { "{}" } else { &raw })
            .unwrap_or_else(|_| {
                serde_json::json!({
                    "description": format!("unreadable answer (HTTP {status})"),
                    "error_code": if status == 200 { 502 } else { status },
                })
            });
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(body.get("result").cloned().unwrap_or(Value::Null));
        }
        let parameters = body.get("parameters");
        Err(ApiError::Refused {
            method: method.to_string(),
            code: body.get("error_code").and_then(Value::as_i64).unwrap_or(status),
            description: body
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
                .to_string(),
            retry_after: parameters
                .and_then(|p| p.get("retry_after"))
                .and_then(Value::as_u64),
            migrate_to: parameters
                .and_then(|p| p.get("migrate_to_chat_id"))
                .and_then(Value::as_i64),
        })
    }

    fn download(&self, file_path: &str, target: &Path) -> Result<(), ApiError> {
        let url = format!("https://api.telegram.org/file/bot{}/{file_path}", self.token);
        let response = self
            .agent
            .get(&url)
            .config()
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .call()
            .map_err(|e| self.network(e))?;
        if !response.status().is_success() {
            return Err(ApiError::Refused {
                method: "file".to_string(),
                code: i64::from(response.status().as_u16()),
                description: format!("download failed (HTTP {})", response.status().as_u16()),
                retry_after: None,
                migrate_to: None,
            });
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| self.network(e))?;
        }
        let mut file = File::create(target).map_err(|e| self.network(e))?;
        let mut reader = response.into_body().into_reader();
        io::copy(&mut reader, &mut file).map_err(|e| self.network(e))?;
        Ok(())
    }
}
