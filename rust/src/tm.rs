//! F(n) and R2(n) from one transfer-matrix sweep over self-avoiding polygons by area.
//!
//! A polygon sits in a strip of `w` cell rows. The sweep visits vertices bottom to top within a
//! column and columns left to right. A state records the w + 2 edges the cut crosses, 2 bits each:
//! 0 unused, 1 or 2 the lower or upper end of an arc already drawn. This is `sap_strip` in
//! `a000104.py`, plus the half-turn test for R2 and the optional mirror merge.

use crate::checkpoint::{self, Checkpoint, Column, modulus_name};
use crate::ring::Ring;
use crate::spill::{self, BucketSet, SpillDir};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;
use std::time::Instant;

/// Largest strip a u64 state can hold: w + 2 edges at 2 bits each.
pub const MAX_STRIP: usize = 30;

/// Counts from one strip of `w` rows, for areas up to n.
pub struct Strip<R> {
    /// `closures[a][l]`: polygons of area a, length l >= w and height <= w, once per placement.
    pub closures: Vec<Vec<R>>,
    /// `r2[kind][a][l]`: half-turn-symmetric polygons centered in the strip. Kind 0 has its
    /// center in a cell column (odd l), kind 1 on a vertex column (even l).
    pub r2: [Vec<Vec<R>>; 2],
    pub steps: u64,
    pub peak: usize,
    pub disk: DiskStats,
}

/// What a strip did on disk.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct DiskStats {
    pub columns: usize,
    pub written: u64,
    /// The most bytes of bucket files at once.
    pub peak: u64,
    pub halvings: usize,
}

/// How a sweep may go past RAM: the heap bytes it may use, and where its bucket files go.
pub struct Limit<'a> {
    pub budget: usize,
    pub dir: &'a SpillDir,
    #[doc(hidden)]
    pub force: Force,
}

/// Settings for tests that force the disk paths.
#[doc(hidden)]
#[derive(Clone, Copy, Default)]
pub struct Force {
    /// Every column on disk from the start, with this many buckets in every set.
    pub buckets: Option<usize>,
    /// Spill once, before the vertex step at (column, row).
    pub spill_at: Option<(usize, usize)>,
    /// Halve every window down to one row, then spread each one-row bucket over more buckets
    /// once.
    pub halve: bool,
}

/// One shard of the states at a point of the sweep. In column c, coefficient i of a state is
/// its count of partial polygons with area c + i.
struct Layer<R> {
    index: FxHashMap<u64, u32>,
    keys: Vec<u64>,
    coef: Vec<R>,
    stride: usize,
}

impl<R: Ring> Layer<R> {
    /// Heap bytes of the layer's tables.
    fn bytes(&self) -> usize {
        index_bytes(self.index.capacity())
            + self.keys.capacity() * 8
            + self.coef.capacity() * size_of::<R>()
    }

    /// Whether `reset_exact(stride, cap)` keeps the current tables.
    fn reuses(&self, stride: usize, cap: usize) -> bool {
        self.index.capacity() >= cap
            && self.keys.capacity() >= cap
            && self.coef.capacity() >= cap * stride
            && self.bytes() <= 2 * fresh_bytes::<R>(cap, stride) + 4096
    }

    /// As `reset`, with tables of exactly `cap` states, so that their bytes are known ahead.
    fn reset_exact(&mut self, stride: usize, cap: usize) {
        if !self.reuses(stride, cap) {
            // Free the old tables before allocating the new ones.
            *self = Layer::with_capacity(stride, 0);
        }
        self.reset(stride, cap);
        self.keys.reserve_exact(cap);
        self.coef.reserve_exact(cap * stride);
    }

