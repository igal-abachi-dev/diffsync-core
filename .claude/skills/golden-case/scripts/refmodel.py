"""Reference model for diffsync-core golden values (plan.md §11.3).

An independent Python model of the spec, used to verify hand-derived golden values.
It is NOT the engine. It covers:
  * line hash (CRC-32 EDB88320, init FFFFFFFF, no final xor, UTF-16LE) and file CRC-32
  * line rules: whitespace modes (D3), blank set (D4), simple ignore-case (D5 approx.),
    unimportant regexes
  * weights, closeness (ALIGN-1.6 + D2)
  * the Standard / None aligner: top-level walk with pending gap (1.3), orphan chunks of 200
    (1.4), the skew window with the L-shaped scan, diagonal runs, run selection with the B3
    clipping and the D1 rule, big runs (B1 + B2), gap DP with partial traces (1.5)
  * row classification (§5.4)
Anything outside that scope (Patience, Myers, Histogram, custom weights) raises OutOfScope ->
derive by hand (see SKILL.md).

Run with `python -I refmodel.py ...`. Output is ASCII-escaped, so any console works.
Commands:
  selftest
  hash TEXT [TEXT...]
  filecrc FILE
  closeness A B [--same-only] [--no-closeness] [--never-align]
  align LEFT RIGHT [--options options.json] [--kind standard|none] [--skew N]
        [--whitespace all|leadingTrailing|changes|ignoreAll] [--ignore-case]
        [--unimportant REGEX ...] [--orphans-sig] [--no-closeness] [--same-only]
        [--never-align] [--json]
"""
import argparse
import json
import re
import sys
import zlib
from collections import Counter

# ---------------------------------------------------------------- hashing
_T = []
for _i in range(256):
    _c = _i
    for _ in range(8):
        _c = (_c >> 1) ^ 0xEDB88320 if _c & 1 else _c >> 1
    _T.append(_c)


def crc_update(crc, data):
    for b in data:
        crc = _T[(crc ^ b) & 0xFF] ^ (crc >> 8)
    return crc


def line_hash(s):
    """spec ALIGN-1.2: '' -> FFFFFFFF; else CRC over UTF-16LE, init FFFFFFFF, no final xor."""
    if s == "":
        return 0xFFFFFFFF
    return crc_update(0xFFFFFFFF, s.encode("utf-16-le"))


def file_crc(data):
    return zlib.crc32(data) & 0xFFFFFFFF


# ---------------------------------------------------------------- rules
BLANKS = set(" \u0009\u000b\u000c ﻿   　")  # D4
BLANKS |= {chr(c) for c in range(0x2000, 0x200B)}


def simple_fold(ch):
    # D5 approximation: Python has no simple case folding; keep length 1 per code point.
    low = ch.lower()
    return low if len(low) == 1 else ch


def normalize(line, opts):
    s = line
    for rx in opts["_regexes"]:
        s = rx.sub("", s)
    ws = opts["whitespace"]
    if ws == "leadingTrailing":
        a, b = 0, len(s)
        while a < b and s[a] in BLANKS:
            a += 1
        while b > a and s[b - 1] in BLANKS:
            b -= 1
        s = s[a:b]
    elif ws in ("changes", "ignoreAll"):
        out, in_blank = [], False
        for ch in s:
            if ch in BLANKS:
                in_blank = True
            else:
                if in_blank and out and ws == "changes":
                    out.append(" ")
                in_blank = False
                out.append(ch)
        s = "".join(out)
    if opts["ignoreCase"]:
        s = "".join(simple_fold(c) for c in s)
    return s


# ---------------------------------------------------------------- options
DEFAULTS = {
    "alignment": "standard",
    "skewTolerance": 2000,
    "useCloseness": None,  # None -> kind != myers
    "closenessSameOnly": False,
    "neverAlignDiffs": False,
    "orphansAlwaysSig": False,
    "ignoreCase": False,
    "whitespace": "leadingTrailing",  # D3 default
    "unimportant": [],
}


