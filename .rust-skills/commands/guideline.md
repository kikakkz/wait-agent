# /guideline

Query Rust coding guidelines and best practices.

## Usage

```
/guideline <query>
/guideline --clippy <lint>
```

## Parameters

- `query` (required): Rule ID (e.g., `P.NAM.01`) or keyword (e.g., `naming`)
- `--clippy <lint>`: Look up a Clippy lint and map it to guideline rules

## Examples

```
/guideline P.NAM.01          # Get specific rule
/guideline naming            # Search naming conventions
/guideline clippy            # Search clippy-related rules
/guideline --clippy needless_clone  # Map clippy lint to rule
```

## Workflow

### Standard Query
1. Parse query type (rule ID or keyword)
2. Check if unsafe-related → route to `unsafe-checker` skill
3. Search in rules files or rules-index.md
4. Return matching rules with:
   - Rule ID and level (P/G)
   - Title and description
   - Code examples
   - Link to full documentation

### Clippy Lint Query (`--clippy`)
1. Use `clippy-researcher` agent
2. Look up lint in `clippy-lints/_index.md`
3. Return:
   - Lint description
   - Mapped rule ID and skill
   - Fix suggestions

## Rule Levels

- **P (Prescribed)**: Must follow - Required rules
- **G (Guidance)**: Should follow - Recommended rules

## Routing

| Query Type | Routed To |
|------------|-----------|
| P.UNS.*, G.UNS.*, FFI, unsafe | `unsafe-checker` skill |
| P.*, G.* (other) | `coding-guidelines` skill |
| --clippy <lint> | `clippy-researcher` agent |

## Related Commands

- `/unsafe-check` - Check file for unsafe issues
- `/unsafe-review` - Interactive unsafe review
