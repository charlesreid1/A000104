//! Sets A000104_COMMIT, the build a checkpoint is tied to: git's short hash, plus a hash of the
//! sources when rust/ has uncommitted changes. Outside git (the EC2 bundle) it passes the
//! A000104_COMMIT variable through.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// FNV-1a over the files under `path`, in name order.
fn hash_tree(path: &Path, h: &mut u64) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            hash_tree(&e, h);
        }
    } else {
        for b in path
            .to_string_lossy()
            .bytes()
            .chain(std::fs::read(path).unwrap())
        {
            *h = (*h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
}

fn main() {
    const SOURCES: [&str; 4] = ["src", "build.rs", "Cargo.toml", "Cargo.lock"];
    for p in SOURCES {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-env-changed=A000104_COMMIT");
    let commit = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(head) => {
            if let Some(log) = git(&["rev-parse", "--git-path", "logs/HEAD"]) {
                println!("cargo:rerun-if-changed={log}");
            }
            let clean = git(&["status", "--porcelain", "--", "."]).is_some_and(|s| s.is_empty());
            if clean {
                head
            } else {
                let mut h = 0xcbf2_9ce4_8422_2325;
                for p in SOURCES {
                    hash_tree(Path::new(p), &mut h);
                }
                format!("{head}-dirty-{:08x}", h as u32)
            }
        }
        None => std::env::var("A000104_COMMIT").unwrap_or_else(|_| "unknown".into()),
    };
    println!("cargo:rustc-env=A000104_COMMIT={commit}");
}
