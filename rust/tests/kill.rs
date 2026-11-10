//! Runs of the program killed with SIGKILL and run again on the same checkpoint directory,
//! against runs never stopped.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_a000104");

/// A directory under the system temporary directory, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        let path = std::env::temp_dir().join(format!("a000104-kill-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The program's stdout for `args`, from a run with no checkpoint.
fn whole(args: &[&str]) -> Vec<u8> {
    let out = Command::new(BIN).args(args).output().unwrap();
    assert!(out.status.success(), "{args:?}");
    out.stdout
}

/// The program for `args`, saving a column checkpoint at every column in `dir`.
fn checkpointed(args: &[&str], dir: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .arg("--checkpoint-dir")
        .arg(dir)
        .args(["--checkpoint-every", "0s"]);
    cmd
}

/// Runs `cmd` until a stderr line contains `at`, then kills it.
fn kill_after(mut cmd: Command, at: &str) {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        if line.unwrap().contains(at) {
            child.kill().unwrap();
            child.wait().unwrap();
            return;
        }
    }
    child.wait().unwrap();
    panic!("the run ended before printing {at:?}");
}

/// A finished run's stdout and stderr.
fn finish(mut cmd: Command) -> (Vec<u8>, String) {
    let out = cmd.output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    (out.stdout, err)
}

fn entries(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    // A run killed at once may not have made `dir` yet.
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    found.sort();
    found
}

#[test]
fn a_run_killed_after_a_column_checkpoint_resumes_from_it() {
    for (args, at) in [
        (
            &["fixed", "32"][..],
            "# checkpoint mod 2^61-1 strip 16 column 5:",
        ),
        (
            &["residues", "28"][..],
            "# checkpoint mod 65536 strip 14 column 5:",
        ),
    ] {
        let want = whole(args);
        let dir = Temp::new(&format!("column-{}", args[0]));
        kill_after(checkpointed(args, &dir.0), at);
        assert_eq!(
            entries(&dir.0, "column-").len(),
            1,
            "{args:?}: one column checkpoint"
        );
        let (got, err) = finish(checkpointed(args, &dir.0));
        assert!(
            err.contains(" from column ") && err.contains(" read, "),
            "{err}"
        );
        assert!(got == want, "{args:?}: differs from a run never stopped");
        assert!(
            entries(&dir.0, "column-").is_empty(),
            "column checkpoints left"
        );
    }
}

#[test]
fn a_damaged_column_checkpoint_is_not_used() {
    let args = ["residues", "28"];
    let want = whole(&args);
    let dir = Temp::new("damaged");
    kill_after(
        checkpointed(&args, &dir.0),
        "# checkpoint mod 65536 strip 14 column 6:",
    );
    let column = &entries(&dir.0, "column-")[0];
    let biggest = entries(column, "layer-")
        .into_iter()
        .max_by_key(|p| p.metadata().unwrap().len())
        .unwrap();
    let mut bytes = fs::read(&biggest).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 1;
    fs::write(&biggest, bytes).unwrap();
    let (got, err) = finish(checkpointed(&args, &dir.0));
    assert!(
        err.contains("is damaged") && err.contains("not used"),
        "{err}"
    );
    assert!(!err.contains(" read, "), "{err}");
    assert!(got == want, "differs from a run never stopped");
}

#[test]
fn a_column_checkpoint_resumes_onto_disk_or_into_ram() {
    let args = ["residues", "28"];
    let want = whole(&args);
    // Each run every column on disk, or in RAM.
    for (first, second) in [(true, true), (true, false), (false, true)] {
        let dir = Temp::new(&format!("modes-{first}-{second}"));
        let spill = Temp::new(&format!("modes-{first}-{second}-spill"));
        fs::create_dir_all(&spill.0).unwrap();
        let run = |disk: bool| {
            let mut cmd = checkpointed(&args, &dir.0);
            if disk {
                cmd.args(on_disk(&spill.0));
            }
            cmd
        };
        kill_after(run(first), "# checkpoint mod 65536 strip 14 column 6:");
        let (got, err) = finish(run(second));
        let read = if second {
            " read onto disk, "
        } else {
            " read, "
        };
        assert!(err.contains(read), "{err}");
        assert!(
            got == want,
            "{first} then {second}: differs from a run never stopped"
        );
    }
}

/// Arguments that run every column on disk, spilling under `spill`.
fn on_disk(spill: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["--mem-limit", "1G", "--force-buckets", "7", "--spill-dir"]
        .map(String::from)
        .into();
    args.push(spill.display().to_string());
    args
}

