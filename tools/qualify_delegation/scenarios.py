# ruff: noqa: E501, C408 -- the scenarios are fixture data: test files and prompts kept verbatim.
"""Create the real-harness delegation scenarios under $QUALIFY_OUT/repos/<name>.

Each scenario is a small repository, its tests, and a meta-harness prompt aimed at one
delegation mechanism. See README.md.
"""

import json
import os
import pathlib
import shutil
import subprocess
import sys

HERE = pathlib.Path(
    os.environ.get("QUALIFY_OUT") or pathlib.Path(__file__).resolve().parents[2] / "target" / "qualify-delegation"
)
RUNNER = '''"""Run tests/test_*.py without pytest: every test_* function must pass."""
import importlib.util, pathlib, sys, traceback

failed = 0
sys.path.insert(0, ".")
only = sys.argv[1:]
for path in sorted(pathlib.Path("tests").glob("test_*.py")):
    if only and path.stem not in only:
        continue
    spec = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(module)
    except Exception:
        failed += 1
        print(f"FAIL import {path}")
        traceback.print_exc()
        continue
    for name in sorted(n for n in dir(module) if n.startswith("test_")):
        try:
            getattr(module, name)()
            print(f"ok   {path.stem}.{name}")
        except Exception:
            failed += 1
            print(f"FAIL {path.stem}.{name}")
            traceback.print_exc()
sys.exit(1 if failed else 0)
'''

FRICTION = (
    " Finally, under a heading 'Branchyard friction', list every error, refusal, confusing "
    "message, missing capability or workaround you hit while using Branchyard's delegation "
    "(by commands, Python module or MCP tools), with the exact command and message. Say 'none' if none."
)
META = (
    "You are the meta-harness for this repository. Do not write the implementation yourself: "
    "delegate it with Branchyard (your branchyard:delegate skill and the `by` command; run `by <cmd> --help` "
    "when unsure). "
)

S = {}

S["calc4"] = dict(
    files={
        "calc/__init__.py": '''from .lexer import tokenize
from .parser import parse
from .evaluator import evaluate, CalcError
from .formatter import format_number


def calculate(text: str) -> str:
    """Evaluate an arithmetic expression and format the result."""
    return format_number(evaluate(parse(tokenize(text))))
''',
        "calc/lexer.py": '''def tokenize(text: str) -> list[tuple[str, str]]:
    """Split text into (kind, value) tokens. Kinds: NUM (digits with optional single '.'),
    OP (one of + - * / ^), LPAREN, RPAREN. Whitespace is skipped. Any other character
    raises ValueError naming it and its index."""
    raise NotImplementedError
''',
        "calc/parser.py": '''def parse(tokens: list[tuple[str, str]]):
    """Parse tokens into an AST of nested tuples:
    ("num", float) | ("neg", node) | (op, left, right) with op in + - * / ^.
    Precedence: ^ (right-assoc) > unary minus > * / > + - (left-assoc).
    Note -2^2 == -(2^2). Raise ValueError on any syntax error (including empty input)."""
    raise NotImplementedError
''',
        "calc/evaluator.py": '''class CalcError(Exception):
    """Raised for division by zero."""


def evaluate(node) -> float:
    """Evaluate an AST from calc.parser. Division by zero raises CalcError."""
    raise NotImplementedError
''',
        "calc/formatter.py": '''def format_number(x: float) -> str:
    """Integers print without a decimal point ("4"); others with at most 10 significant
    digits and no trailing zeros ("0.3333333333", "2.5"). -0.0 prints as "0"."""
    raise NotImplementedError
''',
        "tests/test_calc.py": """from calc import calculate, tokenize, parse, evaluate, format_number, CalcError


def test_lexer():
    assert tokenize("1 + 2.5*(3)") == [("NUM","1"),("OP","+"),("NUM","2.5"),("OP","*"),("LPAREN","("),("NUM","3"),("RPAREN",")")]
    try:
        tokenize("1 $ 2"); assert False
    except ValueError as e:
        assert "$" in str(e) and "2" in str(e)


def test_parser():
    assert parse(tokenize("1+2*3")) == ("+", ("num", 1.0), ("*", ("num", 2.0), ("num", 3.0)))
    assert parse(tokenize("2^3^2")) == ("^", ("num", 2.0), ("^", ("num", 3.0), ("num", 2.0)))
    assert parse(tokenize("-2^2")) == ("neg", ("^", ("num", 2.0), ("num", 2.0)))
    for bad in ["", "1+", "(1", "1)", "*2"]:
        try:
            parse(tokenize(bad)); assert False, bad
        except ValueError:
            pass


def test_evaluator():
    assert evaluate(("+", ("num", 1.0), ("*", ("num", 2.0), ("num", 3.0)))) == 7.0
    try:
        evaluate(("/", ("num", 1.0), ("num", 0.0))); assert False
    except CalcError:
        pass


def test_formatter():
    assert format_number(4.0) == "4"
    assert format_number(1/3) == "0.3333333333"
    assert format_number(2.5) == "2.5"
    assert format_number(-0.0) == "0"


def test_end_to_end():
    assert calculate("2^3^2 / (1 + 7) - -1") == "65"
    assert calculate("10/4") == "2.5"
""",
    },
    prompt=META + "Split the work into four children, one per module in calc/ (lexer, parser, evaluator, formatter), "
    "each with --harness claude-code and at most 1 USD. Every test in tests/test_calc.py must pass on your branch at the end "
    "(`python3 run_tests.py`). Integrate all four children into your branch." + FRICTION,
    budget=6,
)

