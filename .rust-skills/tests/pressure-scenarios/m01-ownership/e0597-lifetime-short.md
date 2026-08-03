# Pressure Scenario: E0597 Lifetime Too Short

## Skill Under Test
m01-ownership

## User Question
"Why am I getting E0597: borrowed value does not live long enough?"

## Code Context
```rust
fn get_str() -> &str {
    let s = String::from("hello");
    &s  // E0597
}
```

## Expected Behavior
- [x] Explain stack vs heap lifetime
- [x] Show why returned reference is invalid
- [x] Provide fix options (return owned, 'static, lifetime params)
- [x] Quick reference for lifetime patterns
- [x] Reference to P.MEM.LFT.01, P.MEM.LFT.02 guidelines

## Baseline Test (without skill)
Date: [To be filled]

Result:
- [ ] Stack/heap lifetime: [PASS/FAIL]
- [ ] Invalid reference: [PASS/FAIL]
- [ ] Fix options: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Post-Skill Test
Date: [To be filled]

Result:
- [ ] Stack/heap lifetime: [PASS/FAIL]
- [ ] Invalid reference: [PASS/FAIL]
- [ ] Fix options: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Edge Cases
1. "What if I use Box?" → Should explain heap allocation
2. "Can I use 'static?" → Should explain when appropriate
3. "What about Cow?" → Should suggest for flexible ownership
