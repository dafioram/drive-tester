//! Self-describing test data.
//!
//! Every 512-byte unit written by a test carries a header (magic, run seed,
//! unit number) followed by pseudo-random bytes derived from the seed and unit.
//! Reading a unit back tells us not only *that* it is wrong but often *why*:
//! a valid header with the right seed and a different unit number means a
//! write landed somewhere else — the signature of a fake-capacity drive.

/// Size of one signed unit. Independent of the drive's sector size.
pub const UNIT: usize = 512;

/// Header magic.
pub const MAGIC: [u8; 8] = *b"DRVTEST1";

/// Fast deterministic PRNG (SplitMix64).
#[derive(Debug, Clone)]
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..bound` (bound > 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    /// Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

/// A seed for a new run, mixed from the clock.
pub fn fresh_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5EED);
    SplitMix64::new(nanos ^ 0xD1B5_4A32_D192_ED03).next_u64()
}

/// Fills one 512-byte unit.
pub fn fill_unit(buf: &mut [u8], seed: u64, unit: u64) {
    debug_assert_eq!(buf.len(), UNIT);
    buf[0..8].copy_from_slice(&MAGIC);
    buf[8..16].copy_from_slice(&seed.to_le_bytes());
    buf[16..24].copy_from_slice(&unit.to_le_bytes());
    let mut rng = SplitMix64::new(seed ^ unit.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    for chunk in buf[24..].chunks_exact_mut(8) {
        chunk.copy_from_slice(&rng.next_u64().to_le_bytes());
    }
}

/// Fills a buffer (a multiple of 512 bytes) with consecutive signed units.
pub fn fill_signed(buf: &mut [u8], seed: u64, first_unit: u64) {
    for (i, chunk) in buf.chunks_exact_mut(UNIT).enumerate() {
        fill_unit(chunk, seed, first_unit + i as u64);
    }
}

/// Result of checking one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitCheck {
    Ok,
    /// A valid unit from this run, but written for a different position.
    Aliased {
        found_unit: u64,
    },
    /// Anything else: stale data, zeros, corruption.
    Corrupt,
}

/// Checks one unit against what should be there.
pub fn check_unit(buf: &[u8], seed: u64, unit: u64, scratch: &mut [u8; UNIT]) -> UnitCheck {
    fill_unit(scratch, seed, unit);
    if buf == &scratch[..] {
        return UnitCheck::Ok;
    }
    if buf[0..8] == MAGIC && buf[8..16] == seed.to_le_bytes() {
        let found = u64::from_le_bytes(buf[16..24].try_into().expect("8 bytes"));
        if found != unit {
            fill_unit(scratch, seed, found);
            if buf == &scratch[..] {
                return UnitCheck::Aliased { found_unit: found };
            }
        }
    }
    UnitCheck::Corrupt
}

/// Counts of bad units in a buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferCheck {
    pub corrupt: u64,
    pub aliased: u64,
    /// Index (within the buffer) of the first bad unit.
    pub first_bad: Option<u64>,
    /// First aliased unit seen: (unit expected here, unit actually found).
    pub first_alias: Option<(u64, u64)>,
}

impl BufferCheck {
    pub fn bad(&self) -> u64 {
        self.corrupt + self.aliased
    }
}

/// Checks a buffer of consecutive units.
pub fn check_signed(buf: &[u8], seed: u64, first_unit: u64) -> BufferCheck {
    let mut scratch = [0u8; UNIT];
    let mut result = BufferCheck::default();
    for (i, chunk) in buf.chunks_exact(UNIT).enumerate() {
        match check_unit(chunk, seed, first_unit + i as u64, &mut scratch) {
            UnitCheck::Ok => continue,
            UnitCheck::Aliased { found_unit } => {
                result.aliased += 1;
                result.first_alias.get_or_insert((first_unit + i as u64, found_unit));
            }
            UnitCheck::Corrupt => result.corrupt += 1,
        }
        result.first_bad.get_or_insert(i as u64);
    }
    result
}

/// Reads the header of a unit if it carries this run's signature.
pub fn signed_unit_number(buf: &[u8], seed: u64) -> Option<u64> {
    if buf.len() >= 24 && buf[0..8] == MAGIC && buf[8..16] == seed.to_le_bytes() {
        Some(u64::from_le_bytes(buf[16..24].try_into().ok()?))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_data_round_trips() {
        let mut buf = vec![0u8; UNIT * 8];
        fill_signed(&mut buf, 42, 1000);
        assert_eq!(check_signed(&buf, 42, 1000), BufferCheck::default());
    }

    #[test]
    fn aliasing_and_corruption_are_distinguished() {
        let mut buf = vec![0u8; UNIT * 4];
        fill_signed(&mut buf, 7, 0);
        // Unit 2 now holds unit 900's data (wraparound), unit 3 is zeroed.
        let mut other = vec![0u8; UNIT];
        fill_unit(&mut other, 7, 900);
        buf[UNIT * 2..UNIT * 3].copy_from_slice(&other);
        buf[UNIT * 3..].fill(0);
        let r = check_signed(&buf, 7, 0);
        assert_eq!(r.aliased, 1);
        assert_eq!(r.corrupt, 1);
        assert_eq!(r.first_bad, Some(2));
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut v: Vec<u32> = (0..100).collect();
        SplitMix64::new(1).shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..100).collect::<Vec<_>>());
        assert_ne!(v, sorted);
    }
}
