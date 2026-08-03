# /skill-index

Query Rust skills by meta-question category or technical subcategory.

## Usage

```
/skill-index <query>
```

## Parameters

- `query` (required): Meta-question code (e.g., `m01`), tech category (e.g., `200`), or keyword

## Examples

```
/skill-index m01             # Memory ownership skills
/skill-index m07             # Concurrency skills
/skill-index 200             # Web development (Axum)
/skill-index 250             # Tokio runtime
/skill-index ownership       # Search by keyword
/skill-index tokio           # Search by crate name
```

## Query Format

### By Meta-Question (XX)
- `m01` - Memory Ownership & Lifetimes
- `m02` - Resource Management Balance
- `m03` - Mutability Boundaries
- `m04` - Zero-Cost Abstractions
- `m05` - Type-Driven Design
- `m06` - Error Handling Philosophy
- `m07` - Concurrency Correctness
- ~~`m08`~~ - Merged into `unsafe-checker`
- `m09` - Domain Constraint Mapping
- `m10` - Performance Optimization Model
- `m11` - Ecosystem Integration
- `m12` - Domain Lifecycle
- `m13` - Domain Error Patterns
- `m14` - Mental Model Construction
- `m15` - Error Pattern Recognition

### By Tech Category (YYY)
- `001-099` - Language Core
- `100-199` - Standard Library
- `200-299` - Web Development
- `250-299` - Async/Concurrency
- `400-499` - Data Processing
- `500-599` - Systems Programming
- `700-799` - Embedded Development
- `800-899` - Cross-Language Integration
- `850-899` - Toolchain & Build

### By Domain Extension
- `F*` - FinTech
- `M*` - Machine Learning
- `CN*` - Cloud Native
- `IoT*` - Internet of Things

## Output

Returns matching skills with:
- Category code and name
- Related technical subcategories
- Key concepts and keywords
- Cognitive level range (L0-L4)