    fn with_capacity(stride: usize, cap: usize) -> Self {
        let mut index = FxHashMap::default();
        index.reserve(cap);
        Layer {
            index,
            keys: Vec::with_capacity(cap),
            coef: Vec::with_capacity(cap * stride),
            stride,
        }
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    /// Empties the layer for reuse, keeping its allocations.
    fn reset(&mut self, stride: usize, cap: usize) {
        self.index.clear();
        self.index.reserve(cap);
        self.keys.clear();
        self.coef.clear();
        self.stride = stride;
    }

    fn row(&self, i: usize) -> &[R] {
        &self.coef[i * self.stride..(i + 1) * self.stride]
    }

    /// The row of state `key`, added as zeros if new.
    #[inline(always)]
    fn slot(&mut self, key: u64) -> usize {
        match self.index.entry(key) {
            Entry::Occupied(e) => *e.get() as usize,
            Entry::Vacant(e) => {
                let i = self.keys.len();
                e.insert(i as u32);
                self.keys.push(key);
                self.coef.resize(self.coef.len() + self.stride, R::ZERO);
                i
            }
        }
    }

    /// Adds `src` into state `key`, `shift` areas higher; coefficients past the window drop.
    fn add(&mut self, key: u64, src: &[R], shift: usize) {
        let i = self.slot(key);
        let dst = &mut self.coef[i * self.stride..(i + 1) * self.stride];
        for (d, s) in dst[shift..].iter_mut().zip(src) {
            *d = d.add(*s);
        }
    }

    /// Adds a bucket record into its state.
    fn add_record(&mut self, rec: &[u8]) {
        let i = self.slot(spill::key_of(rec));
        let dst = &mut self.coef[i * self.stride..(i + 1) * self.stride];
        for (j, d) in dst.iter_mut().enumerate() {
            *d = d.add(spill::count_of(rec, j));
        }
    }
}

/// How a successor's areas move: unchanged, up one cell, or rebased at a column boundary.
const STAY: u16 = 0;
const GROW: u16 = 1;
const REBASE: u16 = 2;

/// Up to two successors of one state, and whether the state closes a polygon instead.
#[derive(Default)]
struct Succ {
    outs: [(u64, u16); 2],
    len: usize,
    closes: bool,
}

impl Succ {
    fn push(&mut self, key: u64, moves: u16) {
        self.outs[self.len] = (key, moves);
        self.len += 1;
    }
}

/// A successor on its way to its shard.
#[derive(Clone, Copy)]
struct Item {
    key: u64,
    row: u32,
    src: u16,
    moves: u16,
}

/// One source shard's successors, by destination shard, and its closing (shard, row) pairs.
type Routed = (Vec<Vec<Item>>, Vec<(usize, usize)>);

fn total<R>(layers: &[Layer<R>]) -> usize {
    layers.iter().map(|l| l.keys.len()).sum()
}

/// Heap bytes of a hash index with room for `cap` keys, laid out as hashbrown does: a power of
/// two of 16-byte slots, at least 8/7 of `cap`, a control byte per slot and a group of 16.
fn index_bytes(cap: usize) -> usize {
    let slots = match cap {
        0 => return 0,
        1..4 => 4,
        4..8 => 8,
        _ => (cap * 8 / 7).next_power_of_two(),
    };
    17 * slots + 16
}

/// Heap bytes of a layer shard built for exactly `cap` states.
fn fresh_bytes<R>(cap: usize, stride: usize) -> usize {
    index_bytes(cap) + cap * (8 + stride * size_of::<R>())
}

/// Heap bytes of `layers`, including the shard list itself.
fn heap<R: Ring>(layers: &Vec<Layer<R>>) -> usize {
    layers.capacity() * size_of::<Layer<R>>() + layers.iter().map(Layer::bytes).sum::<usize>()
}

/// At most the heap bytes of `states` states spread over `shards` shards built exactly. An
/// index of 8 or more states has at most 16/7 slots a state, under 39 bytes, plus its 16-byte
/// group; a smaller one has at most 8 slots. So each shard adds at most 152 bytes, an 8-slot
/// index, besides its `Layer`.
fn layer_bound<R>(states: usize, shards: usize, stride: usize) -> usize {
    states * (39 + 8 + stride * size_of::<R>()) + shards * (152 + size_of::<Layer<R>>())
}

/// At most the heap bytes of routing `states` states from `src` shards to `dst`. A state routes
/// at most two 16-byte items, and a vector grown by doubling holds at most twice its items or
/// its first 4: 64 bytes a state, plus 24 + 64 = 88 bytes a (source, destination) vector. Each
/// source adds its 48-byte `Routed` and a first allocation of 4 for its closing states, 112
/// bytes; at most one state of a layer closes, the one whose only arc is the closing one.
fn routed_bound(states: usize, src: usize, dst: usize) -> usize {
    64 * states + 88 * src * dst + 112 * src
}

/// Heap bytes of routed successors.
fn routed_bytes(routed: &Vec<Routed>) -> usize {
    routed.capacity() * size_of::<Routed>()
        + routed
            .iter()
            .map(|(out, closed)| {
                out.capacity() * size_of::<Vec<Item>>()
                    + out.iter().map(|v| v.capacity()).sum::<usize>() * size_of::<Item>()
                    + closed.capacity() * size_of::<(usize, usize)>()
            })
            .sum::<usize>()
}

/// At most the heap bytes of one vertex step from `states` states: the layer, its routed
/// successors and the next layer, with at most two successors a state.
fn step_need<R>(states: usize, stride: usize) -> usize {
    let (sh, sh2) = (shard_count(states), shard_count(2 * states));
    layer_bound::<R>(states, sh, stride)
        + routed_bound(states, sh, sh2)
        + layer_bound::<R>(2 * states, sh2, stride)
}

/// At most the heap bytes of shifting `states` states in `shards` shards into the next column,
/// besides the states themselves: their routed successors and the next column's first layer.
fn shift_need<R>(states: usize, shards: usize, stride: usize) -> usize {
    let sh = shard_count(states);
    routed_bound(states, shards, sh) + layer_bound::<R>(states, sh, stride)
}

/// Most buckets in one set.
const MAX_BUCKETS: usize = 4096;

/// Write chunk bytes per thread and load chunk bytes for a sweep with heap `budget`.
fn chunks(budget: usize) -> (usize, usize) {
    let t = rayon::current_num_threads();
    (
        (budget / (64 * t)).clamp(64 << 10, 4 << 20),
        (budget / 64).clamp(256 << 10, 16 << 20),
    )
}

/// Heap bytes the bucket I/O may hold at once: each thread's write chunk and bucket offsets,
/// and the load chunk with its routing or re-bucketing.
fn io_bytes(budget: usize) -> usize {
    let t = rayon::current_num_threads();
    let (write, load) = chunks(budget);
    (t + 1) * (2 * write + 16 * MAX_BUCKETS + 32 * disk_shards()) + 5 * load
}

/// The Catalan number C(m).
fn catalan(m: usize) -> usize {
    (0..m).fold(1u128, |c, i| c * (4 * i as u128 + 2) / (i as u128 + 2)) as usize
}

/// The least budget a sweep to n runs in, with counts in R. In a one-row window a group fixes
/// the occupancy of all but two slots, so it holds at most 9 C(m) states, m = (w + 2) / 2;
/// unmerged mirror pairs double its records at a column start.
pub fn floor_budget<R>(n: usize) -> usize {
    (1..=n.div_ceil(2))
        .map(|w| {
            let need = step_need::<R>(2 * 9 * catalan((w + 2) / 2), n - w + 2);
            need + io_bytes(2 * need) + (8 << 20)
        })
        .max()
        .unwrap_or(0)
}

/// Predicted peak states of a sweep to n: the fit to n = 32..42 with the mirror merge, whose
/// absence multiplies the peak by about 1.45.
fn predicted_peak(n: usize, merge: bool) -> f64 {
    1_281_430.0 * 1.578f64.powi(n as i32 - 32) * if merge { 1.0 } else { 1.45 }
}

/// At most the heap bytes of a step in RAM when no layer passes `states` states. A layer's tables
/// are built for its routed successors, at most two for each state of the layer before.
fn ram_need<R>(states: usize, stride: usize) -> usize {
    let sh = shard_count(2 * states);
    2 * layer_bound::<R>(2 * states, sh, stride) + routed_bound(states, sh, sh)
}

/// For a sweep to n with heap `budget`: None if its steps fit in the room, else the bytes of
/// bucket files it is predicted to hold at once (its peak layer as records).
pub fn disk_estimate<R>(n: usize, merge: bool, budget: usize) -> Option<u64> {
    // 10% over the fit, which is within 6% of the peak at n = 22 to 36.
    let peak = 1.1 * predicted_peak(n, merge);
    let stride = n - n.div_ceil(2) + 2;
    let room = budget.saturating_sub(io_bytes(budget));
    let record = (8 + stride * size_of::<R>()) as f64;
    (ram_need::<R>(peak as usize, stride) > room).then_some((peak * record) as u64)
}

/// Predicted bytes of two column checkpoints of the widest layer of a sweep to n, with counts
/// in R: the last one, and the next while it is written.
pub fn checkpoint_estimate<R: Ring>(n: usize, merge: bool) -> u64 {
    let stride = n - n.div_ceil(2) + 2;
    (2.2 * predicted_peak(n, merge) * spill::record_len::<R>(stride) as f64) as u64
}

/// The heap bytes `bytes` reports for a fresh shard of `cap` states, for the allocator test.
#[doc(hidden)]
pub fn probe_layer_bytes<R: Ring>(cap: usize, stride: usize) -> usize {
    Layer::<R>::with_capacity(stride, cap).bytes()
}

/// One shard for small layers, else four per thread.
fn shard_count(len: usize) -> usize {
    if len < 1 << 14 {
        1
    } else {
        (4 * rayon::current_num_threads()).min(1024)
    }
}

fn shard_of(key: u64, shards: usize) -> usize {
    ((key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize) % shards
}

/// Load shards of a large layer; bucket files count their records by these.
fn disk_shards() -> usize {
    shard_count(usize::MAX)
}

/// The low bit of each slot that holds an arc end.
fn occupancy(s: u64) -> u64 {
    (s | s >> 1) & 0x5555_5555_5555_5555
}

/// Mask of the low bits of slots `from..=to`.
fn slots(from: usize, to: usize) -> u64 {
    (from..=to).fold(0, |m, j| m | 1 << (2 * j))
}

/// The splitmix64 finalizer, to spread group keys over buckets.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Columns on disk run in this many windows of rows.
const WINDOWS: usize = 2;

/// Where a window's states go: a bucket set, chosen by their group key in the set's window.
/// With `shift`, the states first move from column `shift` to the next one.
struct Sink<'s, 'a> {
    set: &'s BucketSet<'a>,
    /// The slots outside the set's window, whose occupancy is the group key.
    outside: u64,
    shift: Option<usize>,
}

impl Sink<'_, '_> {
    fn bucket(&self, key: u64) -> usize {
        let h = mix((occupancy(key) & self.outside) ^ self.set.salt());
        (h % self.set.buckets() as u64) as usize
    }
}

/// A thread's scratch for `scatter`, kept from call to call.
#[derive(Default)]
struct Scatter {
    placed: Vec<(u32, u32, u64)>,
    start: Vec<usize>,
    pos: Vec<usize>,
    buf: Vec<u8>,
    hist: Vec<u64>,
}

/// Appends `items` records to `out`, one run per bucket. `key(i)` is item i's key, or None to
/// drop it, and `fill(i, key, rec)` writes its record.
fn scatter(
    out: &Sink,
    items: usize,
    key: impl Fn(usize) -> Option<u64>,
    fill: impl Fn(usize, u64, &mut [u8]),
    sc: &mut Scatter,
) {
    let (nb, rec, shards) = (out.set.buckets(), out.set.record(), disk_shards());
    let Scatter {
        placed,
        start,
        pos,
        buf,
        hist,
    } = sc;
    placed.clear();
    start.clear();
    start.resize(nb + 1, 0);
    for i in 0..items {
        if let Some(k) = key(i) {
            let b = out.bucket(k);
            start[b + 1] += 1;
            placed.push((i as u32, b as u32, k));
        }
    }
    for b in 0..nb {
        start[b + 1] += start[b];
    }
    buf.clear();
    buf.resize(start[nb] * rec, 0);
    pos.clear();
    pos.extend_from_slice(start);
    for &(i, b, k) in placed.iter() {
        let at = pos[b as usize];
        fill(i as usize, k, &mut buf[at * rec..(at + 1) * rec]);
        pos[b as usize] += 1;
    }
    hist.resize(shards, 0);
    for b in 0..nb {
        let run = &buf[start[b] * rec..start[b + 1] * rec];
        if run.is_empty() {
            continue;
        }
        hist.fill(0);
        for r in run.chunks_exact(rec) {
            hist[shard_of(spill::key_of(r), shards)] += 1;
        }
        out.set.append(b, run, hist);
    }
}

/// Maps every state of `cur` to its successors and gathers them, re-sharded, into `next`, in
/// two parallel passes: route each successor to its shard, then build each shard. Returns the
/// (shard, row) of every state that closes a polygon.
///
/// `routed` holds the successors in between. Under a limit it keeps its allocations for the
/// next step; without one it starts fresh, since kept tables would raise the peak.
///
/// With `room`, the step allocates its tables exactly and runs only if `cur`, the routed
/// successors and `next` fit in `room` bytes together; else it returns None, with `cur` intact.
fn step<R: Ring>(
    cur: &Vec<Layer<R>>,
    next: &mut Vec<Layer<R>>,
    routed: &mut Vec<Routed>,
    stride: usize,
    room: Option<usize>,
    succ: impl Fn(u64, &[R]) -> Succ + Sync,
) -> Option<Vec<(usize, usize)>> {
    let shards = shard_count(total(cur));
    if room.is_none() {
        *routed = Vec::new();
    }
    let held = room.map(|room| {
        let held = heap(cur);
        let route = routed_bound(total(cur), cur.len(), shards);
        if held + heap(next) + routed_bytes(routed) + route > room {
            *next = Vec::new();
        }
        if held + routed_bytes(routed) + route > room {
            *routed = Vec::new();
        }
        (room, held)
    });
    if let Some((room, held)) = held
        && held + routed_bound(total(cur), cur.len(), shards) > room
    {
        return None;
    }
    routed.truncate(cur.len());
    routed.resize_with(cur.len(), Routed::default);
    routed
        .par_iter_mut()
        .zip(cur.par_iter())
        .enumerate()
        .for_each(|(src, ((out, closed), layer))| {
            out.truncate(shards);
            out.resize_with(shards, Vec::new);
            out.iter_mut().for_each(Vec::clear);
            closed.clear();
            for row in 0..layer.len() {
                let o = succ(layer.keys[row], layer.row(row));
                if o.closes {
                    closed.push((src, row));
                }
                for &(key, moves) in &o.outs[..o.len] {
                    let item = Item {
                        key,
                        row: row as u32,
                        src: src as u16,
                        moves,
                    };
                    out[shard_of(key, shards)].push(item);
                }
            }
        });
    if let Some((room, held)) = held {
        let need: Vec<usize> = (0..shards)
            .map(|d| routed.iter().map(|(r, _)| r[d].len()).sum())
            .collect();
        let held = held + routed_bytes(routed) + shards * size_of::<Layer<R>>();
        let build = |next: &Vec<Layer<R>>, keep: bool| -> usize {
            (0..shards)
                .map(|d| match next.get(d) {
                    Some(l) if keep && l.reuses(stride, need[d]) => l.bytes(),
                    _ => fresh_bytes::<R>(need[d], stride),
                })
                .sum()
        };
        if held + build(next, true) > room {
            *next = Vec::new();
            if held + build(next, false) > room {
                return None;
            }
        }
    }
    next.truncate(shards);
    next.resize_with(shards, || Layer::with_capacity(stride, 0));
    next.par_iter_mut().enumerate().for_each(|(d, layer)| {
        let cap = routed.iter().map(|(r, _)| r[d].len()).sum();
        if room.is_some() {
            layer.reset_exact(stride, cap);
        } else {
            layer.reset(stride, cap);
        }
        for (r, _) in routed.iter() {
            for it in &r[d] {
                let p = cur[it.src as usize].row(it.row as usize);
                match it.moves {
                    STAY => layer.add(it.key, p, 0),
                    GROW => layer.add(it.key, p, 1),
                    _ => layer.add(it.key, &p[1..], 0),
                }
            }
        }
    });
    Some(
        routed
            .iter()
            .flat_map(|(_, closed)| closed.iter().copied())
            .collect(),
    )
}

/// Successors of state `s` at the vertex in row y of column c. Before the step, slots y and
/// y + 1 hold the edges below and left of the vertex (`d`, `l`); after it, the edges right of
/// and above it. Each candidate pairs a successor with its right edge's label `r`. `low` marks
/// slots 0 to y - 1, the right edges of the vertices below, so `par` and `r` count the used
/// edges below the cell right of this vertex in its column; the cell is inside when they are odd.
fn vertex<R: Ring>(s: u64, p: &[R], y: usize, w: usize, c: usize, low: u64) -> Succ {
    let (sh, sh1) = (2 * y, 2 * y + 2);
    let (d, l) = ((s >> sh) & 3, (s >> sh1) & 3);
    let rest = s & !(15u64 << sh);
    let par = ((s | s >> 1) & low).count_ones() & 1;
    let mut cand = [(0u64, 0u64); 2];
    let n = match (d, l) {
        (0, 0) => {
            cand = [(rest, 0), (rest | 1 << sh | 2 << sh1, 1)];
            if y < w { 2 } else { 1 }
        }
        (0, a) | (a, 0) => {
            cand = [(rest | a << sh, a), (rest | a << sh1, 0)];
            if y < w { 2 } else { 1 }
        }
        // Joining the two ends of one arc closes the polygon.
        (1, 2) => {
            return Succ {
                closes: rest == 0 && c >= w,
                ..Succ::default()
            };
        }
        (2, 1) => {
            cand[0] = (rest, 0);
            1
        }
        (1, 1) => {
            cand[0] = (rest ^ 3 << (2 * partner_up(s, y + 2)), 0);
            1
        }
        _ => {
            cand[0] = (rest ^ 3 << (2 * partner_down(s, y - 1)), 0);
            1
        }
    };
    let mut o = Succ::default();
    let mut below_top = None;
    for &(s2, r) in &cand[..n] {
        // The cell right of this vertex is inside: its area enters. The counts move up one
        // area and the top one leaves the window, so the successor is kept only if a count
        // below the top is nonzero.
        if y < w && par ^ (r != 0) as u32 == 1 {
            if *below_top.get_or_insert_with(|| p[..p.len() - 1].iter().any(|x| !x.is_zero())) {
                o.push(s2, GROW);
            }
        } else {
            o.push(s2, STAY);
        }
    }
    o
}

/// Runs the sweep for one strip of `w` rows, keeping areas up to `n`.
pub fn strip<R: Ring>(w: usize, n: usize, merge: bool) -> Strip<R> {
    strip_with(w, n, merge, None)
}

/// As `strip`, spilling to disk under `limit`.
pub fn strip_with<R: Ring>(w: usize, n: usize, merge: bool, limit: Option<&Limit>) -> Strip<R> {
    sweep(w, n, merge, limit, None, |_, _, _| {})
}

/// As `strip_with`, starting from the newest column checkpoint in `checkpoint` and saving new
/// ones there, and calling `visit(c, y, layer)` before each vertex step that runs in RAM.
fn sweep<'a, R: Ring>(
    w: usize,
    n: usize,
    merge: bool,
    limit: Option<&'a Limit<'a>>,
    checkpoint: Option<&Checkpoint>,
    mut visit: impl FnMut(usize, usize, &[Layer<R>]),
) -> Strip<R> {
    let mut sw = Sweep::new(w, n, merge, limit);
    if let (Some(_), Some(l)) = (checkpoint, limit) {
        l.dir.keep_sums();
    }
    // The column's states on disk, and the row they start at.
    let mut disk: Option<(BucketSet, usize)> = None;
    let mut c = 0;
    match checkpoint.and_then(|ck| sw.resume(ck)) {
        Some((col, set)) => (c, disk) = (col, set.map(|s| (s, 0))),
        None => {
            sw.cur = vec![Layer::with_capacity(sw.top(0) + 1, 1)];
            sw.cur[0].add(0, &[R::ONE], 0);
        }
    }
    // The column of the last column checkpoint, or of the one the sweep resumed from.
    let mut saved = c;
    while sw.top(c) >= c {
        if let Some(ck) = checkpoint
            && c > saved
            && ck.due()
        {
            // At a column boundary, states on disk start at row 0.
            match &disk {
                None => sw.save_column(ck, c, None),
                Some((set, 0)) => sw.save_column(ck, c, Some(set)),
                Some(_) => {}
            }
            saved = c;
        }
        let shifts = sw.top(c + 1) > c;
        if disk.is_none()
            && let Err(y) = sw.run_rows(c, 0..w + 1, shifts, &mut visit)
        {
            let b = sw.window_end(y);
            disk = Some((sw.spill(c, y, b), y));
        }
        let Some((set, y0)) = disk.take() else {
            sw.end_column();
            if !shifts {
                break;
            }
            spill::check_interrupt();
            let (stride, room) = (sw.stride(c + 1), sw.room());
            let spare = heap(&sw.next) + routed_bytes(&sw.routed);
            if let Some(room) = room
                && heap(&sw.cur) + spare + shift_need::<R>(total(&sw.cur), sw.cur.len(), stride)
                    > room
            {
                // Row w's check left room for the shift once the last step's tables are gone.
                (sw.next, sw.routed) = (Vec::new(), Vec::new());
            }
            step(
                &sw.cur,
                &mut sw.next,
                &mut sw.routed,
                stride,
                room,
                |s, p| next_column(s, p, w, c, merge),
            )
            .expect("row w's check leaves room for the shift");
            std::mem::swap(&mut sw.cur, &mut sw.next);
            if total(&sw.cur) == 0 {
                break;
            }
            c += 1;
            continue;
        };
        let out = sw.disk_column(c, y0, set);
        sw.end_column();
        let Some(column) = out else {
            break;
        };
        if column.total() == 0 {
            break;
        }
        c += 1;
        if sw.back_to_ram(&column, c) {
            sw.load(&column, 0..column.buckets(), sw.stride(c));
        } else {
            disk = Some((column, 0));
        }
    }
    sw.finish()
}

/// One strip's sweep: its shape, its limit and the counts it accumulates.
struct Sweep<'a, R> {
    w: usize,
    n: usize,
    merge: bool,
    limit: Option<&'a Limit<'a>>,
    /// Set once the forced spill has happened.
    forced: bool,
    disk: DiskStats,
    written: u64,
    closures: Vec<Vec<R>>,
    r2: [Vec<Vec<R>>; 2],
    steps: u64,
    /// States after each row's step in the current column, summed over its pieces.
    rows: Vec<usize>,
    /// States into each row's step in the current column, and out of its last row.
    ins: Vec<usize>,
    /// `ins` of the last column, to predict how a window grows.
    prev: Vec<usize>,
    peak: usize,
    /// The layer in RAM and the spare one, swapped at every step so their tables are reused,
    /// also from one bucket to the next.
    cur: Vec<Layer<R>>,
    next: Vec<Layer<R>>,
    /// Routed successors, kept from step to step.
    routed: Vec<Routed>,
    /// Scratch for writing bucket files, one per thread, and for reading them.
    pool: Vec<std::sync::Mutex<Scatter>>,
    read_buf: Vec<u8>,
    read_routed: Vec<Vec<Vec<u32>>>,
}

