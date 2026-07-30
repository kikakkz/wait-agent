# TDD for Rust-Skills

Test-Driven Development framework for creating and validating skills.

## Core Principle

**"NO SKILL WITHOUT FAILING TEST FIRST"**

Before creating or modifying a skill, you must:
1. Define a pressure scenario that the skill should handle
2. Test the scenario WITHOUT the skill
3. Document the baseline failure
4. Only then create/modify the skill

## TDD Phases

### RED Phase: Define Failure

1. **Identify pressure scenario**
   - User question that triggers the skill
   - Expected knowledge gaps without the skill

2. **Test WITHOUT skill loaded**
   - Ask Claude the question in a fresh session
   - Document what Claude gets wrong or misses

3. **Document baseline**
   ```markdown
   ## Scenario: E0382 Error Explanation

   User Question: "Why am I getting E0382 error?"

   Baseline (without skill):
   - [ ] Explains move semantics
   - [ ] Shows common patterns
   - [ ] References Rust documentation
   - [x] MISSING: Domain-specific examples
   - [x] MISSING: Quick reference table
   - [x] MISSING: Related guidelines (P.VAR.01)
   ```

### GREEN Phase: Create Minimal Skill

1. **Write minimal skill content**
   - Address ONLY the documented failures
   - Keep content under 200 words (excluding tables)

2. **Test WITH skill loaded**
   - Ask the same question
   - Verify improvements

3. **Verify checklist**
   ```markdown
   ## Verification: E0382 Explanation

   With skill loaded:
   - [x] Explains move semantics
   - [x] Shows common patterns
   - [x] References Rust documentation
   - [x] Domain-specific examples ← NEW
   - [x] Quick reference table ← NEW
   - [x] Related guidelines (P.VAR.01) ← NEW
   ```

### REFACTOR Phase: Close Loopholes

1. **Identify bypass scenarios**
   - How might the skill be skipped?
   - What edge cases aren't covered?

2. **Add explicit counters**
   - CSO keywords for edge cases
   - Cross-references to related skills

3. **Test edge cases**
   ```markdown
   ## Edge Cases: m01-ownership

   - "Why can't I use this variable?" → Should trigger
   - "Borrow checker is wrong" → Should trigger + explain
   - "How to fix E0382" → Should trigger + provide fix
   ```

## Pressure Scenario Template

```markdown
# Pressure Scenario: [Name]

## Skill Under Test
[skill-name]

## User Question
"[Exact question user might ask]"

## Expected Behavior
- [ ] Specific knowledge point 1
- [ ] Specific knowledge point 2
- [ ] Quick reference provided
- [ ] Related guidelines mentioned

## Baseline Test (without skill)
Date: YYYY-MM-DD

Result:
- [ ] Knowledge point 1: [PASS/FAIL]
- [ ] Knowledge point 2: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[What was missing or incorrect]

## Post-Skill Test
Date: YYYY-MM-DD

Result:
- [ ] Knowledge point 1: [PASS/FAIL]
- [ ] Knowledge point 2: [PASS/FAIL]
- [ ] Quick reference: [PASS/FAIL]
- [ ] Guidelines: [PASS/FAIL]

Notes:
[Improvements observed]
```

## Rationalization Prevention

Common excuses and counters for skipping TDD:

| Excuse | Counter |
|--------|---------|
| "I already know what's needed" | Run pressure scenario first to confirm |
| "This is a simple change" | Simple changes have subtle edge cases |
| "I'll test it later" | Technical debt - you'll forget the test cases |
| "The skill is working fine" | Define "fine" with measurable criteria |
| "Testing skills is overkill" | Skills affect every user interaction |

## Quality Metrics

### Token Efficiency
- [ ] Main SKILL.md < 200 words (excluding tables)
- [ ] Quick reference table present
- [ ] Examples compressed (target: 20 words each)

### CSO Compliance
- [ ] Description starts with "Use when:"
- [ ] Error codes listed
- [ ] Symptom keywords included
- [ ] User questions as triggers

### Coverage
- [ ] At least 3 pressure scenarios per skill
- [ ] Edge cases documented
- [ ] Cross-references to related skills

## Running Tests

### Manual Testing
1. Start fresh Claude session (no skills loaded)
2. Ask pressure scenario question
3. Document response quality
4. Load skills
5. Ask same question
6. Compare and document improvements

### Automated Indicators
While fully automated testing isn't available, track:
- User satisfaction (via feedback)
- Routing accuracy (via logs if available)
- Common follow-up questions (indicates gaps)

## Directory Structure

```
tests/pressure-scenarios/
├── m01-ownership/
│   ├── e0382-moved-value.md
│   ├── e0597-lifetime-short.md
│   └── borrow-conflict.md
├── m06-error-handling/
│   ├── when-to-unwrap.md
│   └── error-propagation.md
└── m07-concurrency/
    ├── send-sync-bounds.md
    └── async-lifetime.md
```
