//! Count arithmetic modulo 2^64, the prime 2^61 - 1 or a 16-bit modulus, and the CRT that
//! combines runs.

/// Counts kept modulo a fixed number; each transfer-matrix run uses one modulus.
pub trait Ring: Copy + Send + Sync + PartialEq + std::fmt::Debug + 'static {
    const ZERO: Self;
    const ONE: Self;
    fn add(self, other: Self) -> Self;
    fn sub(self, other: Self) -> Self;
    /// The residue in [0, modulus).
    fn residue(self) -> u64;
    const MODULUS: u128;
    /// Bytes of a count in a bucket record.
    const BYTES: usize;
    /// Writes the count little-endian into `out[..BYTES]`.
    fn write(self, out: &mut [u8]);
    /// Reads a count written by `write`.
    fn read(bytes: &[u8]) -> Self;

    fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

/// Counts modulo 2^64 (wrapping arithmetic).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Wrap64(pub u64);

impl Ring for Wrap64 {
    const ZERO: Self = Wrap64(0);
    const ONE: Self = Wrap64(1);
    const MODULUS: u128 = 1 << 64;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        Wrap64(self.0.wrapping_add(other.0))
    }

    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        Wrap64(self.0.wrapping_sub(other.0))
    }

    fn residue(self) -> u64 {
        self.0
    }

    const BYTES: usize = 8;

    fn write(self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.0.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        Wrap64(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
    }
}

/// Counts modulo 2^16, for low-memory consistency checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Wrap16(pub u16);

impl Ring for Wrap16 {
    const ZERO: Self = Wrap16(0);
    const ONE: Self = Wrap16(1);
    const MODULUS: u128 = 1 << 16;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        Wrap16(self.0.wrapping_add(other.0))
    }

    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        Wrap16(self.0.wrapping_sub(other.0))
    }

    fn residue(self) -> u64 {
        self.0 as u64
    }

    const BYTES: usize = 2;

    fn write(self, out: &mut [u8]) {
        out[..2].copy_from_slice(&self.0.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        Wrap16(u16::from_le_bytes(bytes[..2].try_into().unwrap()))
    }
}

/// Counts modulo a prime P < 2^16, for low-memory runs that the CRT combines.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ModP16<const P: u16>(pub u16);

impl<const P: u16> Ring for ModP16<P> {
    const ZERO: Self = ModP16(0);
    const ONE: Self = ModP16(1);
    const MODULUS: u128 = P as u128;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        let s = self.0 as u32 + other.0 as u32;
        ModP16(if s >= P as u32 { s - P as u32 } else { s } as u16)
    }

    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        let d = self.0 as u32 + P as u32 - other.0 as u32;
        ModP16(if d >= P as u32 { d - P as u32 } else { d } as u16)
    }

    fn residue(self) -> u64 {
        self.0 as u64
    }

    const BYTES: usize = 2;

    fn write(self, out: &mut [u8]) {
        out[..2].copy_from_slice(&self.0.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        ModP16(u16::from_le_bytes(bytes[..2].try_into().unwrap()))
    }
}

/// The Mersenne prime 2^61 - 1.
pub const P61: u64 = (1 << 61) - 1;

/// Counts modulo 2^61 - 1.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mod61(pub u64);

impl Ring for Mod61 {
    const ZERO: Self = Mod61(0);
    const ONE: Self = Mod61(1);
    const MODULUS: u128 = P61 as u128;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        let s = self.0 + other.0;
        Mod61(if s >= P61 { s - P61 } else { s })
    }

    #[inline(always)]
    fn sub(self, other: Self) -> Self {
        Mod61(if self.0 >= other.0 {
            self.0 - other.0
        } else {
            self.0 + P61 - other.0
        })
    }

    fn residue(self) -> u64 {
        self.0
    }

    const BYTES: usize = 8;

    fn write(self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.0.to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        Mod61(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
    }
}

/// The x in [0, 2^64 (2^61 - 1)) with x = r64 (mod 2^64) and x = r61 (mod 2^61 - 1).
pub fn crt(r64: u64, r61: u64) -> u128 {
    // 2^64 = 8 (mod 2^61 - 1), and 8 * 2^58 = 2^61 = 1, so 2^58 inverts 2^64.
    let diff = (r61 + P61 - r64 % P61) % P61;
    let t = (diff as u128 * (1u128 << 58)) % P61 as u128;
    r64 as u128 + (t << 64)
}

/// The 16-bit moduli, in the order runs take them: 2^16, then primes below it.
pub const MODULI_16: [u32; 8] = [65536, 65521, 65519, 65497, 65479, 65449, 65447, 65437];

