# Pressure Scenario: E0382 Moved Value

## Skill Under Test
m01-ownership

## User Question
"Why am I getting E0382 error: use of moved value?"

## Code Context
```rust
fn main() {
    let s = String::from("hello");
    let s2 = s;
    println!("{}", s);  // E0382
}
```

## Expected Behavior
- [x] Explain move semantics (ownership transfer)
- [x] Show that String is not Copy
- [x] Provide fix options (clone, reference, restructure)
- [x] Quick reference table for ownership patterns
- [x] Reference to P.VAR.01, P.VAR.02 guidelines

## Baseline Test (without skill)
Date: [To be filled]

Result:
- [ ] Move semantics: [PASS/FAIL]
- [ ] Not Copy explanation: [PASS/FAIL]
- [ ] Fix options: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Post-Skill Test
Date: [To be filled]

Result:
- [ ] Move semantics: [PASS/FAIL]
- [ ] Not Copy explanation: [PASS/FAIL]
- [ ] Fix options: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Edge Cases
1. "Why does i32 work but String doesn't?" → Should explain Copy trait
2. "Can I just use unsafe to ignore this?" → Should discourage, explain risks
3. "Is clone always the solution?" → Should explain performance implications
