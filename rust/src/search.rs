//! R2, R4, M and D by a Redelmeier search over the orbits of one symmetry.
//!
//! A polyomino fixed by a symmetry g is a union of g-orbits. The search grows connected unions
//! of orbits from a forced core and tracks the Euler characteristic V - E + F one cell at a time;
//! for a connected polyomino, a value of 1 means no holes. This is `invariant_counts` in
//! `a000104.py`.

use crate::checkpoint::Checkpoint;
use rayon::prelude::*;

type Cell = (i32, i32);
type Symmetry = fn(i32, i32) -> (i32, i32);

const NONE: u32 = u32::MAX;

/// Orbits of one symmetry on the cells within distance n + 1 of the origin.
struct Orbits {
    n: usize,
    r: i32,
    s: usize,
    /// Cell -> its orbit's smallest cell, or NONE outside the range.
    rep: Vec<u32>,
    /// By orbit representative: the orbit's cells and the neighboring orbits.
    members: Vec<Vec<u32>>,
    nbrs: Vec<Vec<u32>>,
}

impl Orbits {
    fn new(n: usize, g: Symmetry) -> Self {
        let r = n as i32 + 3;
        let s = (2 * r + 1) as usize;
        let lim = n as i32 + 1;
        let idx = |(x, y): Cell| (x + r) as usize * s + (y + r) as usize;
        let mut rep = vec![NONE; s * s];
        let mut members = vec![Vec::new(); s * s];
        for x in -lim..=lim {
            for y in -lim..=lim {
                let mut orb = vec![(x, y)];
                loop {
                    let (px, py) = *orb.last().unwrap();
                    let q = g(px, py);
                    if q == orb[0] {
                        break;
                    }
                    orb.push(q);
                }
                if orb.iter().all(|&(u, v)| u.abs() <= lim && v.abs() <= lim) {
                    let mut ids: Vec<u32> = orb.iter().map(|&c| idx(c) as u32).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    let first = ids[0];
                    rep[idx((x, y))] = first;
                    members[first as usize] = ids;
                }
            }
        }
        let mut nbrs = vec![Vec::new(); s * s];
        for (r0, cells) in members.iter().enumerate() {
            let mut v: Vec<u32> = cells
                .iter()
                .flat_map(|&i| {
                    let i = i as usize;
                    [i + 1, i - 1, i + s, i - s]
                })
                .map(|j| rep[j])
                .filter(|&q| q != NONE && q as usize != r0)
                .collect();
            v.sort_unstable();
            v.dedup();
            nbrs[r0] = v;
        }
        Orbits {
            n,
            r,
            s,
            rep,
            members,
            nbrs,
        }
    }

    fn idx(&self, (x, y): Cell) -> usize {
        (x + self.r) as usize * self.s + (y + self.r) as usize
    }
}

/// One search, or one part of it: at depth `split`, part `id` takes every `parts`-th node, as
/// in Shirakawa's search.
struct Walker<'a> {
    orb: &'a Orbits,
    occ: Vec<u8>,
    seen: Vec<u8>,
    untried: Vec<u32>,
    counts: Vec<u64>,
    split: usize,
    parts: u64,
    id: u64,
    /// Nodes reached at depth `split` so far. Every part walks the same tree above that depth,
    /// so the nodes there are numbered alike in every part.
    ticket: u64,
}

impl<'a> Walker<'a> {
    fn new(orb: &'a Orbits, split: usize, parts: u64, id: u64) -> Self {
        let cells = orb.s * orb.s;
        Walker {
            orb,
            occ: vec![0; cells],
            seen: vec![0; cells],
            untried: Vec::new(),
            counts: vec![0; orb.n + 1],
            split,
            parts,
            id,
            ticket: 0,
        }
    }

    /// Change in V - E + F from adding cell i.
    #[inline(always)]
    fn delta(&self, i: usize) -> i32 {
        let (o, s) = (&self.occ, self.orb.s);
        let (e, w, nn, so) = (o[i + s], o[i - s], o[i + 1], o[i - 1]);
        let free = ((e | nn | o[i + s + 1]) == 0) as i32
            + ((w | nn | o[i - s + 1]) == 0) as i32
            + ((e | so | o[i + s - 1]) == 0) as i32
            + ((w | so | o[i - s - 1]) == 0) as i32;
        free + (e + w + nn + so) as i32 - 3
    }