impl<'a, R: Ring> Sweep<'a, R> {
    fn new(w: usize, n: usize, merge: bool, limit: Option<&'a Limit<'a>>) -> Self {
        assert!(
            (1..=MAX_STRIP).contains(&w) && 2 * w <= n + 1,
            "strip {w} out of range for n = {n}"
        );
        let table = || vec![vec![R::ZERO; n + 2]; n + 1];
        if let Some(l) = limit {
            l.dir.reset_peak();
        }
        Sweep {
            w,
            n,
            merge,
            limit,
            forced: false,
            disk: DiskStats::default(),
            written: limit.map_or(0, |l| l.dir.written()),
            closures: table(),
            r2: [table(), table()],
            steps: 0,
            rows: vec![0; w + 1],
            ins: vec![0; w + 2],
            prev: vec![0; w + 2],
            peak: 0,
            cur: Vec::new(),
            next: Vec::new(),
            routed: Vec::new(),
            pool: (0..=rayon::current_num_threads())
                .map(|_| Default::default())
                .collect(),
            read_buf: Vec::new(),
            read_routed: Vec::new(),
        }
    }

    /// The most area a state may carry in column c and still close with length >= w.
    fn top(&self, c: usize) -> usize {
        self.n - (self.w - 1).saturating_sub(c)
    }