def make_opts(**over):
    o = dict(DEFAULTS)
    o.update({k: v for k, v in over.items() if v is not None})
    o["_regexes"] = [re.compile(p) for p in o["unimportant"]]
    kind = o["alignment"]
    if kind not in ("standard", "none"):
        raise OutOfScope(f"alignment '{kind}' is not modelled (Standard/None only)")
    use_cl = o["useCloseness"] if o["useCloseness"] is not None else True
    if o["neverAlignDiffs"] and use_cl and o["useCloseness"] is True:
        raise ValueError("neverAlignDiffs and useCloseness cannot both be on")
    if kind == "none":
        o["_skew"], o["_closeness"] = 1, False
    else:
        o["_skew"] = int(o["skewTolerance"])
        o["_closeness"] = use_cl and not o["neverAlignDiffs"]
    o["_same_only"] = o["_closeness"] and o["closenessSameOnly"]
    return o


class OutOfScope(Exception):
    pass


# Event counters for the differential test: which parts of the window machinery ran.
STATS = {}


def _count(name):
    STATS[name] = STATS.get(name, 0) + 1


# ---------------------------------------------------------------- model
GAP = -20
CHUNK = 200
MIN_RUN = 500
BIG_RUN = 5000
RUN_PIECE = 500  # emit a piece when its count exceeds this (501-line pieces)


class Side:
    def __init__(self, lines, opts):
        self.full = lines
        self.cmp = [normalize(l, opts) for l in lines]
        self.sig = [c != "" for c in self.cmp]
        self.hash = [line_hash(c if s else f) for c, s, f in zip(self.cmp, self.sig, lines)]
        self.weight = [2 if f == "" else (70 if s else 31) for f, s in zip(lines, self.sig)]
        # ClosenessText: comparable text, or the full line when it is empty
        self.ctext = [(c if c else f).encode("utf-8") for c, f in zip(self.cmp, lines)]


def overlap(a, b):
    ca, cb = Counter(a), Counter(b)
    return sum(min(ca[k], cb[k]) for k in ca)


def closeness_bytes(a, b, same_only):
    la, lb = len(a), len(b)
    if la == 0 or lb == 0:
        return -18 if same_only else max(-40, min(-11, -4 * (la + lb) - 1))
    c = overlap(a, b)
    if same_only:
        return c
    s = 3 * c - la - lb
    return (s * 25) // min(la, lb) if s > 0 else max(-40, 4 * s - 1)


