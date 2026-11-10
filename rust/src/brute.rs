//! Brute force for small n: every fixed polyomino by Redelmeier's algorithm, reduced to a
//! canonical form under the 8 symmetries of the square. Independent of `tm` and `search`.

use std::collections::{HashMap, HashSet};

type Cell = (i32, i32);

/// Counts for one size: fixed hole-free, free hole-free, and the fixed hole-free ones with
/// 180-degree, 90-degree, vertical-mirror and diagonal-mirror symmetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub fixed: u64,
    pub free: u64,
    pub r2: u64,
    pub r4: u64,
    pub m: u64,
    pub d: u64,
}

const MAPS: [fn(Cell) -> Cell; 8] = [
    |(x, y)| (x, y),
    |(x, y)| (-y, x),
    |(x, y)| (-x, -y),
    |(x, y)| (y, -x),
    |(x, y)| (-x, y),
    |(x, y)| (x, -y),
    |(x, y)| (y, x),
    |(x, y)| (-y, -x),
];

fn normalize(cells: impl Iterator<Item = Cell>) -> Vec<Cell> {
    let mut v: Vec<Cell> = cells.collect();
    let mx = v.iter().map(|c| c.0).min().unwrap();
    let my = v.iter().map(|c| c.1).min().unwrap();
    for c in &mut v {
        *c = (c.0 - mx, c.1 - my);
    }
    v.sort_unstable();
    v
}

fn hole_free(cells: &HashSet<Cell>) -> bool {
    let corners: HashSet<Cell> = cells
        .iter()
        .flat_map(|&(x, y)| [(x, y), (x + 1, y), (x, y + 1), (x + 1, y + 1)])
        .collect();
    let shared = cells
        .iter()
        .filter(|&&(x, y)| cells.contains(&(x + 1, y)))
        .count()
        + cells
            .iter()
            .filter(|&&(x, y)| cells.contains(&(x, y + 1)))
            .count();
    let edges = 4 * cells.len() - shared;
    corners.len() + cells.len() == edges + 1
}

/// Rows for sizes 0..=max (row 0 is all zeros).
pub fn brute(max: usize) -> Vec<Row> {
    let mut rows = vec![Row::default(); max + 1];
    let mut canon: HashMap<usize, HashSet<Vec<Cell>>> = HashMap::new();
    let mut poly = HashSet::new();
    let mut seen = HashSet::from([(0, 0)]);
    grow(
        vec![(0, 0)],
        max,
        &mut poly,
        &mut seen,
        &mut rows,
        &mut canon,
    );
    for (n, row) in rows.iter_mut().enumerate() {
        row.free = canon.get(&n).map_or(0, |s| s.len() as u64);
    }
    rows
}

fn grow(
    mut untried: Vec<Cell>,
    max: usize,
    poly: &mut HashSet<Cell>,
    seen: &mut HashSet<Cell>,
    rows: &mut [Row],
    canon: &mut HashMap<usize, HashSet<Vec<Cell>>>,
) {
    while let Some(c) = untried.pop() {
        poly.insert(c);
        visit(poly, rows, canon);
        if poly.len() < max {
            let (x, y) = c;
            let new: Vec<Cell> = [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)]
                .into_iter()
                .filter(|q| !seen.contains(q) && (q.1 > 0 || (q.1 == 0 && q.0 >= 0)))
                .collect();
            seen.extend(&new);
            let mut next = untried.clone();
            next.extend(&new);
            grow(next, max, poly, seen, rows, canon);
            for q in &new {
                seen.remove(q);
            }
        }
        poly.remove(&c);
    }
}

fn visit(poly: &HashSet<Cell>, rows: &mut [Row], canon: &mut HashMap<usize, HashSet<Vec<Cell>>>) {
    if !hole_free(poly) {
        return;
    }
    let n = poly.len();
    let images: Vec<Vec<Cell>> = MAPS
        .iter()
        .map(|g| normalize(poly.iter().map(|&c| g(c))))
        .collect();
    let row = &mut rows[n];
    row.fixed += 1;
    row.r2 += (images[2] == images[0]) as u64;
    row.r4 += (images[1] == images[0]) as u64;
    row.m += (images[4] == images[0]) as u64;
    row.d += (images[6] == images[0]) as u64;
    canon
        .entry(n)
        .or_default()
        .insert(images.into_iter().min().unwrap());
}