    /// Counts per state in column c.
    fn stride(&self, c: usize) -> usize {
        self.top(c) + 1 - c
    }

    /// The end of the disk window that starts at row a (or contains it).
    fn window_end(&self, a: usize) -> usize {
        let w = self.w;
        (1..=WINDOWS)
            .map(|i| (i * (w + 1)).div_ceil(WINDOWS))
            .find(|&b| b > a)
            .unwrap()
    }

    /// The slots outside the window of rows a..b, which changes slots a to b.
    fn outside(&self, a: usize, b: usize) -> u64 {
        slots(0, self.w + 1) & !slots(a, b)
    }

    /// Write and load chunk sizes for the bucket files.
    fn chunks(&self) -> (usize, usize) {
        chunks(self.limit.map_or(usize::MAX, |l| l.budget))
    }

    /// Heap bytes left for the layers and their routing, if the sweep has a limit.
    fn room(&self) -> Option<usize> {
        let l = self.limit?;
        Some(l.budget.saturating_sub(io_bytes(l.budget) + l.dir.heap()))
    }

    /// Whether a forced spill comes before the vertex step at row y of column c.
    fn spill_now(&mut self, c: usize, y: usize) -> bool {
        let Some(l) = self.limit else {
            return false;
        };
        let at = if l.force.buckets.is_some() {
            Some((0, 0))
        } else {
            l.force.spill_at
        };
        if !self.forced && at == Some((c, y)) {
            self.forced = true;
            return true;
        }
        false
    }

    /// States into row b of this column, predicted from `states` into row a by the last
    /// column's growth.
    fn predict(&self, states: usize, a: usize, b: usize) -> usize {
        let (pa, pb) = (self.prev[a], self.prev[b]);
        if pa == 0 || pb == 0 {
            2 * states
        } else {
            (states as f64 * pb as f64 / pa as f64).ceil() as usize
        }
    }