class Aligner:
    def __init__(self, left, right, opts):
        self.o = opts
        self.L = Side(left, opts)
        self.R = Side(right, opts)
        self.ops = []

    def emit(self, s, t, n):
        if n > 0:
            self.ops.append((s, t, n))

    def score(self, i, j):
        L, R = self.L, self.R
        if L.hash[i] == R.hash[j]:  # D2
            if L.cmp[i] == "" or R.cmp[j] == "":
                return -10
            return 2 * (L.weight[i] + R.weight[j]) - (0 if L.full[i] == R.full[j] else 1)
        if self.o["_closeness"]:
            return closeness_bytes(L.ctext[i], R.ctext[j], self.o["_same_only"])
        return -5000 if self.o["neverAlignDiffs"] else -18

    def gap_dp(self, s0, s1, t0, t1, partial):
        """spec 1.5: backward NW, forward trace, ties pair > src > tgt, coalesced.
        Returns lines consumed (cs, ct)."""
        n, m = s1 - s0, t1 - t0
        if m == 0:
            self.emit(s0, None, n)
            return n, 0
        if n == 0:
            self.emit(None, t0, m)
            return 0, m
        D = [[0] * (m + 1) for _ in range(n + 1)]
        for i in range(n, -1, -1):
            for j in range(m, -1, -1):
                if i == n and j == m:
                    continue
                best = None
                if i < n and j < m:
                    best = self.score(s0 + i, t0 + j) + D[i + 1][j + 1]
                if i < n:
                    v = GAP + D[i + 1][j]
                    best = v if best is None or v > best else best
                if j < m:
                    v = GAP + D[i][j + 1]
                    best = v if best is None or v > best else best
                D[i][j] = best
        i = j = 0
        run = None  # [kind, s, t, count]
        while i < n or j < m:
            if partial and (i == n or j == m):
                break
            cands = []
            if i < n and j < m:
                cands.append(("P", self.score(s0 + i, t0 + j) + D[i + 1][j + 1]))
            if i < n:
                cands.append(("S", GAP + D[i + 1][j]))
            if j < m:
                cands.append(("T", GAP + D[i][j + 1]))
            k = cands[0]
            for c in cands[1:]:
                if c[1] > k[1]:  # strict: earlier kind wins ties
                    k = c
            kind = k[0]
            if run and run[0] == kind:
                run[3] += 1
            else:
                if run:
                    self._emit_run(run)
                run = [kind, s0 + i if kind != "T" else None, t0 + j if kind != "S" else None, 1]
            if kind == "P":
                i += 1
                j += 1
            elif kind == "S":
                i += 1
            else:
                j += 1
        if run:
            self._emit_run(run)
        return i, j

    def _emit_run(self, run):
        self.emit(run[1], run[2], run[3])

    # ---- window machinery (spec 1.4; fixes B1, B2, B3; decision D1) -------------------
    def find_run(self, i, j, ws, wt, visited, runs):
        """Visit cell (i, j). Returns (src, tgt, count) when the run reaches weight >= 5000."""
        if (i, j) in visited:
            return None
        rs, rt, cnt, w = i, j, 0, 0
        while i < ws and j < wt and self.L.hash[i] == self.R.hash[j]:
            w += self.L.weight[i]
            cnt += 1
            i += 1
            j += 1
            if w >= BIG_RUN:
                return (rs, rt, cnt)
        for k in range(cnt + 1):  # the run plus the cell that stops it, inside the window
            a, b = rs + k, rt + k
            if a >= ws or b >= wt:
                break
            visited.add((a, b))
        if w > MIN_RUN:
            runs.append([cnt, w, rs, rt])
        return None

    def search_window(self, s, ws, t, wt):
        visited, runs = set(), []
        big = self.find_run(s, t, ws, wt, visited, runs)
        if big:
            return None, big
        i, j = s, t
        while True:
            if j + 1 < wt:
                j += 1
            if i + 1 < ws:
                i += 1
            for k in range(i, s - 1, -1):  # column j, from row i upwards
                big = self.find_run(k, j, ws, wt, visited, runs)
                if big:
                    return None, big
            for k in range(j - 1, t - 1, -1):  # row i, leftwards from column j - 1
                big = self.find_run(i, k, ws, wt, visited, runs)
                if big:
                    return None, big
            if i + 1 >= ws and j + 1 >= wt:
                return runs, None

    def reweigh(self, r):  # r = [count, weight, src, tgt]
        w = sum(self.L.weight[r[2] + k] for k in range(r[0])) if r[0] > 0 else 0
        r[1] = 0 if w < MIN_RUN else w

    def select(self, runs, s, ws, t, wt, more_after):
        """Clip (B3), rank, emit with gaps between; returns where the window loop goes on."""
        runs = [list(r) for r in runs]
        by_weight = lambda r: (-r[1], r[3], r[2])
        if runs:
            if more_after:
                last_s, last_t = ws - 1, wt - 1
                for r in runs:
                    if r[0] + r[2] > last_s or r[0] + r[3] > last_t:
                        _count('clip')
                        r[0] = min(last_s - r[2], last_t - r[3], r[0])
                        self.reweigh(r)
            runs.sort(key=by_weight)
            for x in range(len(runs)):
                X = runs[x]
                if X[1] <= 0:
                    continue
                changed = False
                a_es, a_et = X[2] + X[0] - 1, X[3] + X[0] - 1
                for y in range(x + 1, len(runs)):
                    Y = runs[y]
                    if Y[1] <= 0:
                        continue
                    b_es, b_et = Y[2] + Y[0] - 1, Y[3] + Y[0] - 1
                    if Y[2] > a_es and Y[3] > a_et:
                        continue
                    if b_es < X[2] and b_et < X[3]:
                        continue
                    if Y[2] < X[2] and Y[3] < X[3] and b_es < a_es and b_et < a_et:
                        _count('trim_tail')
                        Y[0] = min(X[2] - Y[2], X[3] - Y[3])
                        self.reweigh(Y)
                        changed = True
                    elif Y[2] > X[2] and Y[3] > X[3] and b_es > a_es and b_et > a_et:
                        _count('trim_head')
                        d = max(a_es - Y[2] + 1, a_et - Y[3] + 1)
                        Y[0] -= d
                        Y[2] += d
                        Y[3] += d
                        self.reweigh(Y)
                        changed = True
                    else:
                        _count('drop')
                        Y[1] = -1
                if changed:
                    runs[x + 1:] = sorted(runs[x + 1:], key=by_weight)
            runs.sort(key=lambda r: (r[3], r[2]))
        cs, ct, accepted = s, t, 0
        for cnt, w, rs, rt in runs:
            if w <= 0 or rs < cs or rt < ct:
                continue
            accepted += 1
            if rs > cs or rt > ct:
                self.gap_dp(cs, rs, ct, rt, False)
            self.emit(rs, rt, cnt)
            cs, ct = rs + cnt, rt + cnt
        if accepted > 1:
            _count('multi_run_window')
        if accepted and more_after:
            _count('slide_commit')
            return cs, ct
        if cs < ws or ct < wt:
            partial = more_after and cs < ws and ct < wt
            _count('partial' if partial else 'gap_fill_to_stop')
            dc, dt = self.gap_dp(cs, ws, ct, wt, partial)
            return cs + dc, ct + dt
        return cs, ct

    def extend(self, big, s_stop, t_stop):
        """B1: extend a big run to the OUTER block stop, emitting pieces of 501 lines."""
        rs, rt, cnt = big
        i, j = rs + cnt, rt + cnt
        while i < s_stop and j < t_stop:
            if cnt > RUN_PIECE:
                self.emit(rs, rt, cnt)
                cnt, rs, rt = 0, i, j
            if self.L.hash[i] != self.R.hash[j]:
                break
            cnt += 1
            i += 1
            j += 1
        if cnt > 0:
            self.emit(rs, rt, cnt)
        return rs + cnt, rt + cnt

    def block(self, s, s_stop, t, t_stop):
        skew = self.o["_skew"]
        while s < s_stop or t < t_stop:
            if s >= s_stop:
                while t < t_stop:
                    c = min(CHUNK, t_stop - t)
                    self.emit(None, t, c)
                    t += c
                return
            if t >= t_stop:
                while s < s_stop:
                    c = min(CHUNK, s_stop - s)
                    self.emit(s, None, c)
                    s += c
                return
            ws, wt = min(s + skew, s_stop), min(t + skew, t_stop)
            more_after = ws < s_stop or wt < t_stop  # D1
            runs, big = self.search_window(s, ws, t, wt)
            if big:
                _count('big_run')
                self.block(s, big[0], t, big[1])  # B2: the block before the run first
                s, t = self.extend(big, s_stop, t_stop)  # B1
                continue
            s, t = self.select(runs, s, ws, t, wt, more_after)

    def run(self):
        L, R = self.L, self.R
        nL, nR = len(L.full), len(R.full)
        i = j = 0
        ps = pt = None
        oas = self.o["orphansAlwaysSig"]
        while i < nL and j < nR:  # spec 1.3
            sS, sT = L.sig[i], R.sig[j]
            if not sS and not sT:
                if ps is None and pt is None and L.hash[i] == R.hash[j]:
                    self.emit(i, j, 1)
                else:
                    ps = i if ps is None else ps
                    pt = j if pt is None else pt
                i += 1
                j += 1
            elif not sS:
                if oas:
                    break
                ps = i if ps is None else ps
                i += 1
            elif not sT:
                if oas:
                    break
                pt = j if pt is None else pt
                j += 1
            else:
                if L.hash[i] != R.hash[j]:
                    break
                if ps is not None or pt is not None:
                    ps = i if ps is None else ps
                    pt = j if pt is None else pt
                    self.block(ps, i, pt, j)
                    ps = pt = None
                self.emit(i, j, 1)
                i += 1
                j += 1
        if ps is not None:
            i = ps
        if pt is not None:
            j = pt
        self.block(i, nL, j, nR)
        return self.ops

    def rows(self):
        L, R, oas = self.L, self.R, self.o["orphansAlwaysSig"]
        out = []
        for s, t, n in self.ops:
            for k in range(n):
                if s is not None and t is not None:
                    a, b = s + k, t + k
                    if L.full[a] == R.full[b]:
                        kind = "Same"
                    elif L.cmp[a] == R.cmp[b]:
                        kind = "Unimportant"
                    else:
                        kind = "Different"
                    out.append((a, b, kind))
                elif t is None:
                    a = s + k
                    imp = oas or L.sig[a]
                    out.append((a, None, "LeftOnly" if imp else "UnimportantLeftOnly"))
                else:
                    b = t + k
                    imp = oas or R.sig[b]
                    out.append((None, b, "RightOnly" if imp else "UnimportantRightOnly"))
        return out


