---
description: Create high-quality Rust crate skills from llms.txt
argument-hint: <crate_name> <llms_path> [version] [description]
---

Create high-quality skills for a Rust crate based on llms.txt documentation.

Arguments: $ARGUMENTS
- First argument: crate_name (required) - the Rust crate name (e.g., tokio, serde)
- Second argument: llms_path (required) - local path to the llms.txt file
- Third argument: version (optional) - the crate version (e.g., "1.40.0", "2.0.0")
- Fourth argument: description (optional) - additional requirements or information

---

## Task: Create {crate_name} Skills

**llms.txt file location**: {llms_path}

---

## Skill Quality Standards

Each skill must include the following structure:

### SKILL.md Structure

````markdown
---
name: {crate_name}-{feature}
description: |
  CRITICAL: Use for {crate_name} {feature} questions. Triggers on:
  {keyword1}, {keyword2}, {keyword3}, "{common question}",
  {中文关键词1}, {中文关键词2}, {中文问题}
---

# {CrateName} {Feature} Skill

> **Version:** {crate_name} {version} | **Last Updated:** {YYYY-MM-DD}
>
> Check for updates: https://crates.io/crates/{crate_name}

You are an expert at the Rust `{crate_name}` crate. Help users by:
- **Writing code**: Generate Rust code following the patterns below
- **Answering questions**: Explain concepts, troubleshoot issues, reference documentation

## Documentation

Refer to the local files for detailed documentation:
- `./references/{file1}.md` - {description}
- `./references/{file2}.md` - {description}

## IMPORTANT: Documentation Completeness Check

**Before answering questions, Claude MUST:**

1. Read the relevant reference file(s) listed above
2. If file read fails or file is empty:
   - Inform user: "本地文档不完整，建议运行 `/sync-crate-skills {crate_name} --force` 更新文档"
   - Still answer based on SKILL.md patterns + built-in knowledge
3. If reference file exists, incorporate its content into the answer

## Key Patterns

{Core code patterns, 3-5 most commonly used patterns}

## API Reference Table

| Function/Type | Description | Example |
|---------------|-------------|---------|
| ... | ... | ... |

## Deprecated Patterns (Don't Use)

| Deprecated | Correct | Notes |
|------------|---------|-------|
| ... | ... | ... |

## When Writing Code

1. {Best practice 1}
2. {Best practice 2}
3. ...

## When Answering Questions

1. {Key point 1}
2. {Key point 2}
3. ...
````

### References Directory

Each skill's `references/` directory contains detailed documentation:
- API reference documentation
- Configuration options details
- Advanced usage examples
- Feature-specific configurations

---

## Instructions

### 1. Read llms.txt and Analyze

1. **Read the entire llms.txt** content
2. **Identify content domains**: Find functional modules that can be separate skills
3. **Analyze each domain**:
   - What are the core concepts?
   - What APIs/configuration options exist?
   - What are common usage patterns?
   - What content needs detailed documentation?

### 1.5 Confirm Version Number

If the user did not provide a version number (third argument):
1. Use the AskUserQuestion tool to ask the user for the current version
2. Version format examples: "1.40.0", "2.0.0", "latest"
3. Use the version number for all SKILL.md Version fields

### 2. Output Detailed Plan

Output to `~/tmp/{YYYYMMDDHHmm}-{crate_name}-skills-plan.md`:

````markdown
# {CrateName} Skills Plan

## Analysis Summary
- Crate: {crate_name}
- Version: {version}
- Main functional domains: ...

## Skill List

### 1. {crate_name}-{feature1}
**Trigger conditions**: "...", "...", "..."
**Core content**: ...
**Reference files**:
- {file1}.md - {description}
- {file2}.md - {description}

### 2. {crate_name}-{feature2}
...
````

### 3. Create Skills

For each skill:

1. **Create directory structure**:
   ```
   ~/.claude/skills/{crate_name}-{feature}/
   ├── SKILL.md
   └── references/
       ├── {api-reference}.md
       └── {detailed-guide}.md
   ```

2. **Write SKILL.md**:
   - Follow the quality standards above
   - Keep SKILL.md concise (<200 lines)
   - Put complex content in references/

3. **Write reference files**:
   - Complete API reference
   - Configuration options tables
   - Detailed code examples
   - Feature-specific content

### 4. Content Allocation Principles

| Content Type | Location |
|--------------|----------|
| Core patterns (3-5) | SKILL.md |
| Complete API reference | references/ |
| Configuration options details | references/ |
| Feature-specific config | references/ |
| Advanced usage/edge cases | references/ |
| Deprecated patterns table | SKILL.md |
| Best practices | SKILL.md |

---

## Quality Checklist

- [ ] Each SKILL.md has CSO-optimized description with "CRITICAL:" prefix
- [ ] Each SKILL.md description includes Chinese trigger keywords
- [ ] Each SKILL.md has version info and update date
- [ ] Each SKILL.md has "You are an expert..." role definition
- [ ] Each SKILL.md has Documentation navigation list
- [ ] Each SKILL.md has "Documentation Completeness Check" section
- [ ] Each SKILL.md has Key Patterns code examples
- [ ] Each SKILL.md has Deprecated Patterns table (if applicable)
- [ ] Each SKILL.md has "When Writing Code" best practices
- [ ] Each SKILL.md has "When Answering Questions" guidelines
- [ ] Complex content has been split into references/ directory
- [ ] Code examples use latest Rust idioms
- [ ] No redundant documentation files (README.md, etc.)
- [ ] Skills created directly in `~/.claude/skills/` for auto-discovery

---

## Output Location

All skills are created in: `~/.claude/skills/{crate_name}-*/`

This is the local dynamic skills directory, not committed to the rust-skills repository.