    /// A new bucket set for about `states` records of `stride` counts, with enough buckets
    /// that a step on one of them needs at most half the room.
    fn new_set(&self, states: usize, stride: usize) -> BucketSet<'a> {
        let l = self.limit.expect("a spill needs a limit");
        let want = l.force.buckets.unwrap_or_else(|| {
            let half = (self.room().unwrap() / 2).max(1);
            step_need::<R>(states, stride).div_ceil(half)
        });
        let want = want.clamp(1, MAX_BUCKETS);
        BucketSet::new(l.dir, want, disk_shards(), spill::record_len::<R>(stride))
    }

    /// Whether column c's states, on disk in `set`, go back into RAM: when a step on all of
    /// them needs at most half the room.
    fn back_to_ram(&self, set: &BucketSet, c: usize) -> bool {
        let Some(l) = self.limit else {
            return true;
        };
        l.force.buckets.is_none()
            && step_need::<R>(set.total() as usize, self.stride(c)) <= self.room().unwrap() / 2
    }

    /// Whether bucket k of `set` must split before a window of `rows` rows: one step on it
    /// might not fit in the room. `spread` marks a set that a one-row bucket spread into.
    fn too_big(&self, set: &BucketSet, k: usize, stride: usize, rows: usize, spread: bool) -> bool {
        let l = self.limit.unwrap();
        if l.force.halve {
            return rows > 1 || !spread;
        }
        let counts = set.shard_counts(k);
        let states: usize = counts.iter().map(|&x| x as usize).sum();
        let (sh, sh2) = (shard_count(states), shard_count(2 * states));
        let load = if sh == 1 {
            fresh_bytes::<R>(states, stride)
        } else {
            counts
                .iter()
                .map(|&x| fresh_bytes::<R>(x as usize, stride))
                .sum()
        } + sh * size_of::<Layer<R>>();
        let need = load + routed_bound(states, sh, sh2) + layer_bound::<R>(2 * states, sh2, stride);
        need > self.room().unwrap()
    }

    /// This thread's scratch for `scatter`.
    fn scratch(&self) -> std::sync::MutexGuard<'_, Scatter> {
        let i = rayon::current_thread_index().unwrap_or(self.pool.len() - 1);
        self.pool[i].lock().unwrap()
    }

    /// Writes the layer before row y of column c to disk, for the window of rows y..b.
    fn spill(&mut self, c: usize, y: usize, b: usize) -> BucketSet<'a> {
        let mut set = self.new_set(total(&self.cur), self.stride(c));
        let outside = self.outside(y, b);
        self.write_layer(
            &self.cur,
            &Sink {
                set: &set,
                outside,
                shift: None,
            },
        );
        set.seal();
        set
    }

    /// Runs column c on disk from row y0, its states in `set`. Returns column c + 1's states,
    /// or None if the strip ends with this column.
    fn disk_column(
        &mut self,
        c: usize,
        y0: usize,
        mut set: BucketSet<'a>,
    ) -> Option<BucketSet<'a>> {
        self.disk.columns += 1;
        let mut a = y0;
        loop {
            let b = self.window_end(a);
            let last = b == self.w + 1;
            if last && self.top(c + 1) < c + 1 {
                self.run_set(c, a, b, &set, None, 0, false);
                return None;
            }
            let (stride, outside, shift) = if last {
                (
                    self.stride(c + 1),
                    self.outside(0, self.window_end(0)),
                    Some(c),
                )
            } else {
                (self.stride(c), self.outside(b, self.window_end(b)), None)
            };
            let mut out = self.new_set(self.predict(set.total() as usize, a, b), stride);
            let sink = Sink {
                set: &out,
                outside,
                shift,
            };
            self.run_set(c, a, b, &set, Some(&sink), 0, false);
            drop(set);
            out.seal();
            if last {
                return Some(out);
            }
            (set, a) = (out, b);
        }
    }

    /// Runs rows a..b of column c on each bucket of `set` in turn, writing the results to `out`.
    /// A bucket too big for the room splits: its window halves, or with one row left, its
    /// groups spread over more buckets, into a set marked `spread`.
    #[allow(clippy::too_many_arguments)]
    fn run_set(
        &mut self,
        c: usize,
        a: usize,
        b: usize,
        set: &BucketSet<'a>,
        out: Option<&Sink>,
        depth: usize,
        spread: bool,
    ) {
        assert!(
            depth < 64,
            "column {c}, rows {a}..{b}: a group does not fit in the memory limit"
        );
        let stride = self.stride(c);
        for k in 0..set.buckets() {
            if set.records(k) == 0 {
                continue;
            }
            spill::check_interrupt();
            if self.too_big(set, k, stride, b - a, spread) {
                self.disk.halvings += 1;
                let states = set.records(k) as usize;
                let mid = if b - a > 1 { (a + b) / 2 } else { b };
                let mut first = self.new_set(states, stride);
                let outside = self.outside(a, mid);
                self.rebucket(
                    set,
                    k,
                    &Sink {
                        set: &first,
                        outside,
                        shift: None,
                    },
                );
                first.seal();
                if mid == b {
                    self.run_set(c, a, b, &first, out, depth + 1, true);
                    continue;
                }
                let mut second = self.new_set(self.predict(states, a, mid), stride);
                let sink = Sink {
                    set: &second,
                    outside: self.outside(mid, b),
                    shift: None,
                };
                self.run_set(c, a, mid, &first, Some(&sink), depth + 1, false);
                drop(first);
                second.seal();
                self.run_set(c, mid, b, &second, out, depth + 1, false);
                continue;
            }
            self.load(set, k..k + 1, stride);
            match self.run_rows(c, a..b, false, &mut |_, _, _| {}) {
                Ok(()) => {
                    if let Some(o) = out {
                        self.write_layer(&self.cur, o);
                    }
                }
                Err(y) => {
                    let rest = self.spill(c, y, b);
                    self.run_set(c, y, b, &rest, out, depth + 1, false);
                }
            }
        }
    }

    /// Loads buckets `ks` of `set` into `cur`, summing duplicate states, and deletes them.
    fn load(&mut self, set: &BucketSet, ks: Range<usize>, stride: usize) {
        let shards = disk_shards();
        let mut counts = vec![0usize; shards];
        for k in ks.clone() {
            for (d, x) in set.shard_counts(k).into_iter().enumerate() {
                counts[d] += x as usize;
            }
        }
        let all: usize = counts.iter().sum();
        if shard_count(all) == 1 {
            counts = vec![all];
        }
        // Keep the old tables where they fit, unless they, the spares and one step overflow
        // the room.
        let mut layers = std::mem::take(&mut self.cur);
        layers.truncate(counts.len());
        if let Some(room) = self.room() {
            let sh2 = shard_count(2 * all);
            let step =
                routed_bound(all, counts.len(), sh2) + layer_bound::<R>(2 * all, sh2, stride);
            let keep: usize = (0..counts.len())
                .map(|d| match layers.get(d) {
                    Some(l) if l.reuses(stride, counts[d]) => l.bytes(),
                    _ => fresh_bytes::<R>(counts[d], stride),
                })
                .sum();
            if keep + heap(&self.next) + routed_bytes(&self.routed) + step > room {
                self.next = Vec::new();
                self.routed = Vec::new();
            }
            if keep + step > room {
                layers = Vec::new();
            }
        }
        layers.resize_with(counts.len(), || Layer::with_capacity(stride, 0));
        for (layer, &x) in layers.iter_mut().zip(&counts) {
            layer.reset_exact(stride, x);
        }
        let (rec, chunk) = (set.record(), self.chunks().1);
        let mut buf = std::mem::take(&mut self.read_buf);
        let mut routed = std::mem::take(&mut self.read_routed);
        for k in ks {
            set.drain(k, &mut buf, chunk, |buf| {
                if layers.len() == 1 {
                    for r in buf.chunks_exact(rec) {
                        layers[0].add_record(r);
                    }
                    return;
                }
                // Route record offsets to their shards, then build each shard, as `step` does.
                let per = (buf.len() / rec).div_ceil(rayon::current_num_threads()) * rec;
                routed.resize_with(buf.len().div_ceil(per), Vec::new);
                routed
                    .par_iter_mut()
                    .zip(buf.par_chunks(per))
                    .for_each(|(out, piece)| {
                        out.truncate(shards);
                        out.resize_with(shards, Vec::new);
                        out.iter_mut().for_each(Vec::clear);
                        for (i, r) in piece.chunks_exact(rec).enumerate() {
                            out[shard_of(spill::key_of(r), shards)].push(i as u32);
                        }
                    });
                let pieces = buf.len().div_ceil(per);
                layers.par_iter_mut().enumerate().for_each(|(d, layer)| {
                    for (p, r) in routed[..pieces].iter().enumerate() {
                        let piece = &buf[p * per..];
                        for &i in &r[d] {
                            layer.add_record(&piece[i as usize * rec..][..rec]);
                        }
                    }
                });
            });
        }
        (self.cur, self.read_buf, self.read_routed) = (layers, buf, routed);
    }

    /// Moves bucket k of `from` into `to`'s buckets.
    fn rebucket(&mut self, from: &BucketSet, k: usize, to: &Sink) {
        let (rec, chunk) = (from.record(), self.chunks().1);
        let mut buf = std::mem::take(&mut self.read_buf);
        from.drain(k, &mut buf, chunk, |buf| {
            let per = (buf.len() / rec).div_ceil(rayon::current_num_threads()) * rec;
            buf.par_chunks(per).for_each(|piece| {
                scatter(
                    to,
                    piece.len() / rec,
                    |i| Some(spill::key_of(&piece[i * rec..])),
                    |i, _, out| out.copy_from_slice(&piece[i * rec..(i + 1) * rec]),
                    &mut self.scratch(),
                );
            });
        });
        self.read_buf = buf;
    }

    /// Writes `cur` to `out`'s buckets.
    fn write_layer(&self, cur: &[Layer<R>], out: &Sink) {
        let (w, merge) = (self.w, self.merge);
        let per = (self.chunks().0 / (out.set.record() + 16)).max(1);
        cur.par_iter().for_each(|layer| {
            let key = |i: usize| {
                let (s, p) = (layer.keys[i], layer.row(i));
                match out.shift {
                    None => Some(s),
                    Some(c) => {
                        let o = next_column(s, p, w, c, merge);
                        (o.len == 1).then_some(o.outs[0].0)
                    }
                }
            };
            let skip = out.shift.is_some() as usize;
            for lo in (0..layer.len()).step_by(per) {
                let hi = (lo + per).min(layer.len());
                scatter(
                    out,
                    hi - lo,
                    |i| key(lo + i),
                    |i, k, rec| spill::put(rec, k, &layer.row(lo + i)[skip..]),
                    &mut self.scratch(),
                );
            }
        });
    }

    /// Runs the vertex steps of rows `ys` in column c on `cur`. Stops with Err(y) if the layer
    /// before row y must spill. With `shifts`, the last layer then shifts in RAM, so row w also
    /// needs room for that.
    fn run_rows(
        &mut self,
        c: usize,
        ys: Range<usize>,
        shifts: bool,
        visit: &mut impl FnMut(usize, usize, &[Layer<R>]),
    ) -> Result<(), usize> {
        let (mut cur, mut next) = (
            std::mem::take(&mut self.cur),
            std::mem::take(&mut self.next),
        );
        let done = self.rows_on(c, ys, shifts, &mut cur, &mut next, visit);
        (self.cur, self.next) = (cur, next);
        done
    }

    fn rows_on(
        &mut self,
        c: usize,
        ys: Range<usize>,
        shifts: bool,
        cur: &mut Vec<Layer<R>>,
        next: &mut Vec<Layer<R>>,
        visit: &mut impl FnMut(usize, usize, &[Layer<R>]),
    ) -> Result<(), usize> {
        let (w, n) = (self.w, self.n);
        let occ_mask: u64 = (0..w + 2).fold(0, |m, j| m | 1 << (2 * j));
        let stride = self.top(c) + 1 - c;
        for y in ys {
            spill::check_interrupt();
            if self.spill_now(c, y) {
                return Err(y);
            }
            let low = occ_mask & ((1u64 << (2 * y)) - 1);
            visit(c, y, cur);
            let room = self.room();
            let Some(closed) = step(cur, next, &mut self.routed, stride, room, |s, p| {
                vertex(s, p, y, w, c, low)
            }) else {
                return Err(y);
            };
            // If the shift would not fit after row w, row w runs on disk, whose writes shift.
            if shifts
                && y == w
                && let Some(room) = room
                && heap(next) + shift_need::<R>(total(next), next.len(), self.stride(c + 1)) > room
            {
                return Err(y);
            }
            self.steps += total(cur) as u64;
            self.ins[y] += total(cur);
            for (src, row) in closed {
                for (i, x) in cur[src].row(row).iter().enumerate() {
                    let t = &mut self.closures[c + i][c];
                    *t = t.add(*x);
                }
            }
            std::mem::swap(cur, next);
            self.rows[y] += total(cur);
            if y + 1 == w.div_ceil(2) && c >= 1 {
                record_r2(cur, w, n, c, 1, &mut self.r2[1]);
            }
            if y == w {
                record_r2(cur, w, n, c, 0, &mut self.r2[0]);
            }
        }
        Ok(())
    }

    /// Saves column c's states, in `cur` or on disk in `set`, and the tables and counters so far
    /// as a column checkpoint: `cur` as a file a shard, `set` as links to its bucket files. A
    /// failed write is reported, and the sweep goes on without it.
    fn save_column(&self, ck: &Checkpoint, c: usize, set: Option<&BucketSet>) {
        let t = Instant::now();
        let (w, chunk) = (self.w, self.chunks().0);
        let rec = spill::record_len::<R>(self.stride(c));
        let saved = ck.begin_column::<R>(w, c).and_then(|tmp| {
            let layer_file = |k: usize| tmp.join(format!("layer-{k}"));
            let files: std::io::Result<Vec<(u64, u64)>> = match set {
                None => self
                    .cur
                    .par_iter()
                    .enumerate()
                    .map(|(k, layer)| {
                        let sum = checkpoint::write_records(
                            &layer_file(k),
                            rec,
                            layer.len(),
                            chunk,
                            |i, out| spill::put(out, layer.keys[i], layer.row(i)),
                        )?;
                        Ok((layer.len() as u64, sum))
                    })
                    .collect(),
                Some(set) => {
                    let full: Vec<usize> =
                        (0..set.buckets()).filter(|&b| set.records(b) > 0).collect();
                    full.par_iter()
                        .enumerate()
                        .map(|(k, &b)| {
                            let (path, sum) = set.file(b);
                            checkpoint::link_or_copy(&path, &layer_file(k))?;
                            Ok((set.records(b), sum.expect("buckets keep checksums")))
                        })
                        .collect()
                }
            };
            let column = Column {
                c,
                steps: self.steps,
                peak: self.peak,
                prev: self.prev.clone(),
                closures: self.closures.clone(),
                r2: self.r2.clone(),
                files: files?,
            };
            let done = ck.finish_column::<R>(w, self.n, self.merge, &tmp, &column);
            if done.is_err() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            done
        });
        let states = set.map_or(total(&self.cur) as u64, |s| s.total());
        match saved {
            Ok(()) => eprintln!(
                "# checkpoint mod {} strip {w} column {c}: {:.2} GB in {:.1}s",
                modulus_name::<R>(),
                (states * rec as u64) as f64 / 1e9,
                t.elapsed().as_secs_f64()
            ),
            Err(e) => eprintln!(
                "# checkpoint: strip {w} column {c} not saved ({e}); the run goes on without it"
            ),
        }
    }

    /// Restores the newest sound column checkpoint of this strip, with its tables and counters,
    /// and returns its column. The layer loads into `cur`, a shard a file, or, under a limit it
    /// does not fit in RAM within, into a returned bucket set for the column's first window.
    fn resume(&mut self, ck: &Checkpoint) -> Option<(usize, Option<BucketSet<'a>>)> {
        let w = self.w;
        for (path, col) in ck.read_columns::<R>(w, self.n, self.merge) {
            let t = Instant::now();
            if col.c == 0 || self.top(col.c) < col.c || col.prev.len() != w + 2 {
                eprintln!(
                    "# checkpoint: {} does not fit strip {w}; not used",
                    path.display()
                );
                continue;
            }
            let stride = self.stride(col.c);
            let rec = spill::record_len::<R>(stride);
            let states: u64 = col.files.iter().map(|f| f.0).sum();
            let file = |k: usize| path.join(format!("layer-{k}"));
            let on_disk = self.limit.is_some_and(|l| {
                l.force.buckets.is_some()
                    || step_need::<R>(states as usize, stride) > self.room().unwrap() / 2
            });
            let loaded = if on_disk {
                let mut set = self.new_set(states as usize, stride);
                let sink = Sink {
                    set: &set,
                    outside: self.outside(0, self.window_end(0)),
                    shift: None,
                };
                let chunk = self.chunks().1;
                let read = col
                    .files
                    .iter()
                    .enumerate()
                    .try_for_each(|(k, &(records, sum))| {
                        checkpoint::read_records(&file(k), rec, records, sum, chunk, |piece| {
                            let per =
                                (piece.len() / rec).div_ceil(rayon::current_num_threads()) * rec;
                            piece.par_chunks(per).for_each(|p| {
                                scatter(
                                    &sink,
                                    p.len() / rec,
                                    |i| Some(spill::key_of(&p[i * rec..])),
                                    |i, _, out| out.copy_from_slice(&p[i * rec..(i + 1) * rec]),
                                    &mut self.scratch(),
                                )
                            })
                        })
                    });
                read.map(|()| {
                    set.seal();
                    Some(set)
                })
            } else {
                let chunk = self.chunks().0;
                let layers = col
                    .files
                    .par_iter()
                    .enumerate()
                    .map(|(k, &(records, sum))| {
                        let mut layer = Layer::with_capacity(stride, 0);
                        layer.reset_exact(stride, records as usize);
                        checkpoint::read_records(&file(k), rec, records, sum, chunk, |piece| {
                            piece.chunks_exact(rec).for_each(|r| layer.add_record(r))
                        })?;
                        Ok(layer)
                    });
                layers.collect::<Result<Vec<_>, String>>().map(|layers| {
                    self.cur = layers;
                    None
                })
            };
            match loaded {
                Ok(set) => {
                    (self.steps, self.peak, self.prev) = (col.steps, col.peak, col.prev);
                    (self.closures, self.r2) = (col.closures, col.r2);
                    eprintln!(
                        "# checkpoint mod {} strip {w}: column {} read{}, {:.2} GB in {:.1}s",
                        modulus_name::<R>(),
                        col.c,
                        if set.is_some() { " onto disk" } else { "" },
                        (states * rec as u64) as f64 / 1e9,
                        t.elapsed().as_secs_f64()
                    );
                    return Some((col.c, set));
                }
                Err(e) => eprintln!("# checkpoint: {e}; {} not used", path.display()),
            }
        }
        None
    }

    /// Folds the finished column's row totals into the peak.
    fn end_column(&mut self) {
        self.peak = self.peak.max(self.rows.iter().copied().max().unwrap_or(0));
        self.ins[self.w + 1] = self.rows[self.w];
        std::mem::swap(&mut self.ins, &mut self.prev);
        self.ins.fill(0);
        self.rows.fill(0);
    }

    fn finish(mut self) -> Strip<R> {
        if let (Some(l), true) = (self.limit, self.disk.columns > 0) {
            self.disk.written = l.dir.written() - self.written;
            self.disk.peak = l.dir.peak();
        }
        Strip {
            closures: self.closures,
            r2: self.r2,
            steps: self.steps,
            peak: self.peak,
            disk: self.disk,
        }
    }
}

