# Pressure Scenario: Send/Sync Trait Bounds

## Skill Under Test
m07-concurrency

## User Question
"Why am I getting E0277: `Rc<T>` cannot be sent between threads safely?"

## Code Context
```rust
use std::rc::Rc;
use std::thread;

fn main() {
    let data = Rc::new(42);
    thread::spawn(move || {
        println!("{}", data);  // E0277
    });
}
```

## Expected Behavior
- [x] Explain Send/Sync traits
- [x] Explain why Rc is !Send
- [x] Provide fix: use Arc instead
- [x] Quick reference for concurrency patterns
- [x] Reference to P.MTH.LCK.01, G.MTH.LCK.01 guidelines

## Baseline Test (without skill)
Date: [To be filled]

Result:
- [ ] Send/Sync explanation: [PASS/FAIL]
- [ ] Rc !Send reason: [PASS/FAIL]
- [ ] Arc fix: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Post-Skill Test
Date: [To be filled]

Result:
- [ ] Send/Sync explanation: [PASS/FAIL]
- [ ] Rc !Send reason: [PASS/FAIL]
- [ ] Arc fix: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Edge Cases
1. "What about RefCell?" → Should explain !Sync
2. "Can I implement Send manually?" → Should warn about unsafe impl
3. "Arc vs Mutex?" → Should explain shared ownership vs shared mutability
