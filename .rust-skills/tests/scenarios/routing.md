# Rust-Router Test Scenarios

## Meta-Question Routing Tests

### Test 1: Ownership Route
**Prompt:** "E0382 use of moved value"
**Expected Route:** m01-ownership
**Verification:**
- [ ] Correct skill triggered
- [ ] Ownership-specific content returned

### Test 2: Error Handling Route
**Prompt:** "When to use Result vs panic?"
**Expected Route:** m06-error-handling
**Verification:**
- [ ] Correct skill triggered
- [ ] Error handling patterns explained

### Test 3: Concurrency Route
**Prompt:** "Why is Rc not Send?"
**Expected Route:** m07-concurrency
**Verification:**
- [ ] Correct skill triggered
- [ ] Send/Sync traits explained

### Test 4: Performance Route
**Prompt:** "How to profile Rust code?"
**Expected Route:** m10-performance
**Verification:**
- [ ] Correct skill triggered
- [ ] Profiling tools listed

### Test 5: Anti-Pattern Route
**Prompt:** "Is .clone() everywhere bad?"
**Expected Route:** m15-anti-pattern
**Verification:**
- [ ] Correct skill triggered
- [ ] Clone anti-pattern explained

---

## Unsafe Routing Tests

### Test 6: Unsafe to Unsafe-Checker
**Prompt:** "Review my unsafe code"
**Expected Route:** unsafe-checker (NOT m08-safety)
**Verification:**
- [ ] Routed to unsafe-checker skill
- [ ] Detailed checklist provided

### Test 7: FFI to Unsafe-Checker
**Prompt:** "How to call extern C function?"
**Expected Route:** unsafe-checker
**Verification:**
- [ ] FFI rules from unsafe-checker
- [ ] Not just general concurrency

### Test 8: Raw Pointer to Unsafe-Checker
**Prompt:** "*mut T dereference safety"
**Expected Route:** unsafe-checker
**Verification:**
- [ ] Pointer safety rules
- [ ] Detailed checklist

---

## Functional Routing Tests

### Test 9: Version Query to Rust-Learner
**Prompt:** "What's new in Rust 1.75?"
**Expected Route:** rust-learner → rust-changelog agent
**Verification:**
- [ ] Uses rust-changelog agent
- [ ] Does NOT use WebSearch

### Test 10: Crate Query to Crate-Researcher
**Prompt:** "Latest version of serde?"
**Expected Route:** rust-learner → crate-researcher agent
**Verification:**
- [ ] Uses crate-researcher agent
- [ ] Does NOT use WebSearch

### Test 11: Clippy to Clippy-Researcher
**Prompt:** "/guideline --clippy needless_clone"
**Expected Route:** clippy-researcher agent
**Verification:**
- [ ] Uses clippy-researcher agent
- [ ] Maps to guideline rules

### Test 12: Style to Coding-Guidelines
**Prompt:** "Rust naming conventions"
**Expected Route:** coding-guidelines
**Verification:**
- [ ] coding-guidelines skill
- [ ] Style rules returned

---

## Multi-Topic Routing Tests

### Test 13: Ownership + Concurrency
**Prompt:** "Why can't I use Rc across threads?"
**Expected Routes:** m07-concurrency (primary), m01-ownership (related)
**Verification:**
- [ ] Send/Sync explanation
- [ ] Ownership context

### Test 14: Error + Domain
**Prompt:** "Error handling in async web server"
**Expected Routes:** m06-error-handling, m07-concurrency
**Verification:**
- [ ] Async error patterns
- [ ] Result propagation in async

---

## Error Code Routing Tests

### Test 15: E0382 → m01
**Prompt:** "E0382"
**Expected Route:** m01-ownership

### Test 16: E0277 → m04 or m07
**Prompt:** "E0277 trait bound not satisfied"
**Expected Route:** m04-zero-cost or m07-concurrency
**Verification:**
- [ ] Depends on context (Send/Sync → m07)

### Test 17: E0596 → m03
**Prompt:** "E0596 cannot borrow as mutable"
**Expected Route:** m03-mutability

---

## Chinese Trigger Tests

### Test 18: Chinese Ownership Query
**Prompt:** "所有权是什么？"
**Expected Route:** m01-ownership
**Verification:**
- [ ] Correct skill triggered
- [ ] Response can be in Chinese

### Test 19: Chinese Version Query
**Prompt:** "Rust 最新版本是什么？"
**Expected Route:** rust-learner
**Verification:**
- [ ] Uses agents, not WebSearch

### Test 20: Chinese Error Query
**Prompt:** "借用检查器报错怎么办？"
**Expected Route:** m01-ownership
**Verification:**
- [ ] Borrow checker help provided
