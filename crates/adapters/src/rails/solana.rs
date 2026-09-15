//! Solana outbound rail: persist signed bytes, broadcast, quorum lookups.

use std::sync::Mutex;

use application::error::StoreError;
use application::ports::{
    LandingState, OutboundRails, QuorumObservation, RailIdentity, SignaturePresence,
};
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;
use uuid::Uuid;

use super::transport::RailTransport;

/// Production-shaped rail. Signing is injected; this type owns RPC.
pub struct SolanaRails {
    pub identity: RailIdentity,
    transport: RailTransport,
    persisted: Mutex<std::collections::HashMap<Uuid, (Vec<u8>, String)>>,
}

impl SolanaRails {
    /// # Errors
    /// Identity validation or transport construction.
    pub fn new(identity: RailIdentity) -> Result<Self, StoreError> {
        identity
            .validate()
            .map_err(|reason| StoreError::Backend(reason.to_string()))?;
        Ok(Self {
            identity,
            transport: RailTransport::new()?,
            persisted: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Validate every pinned endpoint against the configured cluster, mint,
    /// decimals, and treasury token-account owner before money workers start.
    ///
    /// # Errors
    /// Any timeout, disagreement, or identity mismatch fails startup closed.
    pub async fn validate_remote_identity(&self) -> Result<(), StoreError> {
        for endpoint in &self.identity.rpc_endpoints {
            let genesis = self
                .transport
                .rpc(endpoint, "getGenesisHash", json!([]))
                .await?;
            let first_available = self
                .transport
                .rpc(endpoint, "getFirstAvailableBlock", json!([]))
                .await?;
            if !is_archival_origin(&first_available) {
                return Err(StoreError::Invariant(
                    "pinned quorum endpoint does not retain archival history",
                ));
            }
            let supply = self
                .transport
                .rpc(
                    endpoint,
                    "getTokenSupply",
                    json!([self.identity.usdc_mint, {"commitment": "finalized"}]),
                )
                .await?;
            let treasury = self
                .transport
                .rpc(
                    endpoint,
                    "getAccountInfo",
                    json!([
                        self.identity.treasury_token_account,
                        {"commitment": "finalized", "encoding": "jsonParsed"}
                    ]),
                )
                .await?;
            validate_remote_values(&self.identity, &genesis, &supply, &treasury)?;
        }
        Ok(())
    }

    /// Lookup a signature on one endpoint.
    ///
    /// # Errors
    /// RPC failure.
    pub async fn lookup_on(
        &self,
        endpoint: &str,
        signature: &str,
    ) -> Result<SignaturePresence, StoreError> {
        let result = self
            .transport
            .rpc(
                endpoint,
                "getSignatureStatuses",
                json!([[signature], {"searchTransactionHistory": true}]),
            )
            .await;
        match result {
            Err(_) => Ok(SignaturePresence::Unknown),
            Ok(value) => Ok(presence_from_status(&value)),
        }
    }

    /// 2-of-3 archival observations for the non-landing predicate.
    ///
    /// # Errors
    /// Never fails closed to a panic; per-endpoint errors become unknown.
    pub async fn quorum_observations(
        &self,
        signature: &str,
    ) -> Result<Vec<QuorumObservation>, StoreError> {
        let mut out = Vec::new();
        for endpoint in &self.identity.rpc_endpoints {
            let first_available = self
                .transport
                .rpc(endpoint, "getFirstAvailableBlock", json!([]))
                .await
                .unwrap_or(serde_json::Value::Null);
            let height = self
                .transport
                .rpc(
                    endpoint,
                    "getBlockHeight",
                    json!([{"commitment": "finalized"}]),
                )
                .await
                .ok()
                .and_then(|value| value.as_i64());
            let presence = self
                .lookup_on(endpoint, signature)
                .await
                .unwrap_or(SignaturePresence::Unknown);
            let signature_present = match presence {
                SignaturePresence::Present => Some(true),
                SignaturePresence::Absent => Some(false),
                SignaturePresence::Unknown => None,
            };
            out.push(QuorumObservation {
                endpoint: endpoint.clone(),
                finalized_height: height,
                signature_present,
                pruned: !is_archival_origin(&first_available),
            });
        }
        Ok(out)
    }

    #[must_use]
    pub fn persisted(&self, payment_id: Uuid) -> Option<(Vec<u8>, String)> {
        self.persisted
            .lock()
            .ok()
            .and_then(|map| map.get(&payment_id).cloned())
    }
}

#[async_trait]
impl OutboundRails for SolanaRails {
    async fn persist_signed(
        &self,
        payment_id: Uuid,
        bytes: &[u8],
        signature: &str,
    ) -> Result<(), StoreError> {
        let mut persisted = self
            .persisted
            .lock()
            .map_err(|_| StoreError::Backend("rail persist lock".into()))?;
        if let Some((existing_bytes, existing_signature)) = persisted.get(&payment_id) {
            if existing_bytes.as_slice() != bytes || existing_signature != signature {
                return Err(StoreError::Conflict("persisted signed bytes changed"));
            }
            return Ok(());
        }
        persisted.insert(payment_id, (bytes.to_vec(), signature.to_string()));
        Ok(())
    }

    async fn broadcast(&self, payment_id: Uuid) -> Result<(), StoreError> {
        let Some((bytes, _signature)) = self.persisted(payment_id) else {
            return Err(StoreError::NotFound("persisted signed bytes"));
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let endpoint = self
            .identity
            .rpc_endpoints
            .first()
            .ok_or(StoreError::Invariant("rail identity has no rpc"))?;
        let returned = self
            .transport
            .rpc(
                endpoint,
                "sendTransaction",
                json!([encoded, {"encoding": "base64", "skipPreflight": true}]),
            )
            .await?;
        if returned.as_str() != Some(_signature.as_str()) {
            return Err(StoreError::Invariant(
                "sendTransaction returned a different signature",
            ));
        }
        let _ = LandingState::Broadcast;
        Ok(())
    }
}

fn presence_from_status(result: &serde_json::Value) -> SignaturePresence {
    match result.pointer("/value/0") {
        Some(status) if status.is_null() => SignaturePresence::Absent,
        Some(_) => SignaturePresence::Present,
        None => SignaturePresence::Unknown,
    }
}

fn is_archival_origin(result: &serde_json::Value) -> bool {
    result.as_i64() == Some(0)
}

fn validate_remote_values(
    identity: &RailIdentity,
    genesis: &serde_json::Value,
    supply: &serde_json::Value,
    treasury: &serde_json::Value,
) -> Result<(), StoreError> {
    if genesis.as_str() != Some(identity.genesis_hash.as_str()) {
        return Err(StoreError::Invariant("Solana genesis hash mismatch"));
    }
    if supply
        .pointer("/value/decimals")
        .and_then(serde_json::Value::as_u64)
        != Some(u64::from(identity.decimals))
    {
        return Err(StoreError::Invariant("USDC mint decimals mismatch"));
    }
    if treasury
        .pointer("/value/data/parsed/info/mint")
        .and_then(serde_json::Value::as_str)
        != Some(identity.usdc_mint.as_str())
    {
        return Err(StoreError::Invariant(
            "treasury token-account mint mismatch",
        ));
    }
    if treasury
        .pointer("/value/data/parsed/info/owner")
        .and_then(serde_json::Value::as_str)
        != Some(identity.treasury_owner.as_str())
    {
        return Err(StoreError::Invariant(
            "treasury token-account owner mismatch",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use application::ports::withdraw_fakes::test_rail;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn rpc_server(
        request_count: usize,
        first_available: i64,
        send_signature: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let send_signature = send_signature.to_string();
        let handle = tokio::spawn(async move {
            let mut methods = Vec::new();
            for _ in 0..request_count {
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
                let wire = String::from_utf8_lossy(&captured);
                let (head, body) = wire.split_once("\r\n\r\n").unwrap();
                let request: serde_json::Value = serde_json::from_str(body).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                methods.push(method.clone());
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap();
                let result = match method.as_str() {
                    "getGenesisHash" => json!("gen"),
                    "getFirstAvailableBlock" => json!(first_available),
                    "getTokenSupply" => json!({"value": {"decimals": 6}}),
                    "getAccountInfo" => json!({"value": {"data": {"parsed": {"info": {
                        "mint": "mint", "owner": "owner"
                    }}}}}),
                    "getBlockHeight" => json!(9_999),
                    "getSignatureStatuses" if path == "/a" => json!({"value": [null]}),
                    "getSignatureStatuses" if path == "/b" => json!({"value": [{}]}),
                    "getSignatureStatuses" => json!({}),
                    "sendTransaction" => json!(send_signature),
                    _ => json!(null),
                };
                let body = json!({"jsonrpc": "2.0", "id": 1, "result": result}).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            methods
        });
        (addr, handle)
    }

    fn local_identity(addr: std::net::SocketAddr) -> RailIdentity {
        let mut identity = test_rail();
        identity.rpc_endpoints = ["a", "b", "c"]
            .into_iter()
            .map(|path| format!("http://{addr}/{path}"))
            .collect();
        identity
    }

    #[tokio::test]
    async fn persist_is_required_before_broadcast() {
        let rails = SolanaRails::new(test_rail()).unwrap();
        let id = Uuid::new_v4();
        assert!(rails.broadcast(id).await.is_err());
        rails.persist_signed(id, b"tx", "sig").await.unwrap();
        assert_eq!(rails.persisted(id).unwrap().1, "sig");
    }

    #[tokio::test]
    async fn persisted_payment_bytes_are_immutable() {
        let rails = SolanaRails::new(test_rail()).unwrap();
        let id = Uuid::new_v4();
        rails
            .persist_signed(id, b"first", "sig-first")
            .await
            .unwrap();
        rails
            .persist_signed(id, b"first", "sig-first")
            .await
            .unwrap();

        assert!(matches!(
            rails.persist_signed(id, b"different", "sig-second").await,
            Err(StoreError::Conflict("persisted signed bytes changed"))
        ));
        assert_eq!(
            rails.persisted(id),
            Some((b"first".to_vec(), "sig-first".into()))
        );
    }

    #[tokio::test]
    async fn poisoned_persistence_lock_fails_closed() {
        let rails = SolanaRails::new(test_rail()).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = rails.persisted.lock().unwrap();
            panic!("poison rail persistence lock");
        }));
        assert!(matches!(
            rails.persist_signed(Uuid::new_v4(), b"tx", "sig").await,
            Err(StoreError::Backend(_))
        ));
    }

    #[test]
    fn status_mapping_is_fail_closed() {
        assert_eq!(
            presence_from_status(&json!({"value": [null]})),
            SignaturePresence::Absent
        );
        assert_eq!(
            presence_from_status(&json!({"value": [{"confirmationStatus": "finalized"}]})),
            SignaturePresence::Present
        );
        assert_eq!(presence_from_status(&json!({})), SignaturePresence::Unknown);
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode([0x0f, 0x10]),
            "DxA="
        );
        assert!(is_archival_origin(&json!(0)));
        assert!(!is_archival_origin(&json!(42)));
        assert!(!is_archival_origin(&json!(null)));
    }

    #[test]
    fn quorum_requires_three_distinct_endpoints() {
        let mut identity = test_rail();
        identity.rpc_endpoints[2] = identity.rpc_endpoints[0].clone();
        assert!(SolanaRails::new(identity).is_err());
        let mut identity = test_rail();
        identity.rpc_endpoints.push("https://rpc-4.invalid".into());
        assert!(SolanaRails::new(identity).is_err());
    }

    #[test]
    fn remote_identity_rejects_wrong_cluster_mint_decimals_and_owner() {
        let identity = test_rail();
        let valid_genesis = json!(identity.genesis_hash);
        let valid_supply = json!({"value": {"decimals": identity.decimals}});
        let valid_treasury = json!({"value": {"data": {"parsed": {"info": {
            "mint": identity.usdc_mint,
            "owner": identity.treasury_owner,
        }}}}});
        assert!(
            validate_remote_values(&identity, &valid_genesis, &valid_supply, &valid_treasury)
                .is_ok()
        );
        assert!(validate_remote_values(
            &identity,
            &json!("wrong-cluster"),
            &valid_supply,
            &valid_treasury
        )
        .is_err());
        assert!(validate_remote_values(
            &identity,
            &valid_genesis,
            &json!({"value": {"decimals": 9}}),
            &valid_treasury
        )
        .is_err());
        let wrong_mint = json!({"value": {"data": {"parsed": {"info": {
            "mint": "wrong", "owner": identity.treasury_owner,
        }}}}});
        assert!(
            validate_remote_values(&identity, &valid_genesis, &valid_supply, &wrong_mint).is_err()
        );
        let wrong_owner = json!({"value": {"data": {"parsed": {"info": {
            "mint": identity.usdc_mint, "owner": "wrong",
        }}}}});
        assert!(
            validate_remote_values(&identity, &valid_genesis, &valid_supply, &wrong_owner).is_err()
        );
    }

    #[tokio::test]
    async fn remote_identity_quorum_lookup_and_broadcast_use_the_pinned_transport() {
        let (addr, requests) = rpc_server(24, 0, "sig-rail").await;
        let identity = local_identity(addr);
        let rails = SolanaRails::new(identity.clone()).unwrap();
        rails.validate_remote_identity().await.unwrap();
        assert_eq!(
            rails
                .lookup_on(&identity.rpc_endpoints[2], "sig-rail")
                .await
                .unwrap(),
            SignaturePresence::Unknown
        );
        let observations = rails.quorum_observations("sig-rail").await.unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0].signature_present, Some(false));
        assert_eq!(observations[1].signature_present, Some(true));
        assert_eq!(observations[2].signature_present, None);
        assert!(observations.iter().all(|item| !item.pruned));

        let payment = Uuid::new_v4();
        rails
            .persist_signed(payment, b"signed", "sig-rail")
            .await
            .unwrap();
        rails.broadcast(payment).await.unwrap();
        assert_eq!(
            rails
                .transport
                .rpc(&identity.rpc_endpoints[0], "unexpected", json!([]))
                .await
                .unwrap(),
            serde_json::Value::Null
        );
        assert_eq!(requests.await.unwrap().len(), 24);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            rails.lookup_on(&dead, "sig-rail").await.unwrap(),
            SignaturePresence::Unknown
        );
    }

    #[tokio::test]
    async fn startup_rejects_non_archival_rpc_and_broadcast_signature_drift() {
        let (addr, requests) = rpc_server(2, 1, "unused").await;
        let rails = SolanaRails::new(local_identity(addr)).unwrap();
        assert!(matches!(
            rails.validate_remote_identity().await,
            Err(StoreError::Invariant(
                "pinned quorum endpoint does not retain archival history"
            ))
        ));
        assert_eq!(requests.await.unwrap().len(), 2);

        let (addr, requests) = rpc_server(1, 0, "different").await;
        let rails = SolanaRails::new(local_identity(addr)).unwrap();
        let payment = Uuid::new_v4();
        rails
            .persist_signed(payment, b"signed", "expected")
            .await
            .unwrap();
        assert!(matches!(
            rails.broadcast(payment).await,
            Err(StoreError::Invariant(
                "sendTransaction returned a different signature"
            ))
        ));
        assert_eq!(requests.await.unwrap().len(), 1);

        let mut invalid = test_rail();
        invalid.genesis_hash.clear();
        assert!(SolanaRails::new(invalid).is_err());
    }
}
