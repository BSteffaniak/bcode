#!/usr/bin/env python3
from pathlib import Path
import os

case = os.environ["BCODE_EVAL_CASE_ID"]
root = Path.cwd()
lib = root / "src" / "lib.rs"

def replace(path, old, new):
    p = root / path
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"missing text in {path}: {old!r}")
    p.write_text(text.replace(old, new))

if case == "one-line":
    replace("src/lib.rs", "TIMEOUT_MS: u64 = 1000", "TIMEOUT_MS: u64 = 1500")
elif case == "rename-symbol":
    replace("src/lib.rs", "old_name", "new_name")
elif case == "repeated-pattern":
    replace("src/lib.rs", '"done"', '"complete"')
elif case == "multi-file":
    replace("src/math.rs", "pub fn add", "pub fn sum")
    replace("src/report.rs", "format_result", "format_total")
    replace("src/report.rs", "result={value}", "total={value}")
    replace("src/lib.rs", "math::sum", "math::sum")
    replace("src/lib.rs", "math::sum", "math::sum")
    replace("src/lib.rs", "math::sum", "math::sum")
    replace("src/lib.rs", "math::sum", "math::sum")
    replace("src/lib.rs", "math::sum", "math::sum") if False else None
    replace("src/lib.rs", "math::sum(values)", "math::sum(values)") if False else None
    text = lib.read_text().replace("math::sum(values)", "math::sum(values)")
    text = text.replace("math::sum(values)", "math::sum(values)")
    text = text.replace("math::sum(values)", "math::sum(values)")
    text = text.replace("math::add(values)", "math::sum(values)")
    text = text.replace("report::format_result", "report::format_total")
    lib.write_text(text)
elif case == "update-tests":
    replace("src/lib.rs", 'format!("hello, {name}")', 'format!("HELLO, {name}")')
elif case == "error-handling":
    replace("src/lib.rs", "pub fn parse_port(value: &str) -> u16 {\n    value.parse().unwrap()\n}", "pub fn parse_port(value: &str) -> Result<u16, std::num::ParseIntError> {\n    value.parse()\n}")
elif case == "preserve-formatting":
    replace("src/lib.rs", '("beta",   2)', '("beta",  20)')
elif case == "large-file":
    replace("src/lib.rs", "        137 => 999,", "        137 => 137,")
elif case == "ambiguous":
    pass
elif case == "no-generated":
    replace("src/lib.rs", 'include_str!("../generated/value.txt").trim()', '"source-owned"')
else:
    raise SystemExit(f"unknown eval case: {case}")