/// Runs `cmd` until a column checkpoint named `column-<w>-...` is being written in `dir` while
/// an older one of strip w is complete, then kills it. Returns whether the write was still
/// unfinished after the kill, or None if the run ended first.
fn kill_in_write(mut cmd: Command, dir: &Path, w: usize) -> Option<bool> {
    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let writing = || {
        let found = entries(dir, &format!("column-{w}-"));
        let tmp = |p: &&PathBuf| p.extension() == Some("tmp".as_ref());
        found.iter().any(|p| tmp(&p)) && found.iter().any(|p| !tmp(&p))
    };
    while child.try_wait().unwrap().is_none() {
        if writing() {
            child.kill().unwrap();
            child.wait().unwrap();
            return Some(writing());
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    None
}

/// Kills `residues 30` (with `extra`) three times inside a column checkpoint write of its widest
/// strip, then lets it finish and compares it with a run never stopped.
fn killed_inside_writes(name: &str, extra: &[String]) {
    let args = ["residues", "30"];
    let want = whole(&args);
    let dir = Temp::new(name);
    let run = || {
        let mut cmd = checkpointed(&args, &dir.0);
        cmd.args(extra);
        cmd
    };
    let mut inside = 0;
    while inside < 3 {
        inside +=
            kill_in_write(run(), &dir.0, 15).expect("the run ended before three kills") as usize;
    }
    let (got, err) = finish(run());
    assert!(err.contains(" from column "), "{err}");
    assert!(got == want, "{name}: differs from a run never stopped");
}

#[test]
fn runs_killed_inside_column_checkpoint_writes_resume() {
    killed_inside_writes("inside-ram", &[]);
    let spill = Temp::new("inside-spill");
    fs::create_dir_all(&spill.0).unwrap();
    killed_inside_writes("inside-disk", &on_disk(&spill.0));
}

#[test]
#[ignore = "needs A000104_OTHER_VOLUME"]
fn runs_spilled_to_another_volume_killed_inside_writes_resume() {
    let base = std::env::var_os("A000104_OTHER_VOLUME").expect(
        "set A000104_OTHER_VOLUME to a directory on another volume than the temporary directory",
    );
    let spill = Temp(PathBuf::from(base).join(format!("a000104-kill-{}", std::process::id())));
    fs::create_dir_all(&spill.0).unwrap();
    killed_inside_writes("inside-other", &on_disk(&spill.0));
}

/// Jensen's A006724 b-file, so that `components --fixed-from` runs the search alone.
const FIXED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../data/b006724.txt");

/// The unit files in `dir`, by name, with their contents.
fn units(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    entries(dir, "unit-")
        .into_iter()
        .filter(|p| p.extension() != Some("tmp".as_ref()))
        .map(|p| (p.file_name().unwrap().into(), fs::read(&p).unwrap()))
        .collect()
}

#[test]
fn a_killed_search_resumes_to_the_same_counts() {
    let args = ["components", "30", "--fixed-from", FIXED];
    let whole_dir = Temp::new("search-whole");
    let (want, _) = finish(checkpointed(&args, &whole_dir.0));
    let want_units = units(&whole_dir.0);
    let dir = Temp::new("search-killed");
    for quarter in 1..=3 {
        let mut child = checkpointed(&args, &dir.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        while units(&dir.0).len() < quarter * want_units.len() / 4 {
            assert!(
                child.try_wait().unwrap().is_none(),
                "the search ended first"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let first = &dir.0.join(&units(&dir.0)[0].0);
    let mut bytes = fs::read(first).unwrap();
    bytes[20] ^= 1;
    fs::write(first, bytes).unwrap();
    let (got, err) = finish(checkpointed(&args, &dir.0));
    assert!(
        err.contains(" units done") && err.contains("is damaged; the unit runs again"),
        "{err}"
    );
    assert!(got == want, "differs from a search never stopped");
    assert!(units(&dir.0) == want_units, "the unit files differ");
}

#[test]
fn two_and_fourteen_threads_write_the_same_unit_files() {
    let args = ["components", "28", "--fixed-from", FIXED];
    let found: Vec<Vec<(PathBuf, Vec<u8>)>> = ["2", "14"]
        .map(|threads| {
            let dir = Temp::new(&format!("threads-{threads}"));
            let mut cmd = checkpointed(&args, &dir.0);
            cmd.env("RAYON_NUM_THREADS", threads);
            finish(cmd);
            units(&dir.0)
        })
        .into();
    assert!(!found[0].is_empty());
    assert!(found[0] == found[1], "the unit files differ");
}

#[test]
fn counts_16_print_what_counts_64_print() {
    let want = whole(&["components", "30", "--r2", "both"]);
    let got = whole(&["components", "30", "--r2", "both", "--counts", "16"]);
    assert!(got == want, "components 30 differs with --counts 16");
    let out = Command::new(BIN)
        .args(["components", "30", "--counts", "16", "--fixed-from", FIXED])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "--counts 16 with --fixed-from is refused"
    );
}

#[test]
fn a_16_bit_run_killed_in_its_third_modulus_resumes() {
    let args = ["fixed", "30", "--counts", "16"];
    let want = whole(&["fixed", "30"]);
    let dir = Temp::new("counts16");
    kill_after(
        checkpointed(&args, &dir.0),
        "# checkpoint mod 65519 strip 15 column 5:",
    );
    let (got, err) = finish(checkpointed(&args, &dir.0));
    assert!(
        err.contains("# resuming: mod 65536 done; mod 65521 done; mod 65519 strips 1 to 14 done, strip 15 from column"),
        "{err}"
    );
    assert!(got == want, "differs from a run never stopped");
}

/// xorshift64, for kill times.
fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// What random kills hit: kills inside a write (the `inside` tests make sure of some), and runs
/// resumed from a column checkpoint, into RAM or onto disk.
#[derive(Default, Debug)]
struct Hits {
    in_write: usize,
    into_ram: usize,
    onto_disk: usize,
}

/// Kills runs of `fixed n` and `residues n`, n = 30 to 34, with `extra` arguments and a column
/// checkpoint at every column, eight times each at random times, then lets each finish and
/// compares it with a run never stopped. Each killed run's spill directory under `spill` goes.
fn random_kills(name: &str, extra: &[String], spill: Option<&Path>) -> Hits {
    let mut rng = std::time::SystemTime::UNIX_EPOCH
        .elapsed()
        .unwrap()
        .as_nanos() as u64
        | 1;
    eprintln!("seed {rng}");
    let mut hits = Hits::default();
    for n in 30..=34 {
        let n = n.to_string();
        for args in [["fixed", &n], ["residues", &n]] {
            let want = whole(&args);
            let t = Instant::now();
            let (got, _) = finish({
                let mut cmd = Command::new(BIN);
                cmd.args(args).args(extra);
                cmd
            });
            let span = t.elapsed();
            assert!(
                got == want,
                "{args:?} {extra:?}: differs from a run without them"
            );
            let dir = Temp::new(&format!("random-{name}-{}-{n}", args[0]));
            let (out, err) = (dir.0.with_extension("out"), dir.0.with_extension("err"));
            for run in 0.. {
                let mut cmd = checkpointed(&args, &dir.0);
                cmd.args(extra);
                // Every other run with another thread count, so other shard counts read the files.
                if run % 2 == 1 {
                    cmd.env("RAYON_NUM_THREADS", "3");
                }
                let mut child = cmd
                    .stdout(File::create(&out).unwrap())
                    .stderr(File::create(&err).unwrap())
                    .spawn()
                    .unwrap();
                let wait = Duration::from_millis(next(&mut rng) % span.as_millis().max(1) as u64);
                std::thread::sleep(wait);
                let stopped = run < 8 && child.try_wait().unwrap().is_none();
                if stopped {
                    child.kill().unwrap();
                }
                let status = child.wait().unwrap();
                let log = fs::read_to_string(&err).unwrap();
                hits.into_ram += log.contains(" read, ") as usize;
                hits.onto_disk += log.contains(" read onto disk, ") as usize;
                if stopped {
                    let tmp = |p: &PathBuf| p.extension() == Some("tmp".as_ref());
                    hits.in_write += entries(&dir.0, "").iter().any(tmp) as usize;
                    for d in spill
                        .map(|s| entries(s, "a000104-spill-"))
                        .unwrap_or_default()
                    {
                        fs::remove_dir_all(d).unwrap();
                    }
                    continue;
                }
                assert!(status.success(), "{args:?}: {log}");
                assert!(
                    fs::read(&out).unwrap() == want,
                    "{args:?}: differs after {run} kills"
                );
                eprintln!("{args:?}: {run} kills, then the same output");
                break;
            }
            let _ = (fs::remove_file(&out), fs::remove_file(&err));
        }
    }
    eprintln!("{hits:?}");
    hits
}

#[test]
#[ignore = "kills runs at n = 30 to 34 at random times; several minutes"]
fn runs_killed_at_random_times_resume_to_the_same_output() {
    let hits = random_kills("ram", &[], None);
    assert!(hits.into_ram > 0);
}

#[test]
#[ignore = "kills runs on disk at n = 30 to 34 at random times; ten minutes or more"]
fn runs_on_disk_killed_at_random_times_resume_to_the_same_output() {
    let spill = Temp::new("random-spill");
    fs::create_dir_all(&spill.0).unwrap();
    let hits = random_kills("disk", &on_disk(&spill.0), Some(&spill.0));
    assert!(hits.onto_disk > 0);
}

#[test]
#[ignore = "needs A000104_OTHER_VOLUME; kills runs on disk at random times"]
fn runs_spilled_to_another_volume_killed_at_random_times_resume_to_the_same_output() {
    let base = std::env::var_os("A000104_OTHER_VOLUME").expect(
        "set A000104_OTHER_VOLUME to a directory on another volume than the temporary directory",
    );
    let spill = Temp(PathBuf::from(base).join(format!("a000104-kill-{}", std::process::id())));
    fs::create_dir_all(&spill.0).unwrap();
    let hits = random_kills("other", &on_disk(&spill.0), Some(&spill.0));
    assert!(hits.onto_disk > 0);
}
