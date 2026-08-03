# Rust Skills for OpenCode

> OpenCode integration for rust-skills

## Installation

### Method 1: Instructions Configuration (Recommended)

Add rust-skills as instructions in your OpenCode config:

**Step 1:** Clone the repository

```bash
git clone https://github.com/actionbook/rust-skills.git ~/rust-skills
```

**Step 2:** Add to your OpenCode configuration

Edit `~/.config/opencode/opencode.json` (or `.opencode/opencode.json` in your project):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "instructions": [
    "~/rust-skills/.opencode/instructions/rust-skills.md"
  ]
}
```

### Method 2: Copy Instructions to Global Config

```bash
# Clone rust-skills
git clone https://github.com/actionbook/rust-skills.git

# Create OpenCode instructions directory
mkdir -p ~/.config/opencode/instructions

# Copy instruction files
cp rust-skills/.opencode/instructions/*.md ~/.config/opencode/instructions/
```

Then add to your config:

```json
{
  "instructions": [
    "~/.config/opencode/instructions/rust-skills.md"
  ]
}
```

### Method 3: Project-Level Instructions

For per-project usage, create `.opencode/opencode.json` in your Rust project:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "instructions": [
    "~/rust-skills/.opencode/instructions/rust-skills.md"
  ]
}
```

## Verification

After installation, test by asking OpenCode:

```
How do I fix E0382 in Rust?
```

Expected: OpenCode should reference ownership concepts and provide Rust-specific guidance.

## What's Included

The rust-skills instructions provide:

- **Rust Coding Guidelines**: Naming conventions, formatting rules, best practices
- **Error Code Reference**: E0382, E0597, E0277, etc. with explanations
- **Meta-Question Skills**: Ownership, concurrency, error handling patterns
- **Default Project Settings**: Recommended Cargo.toml configurations

## Configuration Reference

| Config Location | Scope |
|-----------------|-------|
| `~/.config/opencode/opencode.json` | Global (all projects) |
| `.opencode/opencode.json` | Project-specific |

## Troubleshooting

### Instructions not loading?

1. Verify the path exists:
   ```bash
   ls ~/rust-skills/.opencode/instructions/
   ```

2. Check config syntax:
   ```bash
   cat ~/.config/opencode/opencode.json | jq .
   ```

3. Restart OpenCode after config changes

### Path expansion issues?

Use absolute paths instead of `~`:
```json
{
  "instructions": [
    "/Users/yourname/rust-skills/.opencode/instructions/rust-skills.md"
  ]
}
```

## Limitations

OpenCode integration provides instruction-based guidance only. Features requiring Claude Code's specific capabilities are not available:

| Feature | OpenCode | Claude Code |
|---------|----------|-------------|
| Basic Rust guidance | ✅ | ✅ |
| Error code explanations | ✅ | ✅ |
| Coding guidelines | ✅ | ✅ |
| Auto-triggering hooks | ❌ | ✅ |
| Background agents | ❌ | ✅ |
| Dynamic skill loading | ❌ | ✅ |

## Links

- [OpenCode Documentation](https://opencode.ai/docs/)
- [OpenCode Config Reference](https://opencode.ai/docs/config/)
- [Rust Skills GitHub](https://github.com/actionbook/rust-skills)
