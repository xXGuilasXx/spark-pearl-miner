//! Exercise authorize negotiation through a real TCP session. The server deliberately
//! controls the ack independently of the client's requested v2 dialect.
use std::time::Duration;

use serde_json::{json, Value};
use spm_proto::client::{PoolSession, SessionConfig, SessionEvent};
use spm_proto::codec::{write_line, NdjsonReader};
use spm_proto::encode::{decode_proof, ProofEncoding};
use spm_proto::tls::{Connector, TlsMode};
use spm_proto::Dialect;
use tokio::net::TcpListener;

const WAIT: Duration = Duration::from_secs(10);
const PROOF: &[u8] = b"proof bytes whose negotiated wire encoding must round-trip exactly";
const JOB_ID: &str = "encoding-job";

async fn assert_session_encoding(ack_type: Option<&'static str>, expected: ProofEncoding) {
    tokio::time::timeout(WAIT, async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = socket.into_split();
            let mut reader = NdjsonReader::new(rd);
            let auth: Value =
                serde_json::from_str(&reader.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(auth["method"], "mining.authorize");
            assert_eq!(auth["params"]["type"], "v2");
            assert_eq!(auth["params"]["wallet"], "prl1qtestwallet.rig0");

            let mut ack = json!({ "id": auth["id"], "result": true, "error": null });
            if let Some(kind) = ack_type {
                ack["type"] = json!(kind);
            }
            write_line(&mut wr, &ack.to_string()).await.unwrap();
            let notify = json!({
                "method": "mining.notify",
                "params": {
                    "job_id": JOB_ID,
                    "header": "00".repeat(76),
                    "target": "ff".repeat(32),
                    "cert_version": 3
                }
            });
            write_line(&mut wr, &notify.to_string()).await.unwrap();

            let submit: Value =
                serde_json::from_str(&reader.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(submit["method"], "mining.submit");
            assert_eq!(submit["params"]["job_id"], JOB_ID);
            assert_eq!(submit["params"]["worker"], "prl1qtestwallet.rig0");
            let encoded = submit["params"]["plain_proof"]
                .as_str()
                .expect("plain_proof field");
            assert_eq!(decode_proof(encoded, expected).unwrap(), PROOF);
            let reply = json!({ "id": submit["id"], "result": true, "error": null });
            write_line(&mut wr, &reply.to_string()).await.unwrap();
        });

        let mut cfg = SessionConfig::new(
            "127.0.0.1",
            port,
            Dialect::KryptexV2,
            "prl1qtestwallet",
            "rig0",
        );
        cfg.tls = TlsMode::Off;
        let (session, mut events) = PoolSession::spawn(cfg, Connector::default());
        let mut authorized = false;
        loop {
            match events.recv().await.expect("session event") {
                SessionEvent::Authorized { proof_type } => {
                    assert_eq!(proof_type.as_deref(), ack_type);
                    authorized = true;
                }
                SessionEvent::JobReceived(job) => {
                    assert!(authorized, "authorize ack must precede the job");
                    assert_eq!(job.job_id, JOB_ID);
                    break;
                }
                SessionEvent::Disconnected { reason } => {
                    panic!("disconnected before job: {reason:?}")
                }
                _ => {}
            }
        }
        let submit_id = session.submit(JOB_ID, PROOF.to_vec()).await.unwrap();
        match events.recv().await.expect("submit event") {
            SessionEvent::ShareAccepted {
                submit_id: accepted_id,
                job_id,
            } => {
                assert_eq!(accepted_id, submit_id);
                assert_eq!(job_id, JOB_ID);
            }
            event => panic!("unexpected submit event: {event:?}"),
        }
        server.await.unwrap();
        session.shutdown().await;
    })
    .await
    .expect("authorize and submit complete in time");
}

#[tokio::test]
async fn missing_ack_type_falls_back_to_plain_on_the_wire() {
    assert_session_encoding(None, ProofEncoding::Plain).await;
}

#[tokio::test]
async fn unknown_ack_type_falls_back_to_plain_on_the_wire() {
    assert_session_encoding(Some("v3"), ProofEncoding::Plain).await;
}

#[tokio::test]
async fn explicit_plain_ack_sends_plain_on_the_wire() {
    assert_session_encoding(Some("plain"), ProofEncoding::Plain).await;
}

#[tokio::test]
async fn confirmed_v2_ack_sends_gzip_on_the_wire() {
    assert_session_encoding(Some("v2"), ProofEncoding::Gzip).await;
}
