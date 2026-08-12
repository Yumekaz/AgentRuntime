# AgentRT

AgentRT is a local-first execution runtime for multi-step LLM work.

It treats an agent run like a durable workflow:

- steps are persisted in SQLite and can resume after a process kill;
- filesystem tools are typed, policy-controlled, and audited;
- model plans are validated before they can mutate a workspace;
- every important decision is recorded in an exportable audit bundle;
- deterministic evals run through the same executor and fail CI on regression.

The central proof is simple: kill a run after a side effect is durably recorded, resume it, and show that the effect is not silently repeated.

## Quick start

Requirements: Rust stable and Cargo.

```powershell
cargo test
cargo run -- version
cargo run -- eval
```

Run the complete recovery demonstration:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\demo.ps1
```

The demo performs a model-plan workflow, kills it during tool execution, resumes the same run, exports the audit trail, proves a sandbox denial, and intentionally breaks an eval. The final eval failure is expected; the script itself exits successfully only when it observes that failure correctly.

## What is implemented

### Durable execution

Runs and steps are persisted in SQLite. Each completed step gets a checkpoint and an idempotency key. Recovery reconstructs persisted tool specifications, resumes pending work, and emits a deduplication event when a recorded result is encountered again.

### Constrained tools

The runtime exposes typed tools rather than arbitrary shell access:

- `read_file`
- `write_file`
- `list_dir`
- `run_tests`

Paths must remain inside the declared workspace. Traversal, symlink writes, denied tools, read-only writes, and oversized writes are rejected and audited. `run_tests` is restricted to offline Cargo tests, has a bounded timeout, uses a separate target directory, and removes API credentials from its child environment.

The filesystem policy is not a Windows security boundary. It constrains AgentRT’s typed tools; it does not jail arbitrary processes or network traffic.

### Model-driven planning

The reference `repo-fix-model` workflow asks a provider for a versioned JSON plan. AgentRT validates the schema, restricts the actions to the workspace policy, persists the plan, and only then executes reads, writes, verification, and gates.

Available providers:

- deterministic `FakeProvider` for tests and replayable demos;
- Gemini `generateContent`;
- OpenAI-compatible chat-completions endpoints.

Provider requests are audited with structural redaction. Response content is represented by hashes in the audit trail. Credentials are loaded from environment configuration and are never committed.

## Useful commands

Create a small private workspace:

```powershell
New-Item -ItemType Directory -Force .agentrt\workspace | Out-Null
Set-Content .agentrt\workspace\input.txt "status=broken"
```

Run and recover a deterministic workflow:

```powershell
cargo run -- run --store .agentrt\run.db --steps 4 --crash-after 2
cargo run -- status --store .agentrt\run.db --run-id <run-id>
cargo run -- resume --store .agentrt\run.db --run-id <run-id>
cargo run -- audit --store .agentrt\run.db --run-id <run-id>
cargo run -- audit --store .agentrt\run.db --run-id <run-id> --export .agentrt\bundle
```

Use a typed tool with an explicit policy:

```powershell
cargo run -- tool read --workspace .agentrt\workspace --path input.txt --policy fixtures\policy.json
cargo run -- tool write --workspace .agentrt\workspace --path output.txt --contents safe --policy fixtures\policy.json
cargo run -- tool run-tests --workspace . --policy fixtures\policy.json
```

Run the deterministic reference agent:

```powershell
cargo run -- agent repo-fix --workspace .agentrt\workspace --path input.txt --find status=broken --replace status=fixed --store .agentrt\agent.db
```

Run the model-plan workflow from a recorded response:

```powershell
cargo run -- agent repo-fix-model `
  --workspace .agentrt\workspace `
  --prompt repair-fixture `
  --response-file .\fixtures\evals\model-plan\plan.json `
  --store .agentrt\model-agent.db
```

## Live Gemini usage

Put the key in the ignored local `.env` file, never in source or command history:

```text
GEMINI_API_KEY=your-key
GEMINI_MODEL=gemini-3.5-flash
```

Then run one model-planning call:

```powershell
cargo run -- agent repo-fix-model `
  --provider gemini `
  --model gemini-3.5-flash `
  --workspace .agentrt\workspace `
  --prompt "Return only a valid JSON repair plan for input.txt" `
  --store .agentrt\gemini.db
```

Live providers are optional. Tests and CI use deterministic responses and do not require an API key.

## Evals and CI

The built-in suite covers:

- the deterministic repo-fix workflow;
- model-plan execution through gates;
- malformed plan rejection;
- traversal rejection;
- read-only policy denial;
- intentional regression failure.

```powershell
cargo run -- eval
cargo run -- eval --break   # must exit non-zero
```

GitHub Actions runs the locked Rust test suite and the eval command on Linux. Local Windows tests are also supported; toolchain/linker availability can affect native Windows builds.

## Audit bundles

Exported bundles contain run metadata, ordered JSONL events, and hashed evidence. The event stream records run lifecycle, checkpoints, model requests/responses, tool invocations/results, sandbox denials, plan validation, and gate outcomes.

This makes it possible to answer what the agent did, in what order, with which tools, under which policy, and where it failed—without reading the implementation first.

## Architecture

```text
CLI
 └─ Run manager
     ├─ Durable SQLite store
     ├─ Step executor and recovery
     ├─ Model provider adapter
     ├─ Tool router → workspace policy
     ├─ Gate engine
     └─ Append-only audit events → export bundle
            └─ Eval harness uses the same executor
```

## Scope and limitations

AgentRT is a single-machine runtime prototype focused on durable, inspectable execution. It is not a hosted agent platform, IDE, multi-node workflow cluster, container boundary, or model-training system.

It does not claim exactly-once behavior for arbitrary external side effects, bit-identical outputs across LLM providers, or a security boundary around unrestricted host processes. Those boundaries are deliberate and visible in the audit and documentation.

## License

MIT. See [LICENSE](LICENSE).