S["conflict3"] = dict(
    files={
        "fmt/__init__.py": "from .registry import render, FORMATTERS\n",
        "fmt/plain.py": '''def plain(rows: list[dict]) -> str:
    """One line per row: key=value pairs joined by spaces, keys sorted."""
    return "\\n".join(" ".join(f"{k}={r[k]}" for k in sorted(r)) for r in rows)
''',
        "fmt/registry.py": '''from .plain import plain

# Every formatter is registered here, in alphabetical order of its name.
FORMATTERS = {
    "plain": plain,
}


def render(name: str, rows: list[dict]) -> str:
    """Render rows with the named formatter; unknown names raise KeyError."""
    return FORMATTERS[name](rows)
''',
        "docs/FORMATS.md": "# Formats\n\n| Name | Description |\n|---|---|\n| plain | key=value pairs, one row per line |\n",
        "CHANGELOG.md": "# Changelog\n\n## Unreleased\n\n- Added the plain formatter.\n",
        "tests/test_fmt.py": """import json
from fmt import render, FORMATTERS

ROWS = [{"b": 2, "a": "x,y"}, {"b": 3, "a": "z"}]


def test_plain():
    assert render("plain", ROWS) == "a=x,y b=2\\na=z b=3"


def test_json():
    assert json.loads(render("json", ROWS)) == ROWS


def test_csv():
    assert render("csv", ROWS) == 'a,b\\r\\n"x,y",2\\r\\nz,3\\r\\n'


def test_markdown():
    assert render("markdown", ROWS) == "| a | b |\\n|---|---|\\n| x,y | 2 |\\n| z | 3 |"


def test_registry_sorted_and_documented():
    assert list(FORMATTERS) == sorted(FORMATTERS)
    doc = open("docs/FORMATS.md").read()
    log = open("CHANGELOG.md").read()
    for name in FORMATTERS:
        assert f"| {name} |" in doc, name
        assert f"the {name} formatter" in log, name
""",
    },
    prompt=META
    + "Add three formatters, csv, json and markdown, each in its own module fmt/<name>.py, each by a separate child "
    "(--harness claude-code, at most 1 USD each). Every child must register its formatter in fmt/registry.py (alphabetical), "
    "add a row to docs/FORMATS.md and a line 'Added the <name> formatter.' to CHANGELOG.md, so their changes overlap. "
    "Integrate all three into your branch; all tests must pass (`python3 run_tests.py`)." + FRICTION,
    budget=5,
)

