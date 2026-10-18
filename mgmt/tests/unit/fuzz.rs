// Inputs for the parsers' robustness checks (no new dependency, no nightly): random bytes and mutations of a valid
// sample — bytes overwritten, 32-bit fields set to edge values, cuts, repeats, insertions. Deterministic (fixed seed),
// so a failure reproduces; `run` names the case that panicked. Every parser of data from the LAN or an upload must
// return an error on these, never panic, hang or ask for a huge buffer.
// ponytail: mutation fuzzing in the unit tests, cargo-fuzz (coverage-guided, nightly) if a parser keeps growing.

/// xorshift64*: small, fast, good enough to pick offsets and bytes.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    /// 0..n (n > 0).
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub(crate) fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// `seed` changed once at random.
pub(crate) fn mutate(r: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    let at = |r: &mut Rng, b: &[u8]| r.below(b.len().max(1));
    match r.below(7) {
        // A few bytes anywhere.
        0 => {
            for _ in 0..1 + r.below(8) {
                if !b.is_empty() {
                    let i = at(r, &b);
                    b[i] = r.next() as u8;
                }
            }
        }
        // A length / offset field: 0, all ones, the sign bit, random.
        1 if b.len() >= 4 => {
            let i = r.below(b.len() - 3);
            let v: u32 = [0, u32::MAX, 0x8000_0000, 0x7fff_ffff, 1, r.next() as u32][r.below(6)];
            b[i..i + 4].copy_from_slice(&v.to_le_bytes());
        }
        // The same, big-endian (network protocols).
        2 if b.len() >= 4 => {
            let i = r.below(b.len() - 3);
            let v: u32 = [0, u32::MAX, 0x8000_0000, 0xffff, r.next() as u32][r.below(5)];
            b[i..i + 4].copy_from_slice(&v.to_be_bytes());
        }
        // Cut short.
        3 => b.truncate(at(r, &b)),
        // A piece repeated (loops, compression pointers).
        4 if !b.is_empty() => {
            let i = at(r, &b);
            let n = 1 + r.below((b.len() - i).min(64));
            let piece = b[i..i + n].to_vec();
            let j = at(r, &b);
            b.splice(j..j, piece);
        }
        // Random bytes inserted.
        5 => {
            let j = at(r, &b);
            let n = 1 + r.below(32);
            let junk = r.bytes(n);
            b.splice(j..j, junk);
        }
        // A run of 0x00 / 0xff.
        _ if !b.is_empty() => {
            let i = at(r, &b);
            let n = 1 + r.below((b.len() - i).min(256));
            let v = if r.below(2) == 0 { 0 } else { 0xff };
            b[i..i + n].fill(v);
        }
        _ => {}
    }
    b
}

/// `n` inputs (× BROOM_FUZZ for a deeper run, e.g. BROOM_FUZZ=50 cargo test fuzz_; another seed with BROOM_FUZZ_SEED),
/// made one at a time: the samples themselves, then each one 1–4 times mutated, every 8th pure noise up to twice the
/// size.
pub(crate) fn inputs(seeds: &[Vec<u8>], n: usize) -> impl Iterator<Item = Vec<u8>> + '_ {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let n = n * env("BROOM_FUZZ").unwrap_or(1).max(1) as usize;
    let mut r = Rng::new(env("BROOM_FUZZ_SEED").unwrap_or(0x5eed_b100));
    let max = seeds.iter().map(Vec::len).max().unwrap_or(64).max(64);
    (0..n.max(seeds.len())).map(move |i| {
        if i < seeds.len() {
            seeds[i].clone()
        } else if i.is_multiple_of(8) {
            let len = r.below(2 * max);
            r.bytes(len)
        } else {
            let mut b = seeds[r.below(seeds.len())].clone();
            for _ in 0..1 + r.below(4) {
                b = mutate(&mut r, &b);
            }
            b
        }
    })
}

/// `f` on every input; a panic is reported with the case number and the start of the input.
pub(crate) fn run(name: &str, inputs: impl IntoIterator<Item = Vec<u8>>, f: impl Fn(&[u8])) {
    for (i, b) in inputs.into_iter().enumerate() {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&b))) {
            let head: String = b.iter().take(64).map(|x| format!("{x:02x}")).collect();
            panic!(
                "{name}: case {i} ({} bytes, starts {head}) panicked: {:?}",
                b.len(),
                e.downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
            );
        }
    }
}
