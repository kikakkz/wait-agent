#!/usr/bin/env python3
"""Count unwrap/expect, let _ =, unsafe blocks and SAFETY comments in production Rust code.

Skips files ending with `_test.rs` and code inside `#[cfg(test)]` modules/items.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "src"

UNWRAP_EXPECT_RE = re.compile(r"\.unwrap\(\)|\.expect\(")
LET_UNDERSCORE_RE = re.compile(r"let\s+_\s*=\s*")
UNSAFE_BLOCK_RE = re.compile(r"unsafe\s*\{")
SAFETY_COMMENT_RE = re.compile(r"//\s*SAFETY:")


def strip_cfg_test(text: str) -> str:
    """Remove top-level #[cfg(test)] items (modules, functions, etc.)."""
    lines = text.splitlines(keepends=True)
    out = []
    i = 0
    n = len(lines)
    skip_depth = None
    while i < n:
        line = lines[i]
        if skip_depth is not None:
            out.append("\n")
            for ch in line:
                if ch == "{":
                    skip_depth += 1
                elif ch == "}":
                    skip_depth -= 1
            if skip_depth == 0:
                skip_depth = None
            i += 1
            continue
        stripped = line.strip()
        if stripped.startswith("#[cfg(test)]"):
            skip_depth = 0
            out.append("\n")
            i += 1
            continue
        out.append(line)
        i += 1
    return "".join(out)


def count_in_file(path: Path) -> dict:
    text = path.read_text(encoding="utf-8")
    production = strip_cfg_test(text)
    return {
        "unwrap_expect": len(UNWRAP_EXPECT_RE.findall(production)),
        "let_underscore": len(LET_UNDERSCORE_RE.findall(production)),
        "unsafe_blocks": len(UNSAFE_BLOCK_RE.findall(production)),
        "safety_comments": len(SAFETY_COMMENT_RE.findall(production)),
    }


def main() -> int:
    totals = {
        "unwrap_expect": 0,
        "let_underscore": 0,
        "unsafe_blocks": 0,
        "safety_comments": 0,
    }
    file_details = []
    for path in sorted(ROOT.rglob("*.rs")):
        if path.name.endswith("_test.rs"):
            continue
        counts = count_in_file(path)
        file_details.append((path, counts))
        for k in totals:
            totals[k] += counts[k]

    print("Per-file breakdown (production code only):")
    for path, counts in file_details:
        if any(counts.values()):
            rel = path.relative_to(ROOT.parent)
            print(
                f"  {rel}: unwrap/expect={counts['unwrap_expect']}, "
                f"let_=_={counts['let_underscore']}, "
                f"unsafe={counts['unsafe_blocks']}, safety={counts['safety_comments']}"
            )
    print()
    print(f"Totals:")
    print(f"  unwrap/expect: {totals['unwrap_expect']}")
    print(f"  let _ =      : {totals['let_underscore']}")
    print(f"  unsafe blocks: {totals['unsafe_blocks']}")
    print(f"  // SAFETY:   : {totals['safety_comments']}")
    if totals["unsafe_blocks"] == totals["safety_comments"]:
        print("  OK: every unsafe block has a SAFETY comment")
    else:
        print(
            f"  MISSING: {totals['unsafe_blocks'] - totals['safety_comments']} "
            "unsafe block(s) lack a SAFETY comment"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