S["graph"] = dict(
    files={
        "inv/__init__.py": "",
        "inv/models.py": '''"""Item model. Implement: a frozen dataclass Item(sku: str, name: str, qty: int, price_cents: int)
whose __post_init__ raises ValueError when sku is empty, qty < 0 or price_cents < 0."""
''',
        "inv/store.py": '''"""Implement class Store: add(item) (ValueError on duplicate sku), get(sku) (KeyError when missing),
adjust(sku, delta) returning the new Item (ValueError if qty would go negative), items() sorted by sku."""
''',
        "inv/report.py": '''"""Implement report(store) -> str: one line per item "SKU  NAME  QTY  $PRICE" (price as dollars with 2 decimals,
columns separated by two spaces) then a final line "TOTAL $X" with the inventory value (sum qty*price)."""
''',
        "inv/cli.py": '''"""Implement main(argv: list[str], store) -> str handling commands:
add SKU NAME QTY PRICE_CENTS | adjust SKU DELTA | report. Errors return "error: <message>"."""
''',
        "tests/test_inv.py": """from inv.models import Item
from inv.store import Store
from inv.report import report
from inv.cli import main


def test_models():
    Item("a", "Apple", 1, 50)
    for bad in [("", "x", 1, 1), ("a", "x", -1, 1), ("a", "x", 1, -1)]:
        try:
            Item(*bad); assert False, bad
        except ValueError:
            pass


def test_store_and_report():
    s = Store()
    s.add(Item("b", "Bolt", 10, 25)); s.add(Item("a", "Axle", 2, 1000))
    assert [i.sku for i in s.items()] == ["a", "b"]
    assert s.adjust("b", -4).qty == 6
    assert report(s) == "a  Axle  2  $10.00\\nb  Bolt  6  $0.25\\nTOTAL $21.50"


def test_cli():
    s = Store()
    assert main(["add", "x", "Xylo", "3", "200"], s) == "added x"
    assert main(["adjust", "x", "-5"], s).startswith("error:")
    assert main(["report"], s).endswith("TOTAL $6.00")
""",
    },
    prompt=META
    + "Implement inv/ with four children (--harness claude-code, at most 0.9 USD each), using Branchyard's task graph "
    "so the order is enforced by dependencies, not by you waiting: 'models' first; 'store' and 'report' depend on models "
    "being integrated (--depends-on models --after integrated); 'cli' depends on store and report being integrated. "
    "Integrate each child when it is ready so its dependents can start. All tests must pass on your branch (`python3 run_tests.py`)."
    + FRICTION,
    budget=5,
)

S["depth2"] = dict(
    files={
        "strs/__init__.py": "from .case import camel, snake\nfrom .text import title, truncate\n",
        "strs/case.py": 'def camel(s: str) -> str:\n    """"hello_big world" -> "helloBigWorld"."""\n    raise NotImplementedError\n\n\ndef snake(s: str) -> str:\n    """"helloBigWorld" or "Hello Big-World" -> "hello_big_world"."""\n    raise NotImplementedError\n',
        "strs/text.py": 'def title(s: str) -> str:\n    """Title case, keeping small words (a, an, the, of, in, and) lower unless first."""\n    raise NotImplementedError\n\n\ndef truncate(s: str, n: int) -> str:\n    """At most n chars; if cut, end with "…" (counted in n), never cut mid-word unless one word is longer than n."""\n    raise NotImplementedError\n',
        "nums/__init__.py": "from .bounds import clamp, lerp\nfrom .words import roman, ordinal\n",
        "nums/bounds.py": 'def clamp(x, lo, hi):\n    """ValueError if lo > hi."""\n    raise NotImplementedError\n\n\ndef lerp(a, b, t):\n    """a + (b-a)*t, t clamped to [0, 1]."""\n    raise NotImplementedError\n',
        "nums/words.py": 'def roman(n: int) -> str:\n    """1..3999 to Roman numerals; ValueError otherwise."""\n    raise NotImplementedError\n\n\ndef ordinal(n: int) -> str:\n    """1st 2nd 3rd 4th 11th 12th 13th 21st 111th."""\n    raise NotImplementedError\n',
        "tests/test_strs.py": """from strs import camel, snake, title, truncate


def test_case():
    assert camel("hello_big world") == "helloBigWorld"
    assert snake("helloBigWorld") == "hello_big_world" and snake("Hello Big-World") == "hello_big_world"


def test_text():
    assert title("the lord of the rings") == "The Lord of the Rings"
    assert truncate("hello brave new world", 12) == "hello brave…"
    assert truncate("supercalifragilistic", 6) == "super…"
    assert truncate("short", 10) == "short"
""",
        "tests/test_nums.py": """from nums import clamp, lerp, roman, ordinal


def test_bounds():
    assert clamp(5, 0, 3) == 3 and lerp(0, 10, 2) == 10
    try:
        clamp(1, 3, 0); assert False
    except ValueError:
        pass


def test_words():
    assert roman(1994) == "MCMXCIV"
    assert [ordinal(n) for n in (1, 2, 3, 4, 11, 12, 13, 21, 111)] == ["1st","2nd","3rd","4th","11th","12th","13th","21st","111th"]
""",
    },
    prompt=META
    + "Use two levels of delegation. Spawn two lead children (--harness claude-code, --max-depth 1, 2 USD each): "
    "'strs-lead' owns strs/ and 'nums-lead' owns nums/. Each lead must not write code either: it spawns two workers, one per "
    "module file in its package (at most 0.8 USD each), integrates them into its own branch, and runs its package's tests "
    "(`python3 run_tests.py test_strs` or `test_nums`). You then integrate both leads. All tests must pass on your branch."
    + FRICTION,
    budget=7,
    delegate=2,
)

