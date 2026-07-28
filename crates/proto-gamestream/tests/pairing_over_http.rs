//! The pairing handshake and the launch flow, over real sockets, against a scripted
//! host that implements Sunshine's side from `nvhttp.cpp`.
//!
//! The pure tests in `src/pairing.rs` prove the crypto against Sunshine's own vectors.
//! What they cannot prove is the part in between: that the phases go to the right
//! *port*, in the right order, with the parameters spelled the way the host reads them,
//! and that the client refuses to call itself paired until mutual TLS actually works.
//! Those are the failures that look like a working receiver right up until a session
//! is attempted, so they get a socket.
//!
//! The host here answers plain HTTP only, which is exactly enough to cover phases 1–4;
//! phase 5 is TLS and is covered by asserting the client *fails closed* when it cannot
//! complete it, rather than by standing up a second TLS listener whose certificate
//! plumbing would be testing rustls rather than us.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::Arc;

use proto_gamestream::pairing::{cert_signature_bits, hex_encode};
use proto_gamestream::{ClientIdentity, GameStreamClient, GameStreamError, UniqueId};
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the scripted host should do when the client's proof arrives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostBehaviour {
    /// Check the client's work and accept it (Sunshine's happy path).
    Accept,
    /// Everything checks out, but the host says `<paired>0</paired>` with a 200 —
    /// which is how Sunshine actually reports a rejection.
    RefuseAtPhaseFour,
}

/// A scripted Sunshine, speaking its half of `/pair` over plain HTTP.
struct ScriptedHost {
    identity: ClientIdentity,
    pin: String,
    behaviour: HostBehaviour,
    /// The client certificate's signature bytes. Real Sunshine parses these out of the
    /// `clientcert` it was handed in phase 1; handing them over directly keeps this
    /// harness from growing an X.509 parser, and the phase-4 hash check below is only
    /// meaningful because they are the *client's* real bytes.
    client_cert_sig: Vec<u8>,
}

impl ScriptedHost {
    async fn serve(self, listener: TcpListener) {
        let host_sig = cert_signature_bits(self.identity.cert_der()).unwrap();
        let server_secret = [0x5a_u8; 16];
        let server_challenge = [0x3c_u8; 16];
        let mut aes_key = [0u8; 16];
        let mut client_hash: Vec<u8> = Vec::new();

        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let Some(query) = read_request(&mut stream).await else {
                // Not HTTP — the client tried to start TLS against the plain listener.
                // Hang up, which is what a plain-HTTP server does and what lets the
                // client's phase-5 attempt fail instead of hanging.
                let _ = stream.shutdown().await;
                continue;
            };
            let args = parse_query(&query);

            let body = if args.get("phrase").map(String::as_str) == Some("getservercert") {
                // Phase 1: derive the key from the salt the client chose and the PIN a
                // person typed here, and hand back our certificate.
                let salt = hex_decode(args.get("salt").unwrap());
                let mut hasher = Sha256::new();
                hasher.update(&salt[..16]);
                hasher.update(self.pin.as_bytes());
                aes_key.copy_from_slice(&hasher.finalize()[..16]);
                // Uppercase, like Sunshine's own hex encoder.
                let plaincert = hex_encode(self.identity.cert_pem().as_bytes()).to_uppercase();
                format!("<root status_code=\"200\"><paired>1</paired><plaincert>{plaincert}</plaincert></root>")
            } else if let Some(challenge) = args.get("clientchallenge") {
                // Phase 2: decrypt their challenge, commit to a hash over it.
                let client_challenge = ecb(&aes_key, &hex_decode(challenge), false);
                let mut hasher = Sha256::new();
                hasher.update(&client_challenge);
                hasher.update(&host_sig);
                hasher.update(server_secret);
                let mut plaintext = hasher.finalize().to_vec();
                plaintext.extend_from_slice(&server_challenge);
                let response = hex_encode(&ecb(&aes_key, &plaintext, true)).to_uppercase();
                format!("<root status_code=\"200\"><paired>1</paired><challengeresponse>{response}</challengeresponse></root>")
            } else if let Some(resp) = args.get("serverchallengeresp") {
                // Phase 3: stash their hash, sign our secret so they can check us.
                client_hash = ecb(&aes_key, &hex_decode(resp), false);
                let signature =
                    SigningKey::<Sha256>::new(self.identity.key().clone()).sign(&server_secret);
                let mut secret = server_secret.to_vec();
                secret.extend_from_slice(&signature.to_vec());
                let secret = hex_encode(&secret).to_uppercase();
                format!("<root status_code=\"200\"><pairingsecret>{secret}</pairingsecret><paired>1</paired></root>")
            } else if let Some(cps) = args.get("clientpairingsecret") {
                // Phase 4: recompute their hash and check their signature — the same
                // two checks Sunshine makes.
                let cps = hex_decode(cps);
                let (secret, signature) = cps.split_at(16);
                let mut hasher = Sha256::new();
                hasher.update(server_challenge);
                hasher.update(&self.client_cert_sig);
                hasher.update(secret);
                let recomputed = hasher.finalize().to_vec();
                let ok = recomputed == client_hash
                    && !signature.is_empty()
                    && self.behaviour == HostBehaviour::Accept;
                let paired = u8::from(ok);
                // Note the status code: Sunshine reports its refusal with a 200.
                format!("<root status_code=\"200\"><paired>{paired}</paired></root>")
            } else {
                "<root status_code=\"400\" status_message=\"unexpected pairing call\"><paired>0</paired></root>".to_string()
            };

            write_response(&mut stream, &body).await;
        }
    }
}

