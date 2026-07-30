# Ownership (m01) Test Scenarios

## Skill Triggering Tests

### Test 1: Error Code Trigger
**Prompt:** "Why am I getting E0382 error?"
**Expected Skill:** m01-ownership
**Expected Response Elements:**
- [ ] Explanation of "use of moved value"
- [ ] Code example showing the error
- [ ] Fix options (clone, borrow, restructure)

### Test 2: Symptom Trigger
**Prompt:** "Value moved here after borrow"
**Expected Skill:** m01-ownership
**Expected Response Elements:**
- [ ] Borrow checker explanation
- [ ] Lifetime implications
- [ ] Solution patterns

### Test 3: Concept Trigger
**Prompt:** "How does ownership work in Rust?"
**Expected Skill:** m01-ownership
**Expected Response Elements:**
- [ ] Ownership rules (3 rules)
- [ ] Move semantics
- [ ] Borrowing explanation

---

## Content Accuracy Tests

### Test 4: E0382 Detailed Explanation
**Prompt:**
```
I have this code and it gives E0382:
let s = String::from("hello");
let s2 = s;
println!("{}", s);
```

**Expected Response Elements:**
- [ ] Identify that `s` was moved to `s2`
- [ ] Explain move semantics for String
- [ ] Provide fix: `s.clone()` or borrow `&s`

### Test 5: Lifetime Error
**Prompt:**
```
fn longest(x: &str, y: &str) -> &str {
    if x.len() > y.len() { x } else { y }
}
```
Error: missing lifetime specifier

**Expected Response Elements:**
- [ ] Explain why lifetime needed
- [ ] Show correct signature: `fn longest<'a>(x: &'a str, y: &'a str) -> &'a str`
- [ ] Explain lifetime elision rules

### Test 6: Borrow Conflict
**Prompt:**
```
let mut v = vec![1, 2, 3];
let first = &v[0];
v.push(4);
println!("{}", first);
```

**Expected Response Elements:**
- [ ] Explain borrow conflict (immutable + mutable)
- [ ] Mention potential vector reallocation
- [ ] Provide fix: copy value or restructure

---

## Deep Dive Tests

### Test 7: Reference Deep Dive Request
**Prompt:** "Show me common ownership error patterns and fixes"

**Expected Response Elements:**
- [ ] Reference to patterns/common-errors.md
- [ ] Multiple error code examples
- [ ] Categorized fix strategies

### Test 8: Comparison Request
**Prompt:** "How does Rust ownership compare to C++ RAII?"

**Expected Response Elements:**
- [ ] Reference to comparison.md
- [ ] Key differences (move by default)
- [ ] Smart pointer comparison (Box vs unique_ptr)

---

## Edge Cases

### Test 9: Complex Lifetime
**Prompt:**
```
struct Excerpt<'a> {
    part: &'a str,
}

impl<'a> Excerpt<'a> {
    fn level(&self) -> i32 { 3 }
}
```
Why does this need a lifetime?

**Expected Response Elements:**
- [ ] Struct holds reference, must track lifetime
- [ ] Lifetime ensures reference validity
- [ ] Implementation inherits lifetime

### Test 10: Interior Mutability
**Prompt:** "When should I use RefCell vs Mutex?"

**Expected Skill:** m01-ownership (or m02-resource)
**Expected Response Elements:**
- [ ] RefCell for single-threaded
- [ ] Mutex for multi-threaded
- [ ] Runtime vs compile-time checking tradeoff