S["recovery"] = dict(
    files={
        "util/__init__.py": "",
        "util/money.py": 'def round_cents(x: float) -> int:\n    """Round an amount in dollars to whole cents using banker\'s rounding (round half to even)."""\n    raise NotImplementedError\n',
        "util/ids.py": 'def short_id(n: int) -> str:\n    """Base-36 lowercase encoding of a non-negative int; ValueError if negative."""\n    raise NotImplementedError\n',
        "util/dates.py": 'def iso_week(y: int, m: int, d: int) -> str:\n    """ISO week string like "2026-W41" for a calendar date."""\n    raise NotImplementedError\n',
        "tests/test_util.py": """from util.money import round_cents
from util.ids import short_id
from util.dates import iso_week


def test_money():
    # The test is authoritative: half rounds UP (away from zero), contradicting the docstring.
    assert round_cents(0.125) == 13 and round_cents(0.135) == 14 and round_cents(2.5) == 250


def test_ids():
    assert short_id(0) == "0" and short_id(35) == "z" and short_id(36) == "10"


def test_dates():
    assert iso_week(2026, 10, 7) == "2026-W41" and iso_week(2021, 1, 3) == "2020-W53"
""",
    },
    prompt=META
    + "Three children, one per module in util/ (--harness claude-code). Deliberately start the 'dates' child with "
    "--budget-usd 0.01 so it runs out of budget; notice that through Branchyard (inspect/events), cancel it if needed, and "
    "re-run that work in a new child with 0.8 USD. Tell the 'money' child that its docstring and test disagree and that it must "
    "ask you (its parent) which one is authoritative with `by ask` before writing code; answer it with `by answer` that the test "
    "is authoritative. Integrate all working children; all tests must pass (`python3 run_tests.py`)." + FRICTION,
    budget=5,
)

S["compete"] = dict(
    files={
        "cache/__init__.py": "from .lru import LRUCache\n",
        "cache/lru.py": '''class LRUCache:
    """LRU cache with a capacity and an optional per-entry TTL in seconds, using an injectable clock.

    LRUCache(capacity, ttl=None, clock=time.monotonic); get(key, default=None); put(key, value);
    len(); stats() -> {"hits", "misses", "evictions", "expirations"}. Expired entries count as misses
    and are removed when touched or when they would be evicted. Thread-safe."""
''',
        "bench.py": """import time, random
from cache import LRUCache
c = LRUCache(1000)
r = random.Random(1)
t = time.perf_counter()
for i in range(200_000):
    k = r.randrange(5000)
    if c.get(k) is None:
        c.put(k, i)
print(f"{time.perf_counter() - t:.3f}s", c.stats())
""",
        "tests/test_cache.py": """import threading
from cache import LRUCache


class Clock:
    def __init__(self): self.t = 0.0
    def __call__(self): return self.t


def test_lru():
    c = LRUCache(2)
    c.put("a", 1); c.put("b", 2); c.get("a"); c.put("c", 3)
    assert c.get("b") is None and c.get("a") == 1 and c.get("c") == 3
    assert c.stats()["evictions"] == 1 and len(c) == 2


def test_ttl():
    clk = Clock(); c = LRUCache(10, ttl=5, clock=clk)
    c.put("a", 1); clk.t = 4.9; assert c.get("a") == 1
    clk.t = 10; assert c.get("a") is None
    s = c.stats(); assert s["expirations"] == 1 and s["misses"] == 1 and s["hits"] == 1


def test_threads():
    c = LRUCache(100)
    def work(n):
        for i in range(2000):
            c.put((n, i % 150), i); c.get((n, i % 150))
    ts = [threading.Thread(target=work, args=(n,)) for n in range(8)]
    [t.start() for t in ts]; [t.join() for t in ts]
    assert len(c) == 100
""",
    },
    prompt=META
    + "Run a competition: spawn three children (--harness claude-code, at most 1 USD each) that each implement "
    "cache/lru.py independently with a different design (OrderedDict; a hand-written doubly linked list with a dict; and a "
    "design of the child's choosing). Compare their candidates yourself: correctness (`python3 run_tests.py`), `python3 bench.py`, "
    "and code quality. Integrate only the best one, and make sure the losers are stopped and do not leave anything behind that "
    "looks unfinished. Explain the choice." + FRICTION,
    budget=5,
)

