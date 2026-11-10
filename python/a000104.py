"""
A000104: number of n-celled free polyominoes without holes.

Burnside over the 8 symmetries of the square:  8 a(n) = F + R2 + 2 R4 + 2 M + 2 D, where
F counts fixed hole-free n-ominoes (= self-avoiding polygons of area n, A006724) and
R2, R4, M, D count the fixed ones invariant under a 180-degree rotation, a 90-degree rotation,
a vertical mirror and a diagonal mirror.  See a000104.md for the method.

Usage: python3 a000104.py N [--check]
"""

import sys
import time
from pathlib import Path

DATA = Path(__file__).resolve().parent.parent / "data"


# F(n): transfer matrix over self-avoiding polygons, swept vertex by vertex down each column.


def sap_strip(W, n, stats):
    """
    T[L]: packed area polynomial of polygons of length L >= W within W cell rows, per placement.
    """
    # Bits per packed coefficient
    B = 4 * n + 64
    occ = int("01" * (W + 2), 2)
    T = [0] * (n + 2)
    states = {0: 1}
    for c in range(n + 2):
        # Each later column adds at least one cell
        top = n - max(0, W - 1 - c)
        mask = (1 << B * (top + 1)) - 1
        states = {s: p & mask for s, p in states.items() if p & mask}
        for y in range(W + 1):
            sh, sh1 = 2 * y, 2 * y + 2
            low = occ & ((1 << sh) - 1)
            new = {}
            stats["steps"] += len(states)
            for s, p in states.items():
                d, l = (s >> sh) & 3, (s >> sh1) & 3
                rest = s & ~(15 << sh)
                par = (((s | s >> 1) & low).bit_count()) & 1
                if d == 0 and l == 0:
                    outs = [(rest, 0)]
                    if y < W:
                        outs.append((rest | 1 << sh | 2 << sh1, 1))
                elif d == 0 or l == 0:
                    a = d | l
                    outs = [(rest | a << sh, a)]
                    if y < W:
                        outs.append((rest | a << sh1, 0))
                elif d == 1 and l == 2:
                    # Closing the last arc completes the polygon
                    if rest == 0 and c >= W:
                        T[c] += p
                    continue
                elif d == 2 and l == 1:
                    outs = [(rest, 0)]
                elif d == 1:
                    # 1,1: partner of the upper 1 becomes a 1
                    j, depth = y + 2, 1
                    while True:
                        v = (s >> 2 * j) & 3
                        depth += (v == 1) - (v == 2)
                        if depth == 0:
                            break
                        j += 1
                    outs = [(rest ^ 3 << 2 * j, 0)]
                else:
                    # 2,2: partner of the lower 2 becomes a 2
                    j, depth = y - 1, 1
                    while True:
                        v = (s >> 2 * j) & 3
                        depth += (v == 2) - (v == 1)
                        if depth == 0:
                            break
                        j -= 1
                    outs = [(rest ^ 3 << 2 * j, 0)]
                for s2, r in outs:
                    q = (p << B) & mask if y < W and par ^ (r != 0) else p
                    if q:
                        new[s2] = new.get(s2, 0) + q
            states = new
            stats["peak"] = max(stats["peak"], len(states))
        states = {s << 2: p for s, p in states.items() if s}
        if not states:
            break
    cm = (1 << B) - 1
    return [[(T[L] >> B * a) & cm for L in range(n + 2)] for a in range(n + 1)]


def fixed_counts(N):
    """
    F[a] for a <= N: fixed hole-free polyominoes of a cells (A006724), and work counts.
    """
    # height <= length, so height <= (N+1)/2
    H = (N + 1) // 2
    zero = [[0] * (N + 2) for _ in range(N + 1)]
    # States processed; largest state set
    stats = {"steps": 0, "peak": 0}
    T = [zero] + [sap_strip(W, N, stats) for W in range(1, H + 1)]
    F = [0] * (N + 1)
    for h in range(1, H + 1):
        prev2 = T[h - 2] if h >= 2 else zero
        for a in range(N + 1):
            for L in range(h, N + 2):
                e = T[h][a][L] - 2 * T[h - 1][a][L] + prev2[a][L]
                F[a] += e if L == h else 2 * e
    return F, stats


# Symmetric counts: connected unions of orbits of one symmetry g, grown Redelmeier-style.