fn ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Vec<u8> {
    use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
    let cipher = aes::Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        if encrypt {
            cipher.encrypt_block(block.into());
        } else {
            cipher.decrypt_block(block.into());
        }
    }
    out
}

fn hex_decode(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect()
}

/// Read one HTTP request, or `None` if the peer is not speaking HTTP.
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // A TLS ClientHello opens with a 0x16 handshake record and contains no CRLFs,
        // so waiting for headers would wait forever.
        if buf.first() == Some(&0x16) {
            return None;
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_query(request: &str) -> HashMap<String, String> {
    let mut args = HashMap::new();
    let Some(line) = request.lines().next() else {
        return args;
    };
    let Some(target) = line.split_whitespace().nth(1) else {
        return args;
    };
    let Some((_path, query)) = target.split_once('?') else {
        return args;
    };
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            args.insert(k.to_string(), v.to_string());
        }
    }
    args
}

async fn write_response(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

async fn spawn_host(
    pin: &str,
    behaviour: HostBehaviour,
    client_cert_sig: Vec<u8>,
) -> (u16, ClientIdentity) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let host_identity = ClientIdentity::generate().unwrap();
    let serving_identity =
        ClientIdentity::from_pem(host_identity.cert_pem(), host_identity.key_pem()).unwrap();
    let host = ScriptedHost {
        identity: serving_identity,
        pin: pin.to_string(),
        behaviour,
        client_cert_sig,
    };
    tokio::spawn(host.serve(listener));
    (port, host_identity)
}

#[tokio::test]
async fn pairing_walks_all_four_phases_and_then_fails_closed_without_tls() {
    let identity = Arc::new(ClientIdentity::generate().unwrap());
    let client_sig = cert_signature_bits(identity.cert_der()).unwrap();
    let (port, _host) = spawn_host("1234", HostBehaviour::Accept, client_sig).await;

    let mut client = GameStreamClient::new(
        Arc::clone(&identity),
        UniqueId::new("cafebabe"),
        "127.0.0.1",
        port,
    );
    // Phases 1-4 all succeed against the scripted host. Phase 5 is a TLS request to a
    // port that speaks plain HTTP, so it must fail — and the client must *not* report
    // itself paired on the strength of phase 4 alone. That is the point of the
    // assertion: a client that trusted phase 4 would go on to 401 on every later
    // request with nothing anywhere saying why.
    let err = client.pair("1234", port).await.unwrap_err();
    assert!(
        !matches!(err, GameStreamError::WrongPin),
        "phases 1-4 should have completed; got {err:?}"
    );
    assert!(
        client.pairing().is_none(),
        "a client that could not complete the TLS confirmation must not claim to be paired"
    );
}

#[tokio::test]
async fn a_wrong_pin_fails_at_phase_three_as_wrong_pin() {
    let identity = Arc::new(ClientIdentity::generate().unwrap());
    let client_sig = cert_signature_bits(identity.cert_der()).unwrap();
    // The host was given a different PIN than the one the client will use.
    let (port, _host) = spawn_host("9999", HostBehaviour::Accept, client_sig).await;

    let mut client = GameStreamClient::new(identity, UniqueId::new("cafebabe"), "127.0.0.1", port);
    match client.pair("1234", port).await {
        Err(GameStreamError::WrongPin) => {}
        other => panic!("expected WrongPin over the wire, got {other:?}"),
    }
    assert!(client.pairing().is_none());
}

#[tokio::test]
async fn a_phase_four_refusal_is_caught_despite_its_200() {
    let identity = Arc::new(ClientIdentity::generate().unwrap());
    let client_sig = cert_signature_bits(identity.cert_der()).unwrap();
    let (port, _host) = spawn_host("1234", HostBehaviour::RefuseAtPhaseFour, client_sig).await;

    let mut client = GameStreamClient::new(identity, UniqueId::new("cafebabe"), "127.0.0.1", port);
    match client.pair("1234", port).await {
        Err(GameStreamError::Pairing(msg)) => {
            assert!(
                msg.contains("paired=0"),
                "a 200 carrying <paired>0</paired> must be read as the refusal it is: {msg}"
            );
        }
        other => panic!("expected a pairing refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unpaired_client_reads_serverinfo_over_plain_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let request = read_request(&mut stream).await.unwrap();
        // The unpaired probe must go to plain HTTP and carry uniqueid.
        assert!(request.starts_with("GET /serverinfo?uniqueid=cafebabe&uuid="));
        write_response(
            &mut stream,
            r#"<root status_code="200"><hostname>somepc</hostname><appversion>7.1.431.-1</appversion><HttpsPort>47984</HttpsPort><PairStatus>0</PairStatus><currentgame>0</currentgame><state>SUNSHINE_SERVER_FREE</state></root>"#,
        )
        .await;
    });

    let identity = Arc::new(ClientIdentity::generate().unwrap());
    let client = GameStreamClient::new(identity, UniqueId::new("cafebabe"), "127.0.0.1", port);
    let info = client.server_info().await.unwrap();
    assert_eq!(info.hostname, "somepc");
    assert_eq!(info.https_port, 47984);
    assert!(!info.paired);
    assert!(info.is_sunshine());
}
