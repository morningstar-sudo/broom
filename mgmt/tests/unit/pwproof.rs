use super::*;

/// PBKDF2-HMAC-SHA256 against RFC 7914 §11 (also what static/crypto.js must give).
#[test]
fn pbkdf2_vector() {
    let mut k = [0u8; 64];
    pbkdf2::pbkdf2_hmac::<Sha256>(b"passwd", b"salt", 1, &mut k);
    assert_eq!(
        hex(&k),
        "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
    );
}

/// The handshake: the right password proves itself and both ends get the same session key; a wrong password, another
/// transcript (replayed proof) or a tampered proof don't. A sealed verifier opens only with its key, for its account.
#[test]
fn handshake_and_sealing() {
    let salt = b"0123456789abcdef";
    let (client, stored, server) = keys("correct horse", salt, 1000);
    let t = transcript("admin", "c1", "s1");
    let p = proof(&client, &stored, &t);
    assert_eq!(check(&stored, &t, &p), Some(client));
    assert_eq!(
        session_key(&check(&stored, &t, &p).unwrap(), &t),
        session_key(&client, &t)
    );
    let (wrong, wstored, _) = keys("wrong", salt, 1000);
    assert_eq!(
        check(&stored, &t, &proof(&wrong, &wstored, &t)),
        None,
        "wrong password"
    );
    assert_eq!(
        check(&stored, &transcript("admin", "c1", "s2"), &p),
        None,
        "replayed"
    );
    let mut bad = p;
    bad[0] ^= 1;
    assert_eq!(check(&stored, &t, &bad), None, "tampered");
    assert_ne!(mac(&server, &t), mac(&stored, &t));

    let sk = session_key(&client, &t);
    let s = seal(&sk, "staff1", "new password", b"saltsaltsalt");
    let v = open(&sk, "staff1", &s).unwrap();
    let (_, st2, sv2) = keys("new password", &unhex(&v.salt).unwrap(), v.iter);
    assert_eq!((v.stored, v.server), (hex(&st2), hex(&sv2)));
    assert!(
        open(&sk, "admin", &s).is_none(),
        "sealed for another account"
    );
    assert!(
        open(&setup_key("tok"), "staff1", &s).is_none(),
        "another key"
    );
    let mut t2 = s.clone();
    t2.stored.replace_range(
        ..2,
        if s.stored.starts_with("00") {
            "11"
        } else {
            "00"
        },
    );
    assert!(open(&sk, "staff1", &t2).is_none(), "tampered");
}

/// static/crypto.js gives the same keys and seals (its values, made with node, pinned here): the browser and the
/// server agree.
#[test]
fn same_as_the_browser() {
    let (client, stored, server) = keys("correct horse", b"0123456789abcdef", 1000);
    assert_eq!(
        [hex(&client), hex(&stored), hex(&server)],
        [
            "782b833d6612b1b7cd9e8681e9b7e503aaa94969c7aa3f13ff44bb6032691f56",
            "5ab388351aa7a2f880b27b89ca9da7745b29e0418ede015eb5e5a1210ca35af8",
            "dc742ef7eade29afea76b33ac0493f4539ce03e54dabde825d7342a123308539"
        ]
    );
    assert_eq!(
        hex(&setup_key(" tok ")),
        "7f13e102df9371951601b44031b8205aa178f5eb8ce928fa20987369b43507a7"
    );
    // sealPw(key = 32 × 0x07, "staff1", "new password") in the browser.
    let s: Sealed = serde_json::from_str(r#"{"salt":"965ea71cf5e3b293335647a59f71aea1","iter":600000,"stored":"aeee1b27a0346d1ca7865dce8e35cf45cdd79c9944c512fc232468249767ea01","server":"921e3c70dee1d2a5dcf40f988f382efe0b394337526cc1ff78ee5231edd048a7","tag":"1ed5897ba161511104cf57d35a19e2bc3c062f40920b3faaca19ca9c974b72d0"}"#).unwrap();
    let v = open(&[7u8; 32], "staff1", &s).unwrap();
    let (_, st, sv) = keys("new password", &unhex(&v.salt).unwrap(), v.iter);
    assert_eq!((v.stored, v.server), (hex(&st), hex(&sv)));
}
