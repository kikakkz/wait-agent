# /unsafe-check

Check a file for unsafe code issues and potential safety violations.

## Usage

```
/unsafe-check [file]
```

## Parameters

- `file` (optional): Path to the Rust file to check. If not provided, checks the current file or prompts for input.

## Workflow

1. **Read the file** to identify all `unsafe` blocks and `unsafe fn`
2. **Load unsafe-checker skill** rules
3. **Check each unsafe block** against relevant rules:
   - SAFETY comment present? (safety-09)
   - Pointer validity verified? (ptr-*)
   - Panic safety considered? (safety-01)
   - FFI rules followed? (ffi-*)
4. **Report findings** with rule references and fix suggestions

## Checks Performed

### Safety Comments
- Every `unsafe` block should have `// SAFETY:` comment
- Comment should explain invariants, not just say "this is safe"

### Pointer Operations
- Null checks before dereference
- Alignment verification
- Bounds checking
- No aliasing violations

### FFI
- Types have `#[repr(C)]`
- Panics caught at boundary
- String handling correct
- Memory ownership clear

### Send/Sync
- Manual implementations are sound
- No data races possible

## Example Output

```
Checking: src/lib.rs

Found 3 unsafe blocks:

1. Line 42: unsafe { ptr.read() }
   - [WARN] Missing SAFETY comment (safety-09)
   - [WARN] No null check for ptr (ptr-01)
   Suggestion: Add SAFETY comment and verify ptr is non-null

2. Line 87: unsafe impl Send for MyType {}
   - [WARN] Missing Safety docs (safety-10)
   - [OK] Type analysis shows no !Send fields
   Suggestion: Add /// # Safety documentation

3. Line 123: extern "C" fn callback() { ... }
   - [WARN] No catch_unwind (ffi-04)
   Suggestion: Wrap body in std::panic::catch_unwind
```

## Related Commands

- `/unsafe-review` - Interactive unsafe code review
- `/guideline` - Query specific rules
