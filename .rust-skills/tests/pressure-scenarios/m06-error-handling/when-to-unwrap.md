# Pressure Scenario: When to Use unwrap()

## Skill Under Test
m06-error-handling

## User Question
"When is it okay to use .unwrap() in Rust?"

## Expected Behavior
- [x] Explain unwrap() semantics (panic on None/Err)
- [x] List acceptable use cases (tests, examples, guaranteed values)
- [x] Explain alternatives (?, expect, unwrap_or, match)
- [x] Quick reference for error handling patterns
- [x] Reference to G.ERR.01, P.ERR.02 guidelines

## Baseline Test (without skill)
Date: [To be filled]

Result:
- [ ] Semantics explanation: [PASS/FAIL]
- [ ] Use case list: [PASS/FAIL]
- [ ] Alternatives: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Post-Skill Test
Date: [To be filled]

Result:
- [ ] Semantics explanation: [PASS/FAIL]
- [ ] Use case list: [PASS/FAIL]
- [ ] Alternatives: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[To be filled after test]

## Edge Cases
1. "My code will never have None" → Should encourage defensive coding
2. "unwrap vs expect?" → Should explain expect's documentation value
3. "What about unwrap_or_default?" → Should explain lazy evaluation
