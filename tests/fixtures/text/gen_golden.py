"""Generates the golden fixtures G1-G14 of plan.md section 11.3 (backlog E2-S9.1..S9.3).

Run from the repository root:  python -I tests/fixtures/text/gen_golden.py

How the expected values are made (golden-case skill, never from the Rust engine):

* G1-G12: the inputs are written here, the expected ops/rows come from the independent
  reference model (`refmodel.py`), and the script ASSERTS that they equal the hand-derived
  values of plan.md 11.3 that are written next to each case. A disagreement stops the script.
* G13, G14: outside the model (runs heavier than 500, big runs, B1/B2). The expected values
  are the hand-derived ones of plan.md 11.3. The header block alone is checked with the
  model (every header pair scores -40, so the DP gives the diagonal), and the run emission
  is derived by hand in the docstring of the Rust test `golden_g13_g14` (see
  crates/core-diff/tests/golden.rs).
"""
import json
import pathlib
import runpy
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
HERE = ROOT / "tests" / "fixtures" / "text"
model = runpy.run_path(str(ROOT / ".claude" / "skills" / "golden-case" / "scripts" / "refmodel.py"))
make_opts, align = model["make_opts"], model["align"]


def fmt(v):
    return "-" if v is None else str(v)


def write_case(name, left, right, options, ops, rows, header):
    d = HERE / name
    d.mkdir(exist_ok=True)
    # inputs: LF endings and a final newline, except an empty side which is an empty file
    for fname, lines in (("left", left), ("right", right)):
        (d / fname).write_bytes(("".join(l + "\n" for l in lines)).encode("utf-8"))
    if options:
        (d / "options.json").write_text(json.dumps(options, indent=2) + "\n", encoding="utf-8", newline="\n")
    elif (d / "options.json").exists():
        (d / "options.json").unlink()
    (d / "expected.ops").write_text(
        f"# {header}\n" + "".join(f"{fmt(s)} {fmt(t)} {n}\n" for s, t, n in ops), encoding="utf-8", newline="\n")
    (d / "expected.rows").write_text(
        "".join(f"{fmt(a)} {fmt(b)} {k}\n" for a, b, k in rows), encoding="utf-8", newline="\n")


def model_case(name, left, right, options, plan_ops, plan_kinds, header):
    ops, rows = align(left, right, make_opts(**options))
    assert ops == plan_ops, f"{name}: model ops {ops} != plan {plan_ops}"
    assert [k for _, _, k in rows] == plan_kinds, f"{name}: model kinds != plan"
    write_case(name, left, right, options, ops, rows, header + " Verified with refmodel.py and plan.md 11.3.")


S, D, U = "Same", "Different", "Unimportant"
ten = [f"line number {k} unique" for k in range(10)]

model_case("g1_identical_ten_lines", ten, ten, {},
           [(k, k, 1) for k in range(10)], [S] * 10,
           "G1 (spec ALIGN-1.3): identical files, one op per line from the top-level walk.")
model_case("g2_changed_middle_line", ["alpha", "bravo", "charlie"], ["alpha", "bravx", "charlie"], {},
           [(0, 0, 1), (1, 1, 2)], [S, D, S],
           "G2 (ALIGN-1.5): gap DP score 290, the paired differing line and the equal line coalesce.")
model_case("g3_all_different_pair", ["abcdefghij"], ["klmnopqrst"], {},
           [(0, 0, 1)], [D],
           "G3 (ALIGN-1.5): pair (-40) ties two orphans (-40); ties prefer the pair.")
# g4_right_only_insert already exists (E0-S6.1) and is regenerated the same way
model_case("g4_right_only_insert", ["a", "b", "charlie"], ["a", "b", "november", "charlie"], {},
           [(0, 0, 1), (1, 1, 1), (None, 2, 1), (2, 3, 1)], [S, S, "RightOnly", S],
           "G4 (ALIGN-1.5): a lone inserted line; DP score 260.")
model_case("g5_left_only_delete", ["int x = 1;", "int y = 2;"], ["int y = 2;"], {},
           [(0, None, 1), (1, 0, 1)], ["LeftOnly", S],
           "G5 (ALIGN-1.5): a deleted line; DP score 260.")
model_case("g6_pending_gap_blank_orphan", ["x", "", "y"], ["x", "y"], {},
           [(0, 0, 1), (1, None, 1), (2, 1, 1)], [S, "UnimportantLeftOnly", S],
           "G6 (ALIGN-1.3): a blank line joins the pending gap and becomes an unimportant orphan.")
