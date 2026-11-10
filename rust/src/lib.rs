//! Free polyominoes without holes (OEIS A000104), by Burnside's lemma over the 8 symmetries of
//! the square:
//!
//! 8 a(n) = F(n) + R2(n) + 2 R4(n) + 2 M(n) + 2 D(n),
//!
//! where F counts fixed hole-free n-ominoes (A006724) and R2, R4, M, D the fixed ones with a
//! 180-degree, 90-degree, vertical-mirror and diagonal-mirror symmetry. See `../a000104.md`.

pub mod bfile;
pub mod brute;
pub mod checkpoint;
pub mod ring;
pub mod search;
pub mod spill;
pub mod tm;

use checkpoint::Checkpoint;
use ring::{Mod61, Ring, WithRing16, Wrap64, crt, crt_many, moduli_16, with_ring_16};
use std::collections::BTreeMap;

/// Largest n the transfer matrix supports (strips of up to `tm::MAX_STRIP` rows).
pub const MAX_N: usize = 2 * tm::MAX_STRIP;

/// Exact F(n) and R2(n) for n <= N from the transfer matrix.
pub struct FixedCounts {
    pub f: Vec<u128>,
    /// R2 by center: cell, horizontal-edge midpoint, vertex, vertical-edge midpoint.
    pub r2_parts: [Vec<u128>; 4],
    pub steps: u64,
    pub peak: usize,
}

impl FixedCounts {
    pub fn r2(&self) -> Vec<u128> {
        (0..self.f.len())
            .map(|w| self.r2_parts.iter().map(|p| p[w]).sum())
            .collect()
    }
}

/// The counts the sweep keeps per state: 64-bit, in one run modulo 2^64 and one modulo
/// 2^61 - 1; or 16-bit, in one run per 16-bit modulus, with a third of the memory a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counts {
    Bits64,
    Bits16,
}

/// Whether the counts to n can pass 2^64 (F(n) < 4^n), so a second sweep runs modulo 2^61 - 1.
fn needs_mod61(n: usize) -> bool {
    n > 31
}

/// Runs the transfer matrix in the runs `counts` takes for n and combines them by the CRT.
/// Strips finished in `checkpoint` are read, not run.
pub fn fixed_counts(
    n: usize,
    merge: bool,
    counts: Counts,
    limit: Option<&tm::Limit>,
    checkpoint: Option<&Checkpoint>,
) -> FixedCounts {
    assert!(n <= MAX_N, "n = {n} is past the largest supported, {MAX_N}");
    if counts == Counts::Bits16 {
        return fixed_counts_16(n, merge, limit, checkpoint);
    }
    let a = tm::fixed_with::<Wrap64>(n, merge, limit, checkpoint, progress("mod 2^64"));
    let b = needs_mod61(n)
        .then(|| tm::fixed_with::<Mod61>(n, merge, limit, checkpoint, progress("mod 2^61 - 1")));
    FixedCounts {
        f: combine(&a.f, b.as_ref().map(|b| &b.f[..])),
        r2_parts: std::array::from_fn(|i| combine(&a.r2[i], b.as_ref().map(|b| &b.r2[i][..]))),
        steps: a.steps,
        peak: a.peak,
    }
}

/// One 16-bit sweep's F and R2 by center, as residues.
struct Residues {
    f: Vec<u64>,
    r2: [Vec<u64>; 4],
    steps: u64,
    peak: usize,
}

/// A 16-bit sweep to n modulo the ring's modulus.
struct Sweep16<'a> {
    n: usize,
    merge: bool,
    limit: Option<&'a tm::Limit<'a>>,
    checkpoint: Option<&'a Checkpoint>,
}

impl WithRing16 for Sweep16<'_> {
    type Out = Residues;

    fn run<R: Ring>(self, modulus: u32) -> Residues {
        let label = format!("mod {modulus}");
        let fx = tm::fixed_with::<R>(
            self.n,
            self.merge,
            self.limit,
            self.checkpoint,
            progress(&label),
        );
        let res = |v: &[R]| v.iter().map(|x| x.residue()).collect();
        Residues {
            f: res(&fx.f),
            r2: std::array::from_fn(|i| res(&fx.r2[i])),
            steps: fx.steps,
            peak: fx.peak,
        }
    }
}

