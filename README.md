# rvl-kovrex

**CMD+RVL's Kovrex agent wrapper for [rvl](https://github.com/cmdrvl/rvl).**

This is a reference implementation showing how to wrap a CLI tool as a Kovrex agent with a REST API.

## What is this?

[rvl](https://github.com/cmdrvl/rvl) is an open-source CLI that compares CSV files and reveals the smallest set of numeric changes that explain what actually changed.

**rvl-kovrex** wraps rvl as a REST API so it can be:
- Called by Kovrex as an opinionated agent
- Deployed to Railway/Fly.io/etc.
- Integrated into automation pipelines

## Quick Start

### Run locally

```bash
# Clone
git clone https://github.com/cmdrvl/rvl-kovrex.git
cd rvl-kovrex

# Set auth token
export RVL_API_TOKEN=$(openssl rand -base64 32)

# Run
cargo run --release
```

### Test it

```bash
# Health check
curl http://localhost:8080/health

# Compare two CSVs (JSON with base64)
curl -X POST http://localhost:8080/compare \
  -H "Authorization: Bearer $RVL_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "old": "'$(base64 -i old.csv)'",
    "new": "'$(base64 -i new.csv)'",
    "key": "id"
  }'
```

## API

### `GET /health`

Health check (unauthenticated).

```json
{
  "status": "ok",
  "agent": "rvl",
  "operator": "cmd-rvl",
  "version": "0.1.0"
}
```

### `POST /compare`

Compare two CSV files. Requires bearer token if `RVL_API_TOKEN` is set.

**Request:** JSON
```json
{
  "old": "base64-encoded-csv-content",
  "new": "base64-encoded-csv-content",
  "key": "id",
  "threshold": 0.95,
  "tolerance": 1e-9,
  "delimiter": "comma"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `old` | string | ✓ | Base64-encoded old CSV |
| `new` | string | ✓ | Base64-encoded new CSV |
| `key` | string | | Column for row alignment |
| `threshold` | number | | Coverage target (default: 0.95) |
| `tolerance` | number | | Noise floor (default: 1e-9) |
| `delimiter` | string | | Force delimiter |

**Response:** Same JSON structure as `rvl --json`

```json
{
  "version": "rvl.v0",
  "outcome": "REAL_CHANGE",
  "contributors": [
    {
      "row_id": "u8:2",
      "column": "u8:value",
      "old": 200.0,
      "new": 250.0,
      "delta": 50.0,
      "contribution": 50.0,
      "share": 1.0,
      "cumulative_share": 1.0
    }
  ],
  ...
}
```

**Outcomes:**
- `REAL_CHANGE` (HTTP 200) - Found explainable numeric changes
- `NO_REAL_CHANGE` (HTTP 200) - All differences within tolerance
- `REFUSAL` (HTTP 422) - Cannot produce deterministic verdict

## Deploy to Railway

1. Fork this repo or connect directly
2. Create new project in Railway dashboard
3. Set environment variable:
   ```
   RVL_API_TOKEN=<your-secret-token>
   ```
4. Deploy

Railway will auto-detect the Dockerfile and configure healthchecks.

Generate a token:
```bash
openssl rand -base64 32
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RVL_PORT` | `8080` | Port to listen on |
| `RVL_HOST` | `0.0.0.0` | Host to bind to |
| `RVL_API_TOKEN` | (none) | Bearer token for auth (recommended in production) |

## Kovrex Agent Manifest

```yaml
name: rvl
operator: cmd-rvl
version: 0.1.0
description: >
  Reveal the smallest set of numeric changes that explain what actually changed.
  Deterministic CSV comparison with explainable verdicts.

scope:
  does:
    - Compare two CSV files numerically
    - Identify top contributors to change (ranked by abs delta)
    - Confirm no real change when within tolerance
    - Refuse with actionable next steps when alignment is ambiguous
  does_not:
    - Explain why markets moved
    - Validate business logic
    - Handle non-CSV formats
    - Auto-select alignment keys

input_schema:
  type: object
  required: [old_csv, new_csv]
  properties:
    old_csv: { type: string, format: binary }
    new_csv: { type: string, format: binary }
    key: { type: string }
    threshold: { type: number, default: 0.95 }
    tolerance: { type: number, default: 1e-9 }
```

## Why Kovrex?

rvl is **opinionated** - it gives clear verdicts, not probabilities:
- `REAL_CHANGE` with the smallest explanation set
- `NO_REAL_CHANGE` with proof
- `REFUSAL` with a concrete next step

This makes it a perfect Kovrex agent: deterministic, auditable, bounded scope.

## License

MIT