model_case("g7_blank_lines_pair", ["x", "", "y"], ["x", "", "y"], {},
           [(0, 0, 1), (1, 1, 1), (2, 2, 1)], [S] * 3,
           "G7 (ALIGN-1.3): equal blank lines with no pending gap pair directly.")
model_case("g8_orphan_chunks_of_200", [], [f"r{k}" for k in range(450)], {},
           [(None, 0, 200), (None, 200, 200), (None, 400, 50)], ["RightOnly"] * 450,
           "G8 (ALIGN-1.4): one side exhausted, the other emitted in chunks of 200.")
model_case("g9_indentation_is_unimportant", ["  foo()"], ["foo()"], {},
           [(0, 0, 1)], [U],
           "G9 (D3): leading blanks are ignored by default, so the pair is Unimportant.")
model_case("g10_compare_all_is_different", ["  foo()"], ["foo()"], {"whitespace": "all"},
           [(0, 0, 1)], [D],
           "G10 (D3): with CompareAll the same pair is Different (closeness 15 > -40).")
model_case("g11_close_lines_pair_up", ["int x = 1;", "foo();", "int y = 2;"],
           ["int x = 10;", "bar_baz();", "int y = 20;"], {},
           [(0, 0, 3)], [D] * 3,
           "G11 (ALIGN-1.5): three close lines pair as Different; DP score 15.")
model_case("g12_none_pairs_in_order", ["a", "b", "c"], ["a", "x", "y", "c"], {"alignment": "none"},
           [(0, 0, 1), (1, 1, 1), (2, 2, 1), (None, 3, 1)], [S, D, D, "RightOnly"],
           "G12 (ALIGN-1.1): kind None = skew 1, closeness off, in-order pairing.")


# ---- G13 / G14: big runs (hand-derived) -------------------------------------------------
def headers(n, alphabet):
    """n distinct lines of 12 characters over `alphabet` (13 letters). Left and right alphabets
    are disjoint, so every left/right header pair has closeness exactly -40 (common = 0,
    s = -24, 4s - 1 = -97 -> floor)."""
    out = []
    for k in range(n):
        digits = []
        x = k
        for _ in range(3):  # base-13, enough for 13**3 = 2197 distinct values
            digits.append(x % 13)
            x //= 13
        # 12 characters: the three digits repeated with a rotating tail, all inside the alphabet
        s = "".join(alphabet[digits[i % 3]] for i in range(12))
        out.append(s)
    assert len(set(out)) == n
    return out


LEFT_ALPHA, RIGHT_ALPHA = "abcdefghijklm", "nopqrstuvwxyz"


def big_case(name, n_head, n_body, header, ops):
    hl, hr = headers(n_head, LEFT_ALPHA), headers(n_head, RIGHT_ALPHA)
    body = [f"shared line {k}" for k in range(n_body)]
    rows = [(k, k, D) for k in range(n_head)] + [(n_head + k, n_head + k, S) for k in range(n_body)]
    write_case(name, hl + body, hr + body, {}, ops, rows, header)
    return hl, hr


# G13: the header block alone is in the model's scope: every pair scores -40, the DP is all
# ties and the pair-first rule gives the diagonal.
hl, hr = headers(10, LEFT_ALPHA), headers(10, RIGHT_ALPHA)
m_ops, _ = align(hl, hr, make_opts())
assert m_ops == [(0, 0, 10)], m_ops
big_case("g13_big_run_after_header", 10, 100,
         "G13 (ALIGN-1.4, B2): the big run is found at (10,10) when its weight reaches 5040 >= 5000; "
         "the 10x10 header block before it is aligned first. HAND-DERIVED (plan.md 11.3).",
         [(0, 0, 10), (10, 10, 100)])

# G14 (B1): 500 header lines + 8000 shared lines. The run is extended to the outer block stop
# (line 8500), not the 2000-line window edge, in pieces of 501 lines, remainder last.
m_ops, _ = align(headers(500, LEFT_ALPHA), headers(500, RIGHT_ALPHA), make_opts())
assert m_ops == [(0, 0, 500)], m_ops
g14_ops = [(0, 0, 500)] + [(500 + 501 * k, 500 + 501 * k, 501) for k in range(15)] + [(8015, 8015, 485)]
assert sum(n for _, _, n in g14_ops) == 8500
big_case("g14_big_run_crosses_window_edge", 500, 8000,
         "G14 (B1): 15 pieces of 501 lines then 485, with no break at line 2000. HAND-DERIVED (plan.md 11.3).",
         g14_ops)


