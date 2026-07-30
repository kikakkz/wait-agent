# Agent Cache System

## Overview

This directory contains cached responses from agents to reduce redundant web fetches and improve response time.

## Cache Structure

```
cache/
├── README.md
├── config.yaml           # Cache configuration
├── crates/               # Crate information cache
│   ├── tokio.json
│   ├── serde.json
│   └── ...
├── rust-versions/        # Rust version changelog cache
│   ├── 1.75.json
│   ├── 1.76.json
│   └── ...
├── clippy-lints/         # Clippy lint information cache
│   └── lints.json
└── docs/                 # API documentation cache
    ├── tokio/
    ├── serde/
    └── ...
```

## Cache Entry Format

### Crate Cache (`crates/*.json`)

```json
{
  "name": "tokio",
  "version": "1.35.1",
  "description": "An event-driven, non-blocking I/O platform",
  "features": ["full", "rt-multi-thread", "macros", "sync"],
  "repository": "https://github.com/tokio-rs/tokio",
  "cached_at": "2024-01-15T10:30:00Z",
  "ttl_hours": 24,
  "source": "lib.rs"
}
```

### Rust Version Cache (`rust-versions/*.json`)

```json
{
  "version": "1.75.0",
  "release_date": "2023-12-28",
  "highlights": [
    "async fn in traits",
    "RPITIT (return position impl Trait in traits)"
  ],
  "stabilized_features": [
    "async_fn_in_trait",
    "impl_trait_projections"
  ],
  "cached_at": "2024-01-15T10:30:00Z",
  "ttl_hours": 168,
  "source": "releases.rs"
}
```

## Cache Configuration (`config.yaml`)

```yaml
cache:
  enabled: true

  # Time-to-live settings (in hours)
  ttl:
    crates: 24        # Crate info valid for 24 hours
    rust_versions: 168  # Rust versions valid for 1 week
    clippy_lints: 168   # Clippy lints valid for 1 week
    docs: 72           # API docs valid for 3 days

  # Cache size limits
  limits:
    max_entries_per_category: 100
    max_total_size_mb: 50

  # Auto-cleanup
  cleanup:
    enabled: true
    interval_hours: 24
    remove_expired: true
```

## Usage in Agents

### Checking Cache Before Fetch

```
1. Check if cache/<category>/<key>.json exists
2. If exists, check if (now - cached_at) < ttl_hours
3. If valid, return cached data
4. If invalid/missing, fetch fresh data
5. Store result in cache with timestamp
```

### Example Agent Workflow

```markdown
## Cache-Aware Workflow

1. **Check Cache**
   - Read cache/crates/<crate_name>.json
   - If valid (exists and not expired), return cached data

2. **Fetch if Needed**
   - Use actionbook/agent-browser to fetch
   - Parse and structure the data

3. **Update Cache**
   - Write to cache/crates/<crate_name>.json
   - Include cached_at timestamp
```

## Cache Management Commands

### Clear All Cache
```bash
rm -rf cache/crates/* cache/rust-versions/* cache/docs/*
```

### Clear Expired Only
```bash
# Use the cache-cleaner agent or manual script
find cache -name "*.json" -mtime +7 -delete
```

### View Cache Stats
```bash
echo "Crates cached: $(ls cache/crates/*.json 2>/dev/null | wc -l)"
echo "Versions cached: $(ls cache/rust-versions/*.json 2>/dev/null | wc -l)"
echo "Total size: $(du -sh cache 2>/dev/null | cut -f1)"
```

## Best Practices

1. **Always check cache first** - Reduces latency and API load
2. **Use appropriate TTL** - Balance freshness vs. performance
3. **Include source** - Track where data came from
4. **Handle stale gracefully** - Return stale if fetch fails
5. **Don't cache errors** - Only cache successful responses
