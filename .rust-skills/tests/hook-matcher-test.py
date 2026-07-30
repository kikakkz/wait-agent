#!/usr/bin/env python3
"""
TDD tests for rust-skills hook matcher
Run: python3 tests/hook-matcher-test.py
"""

import re
import json
import sys
from pathlib import Path

# Load matcher from hooks.json
hooks_path = Path(__file__).parent.parent / "hooks" / "hooks.json"
with open(hooks_path) as f:
    hooks_config = json.load(f)

MATCHER = hooks_config["hooks"]["UserPromptSubmit"][0]["matcher"]

print(f"=== Hook Matcher TDD Tests ===")
print(f"Matcher loaded from: {hooks_path}\n")

# Test cases: (input, should_match, expected_match_word)
test_cases = [
    # Rust 技术问题 - 应该匹配
    ("支付系统精度问题", True, "问题"),
    ("E0382 错误怎么解决", True, "E0382"),
    ("rust ownership问题", True, "rust"),
    ("how to use tokio", True, "how to"),
    ("为什么会有生命周期错误", True, "为什么"),
    ("帮我写一个异步函数", True, "帮我写"),
    ("最佳实践是什么", True, "最佳实践"),
    ("value moved error", True, "value moved"),
    ("这个函数怎么用", True, "怎么用"),
    ("解释一下这段代码", True, "解释"),
    ("cargo build 报错了", True, "cargo"),
    ("async await 怎么用", True, "async"),
    ("Send Sync trait 是什么", True, "Send"),
    ("借用检查器报错", True, "借用"),
    ("类型不匹配怎么办", True, "类型"),

    # 边界情况 - 可能误匹配但可接受
    ("今天天气怎么样", True, "怎么"),  # 包含 "怎么"
    ("帮我订一张机票", True, "帮我"),  # 包含 "帮我"

    # 纯非技术问题 - 没有关键词不应匹配
    ("明天几点开会", False, None),
    ("晚饭吃什么", False, None),
]

passed = 0
failed = 0

for text, should_match, expected_word in test_cases:
    match = re.search(MATCHER, text)
    matched = match is not None

    if matched == should_match:
        passed += 1
        if matched:
            print(f"✅ PASS: '{text}' -> matched '{match.group()}'")
        else:
            print(f"✅ PASS: '{text}' -> no match (expected)")
    else:
        failed += 1
        if matched:
            print(f"❌ FAIL: '{text}' -> matched '{match.group()}' (should NOT match)")
        else:
            print(f"❌ FAIL: '{text}' -> no match (should match '{expected_word}')")

print(f"\n=== Summary ===")
print(f"Passed: {passed}/{len(test_cases)}")
print(f"Failed: {failed}/{len(test_cases)}")

if failed > 0:
    sys.exit(1)