/// Runs the sweep once per 16-bit modulus that n needs, one after another, and combines them.
fn fixed_counts_16(
    n: usize,
    merge: bool,
    limit: Option<&tm::Limit>,
    checkpoint: Option<&Checkpoint>,
) -> FixedCounts {
    let runs: Vec<(u64, Residues)> = moduli_16(n)
        .iter()
        .map(|&m| {
            let job = Sweep16 {
                n,
                merge,
                limit,
                checkpoint,
            };
            (m as u64, with_ring_16(m, job).unwrap())
        })
        .collect();
    let exact = |pick: &dyn Fn(&Residues) -> &[u64]| -> Vec<u128> {
        (0..=n)
            .map(|w| {
                crt_many(
                    &runs
                        .iter()
                        .map(|(m, r)| (*m, pick(r)[w]))
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    };
    FixedCounts {
        f: exact(&|r| &r.f),
        r2_parts: std::array::from_fn(|i| exact(&|r| &r.r2[i])),
        steps: runs[0].1.steps,
        peak: runs[0].1.peak,
    }
}

/// What `checkpoint` holds of `fixed_counts`'s sweeps, for the "# resuming" line.
pub fn resume_summary(checkpoint: &Checkpoint, n: usize, counts: Counts) -> String {
    struct Describe<'a>(&'a Checkpoint, usize);
    impl WithRing16 for Describe<'_> {
        type Out = String;
        fn run<R: Ring>(self, modulus: u32) -> String {
            self.0.describe::<R>(&format!("mod {modulus}"), self.1)
        }
    }
    if counts == Counts::Bits16 {
        let parts = moduli_16(n)
            .iter()
            .map(|&m| with_ring_16(m, Describe(checkpoint, n)).unwrap());
        return parts.collect::<Vec<_>>().join("; ");
    }
    let mut parts = vec![checkpoint.describe::<Wrap64>("mod 2^64", n)];
    if needs_mod61(n) {
        parts.push(checkpoint.describe::<Mod61>("mod 2^61 - 1", n));
    }
    parts.join("; ")
}

/// A stderr line per finished strip: its height, state-steps, peak states and elapsed time,
/// and for a strip that spilled, its columns on disk, bytes written, peak bytes on disk and
/// window halvings.
pub fn progress<R>(label: &str) -> impl Fn(usize, &tm::Strip<R>) + use<R> {
    let (start, label) = (std::time::Instant::now(), label.to_string());
    move |w, s| {
        let d = &s.disk;
        let disk = if d.columns > 0 {
            format!(
                "; on disk {} columns, {:.2} GB written, peak {:.2} GB, {} halvings",
                d.columns,
                d.written as f64 / 1e9,
                d.peak as f64 / 1e9,
                d.halvings
            )
        } else {
            String::new()
        };
        eprintln!(
            "# {label}: strip {w} done, {} state-steps, peak {} states, {:.1}s{disk}",
            s.steps,
            s.peak,
            start.elapsed().as_secs_f64()
        )
    }
}

fn combine(a: &[Wrap64], b: Option<&[Mod61]>) -> Vec<u128> {
    match b {
        None => a.iter().map(|x| x.0 as u128).collect(),
        Some(b) => a.iter().zip(b).map(|(x, y)| crt(x.0, y.0)).collect(),
    }
}

/// a(n) and its five Burnside terms, for n <= N.
pub struct Terms {
    pub a: Vec<u128>,
    pub f: Vec<u128>,
    pub r2: Vec<u128>,
    pub r4: Vec<u128>,
    pub m: Vec<u128>,
    pub d: Vec<u128>,
    /// Sizes whose Burnside sum is not a multiple of 8 (a wrong input); their a(n) is 0.
    pub bad: Vec<usize>,
}

/// Assembles a(n), n <= N. F comes from `f`; R2 from `r2` if given, else from the search.
pub fn terms(n: usize, f: &[u128], r2: Option<&[u128]>, sym: &search::Symmetric) -> Terms {
    let r2: Vec<u128> = match r2 {
        Some(v) => v.to_vec(),
        None => sym.r2.iter().map(|&x| x as u128).collect(),
    };
    let wide = |v: &[u64]| v.iter().map(|&x| x as u128).collect::<Vec<_>>();
    let (r4, m, d) = (wide(&sym.r4), wide(&sym.m), wide(&sym.d));
    let (mut a, mut bad) = (vec![1u128], Vec::new());
    for w in 1..=n {
        let total = f[w] + r2[w] + 2 * r4[w] + 2 * m[w] + 2 * d[w];
        if !total.is_multiple_of(8) {
            bad.push(w);
        }
        a.push(if total.is_multiple_of(8) {
            total / 8
        } else {
            0
        });
    }
    Terms {
        a,
        f: f[..=n].to_vec(),
        r2,
        r4,
        m,
        d,
        bad,
    }
}

/// F(0..=n) from a b-file of A006724, with F(0) = 0.
pub fn fixed_from_bfile(terms: &BTreeMap<usize, u128>, n: usize) -> Result<Vec<u128>, String> {
    (0..=n)
        .map(|w| {
            if w == 0 {
                Ok(0)
            } else {
                terms
                    .get(&w)
                    .copied()
                    .ok_or(format!("the b-file has no F({w})"))
            }
        })
        .collect()
}
