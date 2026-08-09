# AgentRT

AgentRT is a systems-grade execution runtime for multi-step LLM work:

> durable across crashes, constrained when it uses tools, reconstructable from an audit trail, and measurable through regression evals.

This repository is being built around one proof: kill a run mid-execution, restart it, and show that it resumes from the last durable checkpoint without silently repeating completed work.

## Current status

The repository contains the Rust command-line skeleton and module boundaries. Runtime behavior is intentionally not claimed yet.

## Run it

```text
cargo run -- version
cargo test
```

## Scope

The initial runtime targets a single local machine. It is not a hosted agent platform, an IDE, a multi-node workflow cluster, or a model-training project.

See [PROJECT.md](PROJECT.md) for the product contract, acceptance criteria, and implementation boundaries.

## Language decision

Rust is the runtime language because it gives the core explicit ownership and failure handling.
It produces a small single-binary CLI suitable for local-first distribution.
Its type system makes run, step, checkpoint, and policy states harder to confuse accidentally.
The standard library is enough for the initial executable, keeping the foundation dependency-light.
The tradeoff is a higher implementation cost, accepted in exchange for stronger systems credibility.