# ---- run selection, clipping and the D1 rule (E2-S5, E2-S8) -----------------------------
# Each case has its ops derived BY HAND (the derivation is in the comment) and is then
# cross-checked with the model, which now covers windows, runs and big runs. The model and the
# hand derivation must agree before the fixture is written.
def hand_case(name, left, right, options, hand_ops, hand_kinds, header):
    ops, rows = align(left, right, make_opts(**options))
    assert ops == hand_ops, f"{name}: model {ops} != hand {hand_ops}"
    assert [k for _, _, k in rows] == hand_kinds, f"{name}: kinds {[k for _, _, k in rows]} != {hand_kinds}"
    write_case(name, left, right, options, ops, rows, header + " Hand-derived and checked with refmodel.py.")


def disjoint(n, alphabet, tag):
    """n distinct 12-character lines over a 13-letter alphabet (see `headers`)."""
    return headers(n, alphabet) if tag == "" else [tag + l for l in headers(n, alphabet)]


# D2: the right line pairs with its exact twin, not with the whitespace variant.
# DP: pair(0,0)=14 ('aaa bbb' ~ 'aaa bbc': common 6, s=4, 4*25/7=14); pair(1,1)=279 (equal
# hash, full lines differ); pair(2,1)=280. Path A: 14 -20 +280 = 274. Path B: 14 +279 -20 = 273.
# Without the -1 of D2 both would be 274 and the tie would not prefer the exact twin.
hand_case("d2_whitespace_twin_preferred",
          ["aaa bbb", "  foo bar", "foo bar"], ["aaa bbc", "foo bar"], {},
          [(0, 0, 1), (1, None, 1), (2, 1, 1)], [D, "LeftOnly", S],
          "D2 (ALIGN-1.6): equal-hash closeness is 280 for identical lines and 279 for a whitespace twin.")

# B3: skew 12. Window 1 = lines 0..11 on both sides (more_after: 12 < 16). The 12-line run at
# line 4 shows 8 lines inside the window (4..11, weight 560 > 500) and touches the window's last
# line, so it is clipped to 7 lines = 490 < 500 and dropped. No kept run: the window is
# gap-filled with a partial trace, which consumes the whole diagonal (12, 12): one op. Window 2
# = lines 12..15, no more_after, gap-filled: one op (12,12,4).
hl, hr = disjoint(4, LEFT_ALPHA, ""), disjoint(4, RIGHT_ALPHA, "")
body = [f"shared line {k}" for k in range(12)]
hand_case("b3_clip_drops_run_below_minimum", hl + body, hr + body, {"skewTolerance": 12},
          [(0, 0, 12), (12, 12, 4)], [D] * 4 + [S] * 12,
          "B3 + D1 (ALIGN-1.4): a run clipped at the window edge is re-weighed and can fall below 500.")

# D1: a closed gap longer than the skew slides. Top level pairs E1, stops at the first
# differing pair, and the rest is one block [1,32) x [1,32) with skew 10: windows [1,11),
# [11,21), [21,31) have more_after and no runs, so each is gap-filled with a partial trace that
# consumes its whole diagonal (all header pairs tie at -40, pair first): (1,1,10), (11,11,10),
# (21,21,10). The last window [31,32) has no more_after: E2 pairs (31,31,1).
g_l, g_r = disjoint(30, LEFT_ALPHA, ""), disjoint(30, RIGHT_ALPHA, "")
hand_case("d1_closed_gap_slides", ["first line"] + g_l + ["last line"], ["first line"] + g_r + ["last line"],
          {"skewTolerance": 10},
          [(0, 0, 1), (1, 1, 10), (11, 11, 10), (21, 21, 10), (31, 31, 1)], [S] + [D] * 30 + [S],
          "D1 (ALIGN-1.4): a closed gap larger than the skew slides window by window.")

