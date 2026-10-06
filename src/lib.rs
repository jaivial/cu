//! Minimal client SDK for a running `cu` session server.
pub mod actions;
pub mod server;
use std::io::{Read, Write};
use std::net::TcpStream;

#[derive(Clone, Debug)]
pub struct Client {
    address: String,
    token: String,
}

impl Client {
    /// Connect to a server using the token from its `server.json` file.
    pub fn new(address: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            token: token.into(),
        }
    }
    pub fn status(&self) -> Result<String, String> {
        self.request("GET", "/v1/status", "")
    }
    pub fn navigate(&self, url: &str) -> Result<String, String> {
        self.request(
            "POST",
            "/v1/navigate",
            &format!("{{\"url\":\"{}\"}}", escape(url)),
        )
    }
    /// Compact, LLM-friendly snapshot of the current page.
    ///
    /// A few hundred bytes per call instead of a DOM dump or an image to OCR,
    /// which is what makes the read step of an agent loop cheap.
    pub fn snapshot(&self) -> Result<String, String> {
        self.request("GET", "/v1/snapshot", "")
    }
    pub fn save_session(&self, name: &str) -> Result<String, String> {
        self.request("POST", &format!("/v1/session/{name}"), "{}")
    }
    fn request(&self, method: &str, path: &str, body: &str) -> Result<String, String> {
        let address = self
            .address
            .strip_prefix("http://")
            .unwrap_or(&self.address);
        let mut stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}", self.token, body.len()).map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|e| e.to_string())?;
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or(("", ""));
        if !head.starts_with("HTTP/1.1 2") {
            return Err(body.into());
        }
        Ok(body.into())
    }
}
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_escapes_urls() {
        assert_eq!(
            escape("https://a.test/?q=\"x\""),
            "https://a.test/?q=\\\"x\\\""
        );
    }
}
