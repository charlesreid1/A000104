//! Checkpoints: strip files, and sweeps that resume from them.

use a000104::checkpoint::{COMMIT, Checkpoint};
use a000104::ring::{Mod61, Ring, Wrap16, Wrap64};
use a000104::tm::{self, Fixed};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// No column checkpoints: these tests save finished strips only.
const NEVER: Duration = Duration::MAX;

/// A directory under the system temporary directory, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        let path = std::env::temp_dir().join(format!("a000104-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// F, R2, state-steps and peak, which a resumed sweep must reproduce exactly.
fn same<R: Ring>(a: &Fixed<R>, b: &Fixed<R>) -> bool {
    a.f == b.f && a.r2 == b.r2 && (a.steps, a.peak) == (b.steps, b.peak)
}

/// The sweep for n with checkpoint `ck`, and the strips it ran rather than read.
fn run<R: Ring>(n: usize, ck: &Checkpoint) -> (Fixed<R>, Vec<usize>) {
    let ran = Mutex::new(Vec::new());
    let fx = tm::fixed_with::<R>(n, true, None, Some(ck), |w, _| ran.lock().unwrap().push(w));
    (fx, ran.into_inner().unwrap())
}

fn resumes_after_a_kill_between_strips<R: Ring>(name: &str) {
    let n: usize = 26;
    let all = n.div_ceil(2);
    let whole = tm::fixed::<R>(n, true);
    for k in 1..all {
        let dir = Temp::new(&format!("{name}-{k}"));
        let ck = Checkpoint::open(&dir.0, "test 26", NEVER).unwrap();
        assert!(!ck.resumed());
        let killed = catch_unwind(AssertUnwindSafe(|| {
            tm::fixed_with::<R>(n, true, None, Some(&ck), |w, _| {
                assert_ne!(w, k, "killed after strip {k}")
            })
        }));
        assert!(killed.is_err());
        let ck = Checkpoint::open(&dir.0, "test 26", NEVER).unwrap();
        assert!(ck.resumed());
        let want = if k == 1 {
            "mod strip 1 done".to_string()
        } else {
            format!("mod strips 1 to {k} done")
        };
        assert_eq!(ck.describe::<R>("mod", n), want);
        let (fx, ran) = run::<R>(n, &ck);
        assert!(same(&fx, &whole), "killed after strip {k}");
        assert_eq!(ran, (k + 1..=all).collect::<Vec<_>>());
        assert_eq!(ck.describe::<R>("mod", n), "mod done");
        let (fx, ran) = run::<R>(n, &ck);
        assert!(same(&fx, &whole), "finished");
        assert!(ran.is_empty(), "a finished sweep runs no strip");
    }
}

#[test]
fn resumes_after_a_kill_between_strips_in_wrap16() {
    resumes_after_a_kill_between_strips::<Wrap16>("kill16");
}

#[test]
fn resumes_after_a_kill_between_strips_in_wrap64() {
    resumes_after_a_kill_between_strips::<Wrap64>("kill64");
}

#[test]
fn resumes_after_a_kill_between_strips_in_mod61() {
    resumes_after_a_kill_between_strips::<Mod61>("kill61");
}

#[test]
fn a_damaged_strip_file_runs_again() {
    let n = 24;
    let whole = tm::fixed::<Wrap64>(n, true);
    let dir = Temp::new("damaged");
    let ck = Checkpoint::open(&dir.0, "test 24", NEVER).unwrap();
    run::<Wrap64>(n, &ck);
    let flipped = dir.0.join("strip-5.2^64");
    let mut bytes = std::fs::read(&flipped).unwrap();
    bytes[100] ^= 1;
    std::fs::write(&flipped, bytes).unwrap();
    let cut = dir.0.join("strip-7.2^64");
    let bytes = std::fs::read(&cut).unwrap();
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
    let (fx, ran) = run::<Wrap64>(n, &ck);
    assert!(same(&fx, &whole));
    assert_eq!(ran, [5, 7]);
    let (_, ran) = run::<Wrap64>(n, &ck);
    assert!(ran.is_empty(), "the two files were written again");
}

#[test]
fn strip_files_are_kept_apart_by_modulus_and_size() {
    let dir = Temp::new("apart");
    let ck = Checkpoint::open(&dir.0, "test", NEVER).unwrap();
    run::<Wrap64>(22, &ck);
    let (fx, ran) = run::<Mod61>(22, &ck);
    assert!(same(&fx, &tm::fixed::<Mod61>(22, true)));
    assert_eq!(ran.len(), 11, "no 2^64 strip is read mod 2^61 - 1");
    // A strip's tables depend on n; the file says which n it holds.
    let (fx, ran) = run::<Wrap64>(24, &ck);
    assert!(same(&fx, &tm::fixed::<Wrap64>(24, true)));
    assert_eq!(ran.len(), 12);
}

#[test]
fn a_directory_of_another_command_or_build_is_refused() {
    let dir = Temp::new("settings");
    Checkpoint::open(&dir.0, "components 30", NEVER).unwrap();
    let e = Checkpoint::open(&dir.0, "components 31", NEVER)
        .err()
        .unwrap();
    let fix = format!("rm -r {}", dir.0.display());
    assert!(
        e.contains("holds a checkpoint of components 30 at") && e.ends_with(&fix),
        "{e}"
    );
    let settings = dir.0.join("settings");
    let text = std::fs::read_to_string(&settings).unwrap();
    std::fs::write(&settings, text.replace(COMMIT, "0000000")).unwrap();
    let e = Checkpoint::open(&dir.0, "components 30", NEVER)
        .err()
        .unwrap();
    let want = format!("components 30 at 0000000, not of components 30 at {COMMIT};");
    assert!(e.contains(&want), "{e}");

    let other = Temp::new("settings-other");
    std::fs::create_dir_all(&other.0).unwrap();
    std::fs::write(other.0.join("notes"), "").unwrap();
    let e = Checkpoint::open(&other.0, "components 30", NEVER)
        .err()
        .unwrap();
    assert!(e.contains("holds files but no checkpoint settings"), "{e}");
}