def invariant_counts(N, g, runs):
    """
    counts[w]: hole-free P = g(P) with w <= N cells, summed over runs of (core, forbidden) cells.
    """
    R = N + 3
    S = 2 * R + 1
    rep, members = {}, {}
    for x in range(-N - 1, N + 2):
        for y in range(-N - 1, N + 2):
            orb = [(x, y)]
            while (q := g(*orb[-1])) != orb[0]:
                orb.append(q)
            if all(abs(u) <= N + 1 and abs(v) <= N + 1 for u, v in orb):
                ids = sorted({(u + R) * S + v + R for u, v in orb})
                rep[(x + R) * S + y + R] = ids[0]
                members[ids[0]] = ids
    nbrs = {}
    for r, ids in members.items():
        nbrs[r] = sorted(
            {
                rep[j]
                for i in ids
                for j in (i + 1, i - 1, i + S, i - S)
                if j in rep and rep[j] != r
            }
        )
    occ = bytearray(S * S)

    def delta(i):  # change in V - E + F from adding cell i
        e, w, nn, so = occ[i + S], occ[i - S], occ[i + 1], occ[i - 1]
        v = (
            (not (e or nn or occ[i + S + 1]))
            + (not (w or nn or occ[i - S + 1]))
            + (not (e or so or occ[i + S - 1]))
            + (not (w or so or occ[i - S - 1]))
        )
        return v + e + w + nn + so - 3

    counts = [0] * (N + 1)

    def rec(untried, wt, chi):
        while untried:
            r = untried.pop()
            mem = members[r]
            w2 = wt + len(mem)
            if w2 > N:
                continue
            c2 = chi
            for i in mem:
                c2 += delta(i)
                occ[i] = 1
            if c2 == 1:
                # connected, so Euler characteristic 1 = no holes
                counts[w2] += 1
            if w2 < N:
                new = [u for u in nbrs[r] if not seen[u]]
                for u in new:
                    seen[u] = 1
                rec(untried + new, w2, c2)
                for u in new:
                    seen[u] = 0
            for i in mem:
                occ[i] = 0

    for core, forbid in runs:
        seen = bytearray(S * S)
        core_reps = sorted({rep[(x + R) * S + y + R] for x, y in core})
        wt = sum(len(members[r]) for r in core_reps)
        if wt > N:
            continue
        for x, y in forbid:
            i = (x + R) * S + y + R
            if i in rep:
                seen[rep[i]] = 1
        chi = 0
        for r in core_reps:
            seen[r] = 1
            for i in members[r]:
                chi += delta(i)
                occ[i] = 1
        if chi == 1:
            counts[wt] += 1
        untried = sorted({u for r in core_reps for u in nbrs[r] if not seen[u]})
        for u in untried:
            seen[u] = 1
        rec(untried, wt, chi)
        for r in core_reps:
            for i in members[r]:
                occ[i] = 0
    return counts


