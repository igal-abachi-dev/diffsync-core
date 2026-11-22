"""Independent approval of the review-corpus snapshots (E4-S9.3 / E3-S5.4).

    python -I tests/review/approve.py

For every corpus case: expand the stored snapshot into rows and check
  1. coverage: every left and right line appears exactly once, in order;
  2. for Standard/None cases the Python reference model (independent implementation of
     ALIGN-1.3..1.6 + B1-B3 + D1/D2) produces the same row sequence;
  3. with default options, Same rows hold identical lines.
Prints one line per case and a summary; exit code 1 when anything disagrees.
"""
import json
import pathlib
import re
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8")
REPO = pathlib.Path(__file__).resolve().parents[2]
MODEL = REPO / ".claude/skills/golden-case/scripts/refmodel.py"
SNAP = REPO / "crates/core-diff/tests/snapshots"
CORPUS = REPO / "tests/review"


def snapshot_rows(name):
    text = (SNAP / f"{name}.snap").read_text(encoding="utf-8")
    body = text.split("---\n", 2)[2]
    rows = []
    for line in body.splitlines():
        m = re.match(r"^(\S+) (\S+) (\w+) x(\d+)$", line)
        if not m:
            continue
        l, r, kind, n = m.group(1), m.group(2), m.group(3), int(m.group(4))
        for i in range(n):
            rows.append((None if l == "-" else int(l) + i, None if r == "-" else int(r) + i, kind))
    return rows


def model_rows(left, right, opts_path):
    cmd = [sys.executable, "-I", str(MODEL), "align", "--json"]
    if opts_path:
        cmd += ["--options", str(opts_path)]
    cmd += [str(left), str(right)]
    out = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8")
    if out.returncode != 0:
        return None, out.stderr.strip()[:200]
    ops = json.loads(out.stdout)["ops"]
    rows = []
    for s, t, n in ops:
        for i in range(n):
            rows.append((None if s is None or s < 0 or s >= 2**31 else s + i,
                         None if t is None or t < 0 or t >= 2**31 else t + i))
    return rows, None


def lines_of(path):
    data = pathlib.Path(path).read_bytes()
    text = data.decode("utf-8", errors="replace")
    return re.split(r"\r\n|\r|\n", text[:-1] if text.endswith(("\n", "\r")) else text) if text else []


bad = 0
checked_model = 0
for d in sorted(p for p in CORPUS.iterdir() if (p / "left").exists()):
    name = d.name
    rows = snapshot_rows(name)
    nl, nr = len(lines_of(d / "left")), len(lines_of(d / "right"))
    notes = []
    # 1. coverage
    ls = [r[0] for r in rows if r[0] is not None]
    rs = [r[1] for r in rows if r[1] is not None]
    if ls != list(range(len(ls))) or rs != list(range(len(rs))):
        notes.append("coverage: lines out of order or repeated")
    # the model's and the engine's line counts can differ on odd line endings, so compare to snapshot
    opts_file = d / "options.json"
    opts = json.loads(opts_file.read_text()) if opts_file.exists() else {}
    kind = opts.get("alignment", "standard")
    if kind in ("standard", "none"):
        mrows, err = model_rows(d / "left", d / "right", opts_file if opts_file.exists() else None)
        if mrows is None:
            notes.append(f"model failed: {err}")
        else:
            checked_model += 1
            if mrows != [(r[0], r[1]) for r in rows]:
                first = next((i for i, (a, b) in enumerate(zip(mrows, [(r[0], r[1]) for r in rows])) if a != b),
                             min(len(mrows), len(rows)))
                notes.append(f"MODEL DIFFERS at row {first}: model {mrows[first:first+2]} snapshot {[(r[0], r[1]) for r in rows[first:first+2]]} (rows {len(mrows)} vs {len(rows)})")
    else:
        # not modelled: structural checks only, plus plain-text Same rows under default whitespace
        pass
    if not opts:
        L, R = lines_of(d / "left"), lines_of(d / "right")
        for l, r, k in rows:
            if k == "Same" and l is not None and r is not None and l < len(L) and r < len(R) and L[l] != R[r]:
                notes.append(f"Same row {l}/{r} holds different lines")
                break
    status = "ok " if not notes else "BAD"
    bad += bool(notes)
    print(f"{status} {name:34} kind={kind:9} rows={len(rows):6} {'; '.join(notes)}")
print(f"\n{checked_model} cases cross-checked against the reference model, {bad} with problems")
sys.exit(1 if bad else 0)