# D1 (commit-and-slide): skew 16, left = 4 header lines + a 20-line shared run + 3 trailer
# lines. Window 1 = lines 0..15 (more_after). The run shows 12 lines inside the window (4..15,
# weight 840) and touches the last line, so B3 clips it to 11 lines (770, still kept). Kept run
# + more_after: emit the header gap (DP, all pairs tie at -40 -> diagonal) = (0,0,4), the run
# (4,4,11), commit up to its end (line 15) and slide. Window 2 = lines 15..26, no more_after:
# the run continues (15,15,9) = 630 > 500, then the trailer gap (24,24,3). Without the D1
# commit/clip the run would not be split at line 15.
rl = [f"shared run line {k}" for k in range(20)]
hand_case("d1_run_crosses_window_edge",
          disjoint(4, LEFT_ALPHA, "") + rl + disjoint(3, LEFT_ALPHA, "t"),
          disjoint(4, RIGHT_ALPHA, "") + rl + disjoint(3, RIGHT_ALPHA, "u"),
          {"skewTolerance": 16},
          [(0, 0, 4), (4, 4, 11), (15, 15, 9), (24, 24, 3)], [D] * 4 + [S] * 20 + [D] * 3,
          "D1 (ALIGN-1.4): a run crossing the window edge is clipped by one line, committed, and continues in the next window.")

# S8: swapped blocks. X = 10 lines (700), Y = 12 lines (840). Runs: X at (src 0, tgt 12),
# Y at (src 10, tgt 0). Heavier Y is processed first; X crosses Y (it is before Y on the source
# axis and after it on the target axis), so X is dropped. Emission: the gap before Y has no
# target lines: one op of 10 source orphans; Y; then the 10 target lines after Y are orphans.
bx = [f"block X line {k}" for k in range(10)]
by = [f"block Y line {k}" for k in range(12)]
hand_case("s8_swapped_blocks_heavier_wins", bx + by, by + bx, {},
          [(0, None, 10), (10, 0, 12), (None, 12, 10)], ["LeftOnly"] * 10 + [S] * 12 + ["RightOnly"] * 10,
          "ALIGN-1.4 selection: of two crossing runs the heavier is kept, the other dropped.")


def mixed_run(sig, comments, blanks, tag):
    """A run with the given number of significant lines (70 each), comment-only lines (31
    each, with the option `unimportant: ['//.*$']`) and blank lines (2 each), interleaved."""
    kinds = ["s"] * sig + ["c"] * comments + ["b"] * blanks
    # deterministic interleave: sort by a fixed rotating key so equal kinds are not adjacent
    order = sorted(range(len(kinds)), key=lambda i: (i * 7919) % len(kinds))
    out, counters = [], {"s": 0, "c": 0}
    for i in order:
        k = kinds[i]
        if k == "b":
            out.append("")
        else:
            counters[k] += 1
            out.append(f"{tag} sig {counters[k]}" if k == "s" else f"// {tag} note {counters[k]}")
    return out


def weight_of(lines):
    return sum(2 if l == "" else (31 if l.startswith("//") else 70) for l in lines)


head_l, head_r = disjoint(10, LEFT_ALPHA, ""), disjoint(10, RIGHT_ALPHA, "")
r500 = mixed_run(6, 2, 9, "w500")  # 6*70 + 2*31 + 9*2 = 500
assert weight_of(r500) == 500 and len(r500) == 17
r501 = mixed_run(5, 3, 29, "w501")  # 5*70 + 3*31 + 29*2 = 501
assert weight_of(r501) == 501 and len(r501) == 37
# A run is kept when its weight is GREATER than 500. At exactly 500 it is not kept, so the gap
# DP covers header and run in one go (every pair is on the diagonal): one op. At 501 the run is
# kept: the 10x10 header gap is aligned first, then the run: two ops.
ops500, rows500 = align(head_l + r500, head_r + r500, make_opts(unimportant=["//.*$"]))
assert ops500 == [(0, 0, 27)], ops500
write_case("s8_run_weight_500_is_not_kept", head_l + r500, head_r + r500, {"unimportant": ["//.*$"]}, ops500, rows500,
           "ALIGN-1.4: a run of weight exactly 500 is not kept (one DP op). Hand-derived and checked with refmodel.py.")
ops501, rows501 = align(head_l + r501, head_r + r501, make_opts(unimportant=["//.*$"]))
assert ops501 == [(0, 0, 10), (10, 10, 37)], ops501
write_case("s8_run_weight_501_is_kept", head_l + r501, head_r + r501, {"unimportant": ["//.*$"]}, ops501, rows501,
           "ALIGN-1.4: a run of weight 501 is kept (header gap, then the run). Hand-derived and checked with refmodel.py.")

print("golden fixtures written")
sys.exit(0)
