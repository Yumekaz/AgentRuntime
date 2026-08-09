# AgentRT

AgentRT is a systems-grade execution runtime for multi-step LLM work:

> durable across crashes, constrained when it uses tools, reconstructable from an audit trail, and measurable through regression evals.

This repository is being built around one proof: kill a run mid-execution, restart it, and show that it resumes from the last durable checkpoint without silently repeating completed work.

## Current status

The runtime can create deterministic pure-step runs in SQLite, persist a checkpoint after each completed step, report progress, resume an interrupted run without re-executing completed steps, display the ordered audit event stream, export a hashed audit bundle, and enforce a typed filesystem tool policy. Model calls and eval cases are not implemented yet.

## Run it

```text
cargo run -- version
cargo test
cargo run -- run --store .agentrt/demo.db --steps 4
```

To exercise recovery, stop after two completed steps and resume the same run:

```text
cargo run -- run --store .agentrt/demo.db --steps 4 --crash-after 2
cargo run -- status --store .agentrt/demo.db --run-id <id>
cargo run -- resume --store .agentrt/demo.db --run-id <id>
cargo run -- audit --store .agentrt/demo.db --run-id <id>
cargo run -- audit --store .agentrt/demo.db --run-id <id> --export .agentrt/bundle
cargo run -- tool list --workspace ./fixtures/workspace
cargo run -- tool read --workspace ./fixtures/workspace --path input.txt
cargo run -- tool write --workspace ./fixtures/workspace --path output.txt --contents "safe"
```

Filesystem tools accept only relative paths within the declared workspace. Raw shell execution is not exposed by this interface.

## Scope

The initial runtime targets a single local machine. It is not a hosted agent platform, an IDE, a multi-node workflow cluster, or a model-training project.

See [PROJECT.md](PROJECT.md) for the product contract, acceptance criteria, and implementation boundaries.

## Language decision

Rust is the runtime language because it gives the core explicit ownership and failure handling.
It produces a small single-binary CLI suitable for local-first distribution.
Its type system makes run, step, checkpoint, and policy states harder to confuse accidentally.
The standard library is enough for the initial executable, keeping the foundation dependency-light.
The tradeoff is a higher implementation cost, accepted in exchange for stronger systems credibility.
