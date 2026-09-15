//! The audited rail transport — the ONLY money-path module allowed to
//! construct a `reqwest::Client` (`just deps-check`).

use std::time::Duration;

use application::error::StoreError;
use serde_json::{json, Value};

/// JSON-RPC client confined to the Solana rail.
#[derive(Clone)]
pub struct RailTransport {
    client: reqwest::Client,
}

impl RailTransport {
    /// # Errors
    /// Client builder failure.
    pub fn new() -> Result<Self, StoreError> {
        let result = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build();
        Self::from_build_result(result)
    }

    fn from_build_result(
        result: Result<reqwest::Client, reqwest::Error>,
    ) -> Result<Self, StoreError> {
        match result {
            Ok(client) => Ok(Self { client }),
            Err(err) => Err(StoreError::Backend(err.to_string())),
        }
    }

    /// POST a JSON-RPC method to `endpoint`.
    ///
    /// # Errors
    /// Network / HTTP / JSON failures.
    pub async fn rpc(
        &self,
        endpoint: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, StoreError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response = self
            .client
            .post(endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        if !response.status().is_success() {
            return Err(StoreError::Backend(format!(
                "rpc {} {}",
                method,
                response.status()
            )));
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        if payload.get("error").is_some() {
            return Err(StoreError::Backend(format!("rpc {method} error")));
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn one_shot(response: Vec<u8>) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut captured = Vec::new();
            let mut buf = [0_u8; 32];
            loop {
                let count = socket.read(&mut buf).await.unwrap();
                captured.extend_from_slice(&buf[..count]);
                let text = String::from_utf8_lossy(&captured);
                let complete = text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    body.len() >= content_length
                });
                assert_ne!(count, 0, "client closed before sending a complete request");
                if complete {
                    break;
                }
            }
            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.unwrap();
            captured
        });
        (format!("http://{addr}"), task)
    }

    fn response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn transport_constructs() {
        assert!(super::RailTransport::new().is_ok());
        let error = reqwest::Client::new().get("http://[").build().unwrap_err();
        assert!(super::RailTransport::from_build_result(Err(error)).is_err());
    }

    #[tokio::test]
    async fn rpc_maps_success_http_json_rpc_and_transport_failures() {
        let transport = super::RailTransport::new().unwrap();
        let (endpoint, captured) = one_shot(response("200 OK", r#"{"result":{"ok":true}}"#)).await;
        assert_eq!(
            transport
                .rpc(&endpoint, "method", serde_json::json!([1]))
                .await
                .unwrap(),
            serde_json::json!({"ok": true})
        );
        let request = String::from_utf8_lossy(&captured.await.unwrap()).to_string();
        assert!(request.contains(r#""method":"method""#));
        assert!(request.contains(r#""params":[1]"#));

        let (endpoint, _) = one_shot(response("503 Service Unavailable", "{}")).await;
        assert!(transport
            .rpc(&endpoint, "down", serde_json::json!([]))
            .await
            .is_err());

        let (endpoint, _) = one_shot(response("200 OK", "not-json")).await;
        assert!(transport
            .rpc(&endpoint, "invalid", serde_json::json!([]))
            .await
            .is_err());

        let (endpoint, _) = one_shot(response("200 OK", r#"{"error":{"code":-1}}"#)).await;
        assert!(transport
            .rpc(&endpoint, "rpc-error", serde_json::json!([]))
            .await
            .is_err());

        let (endpoint, _) = one_shot(response("200 OK", "{}")).await;
        assert_eq!(
            transport
                .rpc(&endpoint, "null", serde_json::json!([]))
                .await
                .unwrap(),
            serde_json::Value::Null
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(transport
            .rpc(&endpoint, "offline", serde_json::json!([]))
            .await
            .is_err());
    }
}
