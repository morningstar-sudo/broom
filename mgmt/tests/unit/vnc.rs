use super::*;
use axum::http::HeaderValue;

fn text(m: Message) -> String {
    match m {
        Message::Text(t) => t.to_string(),
        m => panic!("not text: {m:?}"),
    }
}

/// Agent ↔ viewer bookkeeping: lookup by hostname (any case), updates reach the current viewer only, a new viewer
/// replaces the old (whose queue closes), a newer agent connection replaces the old one, the agent leaving closes the
/// viewer.
#[tokio::test]
async fn agents_and_viewers() {
    let mac = "aa:bb:cc:00:00:01";
    let (ta, mut agent_q) = mpsc::channel(QUEUE);
    let a1 = register(mac, "PC-01", ta);
    assert!(connected("AA:BB:CC:00:00:01"));
    let (fmac, host, to_agent) = find("pc-01").expect("found by hostname, any case");
    assert_eq!((fmac.as_str(), host.as_str()), (mac, "PC-01"));
    assert!(find("pc-02").is_none());
    assert!(
        !to_viewer(mac, a1, Message::Binary(vec![1].into())),
        "nobody watching"
    );

    let (v1, mut v1q) = mpsc::channel(QUEUE);
    assert!(attach(mac, 10, v1));
    to_agent.send(start_msg(60)).await.unwrap();
    assert_eq!(
        text(agent_q.recv().await.unwrap()),
        r#"{"op":"start","q":60}"#
    );
    assert!(to_viewer(mac, a1, Message::Binary(vec![1, 2].into())));
    assert!(matches!(v1q.recv().await, Some(Message::Binary(b)) if b[..] == [1, 2]));

    // A second viewer takes over: the first one's queue closes, detaching it does not stop the agent.
    let (v2, mut v2q) = mpsc::channel(QUEUE);
    assert!(attach(mac, 11, v2));
    assert!(v1q.recv().await.is_none(), "replaced viewer closed");
    assert!(!detach(mac, 10), "old viewer: agent keeps streaming");
    assert!(to_viewer(mac, a1, Message::Binary(vec![3].into())));
    assert!(v2q.recv().await.is_some());

    // The machine's agent reconnects: the old connection's updates go nowhere and its end leaves the new one listed.
    let (tb, _agent2_q) = mpsc::channel(QUEUE);
    let a2 = register(mac, "PC-01", tb);
    assert!(
        v2q.recv().await.is_none(),
        "viewer of the old connection closed"
    );
    assert!(!to_viewer(mac, a1, Message::Binary(vec![4].into())));
    unregister(mac, a1);
    assert!(connected(mac), "the newer connection stays");
    assert!(!detach(mac, 11));

    let (v3, mut v3q) = mpsc::channel(QUEUE);
    assert!(attach(mac, 12, v3));
    assert!(detach(mac, 12), "current viewer: the agent is told to stop");
    attach(mac, 13, mpsc::channel(QUEUE).0);
    unregister(mac, a2);
    assert!(!connected(mac) && find("PC-01").is_none());
    assert!(v3q.recv().await.is_none());
    assert!(!attach(mac, 14, mpsc::channel(QUEUE).0), "agent gone");
}

#[test]
fn origin_check() {
    let h = |origin: Option<&str>, host: &str| {
        let mut m = HeaderMap::new();
        m.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(o) = origin {
            m.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        same_origin(&m)
    };
    assert!(h(Some("http://10.0.0.12"), "10.0.0.12"));
    assert!(h(Some("http://10.0.0.12:8080"), "10.0.0.12:8080"));
    assert!(h(None, "10.0.0.12"), "not a browser");
    assert!(!h(Some("http://evil.example"), "10.0.0.12"));
    assert!(!h(Some("http://10.0.0.12:9"), "10.0.0.12"));
    assert!(!h(Some("null"), "10.0.0.12"));
}
