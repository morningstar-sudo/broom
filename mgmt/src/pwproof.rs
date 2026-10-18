// pwproof.rs — sign in without sending the password (the web admin is plain HTTP on a shared LAN). The same logic as a
// WPA2 4-way handshake, in broom's own JSON (auth.rs routes; static/crypto.js is the browser side, capture.rs another
// client):
//   1. client → hello {user, cnonce}            2. server → {hs, salt, iter, snonce}
//   3. client → login {hs, proof}               4. server → {server_proof} + the session cookie
// K = PBKDF2-HMAC-SHA256(password, salt, iter); ClientKey = HMAC(K, "client"); the server keeps only
// stored = SHA256(ClientKey) and server = HMAC(K, "server"). proof = ClientKey ⊕ HMAC(stored, transcript): the server
// gets ClientKey back and checks its hash, so a copied account list logs nobody in, and a sniffed handshake gives
// neither the password nor a reusable proof (fresh nonces each time). server_proof = HMAC(server, transcript) shows
// the client it talks to the server that holds the account. Both ends then know the session key
// sk = HMAC(ClientKey, "session|" + transcript), never sent: a new password travels as its verifier masked with it
// (seal / open), the first one with the setup token.
use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

/// PBKDF2 rounds of new passwords (OWASP 2023 for PBKDF2-HMAC-SHA256; the browser runs them in plain JS: ~1 s).
pub(crate) const ITER: u32 = 600_000;
/// Rounds a stored / offered verifier may have (the client chose them; the server never runs them).
pub(crate) const ITER_RANGE: std::ops::RangeInclusive<u32> = 100_000..=10_000_000;

pub(crate) fn mac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut m = hmac::Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(msg);
    m.finalize().into_bytes().into()
}

fn xor(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// What a password gives: (ClientKey, stored, server).
pub(crate) fn keys(pw: &str, salt: &[u8], iter: u32) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let mut k = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), salt, iter, &mut k);
    let client = mac(&k, b"client");
    (client, Sha256::digest(client).into(), mac(&k, b"server"))
}

/// What both ends sign: the account and both nonces.
pub(crate) fn transcript(user: &str, cnonce: &str, snonce: &str) -> Vec<u8> {
    format!("broom-login|{user}|{cnonce}|{snonce}").into_bytes()
}

/// Client: the proof for this transcript.
pub(crate) fn proof(client: &[u8; 32], stored: &[u8; 32], t: &[u8]) -> [u8; 32] {
    xor(client, &mac(stored, t))
}

/// Server: the ClientKey inside a proof, if it hashes to `stored` (compared in constant time).
pub(crate) fn check(stored: &[u8; 32], t: &[u8], proof: &[u8; 32]) -> Option<[u8; 32]> {
    let client = xor(proof, &mac(stored, t));
    let h: [u8; 32] = Sha256::digest(client).into();
    (blake3::hash(&h) == blake3::hash(stored)).then_some(client)
}

pub(crate) fn session_key(client: &[u8; 32], t: &[u8]) -> [u8; 32] {
    mac(client, &[b"session|".as_slice(), t].concat())
}

/// The key a first-run setup is sealed with: from the setup token (printed in the server log, never sent).
pub(crate) fn setup_key(token: &str) -> [u8; 32] {
    mac(token.trim().as_bytes(), b"broom-setup")
}

/// A password's verifier as stored with the account (hex).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct Verifier {
    pub salt: String,
    pub iter: u32,
    pub stored: String,
    pub server: String,
}

/// A new verifier on its way to the server: stored / server masked with a key both ends know, `tag` over all of it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub(crate) struct Sealed {
    pub salt: String,
    pub iter: u32,
    pub stored: String,
    pub server: String,
    pub tag: String,
}

fn masks(key: &[u8; 32], user: &str, salt: &str, iter: u32) -> ([u8; 32], [u8; 32]) {
    let l = format!("{user}|{salt}|{iter}");
    (
        mac(key, format!("stored|{l}").as_bytes()),
        mac(key, format!("server|{l}").as_bytes()),
    )
}

fn tag(key: &[u8; 32], user: &str, s: &Sealed) -> [u8; 32] {
    let m = format!(
        "cred|{user}|{}|{}|{}|{}",
        s.salt, s.iter, s.stored, s.server
    );
    mac(key, m.as_bytes())
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn unhex32(h: &str) -> Option<[u8; 32]> {
    let b = unhex(h)?;
    b.try_into().ok()
}

pub(crate) fn unhex(h: &str) -> Option<Vec<u8>> {
    (h.len().is_multiple_of(2) && h.is_ascii())
        .then(|| {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok())
                .collect()
        })
        .flatten()
}

/// Client: a new password for `user`, sealed with `key` (the browser does it in crypto.js).
#[cfg(test)]
pub(crate) fn seal(key: &[u8; 32], user: &str, pw: &str, salt: &[u8]) -> Sealed {
    let salt = hex(salt);
    let (_, stored, server) = keys(pw, &unhex(&salt).unwrap_or_default(), ITER);
    let (m1, m2) = masks(key, user, &salt, ITER);
    let mut s = Sealed {
        salt,
        iter: ITER,
        stored: hex(&xor(&stored, &m1)),
        server: hex(&xor(&server, &m2)),
        tag: String::new(),
    };
    s.tag = hex(&tag(key, user, &s));
    s
}

/// Server: the verifier inside `s`, if it was sealed with `key` for `user` (else None: wrong key / tampered).
pub(crate) fn open(key: &[u8; 32], user: &str, s: &Sealed) -> Option<Verifier> {
    let want = tag(key, user, s);
    if !(unhex32(&s.tag).is_some_and(|t| blake3::hash(&t) == blake3::hash(&want)))
        || !ITER_RANGE.contains(&s.iter)
        || unhex(&s.salt).is_none_or(|b| !(8..=64).contains(&b.len()))
    {
        return None;
    }
    let (m1, m2) = masks(key, user, &s.salt, s.iter);
    Some(Verifier {
        salt: s.salt.clone(),
        iter: s.iter,
        stored: hex(&xor(&unhex32(&s.stored)?, &m1)),
        server: hex(&xor(&unhex32(&s.server)?, &m2)),
    })
}

#[cfg(test)]
#[path = "../tests/unit/pwproof.rs"]
mod tests;