S["steer"] = dict(
    files={
        "conv/__init__.py": "",
        "conv/units.py": '''"""Implement: c_to_f(c), f_to_c(f), km_to_mi(km), mi_to_km(mi), kg_to_lb(kg), lb_to_kg(lb).
Results rounded to 3 decimals."""
''',
        "tests/test_units.py": """from conv import units


def test_values():
    assert units.c_to_f(100) == 212.0 and units.f_to_c(32) == 0.0
    assert units.km_to_mi(10) == 6.214 and units.mi_to_km(1) == 1.609
    assert units.kg_to_lb(1) == 2.205 and units.lb_to_kg(10) == 4.536
""",
    },
    prompt=META
    + "Spawn one child (--harness claude-code, 1 USD) to implement conv/units.py with a docstring and a doctest for "
    "every function. As soon as you see it running, steer it while its turn is still running (`by send <child> --steer`) with a "
    "new requirement: every function must raise TypeError when given a bool or a str (bools are ints in Python, so check "
    "explicitly). Then confirm through Branchyard whether the steer was delivered into the running turn or not, and whether the "
    "child honoured it, before integrating. Add tests for the new requirement to tests/test_units.py yourself after integrating "
    "(that small test edit is the only file you may write). All tests must pass." + FRICTION,
    budget=4,
)

S["artifacts"] = dict(
    files={
        "geo/__init__.py": "",
        "geo/stats.py": '''"""Implement summarize(rows) -> dict with keys: count, countries (number of distinct), largest (name of the city
with the largest population), mean_population (rounded to whole number). rows are dicts with name, country, population."""
''',
        "tests/test_geo.py": """import json, pathlib
from geo.stats import summarize


def test_summary_against_expected():
    expected = json.loads(pathlib.Path("tests/expected_summary.json").read_text())
    rows = [{"name": "A", "country": "X", "population": 10}, {"name": "B", "country": "Y", "population": 30},
            {"name": "C", "country": "X", "population": 20}]
    assert summarize(rows) == {"count": 3, "countries": 2, "largest": "B", "mean_population": 20}
    assert expected["count"] == 500 and set(expected) == {"count", "countries", "largest", "mean_population"}
""",
    },
    prompt=META
    + "Two children (--harness claude-code, 0.9 USD each). Child 'gen' writes tools/gen_cities.py, a deterministic "
    "generator (seed 7) of 500 fake city rows, runs it, and publishes the generated cities.json as a Branchyard artifact "
    "(`by artifact publish`), NOT committing the data file. Child 'stats' implements geo/stats.py, obtains cities.json from "
    "gen's artifact (you may need to share it; see `by artifact --help`), computes the summary and commits it as "
    "tests/expected_summary.json. 'stats' should depend on 'gen'. Integrate both; all tests must pass." + FRICTION,
    budget=4,
)