def align(left, right, opts):
    a = Aligner(left, right, opts)
    a.run()
    return a.ops, a.rows()


# ---------------------------------------------------------------- formatting
def fmt_side(v):
    return "-" if v is None else str(v)


def ops_text(ops):
    return "\n".join(f"{fmt_side(s)} {fmt_side(t)} {n}" for s, t, n in ops)


def rows_text(rows):
    return "\n".join(f"{fmt_side(a)} {fmt_side(b)} {k}" for a, b, k in rows)


def read_lines(path):
    with open(path, "rb") as f:
        data = f.read()
    if data.startswith(b"\xef\xbb\xbf"):
        data = data[3:]
    text = data.decode("utf-8")
    if text == "":
        return []
    parts = re.split(r"\r\n|\n|\r", text)
    if parts and parts[-1] == "":
        parts.pop()  # a final line ending does not start a new line
    return parts


# ---------------------------------------------------------------- selftest
def selftest():
    fails = []

    def check(name, got, want):
        if got != want:
            fails.append(f"{name}: got {got!r}, want {want!r}")

    for text, want in [("", 0xFFFFFFFF), ("a", 0xC2C0B7E6), ("abc", 0x526A854F),
                       ("123456789", 0x5D6F1788), ("  x  ", 0x7899F23F),
                       ("שלום", 0xAADBA6C2), ("\U0001F600", 0x3E0B9BC4)]:
        check(f"hash {ascii(text)}", line_hash(text), want)
    check("file crc", file_crc(b"123456789"), 0xCBF43926)

    for a, b, sc, so in [("bravo", "bravx", 10, 4), ("int x = 1;", "int x = 2;", 17, 9),
                         ("return result;", "return value;", 11, 11), ("a = b + c", "c = a + b", 25, 9),
                         ("aaaa", "zzzz", -33, 0), ("abcdefghij", "klmnopqrst", -40, 0),
                         ("", "abc", -13, -18), ("x", "", -11, -18)]:
        A, B = a.encode(), b.encode()
        check(f"closeness {a}|{b}", closeness_bytes(A, B, False), sc)
        check(f"same-only {a}|{b}", closeness_bytes(A, B, True), so)

    S, D, U = "Same", "Different", "Unimportant"
    ten = [f"line number {k} unique" for k in range(10)]
    cases = [
        ("G1", ten, ten, {}, [(k, k, 1) for k in range(10)], [S] * 10),
        ("G2", ["alpha", "bravo", "charlie"], ["alpha", "bravx", "charlie"], {},
         [(0, 0, 1), (1, 1, 2)], [S, D, S]),
        ("G3", ["abcdefghij"], ["klmnopqrst"], {}, [(0, 0, 1)], [D]),
        ("G4", ["a", "b", "charlie"], ["a", "b", "november", "charlie"], {},
         [(0, 0, 1), (1, 1, 1), (None, 2, 1), (2, 3, 1)], [S, S, "RightOnly", S]),
        ("G5", ["int x = 1;", "int y = 2;"], ["int y = 2;"], {},
         [(0, None, 1), (1, 0, 1)], ["LeftOnly", S]),
        ("G6", ["x", "", "y"], ["x", "y"], {},
         [(0, 0, 1), (1, None, 1), (2, 1, 1)], [S, "UnimportantLeftOnly", S]),
        ("G7", ["x", "", "y"], ["x", "", "y"], {}, [(0, 0, 1), (1, 1, 1), (2, 2, 1)], [S] * 3),
        ("G8", [], [f"r{k}" for k in range(450)], {},
         [(None, 0, 200), (None, 200, 200), (None, 400, 50)], ["RightOnly"] * 450),
        ("G9", ["  foo()"], ["foo()"], {}, [(0, 0, 1)], [U]),
        ("G10", ["  foo()"], ["foo()"], {"whitespace": "all"}, [(0, 0, 1)], [D]),
        ("G11", ["int x = 1;", "foo();", "int y = 2;"],
         ["int x = 10;", "bar_baz();", "int y = 20;"], {}, [(0, 0, 3)], [D] * 3),
        ("G12", ["a", "b", "c"], ["a", "x", "y", "c"], {"alignment": "none"},
         [(0, 0, 1), (1, 1, 1), (2, 2, 1), (None, 3, 1)], [S, D, D, "RightOnly"]),
    ]
    for name, l, r, o, want_ops, want_kinds in cases:
        ops, rows = align(l, r, make_opts(**o))
        check(f"{name} ops", ops, want_ops)
        check(f"{name} rows", [k for _, _, k in rows], want_kinds)

    # G13 / G14 (hand-derived in plan.md 11.3): big runs, fixes B1 and B2
    def headers(n, alphabet):
        out = []
        for k in range(n):
            x, digits = k, []
            for _ in range(3):
                digits.append(x % 13)
                x //= 13
            out.append("".join(alphabet[digits[i % 3]] for i in range(12)))
        return out

    la, ra = "abcdefghijklm", "nopqrstuvwxyz"
    for name, nh, nb, want in [
        ("G13", 10, 100, [(0, 0, 10), (10, 10, 100)]),
        ("G14", 500, 8000, [(0, 0, 500)] + [(500 + 501 * k, 500 + 501 * k, 501) for k in range(15)] + [(8015, 8015, 485)]),
    ]:
        body = [f"shared line {k}" for k in range(nb)]
        ops, _ = align(headers(nh, la) + body, headers(nh, ra) + body, make_opts())
        check(f"{name} ops", ops, want)

    if fails:
        print("SELFTEST FAILED")
        for f in fails:
            print("  " + ascii(f))
        return 1
    print("selftest ok: 7 hash vectors, 1 file CRC, 16 closeness values, G1-G14")
    return 0


