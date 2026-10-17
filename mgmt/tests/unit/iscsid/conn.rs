use super::*;

/// A client IP is "connected" while a session holds its counter; its bytes stay counted after it logs out.
#[test]
fn peer_connected_while_held() {
    let ip = "192.0.2.77";
    let c = peer_counter(ip);
    c.fetch_add(5, Ordering::Relaxed);
    let get = || peers().into_iter().find(|p| p.ip == ip).unwrap();
    assert!(get().connected && get().read_bytes == 5);
    drop(c);
    assert!(!get().connected && get().read_bytes == 5);
}

#[test]
fn login_keys_answered() {
    assert_eq!(
        answer("HeaderDigest", "CRC32C,None").as_deref(),
        Some("None")
    );
    assert_eq!(answer("DataDigest", "CRC32C").as_deref(), Some("Reject"));
    assert_eq!(answer("AuthMethod", "CHAP,None").as_deref(), Some("None"));
    assert_eq!(
        answer("MaxBurstLength", "16776192").as_deref(),
        Some("262144")
    );
    assert_eq!(answer("MaxBurstLength", "65536").as_deref(), Some("65536"));
    assert_eq!(answer("ImmediateData", "Yes").as_deref(), Some("No"));
    assert_eq!(
        answer("X-com.example", "1").as_deref(),
        Some("NotUnderstood")
    );
    assert_eq!(
        answer("MaxRecvDataSegmentLength", "8192"),
        None,
        "declarative"
    );
    assert_eq!(
        keys(b"A=1\0B=x=y\0\0"),
        [("A".into(), "1".into()), ("B".into(), "x=y".into())]
    );
}

/// A machine known to be off: every session of its IP told to close, the others left alone.
#[tokio::test]
async fn kick_closes_one_client_only() {
    let (a, b, c) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    {
        let mut s = ACTIVE.lock();
        s.insert(("kick-t-a".into(), [1; 6]), (a.clone(), "10.9.9.1".into()));
        s.insert(("kick-t-b".into(), [2; 6]), (b.clone(), "10.9.9.1".into()));
        s.insert(("kick-t-c".into(), [3; 6]), (c.clone(), "10.9.9.2".into()));
    }
    assert_eq!(kick_peer("10.9.9.1"), 2);
    let quick = |n: Arc<Notify>| async move {
        tokio::time::timeout(Duration::from_millis(50), n.notified())
            .await
            .is_ok()
    };
    assert!(quick(a).await && quick(b).await, "both told to close");
    assert!(!quick(c).await, "another client untouched");
    ACTIVE.lock().retain(|k, _| !k.0.starts_with("kick-t-"));
}
