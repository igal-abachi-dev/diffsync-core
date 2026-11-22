"""Differential test: the Rust aligner (`dsdev align --json`) against the Python reference
model (`refmodel.py`), on random structured inputs with small skew values.

    cargo build -p dsdev
    python -I tests/differential/align_vs_refmodel.py [--cases 300] [--seed 1] [--bin PATH]

The model is an independent implementation of spec ALIGN-1.3..1.6 with fixes B1-B3 and
decisions D1/D2 (plan.md 5.2). Any difference is a bug in one of the two and must be
understood before changing either (golden-case skill, "When an existing golden test fails").
Failing cases are written to a folder printed at the end, ready for `dsdev align` and
`refmodel.py align`.

Inputs are built to exercise windows, runs and selection: shared blocks of 3-90 lines (runs
above 500 weight need 8+ equal lines, big runs 72+), edits, moves, duplicates, blank lines,
whitespace variants, similar-but-different lines, and skew in {1, 2, 3, 5, 7, 16, 40, 100}.
"""
import argparse
import json
import pathlib
import random
import runpy
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
model = runpy.run_path(str(ROOT / ".claude" / "skills" / "golden-case" / "scripts" / "refmodel.py"))
make_opts, align, STATS = model["make_opts"], model["align"], model["STATS"]

WORDS = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi "
         "rho sigma tau upsilon phi chi psi omega").split()


def uniq(rng, n):
    """A significant line, unique per counter n, 12-40 characters."""
    return f"{rng.choice(WORDS)} {n} {rng.choice(WORDS)} {rng.choice(WORDS)}"[: rng.randint(12, 40)] + f"#{n}"


def make_block(rng, counter, length):
    lines = []
    for _ in range(length):
        counter[0] += 1
        kind = rng.random()
        if kind < 0.08:
            lines.append("")  # blank: insignificant, weight 2
        elif kind < 0.12:
            lines.append("    " + rng.choice(WORDS))  # indented: same comparable text
        elif kind < 0.16:
            lines.append("// note " + str(counter[0]))
        else:
            lines.append(uniq(rng, counter[0]))
    return lines


def mutate_line(rng, line, counter):
    r = rng.random()
    if r < 0.3 and line.strip():
        return "  " + line + "  "  # whitespace only
    if r < 0.6 and line.strip():
        i = rng.randrange(len(line))
        return line[:i] + rng.choice("xyz0123") + line[i + 1:]  # one character changed
    counter[0] += 1
    return uniq(rng, counter[0])  # unrelated


def make_pair(rng):
    counter = [0]
    left, right = [], []
    for _ in range(rng.randint(1, 7)):
        op = rng.random()
        n = rng.choice([1, 2, 3, 5, 8, 9, 12, 20, 40, 75, 90]) if rng.random() < 0.7 else rng.randint(1, 15)
        block = make_block(rng, counter, n)
        if op < 0.45:  # shared
            left += block
            right += block
        elif op < 0.60:  # changed
            left += block
            right += [mutate_line(rng, l, counter) if rng.random() < 0.7 else l for l in block]
        elif op < 0.70:  # deleted on the right
            left += block
        elif op < 0.80:  # inserted on the right
            right += block
        elif op < 0.86:  # periodic: a pattern repeated a different number of times on each side,
            # sometimes with a broken line in one repeat; overlapping diagonals give trims
            pat = make_block(rng, counter, rng.randint(8, 14))
            for side, reps in ((left, rng.randint(1, 4)), (right, rng.randint(1, 4))):
                for _ in range(reps):
                    rep = list(pat)
                    if rng.random() < 0.4:
                        rep[rng.randrange(len(rep))] = mutate_line(rng, rep[0], counter)
                    side += rep
        elif op < 0.90:  # moved: shared content but in the other order relative to the next block
            nxt = make_block(rng, counter, rng.choice([8, 10, 12, 20]))
            left += block + nxt
            right += nxt + block
        else:  # duplicated block on the right
            left += block
            right += block + block
    if rng.random() < 0.15:
        left, right = right, left
    return left, right


def make_options(rng):
    o = {"skewTolerance": rng.choice([1, 2, 3, 5, 7, 12, 16, 20, 24, 30, 40, 60, 100])}
    r = rng.random()
    if r < 0.12:
        o["alignment"] = "none"
    if rng.random() < 0.15:
        o["useCloseness"] = False
        if rng.random() < 0.5:
            o["neverAlignDiffs"] = True
    elif rng.random() < 0.15:
        o["closenessSameOnly"] = True
    if rng.random() < 0.15:
        o["ignoreCase"] = True
    if rng.random() < 0.25:
        o["whitespace"] = rng.choice(["all", "changes", "ignoreAll"])
    if rng.random() < 0.15:
        o["orphansAlwaysSig"] = True
    if rng.random() < 0.15:
        o["unimportant"] = ["//.*$"]
    return o


def to_model_opts(o):
    # the Rust TextOptions turns closeness off when neverAlignDiffs is set (Delphi behaviour)
    m = dict(o)
    if m.get("neverAlignDiffs"):
        m["useCloseness"] = False
    return m


def write_lines(path, lines):
    path.write_bytes("".join(l + "\n" for l in lines).encode("utf-8"))


def run_rust(binary, left, right, opts_path):
    out = subprocess.run([binary, "align", str(left), str(right), "--options", str(opts_path), "--json"],
                         capture_output=True, text=True, timeout=120)
    if out.returncode != 0:
        return None, out.stderr.strip()
    return [tuple(op) for op in json.loads(out.stdout)["ops"]], None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", type=int, default=300)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--bin", default=str(ROOT / "target" / "debug" / "dsdev.exe"))
    args = ap.parse_args()
    rng = random.Random(args.seed)
    work = pathlib.Path(tempfile.mkdtemp(prefix="difftest_"))
    failures = 0
    compared = skipped = with_runs = with_big = 0
    for n in range(args.cases):
        left, right = make_pair(rng)
        opts = make_options(rng)
        case = work / f"case{n:04d}"
        case.mkdir()
        write_lines(case / "left", left)
        write_lines(case / "right", right)
        (case / "options.json").write_text(json.dumps(opts), encoding="utf-8")
        try:
            want, _ = align(left, right, make_opts(**to_model_opts(opts)))
            want = [tuple(op) for op in want]
        except ValueError:  # an invalid combination the model refuses; skip
            skipped += 1
            continue
        compared += 1
        amounts = [op[2] for op in want if op[0] is not None and op[1] is not None]
        with_runs += any(a >= 8 for a in amounts)
        with_big += any(a >= 72 for a in amounts)
        got, err = run_rust(args.bin, case / "left", case / "right", case / "options.json")
        if got != want:
            failures += 1
            print(f"MISMATCH case {n} ({len(left)} x {len(right)} lines, options {opts}) in {case}")
            if err:
                print("  rust error:", err)
            else:
                for i, (a, b) in enumerate(zip(got + [None] * len(want), want + [None] * len(got))):
                    if a != b:
                        print(f"  first difference at op {i}: rust {a} model {b}")
                        break
            if failures >= 5:
                break
    print(f"{compared} compared ({skipped} skipped as invalid), {with_runs} with a paired op of >= 8 lines, "
          f"{with_big} with >= 72 lines; seed {args.seed}; {failures} mismatch(es); work folder {work}")
    print("model events:", ", ".join(f"{k}={v}" for k, v in sorted(STATS.items())))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
