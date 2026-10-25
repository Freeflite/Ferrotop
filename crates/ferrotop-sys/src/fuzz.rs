//! Deterministic mutation testing for parsers: no dependencies, runs with every `cargo test`.
//!
//! Each run starts from a valid seed and applies random truncations, bit flips, byte
//! replacements, insertions, deletions and span duplications, plus fully random inputs. The
//! generator is a fixed-seed xorshift, so any failure reproduces exactly.

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// Calls `check` on `iterations` mutated copies of `seed` (and some random buffers).
pub(crate) fn mutate(seed: &[u8], iterations: usize, mut check: impl FnMut(&[u8])) {
    // Miri runs about a thousand times slower; a few dozen rounds still cover every mutation kind.
    let iterations = if cfg!(miri) { iterations.min(40) } else { iterations };
    let mut rng = Rng::new(0x0F3E_770F_u64 ^ seed.len() as u64);
    check(seed);
    check(&[]);
    for i in 0..iterations {
        let mut data = seed.to_vec();
        if i % 16 == 15 {
            data = (0..rng.below(256)).map(|_| rng.next() as u8).collect();
        } else {
            for _ in 0..=rng.below(4) {
                mutate_once(&mut rng, &mut data);
            }
        }
        check(&data);
    }
}

fn mutate_once(rng: &mut Rng, data: &mut Vec<u8>) {
    let len = data.len();
    match rng.below(6) {
        0 => data.truncate(rng.below(len + 1)),
        1 if len > 0 => {
            let i = rng.below(len);
            data[i] ^= 1 << rng.below(8);
        }
        2 if len > 0 => {
            let i = rng.below(len);
            // Bias toward boundary values that trip length and offset checks.
            data[i] = [0x00, 0xFF, 0x7F, 0x80, b'"', b'\\', b'{', b'[', rng.next() as u8][rng.below(9)];
        }
        3 => {
            let i = rng.below(len + 1);
            data.insert(i, rng.next() as u8);
        }
        4 if len > 0 => {
            let i = rng.below(len);
            let n = rng.below(len - i + 1);
            data.drain(i..i + n);
        }
        5 if len > 0 => {
            let i = rng.below(len);
            let n = rng.below((len - i).min(64) + 1);
            let span: Vec<u8> = data[i..i + n].to_vec();
            let at = rng.below(data.len() + 1);
            data.splice(at..at, span);
        }
        _ => {}
    }
}
