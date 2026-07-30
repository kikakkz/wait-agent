# Rust Compiler Error Code Index

Quick lookup table: Error code → Skill routing

## Ownership & Lifetimes (m01)

| Error Code | Message | Skill | Common Fix |
|------------|---------|-------|------------|
| E0382 | use of moved value | m01-ownership | Use `clone()`, restructure ownership |
| E0597 | borrowed value does not live long enough | m01-ownership | Extend lifetime, restructure scopes |
| E0499 | cannot borrow as mutable more than once | m01-ownership | Use `RefCell`, restructure code |
| E0502 | cannot borrow as mutable while immutable borrow exists | m01-ownership | Split borrows, use interior mutability |
| E0506 | cannot assign to borrowed value | m01-ownership | Drop borrow before assignment |
| E0507 | cannot move out of borrowed content | m01-ownership | Use `clone()`, `take()`, or restructure |
| E0515 | cannot return reference to local variable | m01-ownership | Return owned value, use lifetime params |
| E0716 | temporary value dropped while borrowed | m01-ownership | Bind to variable, extend lifetime |
| E0621 | explicit lifetime required in the type | m01-ownership | Add lifetime annotations |

## Mutability (m03)

| Error Code | Message | Skill | Common Fix |
|------------|---------|-------|------------|
| E0596 | cannot borrow as mutable | m03-mutability | Add `mut`, use interior mutability |

## Type System (m04)

| Error Code | Message | Skill | Common Fix |
|------------|---------|-------|------------|
| E0277 | the trait bound `X` is not satisfied | m04-zero-cost / m07-concurrency | Implement trait, add bound, use `dyn` |
| E0308 | mismatched types | m04-zero-cost | Type conversion, fix generics |
| E0599 | no method named `X` found for type `Y` | m04-zero-cost | Import trait, check types |

## Concurrency (m07)

| Error Code | Message | Skill | Common Fix |
|------------|---------|-------|------------|
| E0277 (Send) | `X` cannot be sent between threads safely | m07-concurrency | Use `Arc`, ensure `Send` |
| E0277 (Sync) | `X` cannot be shared between threads safely | m07-concurrency | Use `Mutex`, ensure `Sync` |

## Ecosystem (m11)

| Error Code | Message | Skill | Common Fix |
|------------|---------|-------|------------|
| E0425 | cannot find value `X` in this scope | m11-ecosystem | Import with `use`, check visibility |
| E0433 | failed to resolve: could not find `X` | m11-ecosystem | Add dependency, fix path |
| E0603 | `X` is private | m11-ecosystem | Use public API, check exports |

## Quick Diagnosis Flow

```
Compiler Error
    ↓
Contains "moved" or "borrow"?
    → m01-ownership
    ↓
Contains "mutable"?
    → m03-mutability
    ↓
Contains "Send" or "Sync" or "thread"?
    → m07-concurrency
    ↓
Contains "trait bound" or "type"?
    → m04-zero-cost
    ↓
Contains "cannot find" or "private"?
    → m11-ecosystem
```

## Common Error Patterns

### "value moved here" → m01
```rust
let s = String::from("hello");
let s2 = s;  // s moved here
println!("{}", s);  // E0382: use of moved value
```

### "does not live long enough" → m01
```rust
fn dangling() -> &str {
    let s = String::from("hello");
    &s  // E0597: s does not live long enough
}
```

### "cannot be sent between threads" → m07
```rust
let rc = Rc::new(42);
thread::spawn(move || {
    println!("{}", rc);  // E0277: Rc<i32> cannot be sent
});
```

### "trait bound not satisfied" → m04
```rust
fn print_debug<T: Debug>(t: T) {
    println!("{:?}", t);
}
print_debug(SomeType);  // E0277 if SomeType: !Debug
```
