//! Quiet-segment client for the ops-plane invariant snapshot.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::ports::SwarmError;
use crate::transport::HttpTransport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantIdentity {
    pub identity: String,
    pub pass: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantReport {
    pub as_of: String,
    pub pass: bool,
    pub identities: Vec<InvariantIdentity>,
}

#[async_trait]
pub trait InvariantApi: Send + Sync {
    async fn invariant_report(&self, bearer: &str) -> Result<InvariantReport, SwarmError>;
}

#[async_trait]
impl InvariantApi for HttpTransport {
    async fn invariant_report(&self, bearer: &str) -> Result<InvariantReport, SwarmError> {
        let value = self.get_json_bearer("/admin/invariants", bearer).await?;
        serde_json::from_value(value).map_err(|error| SwarmError::Protocol(error.to_string()))
    }
}

pub struct InvariantsClient<'a> {
    pub api: &'a dyn InvariantApi,
    pub bearer: &'a str,
}

impl InvariantsClient<'_> {
    pub async fn check_quiet_segment(
        &self,
        quiet: bool,
    ) -> Result<Option<InvariantReport>, SwarmError> {
        if !quiet {
            return Ok(None);
        }
        let report = self.api.invariant_report(self.bearer).await?;
        if !report.pass || report.identities.iter().any(|identity| !identity.pass) {
            return Err(SwarmError::Protocol("invariant sweep failed".into()));
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::transport::HttpTransport;
    use tokio::net::TcpListener;

    struct FakeApi {
        calls: AtomicUsize,
        report: InvariantReport,
    }

    #[async_trait]
    impl InvariantApi for FakeApi {
        async fn invariant_report(&self, bearer: &str) -> Result<InvariantReport, SwarmError> {
            assert_eq!(bearer, "ops-token");
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.report.clone())
        }
    }

    fn report(pass: bool) -> InvariantReport {
        InvariantReport {
            as_of: "2026-01-01T00:00:00Z".into(),
            pass,
            identities: vec![InvariantIdentity {
                identity: "per-tx-sum-zero".into(),
                pass,
                detail: None,
            }],
        }
    }

    #[tokio::test]
    async fn calls_only_in_quiet_segments_and_fails_closed() {
        let api = FakeApi {
            calls: AtomicUsize::new(0),
            report: report(true),
        };
        let client = InvariantsClient {
            api: &api,
            bearer: "ops-token",
        };
        assert_eq!(client.check_quiet_segment(false).await.unwrap(), None);
        assert!(client.check_quiet_segment(true).await.unwrap().is_some());
        assert_eq!(api.calls.load(Ordering::Relaxed), 1);

        let api = FakeApi {
            calls: AtomicUsize::new(0),
            report: report(false),
        };
        let client = InvariantsClient {
            api: &api,
            bearer: "ops-token",
        };
        assert!(client.check_quiet_segment(true).await.is_err());
    }

    #[tokio::test]
    async fn inconsistent_top_level_pass_is_rejected() {
        let mut inconsistent = report(true);
        inconsistent.identities[0].pass = false;
        let api = FakeApi {
            calls: AtomicUsize::new(0),
            report: inconsistent,
        };
        let client = InvariantsClient {
            api: &api,
            bearer: "ops-token",
        };
        assert!(client.check_quiet_segment(true).await.is_err());
    }

    #[tokio::test]
    async fn http_adapter_sends_bearer_and_decodes_wire_report() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 2048];
            let read = tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                .await
                .unwrap();
            let body = r#"{"as_of":"2026-01-01T00:00:00Z","pass":true,"identities":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
            String::from_utf8(request[..read].to_vec()).unwrap()
        });
        let api = HttpTransport::new(&format!("http://{address}")).unwrap();
        let report = api.invariant_report("ops-token").await.unwrap();
        assert!(report.pass);
        assert!(server
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer ops-token"));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request)
                .await
                .unwrap();
            let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });
        let api = HttpTransport::new(&format!("http://{address}")).unwrap();
        assert!(matches!(
            api.invariant_report("ops-token").await,
            Err(SwarmError::Protocol(_))
        ));
    }
}