    /// Counts every connected superset of `core` that avoids `forbid`.
    fn run(&mut self, core: &[Cell], forbid: &[Cell]) {
        let orb = self.orb;
        let mut reps: Vec<u32> = core.iter().map(|&c| orb.rep[orb.idx(c)]).collect();
        reps.sort_unstable();
        reps.dedup();
        let wt: usize = reps.iter().map(|&r| orb.members[r as usize].len()).sum();
        if wt > orb.n {
            return;
        }
        for &c in forbid {
            let q = orb.rep[orb.idx(c)];
            if q != NONE {
                self.seen[q as usize] = 1;
            }
        }
        let mut chi = 0;
        for &r in &reps {
            self.seen[r as usize] = 1;
            for &i in &orb.members[r as usize] {
                chi += self.delta(i as usize);
                self.occ[i as usize] = 1;
            }
        }
        // The core is above any split, so part 0 alone counts it, as `grow` does.
        if chi == 1 && self.id == 0 {
            self.counts[wt] += 1;
        }
        for &r in &reps {
            for &u in &orb.nbrs[r as usize] {
                if self.seen[u as usize] == 0 {
                    self.seen[u as usize] = 1;
                    self.untried.push(u);
                }
            }
        }
        self.grow(0, wt, chi, 0);
        for &r in &reps {
            for &i in &orb.members[r as usize] {
                self.occ[i as usize] = 0;
            }
        }
        self.seen.fill(0);
        self.untried.clear();
        self.ticket = 0;
    }

    /// Redelmeier's step: each untried orbit is either added or excluded for this branch.
    fn grow(&mut self, lo: usize, wt: usize, chi: i32, depth: usize) {
        let orb = self.orb;
        let hi = self.untried.len();
        for at in lo..hi {
            let r = self.untried[at] as usize;
            let w2 = wt + orb.members[r].len();
            if w2 > orb.n {
                continue;
            }
            let nd = depth + 1;
            if nd == self.split {
                self.ticket += 1;
                if self.ticket % self.parts != self.id {
                    continue;
                }
            }
            let mut c2 = chi;
            for &i in &orb.members[r] {
                c2 += self.delta(i as usize);
                self.occ[i as usize] = 1;
            }
            // Every part walks the nodes above depth `split`, so part 0 alone counts them.
            if c2 == 1 && (nd >= self.split || self.id == 0) {
                self.counts[w2] += 1;
            }
            if w2 < orb.n {
                let mark = self.untried.len();
                for &u in &orb.nbrs[r] {
                    if self.seen[u as usize] == 0 {
                        self.seen[u as usize] = 1;
                        self.untried.push(u);
                    }
                }
                self.grow(at + 1, w2, c2, nd);
                for t in mark..self.untried.len() {
                    let u = self.untried[t] as usize;
                    self.seen[u] = 0;
                }
                self.untried.truncate(mark);
            }
            for &i in &orb.members[r] {
                self.occ[i as usize] = 0;
            }
        }
    }
}

/// The fixed hole-free polyominoes with each symmetry, by size 0..=n.
pub struct Symmetric {
    /// 180-degree rotation, by center: cell, edge midpoint (one orientation), vertex.
    pub r2_parts: [Vec<u64>; 3],
    pub r2: Vec<u64>,
    pub r4: Vec<u64>,
    pub m: Vec<u64>,
    pub d: Vec<u64>,
}

struct Job {
    g: Symmetry,
    /// Size of an orbit away from the center.
    orbit: usize,
    runs: Vec<(Vec<Cell>, Vec<Cell>)>,
}

fn jobs(n: usize) -> Vec<Job> {
    let lim = n as i32 + 1;
    let rng = || -lim..=lim;
    let quad = vec![(0, 0), (1, 0), (0, 1), (1, 1)];
    let one = |core: Vec<Cell>| vec![(core, Vec::new())];
    vec![
        Job {
            g: |x, y| (-x, -y),
            orbit: 2,
            runs: one(vec![(0, 0)]),
        },
        Job {
            g: |x, y| (1 - x, -y),
            orbit: 2,
            runs: one(vec![(0, 0), (1, 0)]),
        },
        Job {
            g: |x, y| (1 - x, 1 - y),
            orbit: 2,
            runs: one(quad.clone()),
        },
        Job {
            g: |x, y| (-y, x),
            orbit: 4,
            runs: one(vec![(0, 0)]),
        },
        Job {
            g: |x, y| (1 - y, x),
            orbit: 4,
            runs: one(quad),
        },
        // Mirror through cell centers: the axis cells are exactly a column of k.
        Job {
            g: |x, y| (-x, y),
            orbit: 2,
            runs: (1..=lim)
                .map(|k| {
                    (
                        (0..k).map(|j| (0, j)).collect(),
                        rng()
                            .filter(|j| !(0..k).contains(j))
                            .map(|j| (0, j))
                            .collect(),
                    )
                })
                .collect(),
        },
        // Mirror between cells: the straddling pairs are exactly k in a column.
        Job {
            g: |x, y| (1 - x, y),
            orbit: 2,
            runs: (1..=lim / 2)
                .map(|k| {
                    let pairs =
                        |js: Vec<i32>| js.into_iter().flat_map(|j| [(0, j), (1, j)]).collect();
                    (
                        pairs((0..k).collect()),
                        pairs(rng().filter(|j| !(0..k).contains(j)).collect()),
                    )
                })
                .collect(),
        },
        // Diagonal mirror: k diagonal cells and the cells at their shared corners.
        Job {
            g: |x, y| (y, x),
            orbit: 2,
            runs: (1..=(lim + 1) / 3)
                .map(|k| {
                    let mut core: Vec<Cell> = (0..k).map(|j| (j, j)).collect();
                    core.extend((0..k - 1).flat_map(|j| [(j + 1, j), (j, j + 1)]));
                    (
                        core,
                        rng()
                            .filter(|j| !(0..k).contains(j))
                            .map(|j| (j, j))
                            .collect(),
                    )
                })
                .collect(),
        },
    ]
}

