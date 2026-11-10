//! Checks against the OEIS data sections, the Python reference program and the brute force.

use a000104::ring::{Mod61, P61, Wrap64, crt};
use a000104::{Counts, brute, fixed_counts, search, terms, tm};

/// A000104, n = 0..28 (the entry's data section).
const A000104: &str = "1,1,1,2,5,12,35,107,363,1248,4460,16094,58937,217117,805475,3001127,11230003,\
42161529,158781106,599563893,2269506062,8609442688,32725637373,124621833354,475368834568,\
1816103345752,6948228104703,26618671505989,102102788362303";

/// A006724, n = 1..25 (the entry's data section).
const A006724: &str = "1,2,6,19,63,216,756,2684,9638,34930,127560,468837,1732702,6434322,23993874,\
89805691,337237337,1270123530,4796310672,18155586993,68874803609,261803388854,996971935098,\
3802944302442,14528816598358";

fn parse(s: &str) -> Vec<u128> {
    s.split(',').map(|t| t.parse().unwrap()).collect()
}

#[test]
fn step_counts_match_python() {
    // `python3 a000104.py N` prints these for its transfer matrix (no mirror merge).
    for (n, steps, peak) in [(16, 185_242, 1_312), (20, 1_691_773, 7_904)] {
        let fc = tm::fixed::<Wrap64>(n, false);
        assert_eq!((fc.steps, fc.peak), (steps, peak), "n = {n}");
    }
}

#[test]
fn fixed_matches_a006724() {
    let want = parse(A006724);
    for merge in [false, true] {
        let fc = fixed_counts(25, merge, Counts::Bits64, None, None);
        assert_eq!(&fc.f[1..], &want[..], "merge = {merge}");
    }
}

#[test]
fn crt_combines_both_runs() {
    let n = 20;
    let (a, b) = (tm::fixed::<Wrap64>(n, true), tm::fixed::<Mod61>(n, true));
    let want = parse(A006724);
    for w in 1..=n {
        assert_eq!(b.f[w].0 as u128, want[w - 1] % P61 as u128);
        assert_eq!(crt(a.f[w].0, b.f[w].0), want[w - 1]);
    }
}

#[test]
fn r2_from_sweep_matches_search() {
    let n = 20;
    let sym = search::symmetric(n, true, None);
    for merge in [false, true] {
        let r2 = fixed_counts(n, merge, Counts::Bits64, None, None).r2();
        let want: Vec<u128> = sym.r2.iter().map(|&x| x as u128).collect();
        assert_eq!(r2[1..], want[1..], "merge = {merge}");
    }
}

#[test]
fn components_match_brute_force() {
    let n = 10;
    let rows = brute::brute(n);
    let fc = fixed_counts(n, true, Counts::Bits64, None, None);
    let sym = search::symmetric(n, true, None);
    let t = terms(n, &fc.f, Some(&fc.r2()), &sym);
    for (w, r) in rows.iter().enumerate().skip(1) {
        let got = (t.f[w], t.a[w], t.r2[w], t.r4[w], t.m[w], t.d[w]);
        let want = (
            r.fixed as u128,
            r.free as u128,
            r.r2 as u128,
            r.r4 as u128,
            r.m as u128,
            r.d as u128,
        );
        assert_eq!(got, want, "n = {w}");
    }
}

#[test]
fn terms_match_a000104() {
    let n = 22;
    let fc = fixed_counts(n, true, Counts::Bits64, None, None);
    let sym = search::symmetric(n, false, None);
    let t = terms(n, &fc.f, Some(&fc.r2()), &sym);
    assert_eq!(t.a, parse(A000104)[..=n]);
}

#[test]
fn counts_16_match_counts_64() {
    // n = 32 takes five 16-bit runs and two 64-bit ones; n = 24, four and one.
    for n in [24, 32] {
        let (a, b) = (
            fixed_counts(n, true, Counts::Bits64, None, None),
            fixed_counts(n, true, Counts::Bits16, None, None),
        );
        assert_eq!(a.f, b.f, "F at n = {n}");
        assert_eq!(a.r2_parts, b.r2_parts, "R2 at n = {n}");
        assert_eq!((a.steps, a.peak), (b.steps, b.peak), "work at n = {n}");
    }
}