# ---------------------------------------------------------------- CLI
def main(argv):
    p = argparse.ArgumentParser(prog="refmodel")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("selftest")
    h = sub.add_parser("hash")
    h.add_argument("text", nargs="+")
    fc = sub.add_parser("filecrc")
    fc.add_argument("file")
    c = sub.add_parser("closeness")
    c.add_argument("a")
    c.add_argument("b")
    c.add_argument("--same-only", action="store_true")
    a = sub.add_parser("align")
    a.add_argument("left")
    a.add_argument("right")
    a.add_argument("--options")
    a.add_argument("--kind", choices=["standard", "none"])
    a.add_argument("--skew", type=int)
    a.add_argument("--whitespace", choices=["all", "leadingTrailing", "changes", "ignoreAll"])
    a.add_argument("--ignore-case", action="store_true", default=None)
    a.add_argument("--unimportant", action="append")
    a.add_argument("--orphans-sig", action="store_true", default=None)
    a.add_argument("--no-closeness", action="store_true")
    a.add_argument("--same-only", action="store_true", default=None)
    a.add_argument("--never-align", action="store_true", default=None)
    a.add_argument("--json", action="store_true")
    args = p.parse_args(argv)

    if args.cmd == "selftest":
        return selftest()
    if args.cmd == "hash":
        for t in args.text:
            print(f"{ascii(t)} 0x{line_hash(t):08X}")
        return 0
    if args.cmd == "filecrc":
        with open(args.file, "rb") as f:
            print(f"0x{file_crc(f.read()):08X}")
        return 0
    if args.cmd == "closeness":
        A, B = args.a.encode(), args.b.encode()
        print(closeness_bytes(A, B, args.same_only))
        return 0

    over = {}
    if args.options:
        with open(args.options, encoding="utf-8") as f:
            over.update(json.load(f))
    cli = {"alignment": args.kind, "skewTolerance": args.skew, "whitespace": args.whitespace,
           "ignoreCase": args.ignore_case, "unimportant": args.unimportant,
           "orphansAlwaysSig": args.orphans_sig, "closenessSameOnly": args.same_only,
           "neverAlignDiffs": args.never_align,
           "useCloseness": False if args.no_closeness else None}
    over.update({k: v for k, v in cli.items() if v is not None})
    try:
        opts = make_opts(**over)
        ops, rows = align(read_lines(args.left), read_lines(args.right), opts)
    except OutOfScope as e:
        print(f"OUT OF SCOPE: {e}")
        return 2
    if args.json:
        print(json.dumps({"ops": ops, "rows": rows}))
    else:
        print("# expected.ops")
        print(ops_text(ops))
        print("# expected.rows")
        print(rows_text(rows))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