/// The successor of a column's last state in the next column: the edges shift up one slot,
/// the areas rebase, and a state merges with its mirror image.
fn next_column<R: Ring>(s: u64, p: &[R], w: usize, c: usize, merge: bool) -> Succ {
    let mut o = Succ::default();
    if s != 0 && p[1..].iter().any(|x| !x.is_zero()) {
        debug_assert!(p[0].is_zero(), "a state after column {c} has area > {c}");
        let key = if merge {
            (s << 2).min(mirror(s << 2, w))
        } else {
            s << 2
        };
        o.push(key, REBASE);
    }
    o
}

fn partner_up(s: u64, from: usize) -> usize {
    let (mut depth, mut j) = (1, from);
    loop {
        match (s >> (2 * j)) & 3 {
            1 => depth += 1,
            2 => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
}

fn partner_down(s: u64, from: usize) -> usize {
    let (mut depth, mut j) = (1, from);
    loop {
        match (s >> (2 * j)) & 3 {
            2 => depth += 1,
            1 => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j -= 1;
    }
}

/// Top-bottom mirror of a column-boundary state (edges in slots 1..=w+1).
fn mirror(key: u64, w: usize) -> u64 {
    let mut m = 0;
    for p in 1..=w + 1 {
        let v = (key >> (2 * p)) & 3;
        if v != 0 {
            m |= (3 - v) << (2 * (w + 2 - p));
        }
    }
    m
}

/// True if the state's arcs plus their half-turned copies close into one loop.
/// Kind 0: the cut between vertex columns (slots 0..=w, slot j faces w - j).
/// Kind 1: the cut partway down a vertex column at its center (slots 0..=w+1, j faces w + 1 - j).
fn glues(s: u64, w: usize, kind: usize) -> bool {
    let npos = if kind == 0 { w + 1 } else { w + 2 };
    let face = |j: usize| if kind == 0 { w - j } else { w + 1 - j };
    let mask: u64 = (0..npos).fold(0, |m, j| m | 1 << (2 * j));
    let o = (s | s >> 1) & mask;
    if o == 0 || o.reverse_bits() >> (65 - 2 * npos) != o {
        return false;
    }
    let used = |j: usize| (s >> (2 * j)) & 3 != 0;
    let k = w.div_ceil(2);
    let center_used = match (kind, w % 2) {
        (0, 0) => used(w / 2),
        (0, _) => false,
        (_, 1) => used(k),
        _ => used(k) || used(k + 1),
    };
    if center_used {
        return false;
    }
    // mate[j]: the slot of the other end of the arc that ends at slot j, from matching 1s and
    // 2s as parentheses. count: the arc ends.
    let (mut mate, mut stack, mut sp, mut count) = ([0u8; 34], [0u8; 34], 0, 0);
    for j in 0..npos {
        match (s >> (2 * j)) & 3 {
            1 => {
                stack[sp] = j as u8;
                sp += 1;
                count += 1;
            }
            2 => {
                sp -= 1;
                let i = stack[sp] as usize;
                mate[i] = j as u8;
                mate[j] = i as u8;
                count += 1;
            }
            _ => {}
        }
    }
    // Walk the union from the lowest used slot. From slot p, the state's arc leads to
    // q = mate[p]; the half-turned copy of the arc that ends at face(q) also ends at q, and
    // leads on to face(mate[face(q)]). The union has `count` arcs, two a step, and is one loop
    // when the walk crosses them all before it returns to `start`.
    let start = (o.trailing_zeros() / 2) as usize;
    let (mut p, mut walked) = (start, 0);
    loop {
        let q = mate[p] as usize;
        p = face(mate[face(q)] as usize);
        walked += 2;
        if p == start || walked > count {
            break;
        }
    }
    walked == count
}

/// Adds the states that glue at this cut to `out[full area][length]`.
fn record_r2<R: Ring>(
    cur: &[Layer<R>],
    w: usize,
    n: usize,
    c: usize,
    kind: usize,
    out: &mut [Vec<R>],
) {
    let l = if kind == 0 { 2 * c + 1 } else { 2 * c };
    if l > n + 1 {
        return;
    }
    // Cells the cut splits: all of column c (kind 0), or the cells below the center (kind 1),
    // whose half-turned copies are the split cells above it.
    let below = if kind == 0 { w } else { w.div_ceil(2) };
    let zero = || vec![R::ZERO; n + 1];
    let sums = cur
        .par_iter()
        .map(|layer| {
            let mut by_area = zero();
            for row in 0..layer.len() {
                let s = layer.keys[row];
                if !glues(s, w, kind) {
                    continue;
                }
                let (mut par, mut inside) = (false, 0);
                for y in 0..below {
                    par ^= (s >> (2 * y)) & 3 != 0;
                    inside += par as usize;
                }
                let split = if kind == 0 { inside } else { 2 * inside };
                for (i, x) in layer.row(row).iter().enumerate() {
                    let a = c + i;
                    if !x.is_zero() && 2 * a >= split && 2 * a - split <= n {
                        by_area[2 * a - split] = by_area[2 * a - split].add(*x);
                    }
                }
            }
            by_area
        })
        .reduce(zero, |mut a, b| {
            for (x, y) in a.iter_mut().zip(b) {
                *x = x.add(y);
            }
            a
        });
    for (full, v) in sums.into_iter().enumerate() {
        out[full][l] = out[full][l].add(v);
    }
}

/// F(a) and R2(a) for a <= n, modulo R's modulus.
pub struct Fixed<R> {
    pub f: Vec<R>,
    /// R2 by center: cell, horizontal-edge midpoint, vertex, vertical-edge midpoint.
    pub r2: [Vec<R>; 4],
    pub steps: u64,
    pub peak: usize,
}

/// Runs every strip, one at a time (each is parallel inside), and combines them.
pub fn fixed<R: Ring>(n: usize, merge: bool) -> Fixed<R> {
    fixed_with(n, merge, None, None, |_, _| {})
}

/// As `fixed`, spilling to disk under `limit`, reading finished strips from `checkpoint` and
/// saving the others there, and calling `done(w, strip)` after each strip it runs.
#[allow(clippy::needless_range_loop)] // one index into several tables
pub fn fixed_with<R: Ring>(
    n: usize,
    merge: bool,
    limit: Option<&Limit>,
    checkpoint: Option<&Checkpoint>,
    done: impl Fn(usize, &Strip<R>),
) -> Fixed<R> {
    let hmax = n.div_ceil(2);
    assert!(
        hmax <= MAX_STRIP,
        "n = {n} needs strips wider than {MAX_STRIP}"
    );
    let strips: Vec<(usize, Strip<R>)> = (1..=hmax)
        .map(|w| {
            if let Some(s) = checkpoint.and_then(|ck| ck.read_strip(w, n, merge)) {
                return (w, s);
            }
            let s = sweep(w, n, merge, limit, checkpoint, |_, _, _| {});
            if let Some(ck) = checkpoint {
                ck.write_strip(w, n, merge, &s);
            }
            done(w, &s);
            (w, s)
        })
        .collect();
    let zero = vec![vec![R::ZERO; n + 2]; n + 1];
    let closures: Vec<&Vec<Vec<R>>> = std::iter::once(&zero)
        .chain(strips.iter().map(|(_, s)| &s.closures))
        .collect();
    let r2_tab = |h: usize, kind: usize| {
        if h == 0 {
            &zero
        } else {
            &strips[h - 1].1.r2[kind]
        }
    };

    // Exact height h: a second difference of the per-placement counts, and a first difference
    // (step 2) for R2, whose centered placements need h = w (mod 2). Height < length counts twice.
    let mut f = vec![R::ZERO; n + 1];
    let mut r2: [Vec<R>; 4] = std::array::from_fn(|_| vec![R::ZERO; n + 1]);
    for h in 1..=hmax {
        for a in 0..=n {
            for l in h..=n + 1 {
                let below = if h >= 2 {
                    closures[h - 2][a][l]
                } else {
                    R::ZERO
                };
                let e = closures[h][a][l]
                    .sub(closures[h - 1][a][l])
                    .sub(closures[h - 1][a][l])
                    .add(below);
                f[a] = f[a].add(e);
                if l > h {
                    f[a] = f[a].add(e);
                }
                for kind in 0..2 {
                    let below = if h >= 3 {
                        r2_tab(h - 2, kind)[a][l]
                    } else {
                        R::ZERO
                    };
                    let e = r2_tab(h, kind)[a][l].sub(below);
                    // The center, in `Fixed::r2`'s order. Odd length (kind 0) puts it at a
                    // cell center for odd h and a horizontal-edge midpoint for even h; even
                    // length (kind 1), at a vertex for even h and a vertical-edge midpoint
                    // for odd h.
                    let part = [[1, 0], [2, 3]][kind][h % 2];
                    r2[part][a] = r2[part][a].add(e);
                    if l > h {
                        r2[part][a] = r2[part][a].add(e);
                    }
                }
            }
        }
    }
    let steps = strips.iter().map(|(_, s)| s.steps).sum();
    let peak = strips.iter().map(|(_, s)| s.peak).max().unwrap_or(0);
    Fixed { f, r2, steps, peak }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{Wrap16, Wrap64};

    #[test]
    fn vertex_keeps_occupancy_outside_its_slots() {
        let n: usize = 24;
        for w in 1..=n.div_ceil(2) {
            let low_bits = |y: usize| slots(0, w + 1) & ((1u64 << (2 * y)) - 1);
            sweep::<Wrap64>(w, n, true, None, None, |c, y, cur| {
                let keep = !slots(y, y + 1);
                for layer in cur {
                    for row in 0..layer.len() {
                        let s = layer.keys[row];
                        let o = vertex(s, layer.row(row), y, w, c, low_bits(y));
                        for &(t, _) in &o.outs[..o.len] {
                            assert_eq!(
                                occupancy(t) & keep,
                                occupancy(s) & keep,
                                "w {w} c {c} y {y}"
                            );
                        }
                    }
                }
            });
        }
    }

    /// For `k` windows per column, the largest group's share of each window-start layer, as
    /// (layer size, largest group) pairs.
    fn group_sizes(n: usize, k: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for w in 1..=n.div_ceil(2) {
            let bounds: Vec<usize> = (0..=k).map(|i| (i * (w + 1)).div_ceil(k)).collect();
            sweep::<Wrap16>(w, n, true, None, None, |_, y, cur| {
                let Some(i) = bounds[..k].iter().position(|&b| b == y) else {
                    return;
                };
                let outside = slots(0, w + 1) & !slots(bounds[i], bounds[i + 1]);
                let groups = cur
                    .par_iter()
                    .map(|layer| {
                        let mut g = FxHashMap::<u64, usize>::default();
                        for &s in &layer.keys {
                            *g.entry(occupancy(s) & outside).or_default() += 1;
                        }
                        g
                    })
                    .reduce(FxHashMap::default, |mut a, b| {
                        for (key, v) in b {
                            *a.entry(key).or_default() += v;
                        }
                        a
                    });
                out.push((total(cur), groups.values().copied().max().unwrap_or(0)));
            });
        }
        out
    }

    /// The largest group's share of its layer, over layers of at least 1% of the largest one.
    #[test]
    #[ignore]
    fn group_shares() {
        for n in 30..=38 {
            let shares: Vec<String> = (2..=4)
                .map(|k| {
                    let sizes = group_sizes(n, k);
                    let peak = sizes.iter().map(|&(t, _)| t).max().unwrap();
                    let worst = sizes
                        .iter()
                        .filter(|&&(t, _)| 100 * t >= peak)
                        .map(|&(t, g)| g as f64 / t as f64)
                        .fold(0.0, f64::max);
                    let (_, g) = sizes.iter().find(|&&(t, _)| t == peak).unwrap();
                    format!(
                        "k = {k}: {:.4} (at the largest layer {:.4})",
                        worst,
                        *g as f64 / peak as f64
                    )
                })
                .collect();
            println!("n = {n}: {}", shares.join(", "));
        }
    }
}