/// The first of `MODULI_16` whose product passes 4^n, which bounds F(n) and R2(n).
pub fn moduli_16(n: usize) -> &'static [u32] {
    let mut product = 1u128;
    for (k, &m) in MODULI_16.iter().enumerate() {
        product *= m as u128;
        if n < 64 && product > 1u128 << (2 * n) {
            return &MODULI_16[..=k];
        }
    }
    panic!("n = {n} needs more 16-bit moduli than MODULI_16 has")
}

/// Something to run with the ring of a 16-bit modulus.
pub trait WithRing16 {
    type Out;
    fn run<R: Ring>(self, modulus: u32) -> Self::Out;
}

/// Runs `job` with the ring of `modulus`, or returns None if it is not one of `MODULI_16`.
pub fn with_ring_16<J: WithRing16>(modulus: u32, job: J) -> Option<J::Out> {
    Some(match modulus {
        65536 => job.run::<Wrap16>(modulus),
        65521 => job.run::<ModP16<65521>>(modulus),
        65519 => job.run::<ModP16<65519>>(modulus),
        65497 => job.run::<ModP16<65497>>(modulus),
        65479 => job.run::<ModP16<65479>>(modulus),
        65449 => job.run::<ModP16<65449>>(modulus),
        65447 => job.run::<ModP16<65447>>(modulus),
        65437 => job.run::<ModP16<65437>>(modulus),
        _ => return None,
    })
}

/// The x below the product of the moduli with x = r (mod m) for each (m, r), by Garner's
/// algorithm. The moduli must be coprime, with a product below 2^128.
pub fn crt_many(residues: &[(u64, u64)]) -> u128 {
    let (mut x, mut product) = (0u128, 1u128);
    for &(m, r) in residues {
        let m = m as u128;
        let diff = (r as u128 % m + m - x % m) % m;
        x += product * (diff * inverse((product % m) as u64, m as u64) as u128 % m);
        product *= m;
    }
    x
}

/// The inverse of a modulo m, for coprime a and m.
fn inverse(a: u64, m: u64) -> u64 {
    let (mut r0, mut r1, mut t0, mut t1) = (m as i128, a as i128, 0i128, 1i128);
    while r1 != 0 {
        let q = r0 / r1;
        (r0, r1, t0, t1) = (r1, r0 - q * r1, t1, t0 - q * t1);
    }
    assert_eq!(r0, 1, "{a} has no inverse modulo {m}");
    t0.rem_euclid(m as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crt_many_recovers_large_values() {
        let all: Vec<u64> = MODULI_16.iter().map(|&m| m as u64).collect();
        for x in [
            0u128,
            1,
            54214,
            134071571821918373469990,
            u128::MAX / 3,
            (1u128 << 127) + 5,
        ] {
            assert_eq!(
                crt_many(
                    &all.iter()
                        .map(|&m| (m, (x % m as u128) as u64))
                        .collect::<Vec<_>>()
                ),
                x
            );
        }
    }

    #[test]
    fn moduli_16_pass_4_to_the_n() {
        let count = |n| moduli_16(n).len();
        assert_eq!((count(23), count(24), count(31), count(32)), (3, 4, 4, 5));
        assert_eq!(
            (count(47), count(48), count(55), count(56), count(60)),
            (6, 7, 7, 8, 8)
        );
    }

    #[test]
    fn crt_recovers_large_values() {
        for x in [
            0u128,
            1,
            12345,
            u64::MAX as u128 + 7,
            134071571821918373415776,
            (1u128 << 124) + 99,
        ] {
            assert_eq!(crt(x as u64, (x % P61 as u128) as u64), x);
        }
    }

    #[test]
    fn modp16_wraps() {
        type M = ModP16<65521>;
        assert_eq!(M::ONE.sub(ModP16(2)), ModP16(65520));
        assert_eq!(ModP16::<65521>(65520).add(ModP16(3)), ModP16(2));
    }

    #[test]
    fn records_round_trip() {
        fn check<R: Ring>(values: &[R]) {
            let mut buf = vec![0u8; R::BYTES];
            for &v in values {
                v.write(&mut buf);
                assert_eq!(R::read(&buf), v);
            }
        }
        check(&[
            Wrap64(0),
            Wrap64(1),
            Wrap64(u64::MAX),
            Wrap64(0x0123_4567_89ab_cdef),
        ]);
        check(&[Wrap16(0), Wrap16(1), Wrap16(u16::MAX), Wrap16(0x1234)]);
        check(&[ModP16::<65521>(0), ModP16(1), ModP16(65520)]);
        check(&[Mod61(0), Mod61(1), Mod61(P61 - 1)]);
        let mut buf = [0u8; 2];
        Wrap16(0x1234).write(&mut buf);
        assert_eq!(buf, [0x34, 0x12], "little-endian");
    }

    #[test]
    fn mod61_wraps() {
        assert_eq!(Mod61(P61 - 1).add(Mod61(2)), Mod61(1));
        assert_eq!(Mod61(1).sub(Mod61(2)), Mod61(P61 - 1));
    }
}
