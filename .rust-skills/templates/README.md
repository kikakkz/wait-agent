# Rust Code Templates

## Overview

Ready-to-use code templates for common Rust patterns. These templates follow coding guidelines and best practices.

## Directory Structure

```
templates/
├── error-handling/     # Error type definitions
│   ├── thiserror.rs    # Library error with thiserror
│   ├── anyhow.rs       # Application error with anyhow
│   └── custom.rs       # Manual error implementation
│
├── concurrency/        # Concurrent patterns
│   ├── worker-pool.rs  # Thread pool pattern
│   ├── actor.rs        # Actor pattern with channels
│   └── async-task.rs   # Async task spawning
│
├── ffi/               # FFI patterns
│   ├── c-bindings.rs  # Calling C from Rust
│   ├── expose-api.rs  # Exposing Rust to C
│   └── safe-wrapper.rs # Safe wrapper for unsafe FFI
│
├── testing/           # Testing patterns
│   ├── unit-tests.rs  # Unit test examples
│   ├── mock.rs        # Mocking with traits
│   └── integration.rs # Integration test setup
│
└── project/           # Project templates
    ├── lib.rs         # Library crate structure
    ├── main.rs        # Binary crate structure
    └── Cargo.toml     # Cargo.toml with common deps
```

## Usage

Copy and adapt templates for your needs:

```bash
# Copy error template
cp templates/error-handling/thiserror.rs src/error.rs
```

## Templates Reference

| Template | Use When |
|----------|----------|
| thiserror.rs | Library with specific error types |
| anyhow.rs | Application with error context |
| worker-pool.rs | CPU-bound parallel processing |
| actor.rs | Message-passing concurrency |
| async-task.rs | I/O-bound async operations |
| c-bindings.rs | Calling existing C libraries |
| expose-api.rs | Building Rust library for C |
| safe-wrapper.rs | Wrapping unsafe FFI safely |