def symmetric_counts(N):
    """
    R2, R4, M, D as lists over 0..N; cores and spines are forced for hole-free P (see a000104.md).
    """
    rng = range(-N - 1, N + 2)
    quad = [(0, 0), (1, 0), (0, 1), (1, 1)]
    r2 = [
        invariant_counts(N, lambda x, y: (-x, -y), [([(0, 0)], [])]),
        invariant_counts(N, lambda x, y: (1 - x, -y), [([(0, 0), (1, 0)], [])]),
        invariant_counts(N, lambda x, y: (1 - x, 1 - y), [(quad, [])]),
    ]
    r4 = [
        invariant_counts(N, lambda x, y: (-y, x), [([(0, 0)], [])]),
        invariant_counts(N, lambda x, y: (1 - y, x), [(quad, [])]),
    ]
    m = [
        invariant_counts(
            N,
            lambda x, y: (-x, y),
            [
                ([(0, j) for j in range(k)], [(0, j) for j in rng if not 0 <= j < k])
                for k in range(1, N + 1)
            ],
        ),
        invariant_counts(
            N,
            lambda x, y: (1 - x, y),
            [
                (
                    [(x, j) for j in range(k) for x in (0, 1)],
                    [(x, j) for j in rng if not 0 <= j < k for x in (0, 1)],
                )
                for k in range(1, N // 2 + 1)
            ],
        ),
    ]
    d = invariant_counts(
        N,
        lambda x, y: (y, x),
        [
            (
                [(j, j) for j in range(k)]
                + [(j + 1, j) for j in range(k - 1)]
                + [(j, j + 1) for j in range(k - 1)],
                [(j, j) for j in rng if not 0 <= j < k],
            )
            for k in range(1, (N + 2) // 3 + 1)
        ],
    )
    R2 = [r2[0][w] + 2 * r2[1][w] + r2[2][w] for w in range(N + 1)]
    R4 = [r4[0][w] + r4[1][w] for w in range(N + 1)]
    M = [m[0][w] + m[1][w] for w in range(N + 1)]
    return R2, R4, M, d


def terms(N):
    t0 = time.time()
    F, st = fixed_counts(N)
    t1 = time.time()
    R2, R4, M, D = symmetric_counts(N)
    t2 = time.time()
    print(
        f"# transfer matrix {t1 - t0:.1f}s ({st['steps']:,} state-steps, peak {st['peak']:,} states),"
        f" symmetric enumeration {t2 - t1:.1f}s",
        file=sys.stderr,
    )
    a = [1]
    for n in range(1, N + 1):
        tot = F[n] + R2[n] + 2 * R4[n] + 2 * M[n] + 2 * D[n]
        assert tot % 8 == 0, (n, tot)
        a.append(tot // 8)
    return a, F, R2, R4, M, D


# Brute force for checking: every fixed polyomino, by Redelmeier's algorithm.


def brute(N):
    """
    Per size n <= N: [fixed hole-free, free hole-free, R2, R4, M, D], all by direct enumeration.
    """
    out = [[0] * 6 for _ in range(N + 1)]
    canon = [set() for _ in range(N + 1)]
    maps = [
        lambda x, y: (x, y),
        lambda x, y: (-y, x),
        lambda x, y: (-x, -y),
        lambda x, y: (y, -x),
        lambda x, y: (-x, y),
        lambda x, y: (x, -y),
        lambda x, y: (y, x),
        lambda x, y: (-y, -x),
    ]

    def norm(cells):
        mx = min(x for x, _ in cells)
        my = min(y for _, y in cells)
        return tuple(sorted((x - mx, y - my) for x, y in cells))

    def holefree(cells):
        verts = {(x + dx, y + dy) for x, y in cells for dx in (0, 1) for dy in (0, 1)}
        edges = 4 * len(cells) - sum(
            ((x + 1, y) in cells) + ((x, y + 1) in cells) for x, y in cells
        )
        return len(verts) - edges + len(cells) == 1

    def visit(cells):
        n = len(cells)
        if not holefree(cells):
            return
        row = out[n]
        row[0] += 1
        base = norm(cells)
        images = [norm([g(x, y) for x, y in cells]) for g in maps]
        canon[n].add(min(images))
        row[2] += images[2] == base
        row[3] += images[1] == base
        row[4] += images[4] == base
        row[5] += images[6] == base

    poly, seen = set(), {(0, 0)}

    def rec(untried):
        while untried:
            c = untried.pop()
            poly.add(c)
            visit(poly)
            if len(poly) < N:
                x, y = c
                new = [
                    q
                    for q in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1))
                    if q not in seen and (q[1] > 0 or (q[1] == 0 and q[0] >= 0))
                ]
                seen.update(new)
                rec(untried + new)
                seen.difference_update(new)
            poly.remove(c)

    rec([(0, 0)])
    for n in range(N + 1):
        out[n][1] = len(canon[n])
    return out


def read_bfile(path):
    with open(path) as f:
        return {
            int(k): int(v)
            for k, v in (
                line.split() for line in f if line.strip() and not line.startswith("#")
            )
        }


def main():
    N = int(sys.argv[1])
    a, F, R2, R4, M, D = terms(N)
    if "--check" in sys.argv:
        b104 = read_bfile(DATA / "b000104.original.txt")
        b6724 = read_bfile(DATA / "b006724.original.ERROR.txt")
        bad = [n for n in range(N + 1) if n in b104 and a[n] != b104[n]]
        bad += [n for n in range(1, N + 1) if n in b6724 and F[n] != b6724[n]]
        nb = min(N, 11)
        for n, row in enumerate(brute(nb)):
            if n and row != [F[n], a[n], R2[n], R4[n], M[n], D[n]]:
                bad.append(n)
        print(
            f"# checked against the OEIS b-files and brute force (n <= {nb}):",
            "OK" if not bad else f"MISMATCH at {sorted(set(bad))}",
            file=sys.stderr,
        )
    for n, v in enumerate(a):
        print(n, v)


if __name__ == "__main__":
    main()