S["pysdk"] = dict(
    files={
        "shapes/__init__.py": "",
        "shapes/area.py": '"""Implement circle(r), rect(w, h), triangle(a, b, c) (Heron; ValueError if impossible), rounded to 4 decimals."""\n',
        "shapes/perimeter.py": '"""Implement circle(r), rect(w, h), polygon(points) for a list of (x, y) tuples (closed), rounded to 4 decimals."""\n',
        "shapes/hull.py": '"""Implement convex_hull(points) -> list of points counter-clockwise starting from the lowest-then-leftmost point; collinear points on edges excluded."""\n',
        "tests/test_shapes.py": """from shapes import area, perimeter, hull


def test_area():
    assert area.circle(1) == 3.1416 and area.rect(2, 3) == 6 and area.triangle(3, 4, 5) == 6.0
    try:
        area.triangle(1, 1, 3); assert False
    except ValueError:
        pass


def test_perimeter():
    assert perimeter.circle(1) == 6.2832 and perimeter.rect(2, 3) == 10
    assert perimeter.polygon([(0, 0), (3, 0), (3, 4)]) == 12.0


def test_hull():
    pts = [(0, 0), (2, 0), (1, 0), (2, 2), (0, 2), (1, 1)]
    assert hull.convex_hull(pts) == [(0, 0), (2, 0), (2, 2), (0, 2)]
""",
    },
    prompt=META
    + "Orchestrate entirely from Python with Branchyard's Python module (`import branchyard`, already on PYTHONPATH "
    "in your turn): write a script under /tmp (not in the repo) that spawns three children in parallel, one per module in "
    "shapes/ (harness claude-code, 0.9 USD each), waits for all of them, integrates each one that is ready, and prints a summary. "
    "Run it. If integration of one fails because the others are not integrated yet, handle it in the script. All tests must pass."
    + FRICTION,
    budget=4,
)

S["mcponly"] = dict(
    files={
        "seq/__init__.py": "",
        "seq/fib.py": '"""Implement fib(n) for n >= 0 iteratively; ValueError for negative n."""\n',
        "seq/primes.py": '"""Implement primes_upto(n) -> list of primes <= n (sieve)."""\n',
        "tests/test_seq.py": """from seq.fib import fib
from seq.primes import primes_upto


def test_fib():
    assert [fib(i) for i in range(8)] == [0, 1, 1, 2, 3, 5, 8, 13]


def test_primes():
    assert primes_upto(20) == [2, 3, 5, 7, 11, 13, 17, 19]
""",
    },
    prompt="You are the meta-harness for this repository, and you have no shell in this turn. Delegate with Branchyard's MCP "
    "tools (the `branchyard` MCP server): spawn two children (harness claude-code, 0.8 USD each), one per module in seq/, "
    "wait for them by inspecting, then integrate both into your branch. The check (`python3 run_tests.py`) runs on integration."
    + FRICTION,
    budget=4,
    extra=["--deny", "Bash"],
)

S["envelope"] = dict(
    files={
        "text/__init__.py": "",
        "text/a.py": '"""Implement vowels(s) -> int counting a e i o u, case-insensitive."""\n',
        "text/b.py": '"""Implement words(s) -> int counting whitespace-separated words."""\n',
        "text/c.py": '"""Implement palindrome(s) -> bool ignoring case and non-alphanumerics."""\n',
        "text/d.py": '"""Implement rot13(s) -> str."""\n',
        "tests/test_text.py": """from text.a import vowels
from text.b import words
from text.c import palindrome
from text.d import rot13


def test_all():
    assert vowels("Education") == 5 and words("  a b\\tc ") == 3
    assert palindrome("A man, a plan, a canal: Panama") and rot13("Hello") == "Uryyb"
""",
    },
    prompt=META + "You have a total budget of 1.5 USD. First try to spawn four children (one per file in text/) with "
    "--budget-usd 0.6 each, and also try one with --harness codex. Record exactly what Branchyard refuses and why, then adapt "
    "within your envelope (smaller budgets, the allowed harness) so all four modules get done by children and integrated. "
    "All tests must pass." + FRICTION,
    budget=1.5,
)

S["crash"] = dict(files=S["calc4"]["files"], prompt=S["calc4"]["prompt"], budget=6, crash=True)


def make(name, spec):
    root = HERE / "repos" / name
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    for rel, text in spec["files"].items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
    (root / "run_tests.py").write_text(RUNNER)
    (root / ".gitignore").write_text("__pycache__/\n*.pyc\n")
    git = ["git", "-c", "user.email=demo@example.com", "-c", "user.name=demo"]
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=root, check=True)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run([*git, "commit", "-qm", f"{name}: seed"], cwd=root, check=True)
    meta = {k: v for k, v in spec.items() if k != "files"}
    (HERE / "repos" / f"{name}.json").write_text(json.dumps(meta, indent=1))


for name in sys.argv[1:] or S:
    make(name, S[name])
    print("made", name)
