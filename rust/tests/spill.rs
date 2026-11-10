//! The spill files, and sweeps that spill.

use a000104::ring::{Mod61, ModP16, Ring, Wrap16, Wrap64};
use a000104::spill::{self, BucketSet, SpillDir};
use a000104::tm::{self, Force, Limit, Strip};
use rayon::prelude::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Counts the heap bytes in use, and the most at once since `reset_peak`.
struct Counting;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Bytes allocated by this thread, not counting frees.
    static MINE: Cell<usize> = const { Cell::new(0) };
}

fn count(size: usize) {
    let now = IN_USE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(now, Relaxed);
    let _ = MINE.try_with(|m| m.set(m.get() + size));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            count(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            count(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        IN_USE.fetch_sub(layout.size(), Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Tests that measure the heap hold this exclusively; the others share it.
static HEAP: RwLock<()> = RwLock::new(());

fn reset_peak() -> usize {
    let now = IN_USE.load(Relaxed);
    PEAK.store(now, Relaxed);
    now
}

/// The strip's tables, state-steps and peak, which a run on disk must reproduce exactly.
fn same<R: Ring>(a: &Strip<R>, b: &Strip<R>) -> bool {
    a.closures == b.closures && a.r2 == b.r2 && (a.steps, a.peak) == (b.steps, b.peak)
}

fn columns_on_disk_match<R: Ring>() {
    let _shared = HEAP.read().unwrap();
    let jobs: Vec<(usize, usize, bool)> = (20..=28)
        .flat_map(|n| (1..=n / 2 + n % 2).flat_map(move |w| [(n, w, false), (n, w, true)]))
        .collect();
    jobs.into_par_iter().for_each(|(n, w, merge)| {
        let ram = tm::strip::<R>(w, n, merge);
        for buckets in [1, 7, 64] {
            let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
            let force = Force {
                buckets: Some(buckets),
                ..Force::default()
            };
            let limit = Limit {
                budget: usize::MAX,
                dir: &dir,
                force,
            };
            let disk = tm::strip_with::<R>(w, n, merge, Some(&limit));
            assert!(disk.disk.columns > 0, "n {n} w {w}: ran on disk");
            assert!(
                same(&ram, &disk),
                "n {n} w {w} merge {merge} buckets {buckets}: differs from RAM"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                0,
                "files left"
            );
        }
    });
}

#[test]
fn columns_on_disk_match_ram_in_wrap16() {
    columns_on_disk_match::<Wrap16>();
}

#[test]
fn columns_on_disk_match_ram_in_wrap64() {
    columns_on_disk_match::<Wrap64>();
}

fn records_round_trip<R: Ring>(values: &[R]) {
    let stride = values.len() + 2;
    let mut rec = vec![0xffu8; spill::record_len::<R>(stride)];
    spill::put(&mut rec, 0x0123_4567_89ab_cdef, values);
    assert_eq!(spill::key_of(&rec), 0x0123_4567_89ab_cdef);
    for (j, &v) in values.iter().enumerate() {
        assert_eq!(spill::count_of::<R>(&rec, j), v);
    }
    for j in values.len()..stride {
        assert_eq!(spill::count_of::<R>(&rec, j), R::ZERO, "padding");
    }
}

#[test]
fn records_round_trip_in_every_ring() {
    let _shared = HEAP.read().unwrap();
    records_round_trip(&[Wrap64(1), Wrap64(u64::MAX), Wrap64(0)]);
    records_round_trip(&[Wrap16(7), Wrap16(u16::MAX)]);
    records_round_trip(&[ModP16::<65521>(65520), ModP16(3)]);
    records_round_trip(&[Mod61(5), Mod61((1 << 61) - 2)]);
}

#[test]
fn bucket_sets_write_and_load_in_parallel() {
    let _shared = HEAP.read().unwrap();
    let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
    let (buckets, shards, stride) = (7, 5, 3);
    let len = spill::record_len::<Wrap64>(stride);
    let mut set = BucketSet::new(&dir, buckets, shards, len);
    assert_eq!(set.buckets(), buckets);
    // Each thread appends runs of keys t, t + 64, t + 128, ...: bucket key % 7, shard key % 5.
    (0..64u64).into_par_iter().for_each(|t| {
        for b in 0..buckets {
            let keys: Vec<u64> = (0..300)
                .map(|i| t + 64 * i)
                .filter(|k| k % 7 == b as u64)
                .collect();
            let mut run = vec![0u8; keys.len() * len];
            let mut per_shard = vec![0u64; shards];
            for (rec, &k) in run.chunks_mut(len).zip(&keys) {
                spill::put(&mut rec[..], k, &[Wrap64(k), Wrap64(2 * k)]);
                per_shard[(k % 5) as usize] += 1;
            }
            set.append(b, &run, &per_shard);
        }
    });
    set.seal();
    assert_eq!(set.total(), 64 * 300);
    assert_eq!(dir.written(), 64 * 300 * len as u64);
    let mut seen = HashMap::new();
    for b in 0..buckets {
        let want = set.shard_counts(b);
        let mut by_shard = vec![0u64; shards];
        set.drain(b, &mut Vec::new(), 1000, |chunk| {
            for rec in chunk.chunks(len) {
                let k = spill::key_of(rec);
                assert_eq!(k % 7, b as u64);
                assert_eq!(spill::count_of::<Wrap64>(rec, 1), Wrap64(2 * k));
                by_shard[(k % 5) as usize] += 1;
                *seen.entry(k).or_insert(0) += 1;
            }
        });
        assert_eq!(set.records(b), 0, "drained");
        assert!(set.shard_counts(b).iter().all(|&k| k == 0));
        assert_eq!(by_shard, want, "bucket {b}");
    }
    assert_eq!(seen.len(), 64 * 300);
    assert!(seen.values().all(|&v| v == 1));
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "drained files are deleted"
    );
}

#[test]
fn shard_counts_follow_appends() {
    let _shared = HEAP.read().unwrap();
    let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
    let len = spill::record_len::<Wrap16>(1);
    let mut set = BucketSet::new(&dir, 2, 3, len);
    let mut run = vec![0u8; 4 * len];
    for (i, rec) in run.chunks_mut(len).enumerate() {
        spill::put(rec, i as u64, &[Wrap16(1)]);
    }
    set.append(1, &run, &[1, 0, 3]);
    set.append(1, &run[..len], &[0, 1, 0]);
    set.seal();
    assert_eq!(set.records(0), 0);
    assert_eq!(set.records(1), 5);
    assert_eq!(set.shard_counts(1), vec![1, 1, 3]);
}

#[test]
fn spill_dir_is_removed_after_a_run_and_after_a_failure() {
    let _shared = HEAP.read().unwrap();
    let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
    let path = dir.path().to_path_buf();
    let set = BucketSet::new(&dir, 3, 1, 16);
    set.append(2, &[0u8; 32], &[2]);
    drop(set);
    drop(dir);
    assert!(!path.exists());

    let path = std::sync::Mutex::new(None);
    let failed = std::panic::catch_unwind(|| {
        let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
        *path.lock().unwrap() = Some(dir.path().to_path_buf());
        let set = BucketSet::new(&dir, 3, 1, 16);
        set.append(0, &[0u8; 16], &[1]);
        panic!("a failing run");
    });
    assert!(failed.is_err());
    assert!(!path.lock().unwrap().take().unwrap().exists());
}

fn limit(dir: &SpillDir, budget: usize, force: Force) -> Limit<'_> {
    Limit { budget, dir, force }
}

#[test]
fn layer_bytes_match_the_allocator() {
    for cap in [0, 1, 3, 4, 7, 8, 13, 100, 1000, 12345, 1 << 16] {
        for stride in [1, 5, 23] {
            let before = MINE.with(Cell::get);
            let predicted = tm::probe_layer_bytes::<Wrap16>(cap, stride);
            let used = MINE.with(Cell::get) - before;
            assert!(
                used <= predicted && predicted <= used + 64,
                "cap {cap} stride {stride}: allocated {used}, predicted {predicted}"
            );
        }
    }
}

#[test]
fn a_mid_column_spill_matches_ram() {
    let _shared = HEAP.read().unwrap();
    let n = 24;
    let jobs: Vec<(usize, bool)> = (1..=n / 2).flat_map(|w| [(w, false), (w, true)]).collect();
    jobs.into_par_iter().for_each(|(w, merge)| {
        let ram = tm::strip::<Wrap64>(w, n, merge);
        let m = (w + 1).div_ceil(2);
        for (c, y) in [(w, 0), (w, m / 2), (w, m), (w + 1, w), (2, 1)] {
            let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
            let force = Force {
                spill_at: Some((c, y)),
                ..Force::default()
            };
            let disk = tm::strip_with::<Wrap64>(w, n, merge, Some(&limit(&dir, usize::MAX, force)));
            assert!(
                same(&ram, &disk),
                "w {w} merge {merge}: spill at ({c}, {y}) differs"
            );
        }
    });
}

#[test]
fn halving_down_to_one_row_windows_matches_ram() {
    let _shared = HEAP.read().unwrap();
    let jobs: Vec<(usize, usize, bool)> = (20..=21)
        .flat_map(|n| (1..=n / 2 + n % 2).flat_map(move |w| [(n, w, false), (n, w, true)]))
        .collect();
    jobs.into_par_iter().for_each(|(n, w, merge)| {
        let ram = tm::strip::<Wrap16>(w, n, merge);
        let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
        let force = Force {
            buckets: Some(3),
            halve: true,
            ..Force::default()
        };
        let disk = tm::strip_with::<Wrap16>(w, n, merge, Some(&limit(&dir, usize::MAX, force)));
        assert!(disk.disk.halvings > 0, "w {w}: nothing split");
        assert!(
            same(&ram, &disk),
            "n {n} w {w} merge {merge}: halving differs"
        );
    });
}

#[test]
fn every_sweep_that_spills_gets_a_disk_check() {
    let _shared = HEAP.read().unwrap();
    // Budgets a little under where each sweep stops spilling.
    for (n, merge, mib) in [
        (28, false, 80),
        (28, false, 100),
        (30, true, 120),
        (30, true, 140),
    ] {
        let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
        let spilled = Cell::new(false);
        let lim = limit(&dir, mib << 20, Force::default());
        tm::fixed_with::<Wrap16>(n, merge, Some(&lim), None, |_, s| {
            spilled.set(spilled.get() || s.disk.columns > 0)
        });
        assert!(spilled.get(), "n {n} merge {merge}: spills at {mib} MiB");
        assert!(
            tm::disk_estimate::<Wrap16>(n, merge, mib << 20).is_some(),
            "n {n} merge {merge}: no disk check at {mib} MiB"
        );
    }
}

#[test]
fn the_heap_stays_under_a_32_mib_limit_at_n_30() {
    let n = 30;
    let ram = tm::fixed::<Wrap64>(n, true);
    let _alone = HEAP.write().unwrap();
    let dir = SpillDir::create(&std::env::temp_dir()).unwrap();
    // The budget leaves 2 MiB of the limit for the strip tables and the results.
    let lim = limit(&dir, 30 << 20, Force::default());
    let stats = std::sync::Mutex::new(Vec::new());
    let before = reset_peak();
    let disk = tm::fixed_with::<Wrap64>(n, true, Some(&lim), None, |w, s| {
        stats.lock().unwrap().push((w, s.disk))
    });
    let used = PEAK.load(Relaxed) - before;
    let stats = stats.into_inner().unwrap();
    for (w, d) in &stats {
        if d.columns > 0 {
            eprintln!("strip {w}: {d:?}");
        }
    }
    eprintln!("peak heap {:.1} MiB", used as f64 / (1 << 20) as f64);
    assert!(used < 32 << 20, "peak heap {used} bytes");
    assert!(
        stats.iter().any(|(_, d)| d.columns > 0),
        "some strip spilled"
    );
    assert_eq!(disk.f, ram.f);
    assert_eq!(disk.r2, ram.r2);
    assert_eq!((disk.steps, disk.peak), (ram.steps, ram.peak));
}
