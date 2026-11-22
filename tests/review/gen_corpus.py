"""Generates the review corpus (backlog E4-S9.2, plan.md 11.4): about 60 left/right pairs of the
kinds named in the plan. Run from the repository root:  python -I tests/review/gen_corpus.py

Every input is generated here from fixed seeds (no private or customer files, see SOURCES.md),
so the corpus is reproducible. Each case is a folder `tests/review/<name>/{left,right,options.json}`.
The large cases of the plan (100k-line logs, 2000+ line moves at full size) are generated at
reduced size so the repository stays small; `--big` writes them at full size into a folder
that is git-ignored.
"""
import json
import pathlib
import random
import sys

ROOT = pathlib.Path(__file__).resolve().parent
BIG = "--big" in sys.argv
CASES = {}


def case(name, left, right, options=None, encoding="utf-8", newline="\n"):
    CASES[name] = (left, right, options or {}, encoding, newline)


def lines(text):
    return text.strip("\n").split("\n")


# ---- refactors: rename, extract function, reorder arguments (6 variants) ------------------
for v in range(10):
    rng = random.Random(100 + v)
    body = [f"    total += item_{k}.price * item_{k}.qty;" for k in range(8 + v)]
    left = ["fn compute(items: &[Item]) -> f64 {", "    let mut total = 0.0;"] + body + ["    total", "}", ""]
    right = ["fn compute_total(items: &[Item], tax: f64) -> f64 {", "    let mut total = 0.0;"] + [
        l.replace("price", "unit_price") if rng.random() < 0.4 else l for l in body] + ["    total * (1.0 + tax)", "}", ""]
    case(f"refactor_{v}", "\n".join(left), "\n".join(right))

# ---- re-indentation (tabs to spaces, 2 to 4 spaces) with three whitespace modes ----------------
code = [l for k in range(10) for l in (f"if (x{k} > {k}) {{", f"  call_{k}(x{k});", "}")]
for i, ws in enumerate(["leadingTrailing", "all", "changes"]):
    case(f"reindent_{ws}", "\n".join(code), "\n".join("    " + l if l.startswith("  ") else l for l in code),
         {"whitespace": ws})
case("tabs_to_spaces", "\n".join("\t" + l for l in code), "\n".join("    " + l for l in code))

# ---- reordered JSON keys and list items ------------------------------------------------------
def json_lines(keys):
    return ["{"] + [f'  "{k}": {i},' for i, k in enumerate(keys)] + ['  "end": 0', "}"]

ks = [f"key{c}" for c in "abcdefghij"]
for v in range(8):
    rng = random.Random(200 + v)
    shuffled = ks[:]
    rng.shuffle(shuffled)
    case(f"json_reordered_{v}", "\n".join(json_lines(ks)), "\n".join(json_lines(shuffled)))

# ---- CSV edits --------------------------------------------------------------------------------
for v in range(7):
    rng = random.Random(300 + v)
    rows = [f"{i},name{i},{rng.randint(1, 999)},{rng.choice(['a', 'b', 'c'])}" for i in range(40)]
    right = rows[:]
    for _ in range(3 + v):
        right[rng.randrange(len(right))] = f"{rng.randint(100, 140)},changed,{rng.randint(1, 9)},z"
    del right[rng.randrange(len(right))]
    right.insert(rng.randrange(len(right)), "999,inserted,0,q")
    case(f"csv_edits_{v}", "\n".join(["id,name,qty,flag"] + rows), "\n".join(["id,name,qty,flag"] + right))

# ---- Hebrew / English text --------------------------------------------------------------------
heb = ["שלום עולם", "זהו משפט בעברית", "הכל בסדר", "תודה רבה", "להתראות"]
eng = ["Hello world", "This is an English line", "All is well", "Thank you", "Goodbye"]
for v in range(7):
    rng = random.Random(400 + v)
    left = [x for pair in zip(heb, eng) for x in pair] * 3
    right = left[:]
    right[rng.randrange(len(right))] = "שורה חדשה 😀 new line"
    right.insert(rng.randrange(len(right)), heb[v % 5] + " " + eng[v % 5])
    case(f"hebrew_english_{v}", "\n".join(left), "\n".join(right))
