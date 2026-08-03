# Unsafe-Checker Test Scenarios

## Skill Triggering Tests

### Test 1: Unsafe Keyword Trigger
**Prompt:** "Review this unsafe code block"
**Expected Skill:** unsafe-checker
**Expected Response Elements:**
- [ ] Reference to unsafe rules
- [ ] Checklist approach
- [ ] Safety documentation check

### Test 2: FFI Trigger
**Prompt:** "How to call C functions from Rust?"
**Expected Skill:** unsafe-checker
**Expected Response Elements:**
- [ ] FFI rules (ffi-*)
- [ ] extern "C" fn usage
- [ ] Type compatibility

### Test 3: Raw Pointer Trigger
**Prompt:** "Is *mut T safe to use?"
**Expected Skill:** unsafe-checker
**Expected Response Elements:**
- [ ] Pointer rules (ptr-*)
- [ ] Validity requirements
- [ ] Safe abstraction patterns

---

## Content Accuracy Tests

### Test 4: SAFETY Comment Check
**Prompt:**
```rust
unsafe {
    ptr.read()
}
```
What's wrong with this code?

**Expected Response Elements:**
- [ ] Missing SAFETY comment
- [ ] Reference to safety-09 rule
- [ ] Example of proper comment

### Test 5: Transmute Review
**Prompt:**
```rust
let x: u32 = 42;
let y: f32 = unsafe { std::mem::transmute(x) };
```

**Expected Response Elements:**
- [ ] Valid transmute (same size)
- [ ] Reference mem rules
- [ ] Warning about safer alternatives

### Test 6: FFI Panic Safety
**Prompt:**
```rust
#[no_mangle]
pub extern "C" fn callback(x: i32) -> i32 {
    if x < 0 {
        panic!("negative!");
    }
    x * 2
}
```

**Expected Response Elements:**
- [ ] Panic crosses FFI boundary (UB)
- [ ] Reference ffi-04 rule
- [ ] Suggest catch_unwind wrapper

---

## Checklist Tests

### Test 7: Before Writing Unsafe
**Prompt:** "I want to write unsafe code for performance"

**Expected Response Elements:**
- [ ] Reference to checklists/before-unsafe.md
- [ ] Questions: Is unsafe needed?
- [ ] Safer alternatives
- [ ] Profiling advice

### Test 8: Code Review Request
**Prompt:** "Review this unsafe impl for safety:"
```rust
unsafe impl Send for MyType {}
```

**Expected Response Elements:**
- [ ] Reference to review checklist
- [ ] Check: are all fields Send?
- [ ] Reference to safety-10 rule
- [ ] Documentation requirement

---

## FFI Specific Tests

### Test 9: CString Usage
**Prompt:** "How to pass a string to C?"
**Expected Response Elements:**
- [ ] Use CString/CStr
- [ ] Reference ffi-01 (no direct String)
- [ ] Null terminator handling
- [ ] Memory ownership

### Test 10: Struct Layout
**Prompt:**
```rust
struct MyStruct {
    a: u8,
    b: u64,
}
```
Can I pass this to C?

**Expected Response Elements:**
- [ ] Missing #[repr(C)]
- [ ] Padding/alignment issues
- [ ] Reference mem-01 rule
- [ ] Correct example with repr(C)

---

## Edge Cases

### Test 11: Union Type
**Prompt:**
```rust
union MyUnion {
    i: i32,
    f: f32,
}
```

**Expected Response Elements:**
- [ ] Reference union rules (union-*)
- [ ] Reading requires unsafe
- [ ] No cross-lifetime references
- [ ] FFI use case

### Test 12: MaybeUninit
**Prompt:** "How to use MaybeUninit correctly?"
**Expected Response Elements:**
- [ ] Reference mem-06 rule
- [ ] Initialization requirements
- [ ] assume_init safety
- [ ] Example patterns
