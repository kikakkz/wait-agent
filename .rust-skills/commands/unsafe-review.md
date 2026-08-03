# /unsafe-review

Interactive review session for unsafe Rust code.

## Usage

```
/unsafe-review
```

## Description

Starts an interactive review session that guides you through reviewing unsafe code using the `unsafe-checker` skill checklists.

## Workflow

1. **Identify unsafe code** in the current file/selection
2. **Load review checklist** from `unsafe-checker/checklists/review-unsafe.md`
3. **Step through each check**:
   - Ask clarifying questions
   - Verify invariants
   - Suggest improvements
4. **Generate report** with findings and recommendations

## Interactive Prompts

The review will ask questions like:

```
Reviewing: unsafe { *ptr }

1. Is this pointer guaranteed non-null?
   - How is null prevented?
   - Show me the null check

2. Is the pointer properly aligned?
   - What type is it pointing to?
   - Where does the pointer come from?

3. Is the pointed-to memory valid?
   - Who allocated it?
   - Is it initialized?
   - How long is it valid?

4. Could this panic?
   - What happens if it panics here?
   - Is cleanup needed?
```

## Checklist Categories

### Surface-Level
- SAFETY comments present and meaningful?
- Safety documentation for unsafe fn?
- Unsafe blocks minimized?

### Memory Safety
- Pointer validity (non-null, aligned, valid)
- No aliasing violations
- No use-after-free
- No double-free
- Bounds checking

### Type Safety
- Correct transmutes
- Valid enum discriminants
- Proper repr attributes

### Concurrency
- Send/Sync correctness
- No data races
- Proper synchronization

### FFI
- Type compatibility
- Panic handling
- Error handling
- Memory ownership

## Example Session

```
/unsafe-review

Scanning for unsafe code...
Found 2 unsafe blocks and 1 unsafe fn.

--- Review 1/3 ---
Location: src/buffer.rs:42
Code: unsafe { slice::from_raw_parts(self.ptr, self.len) }

[Checklist]
[ ] SAFETY comment present?
    > Yes: "// SAFETY: ptr and len are validated in new()"

[ ] Pointer non-null?
    > Checking... new() uses NonNull, so guaranteed

[ ] Pointer aligned?
    > Type is u8, alignment is 1, always aligned

[ ] Length valid?
    > len is set in new() and never changed

[Result] PASS - All checks satisfied

--- Review 2/3 ---
...
```

## Output

After review completes:

```
=== Unsafe Review Summary ===

Total unsafe items: 3
- Passed: 2
- Warnings: 1
- Errors: 0

Warnings:
1. src/ffi.rs:87 - Missing catch_unwind in extern "C" fn

Recommendations:
- Add panic handling to FFI functions
- Consider using NonNull instead of raw pointers
```

## Related Commands

- `/unsafe-check [file]` - Quick automated check
- `/guideline P.UNS.*` - Query unsafe rules
