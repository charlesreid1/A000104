//! OEIS b-files: lines "n a(n)", with comment lines starting with '#'.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// Reads a b-file into a map from n to a(n).
pub fn read(path: &Path) -> io::Result<BTreeMap<usize, u128>> {
    let text = fs::read_to_string(path)?;
    let mut terms = BTreeMap::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}:{}: {line:?}", path.display(), lineno + 1),
            )
        };
        let mut parts = line.split_whitespace();
        let n = parts.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
        let v = parts.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
        terms.insert(n, v);
    }
    Ok(terms)
}
