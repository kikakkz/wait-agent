# Agent Integration Test Scenarios

## Crate-Researcher Tests

### Test 1: Popular Crate Query
**Prompt:** "What's the latest version of tokio?"
**Expected Agent:** crate-researcher
**Expected Sources (priority order):**
1. cache/crates/tokio.json (if exists and fresh)
2. actionbook MCP → lib.rs
3. agent-browser → lib.rs
4. cargo search (last resort)

**Verification:**
- [ ] Agent launched correctly
- [ ] Returns version number
- [ ] Returns features list
- [ ] Cache updated after fetch

### Test 2: Less Popular Crate
**Prompt:** "Info about the 'thirtyfour' crate"
**Expected Agent:** crate-researcher
**Verification:**
- [ ] Falls back correctly if not in cache
- [ ] Returns accurate info

### Test 3: Cache Hit
**Setup:** Create cache/crates/serde.json with recent timestamp
**Prompt:** "serde latest version"
**Verification:**
- [ ] Returns cached data
- [ ] Response indicates "Cached: yes"
- [ ] No network fetch

---

## Rust-Changelog Tests

### Test 4: Specific Version Query
**Prompt:** "What's new in Rust 1.75?"
**Expected Agent:** rust-changelog
**Expected Sources (priority order):**
1. cache/rust-versions/1.75.json
2. actionbook → releases.rs
3. agent-browser → releases.rs

**Verification:**
- [ ] Agent launched correctly
- [ ] Returns release date
- [ ] Returns key features
- [ ] Returns stabilized APIs

### Test 5: Latest Version Query
**Prompt:** "Latest Rust version features"
**Expected Agent:** rust-changelog
**Verification:**
- [ ] Determines latest version
- [ ] Returns current stable info

---

## Docs-Researcher Tests

### Test 6: API Documentation Query
**Prompt:** "How to use tokio::spawn?"
**Expected Agent:** docs-researcher
**Verification:**
- [ ] Fetches from docs.rs
- [ ] Returns function signature
- [ ] Returns examples
- [ ] Returns parameters

### Test 7: Module Documentation
**Prompt:** "What's in tokio::sync?"
**Expected Agent:** docs-researcher
**Verification:**
- [ ] Lists module contents
- [ ] Brief descriptions

---

## Clippy-Researcher Tests

### Test 8: Lint Query
**Prompt:** "/guideline --clippy needless_clone"
**Expected Agent:** clippy-researcher
**Verification:**
- [ ] Returns lint description
- [ ] Maps to guideline rule
- [ ] Provides fix suggestion

### Test 9: Unknown Lint
**Prompt:** "/guideline --clippy nonexistent_lint"
**Verification:**
- [ ] Graceful error handling
- [ ] Suggests similar lints if possible

---

## Cache Behavior Tests

### Test 10: Cache Expiry
**Setup:**
1. Create cache/crates/test.json with timestamp 48 hours ago
2. Set TTL to 24 hours

**Prompt:** "test crate info"
**Verification:**
- [ ] Detects expired cache
- [ ] Fetches fresh data
- [ ] Updates cache

### Test 11: Stale-While-Revalidate
**Setup:**
1. Create expired cache
2. Simulate network failure

**Verification:**
- [ ] Returns stale data with warning
- [ ] Indicates data may be outdated

---

## Error Handling Tests

### Test 12: Network Failure
**Setup:** Simulate actionbook/agent-browser unavailable
**Prompt:** "latest serde version"
**Verification:**
- [ ] Falls back to cargo search
- [ ] Returns data (possibly less detailed)
- [ ] Logs the fallback

### Test 13: Invalid Crate
**Prompt:** "info about nonexistent-crate-xyz"
**Verification:**
- [ ] Returns "crate not found"
- [ ] Does not cache error
- [ ] Suggests similar crates if possible

---

## Concurrent Agent Tests

### Test 14: Parallel Crate Queries
**Prompt:** "Compare tokio vs async-std"
**Verification:**
- [ ] Launches multiple agents if needed
- [ ] Aggregates results
- [ ] No race conditions in cache

### Test 15: Agent + Skill Combination
**Prompt:** "How to use async/await with tokio?"
**Verification:**
- [ ] m07-concurrency skill content
- [ ] tokio-specific info from agent
- [ ] Combined, coherent response

---

## Performance Tests

### Test 16: Cache Speed
**Setup:** Warm cache
**Prompt:** "serde version"
**Verification:**
- [ ] Response < 1 second
- [ ] No network calls

### Test 17: Cold Start
**Prompt:** New crate query (no cache)
**Verification:**
- [ ] Agent launches correctly
- [ ] Reasonable response time
- [ ] Cache populated for next query