/// A unit of the search: part `id` of the `parts` that run `run` of job `job` splits into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Unit {
    pub job: usize,
    pub run: usize,
    pub parts: u64,
    pub id: u64,
}

/// The parts of a run `depth` orbits deep: whole below 12, else 4^(depth - 11) up to 1024,
/// split at depth 8. They depend on the run alone, so any machine resumes any units.
fn parts(depth: usize) -> u64 {
    if depth < 12 {
        1
    } else {
        4u64.pow((depth - 11).min(5) as u32)
    }
}

fn units_of(n: usize, jobs: &[Job], orbits: &[Orbits]) -> Vec<Unit> {
    let mut units = Vec::new();
    for (job, j) in jobs.iter().enumerate() {
        for (run, (core, _)) in j.runs.iter().enumerate() {
            let orb = &orbits[job];
            let mut reps: Vec<u32> = core.iter().map(|&c| orb.rep[orb.idx(c)]).collect();
            reps.sort_unstable();
            reps.dedup();
            let wt: usize = reps.iter().map(|&r| orb.members[r as usize].len()).sum();
            let parts = parts(n.saturating_sub(wt) / j.orbit);
            units.extend((0..parts).map(|id| Unit {
                job,
                run,
                parts,
                id,
            }));
        }
    }
    units
}

fn prepared(n: usize, with_r2: bool) -> (Vec<Job>, Vec<Orbits>) {
    let mut jobs = jobs(n);
    if !with_r2 {
        for job in &mut jobs[..3] {
            job.runs.clear();
        }
    }
    let orbits = jobs.par_iter().map(|j| Orbits::new(n, j.g)).collect();
    (jobs, orbits)
}

/// The units of `symmetric(n, with_r2, ..)`.
pub fn units(n: usize, with_r2: bool) -> Vec<Unit> {
    let (jobs, orbits) = prepared(n, with_r2);
    units_of(n, &jobs, &orbits)
}

/// Runs the searches, their units spread across rayon's threads, reading finished units from
/// `checkpoint` and saving the others there. Without `with_r2` the three R2 searches, the
/// costliest, are skipped and R2 is left at zero.
pub fn symmetric(n: usize, with_r2: bool, checkpoint: Option<&Checkpoint>) -> Symmetric {
    let (jobs, orbits) = prepared(n, with_r2);
    let units = units_of(n, &jobs, &orbits);
    let results: Vec<(usize, Vec<u64>)> = units
        .par_iter()
        .with_max_len(1)
        .map(|u| {
            if let Some(counts) = checkpoint.and_then(|ck| ck.read_unit(n, u)) {
                return (u.job, counts);
            }
            let split = if u.parts > 1 { 8 } else { usize::MAX };
            let mut walker = Walker::new(&orbits[u.job], split, u.parts, u.id);
            let (core, forbid) = &jobs[u.job].runs[u.run];
            walker.run(core, forbid);
            if let Some(ck) = checkpoint {
                ck.write_unit(n, u, &walker.counts);
            }
            (u.job, walker.counts)
        })
        .collect();
    let mut by_job = vec![vec![0u64; n + 1]; jobs.len()];
    for (ji, counts) in results {
        for (t, c) in by_job[ji].iter_mut().zip(counts) {
            *t += c;
        }
    }
    let at = |j: usize, w: usize| by_job[j][w];
    let size = 0..=n;
    Symmetric {
        r2: size
            .clone()
            .map(|w| at(0, w) + 2 * at(1, w) + at(2, w))
            .collect(),
        r4: size.clone().map(|w| at(3, w) + at(4, w)).collect(),
        m: size.clone().map(|w| at(5, w) + at(6, w)).collect(),
        d: size.map(|w| at(7, w)).collect(),
        r2_parts: [by_job[0].clone(), by_job[1].clone(), by_job[2].clone()],
    }
}