case("hebrew_ignorecase", "\n".join(eng), "\n".join(x.upper() for x in eng), {"ignoreCase": True})

# ---- minified JS: one huge line with a few changes -------------------------------------------
tokens = [f"var a{k}=function(x){{return x*{k}+{k}}};" for k in range(300)]
case("minified_js_one_line", "".join(tokens), "".join(tokens).replace("x*17+17", "x*18+17").replace("a250", "b250"))
case("minified_js_many_tokens", "".join(tokens) * 40, "".join(tokens).replace("a1=", "a1 =") * 40)  # over 10k tokens

# ---- brace / `end;` storms --------------------------------------------------------------------
for v, closer in enumerate(["}", "end;", ")"]):
    rng = random.Random(500 + v)
    left = []
    for k in range(25):
        left += [f"block_{k} {{" if closer == "}" else f"begin {k}", f"  step_{k};", closer, ""]
    right = left[:]
    del right[rng.randrange(len(right))]
    right.insert(rng.randrange(len(right)), closer)
    case(f"closer_storm_{v}", "\n".join(left), "\n".join(right))

# ---- moved blocks (short and 2000+ lines) ---------------------------------------------------------
for v, n in enumerate([30, 120, 2300 if not BIG else 2300]):
    a = [f"section A line {k} {'x' * (k % 7)}" for k in range(n)]
    b = [f"section B line {k} {'y' * (k % 5)}" for k in range(n)]
    case(f"moved_block_{n}", "\n".join(a + b), "\n".join(b + a))

# ---- logs --------------------------------------------------------------------------------------
logn = 100_000 if BIG else 3000
rng = random.Random(600)
log = [f"2026-11-21 10:{k // 60 % 60:02}:{k % 60:02} INFO request {k} ok in {rng.randint(1, 99)} ms" for k in range(logn)]
log2 = log[:]
for k in range(5, logn, logn // 12):
    log2[k] = log2[k].replace("ok", "FAILED")
case("log_few_changes", "\n".join(log), "\n".join(log2))

# ---- comments, unimportant text, blank lines, line endings --------------------------------------
src = ["int main() {", "  int a = 1; // first", "  /* block */ int b = 2;", "", "  return a + b; // sum", "}"]
case("comments_unimportant", "\n".join(src), "\n".join(l.replace("first", "1st").replace("sum", "total") for l in src),
     {"unimportant": ["/\\*.*?\\*/", "//.*$"]})
case("blank_lines_inserted", "\n".join(src), "\n".join(l for pair in zip(src, [""] * len(src)) for l in pair))
case("crlf_vs_lf", "\r\n".join(src) + "\r\n", "\n".join(src) + "\n")
case("trailing_whitespace", "\n".join(src), "\n".join(l + "   " for l in src))
case("orphans_always_sig", "\n".join(src), "\n".join(src[:3] + [""] + src[3:]), {"orphansAlwaysSig": True})

# ---- different alignment kinds on the same input ----------------------------------------------
base = [f"line {k % 17} of the repeated file" for k in range(60)]
edited = base[:]
edited[10], edited[40] = "changed ten", "changed forty"
del edited[25]
for kind in ["standard", "none", "patience", "myers", "histogram"]:
    case(f"kinds_{kind}", "\n".join(base), "\n".join(edited), {"alignment": kind})

# ---- degenerate -----------------------------------------------------------------------------------
case("empty_vs_text", "", "\n".join(src))
case("identical", "\n".join(src), "\n".join(src))
case("single_line_changed", "hello world", "hello there world")
case("all_different", "\n".join(f"left {k}" for k in range(20)), "\n".join(f"RIGHT {k}" for k in range(20)))


def main():
    out = ROOT
    for name, (left, right, options, enc, nl) in CASES.items():
        d = out / name
        d.mkdir(exist_ok=True)
        (d / "left").write_bytes(left.replace("\n", nl).encode(enc) if nl != "\n" and "\r" not in left else left.encode(enc))
        (d / "right").write_bytes(right.encode(enc))
        if options:
            (d / "options.json").write_text(json.dumps(options), encoding="utf-8", newline="\n")
    print(f"{len(CASES)} cases")


if __name__ == "__main__":
    main()
