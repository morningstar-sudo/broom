use super::*;

/// RFC 4231 test cases 1, 2 and 6 (key longer than the block).
#[test]
fn hmac_sha256_vectors() {
    assert_eq!(
        hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        hex(&hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

/// The agent's proof: right key → its nonce; wrong key, another challenge, a malformed reply → refused.
#[test]
fn agent_proof() {
    let (k, ns, na) = ([3u8; 32], "a".repeat(32), "b".repeat(32));
    let p = |key: &[u8; 32], chal: &str| {
        format!(
            "proof {na} {}",
            hex(&hmac_sha256(key, format!("agent:{chal}:{na}").as_bytes()))
        )
    };
    assert_eq!(check_proof(&k, &ns, &p(&k, &ns)), Some(na.clone()));
    assert_eq!(check_proof(&k, &ns, &p(&[4u8; 32], &ns)), None, "wrong key");
    assert_eq!(
        check_proof(&k, &ns, &p(&k, &"c".repeat(32))),
        None,
        "replayed from another challenge"
    );
    assert_eq!(check_proof(&k, &ns, "proof short x"), None);
    assert_eq!(check_proof(&k, &ns, "key 00"), None);
    assert_eq!(check_proof(&k, &ns, ""), None);
}

/// A signed command: its tag covers the counter and the text — another counter or text does not verify.
#[test]
fn signed_commands() {
    let s = [9u8; 32];
    let m = signed(&s, 7, r#"{"op":"mouse","x":1}"#);
    let mut f = m.splitn(3, ' ');
    let (n, tag, json) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap());
    assert_eq!((n, json), ("7", r#"{"op":"mouse","x":1}"#));
    assert_eq!(tag, hex(&hmac_sha256(&s, format!("7 {json}").as_bytes())));
    assert_ne!(tag, hex(&hmac_sha256(&s, format!("8 {json}").as_bytes())));
    assert_eq!(unhex32(&"ab".repeat(32)), Some([0xab; 32]));
    assert_eq!(unhex32("ab"), None);
}

/// The whole handshake against an agent doing what broom-vnc.cs does: the right key → both sides end with the same
/// session key; a wrong key → refused.
#[tokio::test]
async fn handshake_both_ways() {
    async fn run(
        server_key: [u8; 32],
        agent_key: [u8; 32],
    ) -> (Option<[u8; 32]>, Option<[u8; 32]>) {
        let (to_agent, mut agent_in) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (agent_out, from_agent) =
            tokio::sync::mpsc::unbounded_channel::<Result<Message, axum::Error>>();
        let mut sink = Box::pin(futures_util::sink::unfold(
            to_agent,
            |tx, m: Message| async move {
                tx.send(m).map_err(|_| ())?;
                Ok::<_, ()>(tx)
            },
        ));
        let mut stream = Box::pin(futures_util::stream::unfold(
            from_agent,
            |mut rx| async move { rx.recv().await.map(|m| (m, rx)) },
        ));
        let agent = tokio::spawn(async move {
            let Some(Message::Text(ch)) = agent_in.recv().await else {
                return None;
            };
            let ns = ch.as_str().strip_prefix("auth ")?.to_string();
            let na = "c".repeat(32);
            let proof = hex(&hmac_sha256(
                &agent_key,
                format!("agent:{ns}:{na}").as_bytes(),
            ));
            agent_out
                .send(Ok(Message::Text(format!("proof {na} {proof}").into())))
                .ok()?;
            let Some(Message::Text(ok)) = agent_in.recv().await else {
                return None;
            };
            let want = hex(&hmac_sha256(
                &agent_key,
                format!("server:{ns}:{na}").as_bytes(),
            ));
            (ok.as_str() == format!("ok {want}"))
                .then(|| hmac_sha256(&agent_key, format!("session:{ns}:{na}").as_bytes()))
        });
        let s = handshake(&mut sink, &mut stream, &server_key, "test").await;
        drop(sink);
        (s, agent.await.unwrap())
    }
    let (s, a) = run([5u8; 32], [5u8; 32]).await;
    assert!(s.is_some() && s == a, "same session key on both sides");
    let (s, a) = run([5u8; 32], [6u8; 32]).await;
    assert!(s.is_none() && a.is_none(), "wrong key: refused, no session");
}
