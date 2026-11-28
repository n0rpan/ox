# Hosted OpenCode route controller

The route controller is a local proof for stock OpenCode 1.19.6. The host opens it only through an explicit worker call in the fixture. No client advertises an OpenCode hosted capability. The standalone `ox opencode` launcher has its own lifecycle and configuration.

## Authority

- The host selects the canonical project directory and exact OX API model alias. A relay for that pair uses the existing Rust gateway and the same route catalog as the standalone launcher. The native project, state and config paths and the pinned version are verified before opening a session.
- Each owner has a separate profile and registry. A process-lifetime lock holds the registry even if the stock server exits. The server helper separately locks its profile. The registry binds OX-issued UUIDs to stock session IDs, and its version and owner fingerprint are checked on open. Caller-supplied stock session IDs do not open a session.
- The registry persists one monotonically increasing send sequence per session and its current receipt. A retry of the current sequence is matched to the original prompt digest. Older, skipped or conflicting sequences are rejected. The journal records a pending send before the native asynchronous POST. A lost reply leaves an unresolved receipt. Resubmission does not replay that POST. The existing host operation-receipt layer should preserve its own request and draft identities and translate a new send into the next sequence.
- Tool permissions use a session-level native ruleset, including when the native project or selected agent allows tools. Once and reject are the controller's two choices. The first controller disables native questions and delegated tasks because those use separate request or session ownership. The host supplies a single-use token that is scoped to the pending ask and retired when the turn ends. An ambiguous native answer is never retried using the old token. Pending approvals and commands stay in memory.
- Stop is scoped to the current receipt. A pending `prompt_async` can acknowledge before the native run is registered. The controller waits for stock to report an active run or produce the corresponding assistant record before relying on abort. A timeout leaves the receipt fenced. The host then requires native idle and resolves any leftover permission for that exact session. On a new native child the host reconciles an interrupted receipt against durable native history before allowing work again.

## Reconciliation

The native SSE stream promptly announces changes. It is volatile and can drop events. Subscribe before the first read, and use authoritative native status, history and permission reads after reconnect. The controller emits OX `SessionView`, `Timeline` and `AskView` from those reads. Timeline identities retain the stock message and part identity so repeated snapshots or paging can replace and deduplicate the same records. The newest snapshot is bounded to 50 native messages, and a separate bounded page call returns an opaque older cursor supplied by stock.

A terminal assistant must belong to the exact submitted user message. With default stock behavior, rejecting a pending tool can end a turn with the pinned tool rejection marker. Host restart retires unfinished approval tokens and preserves already completed transcript data. An immediate Stop can leave the original user message in history. Stock may combine it with the next adjacent user message in the upstream conversation, while the controller still reconciles the new assistant to the new receipt.

The worker currently provides a synchronous local controller. The next bridge should use one SSE stream for the route, coalesce text updates and apply exact reads at approval, terminal, reconnect and history boundaries. Benchmark that bridge before exposing continuous streaming. The existing host device protocol should project the controller through a genuine runtime branch and a negotiated capability. The runtime startup worker and Store event mapping in #56 are the extension points. Keep #66's canonical writer/move boundary, the host operation receipts, and permission dispatch in control. Phone and desktop should use the same canonical session ID throughout a locally owned conversation. Cross-host move remains disabled until a separate OpenCode history transfer ownership contract is verified.

Stock can execute project-configured plugin and MCP code outside the native tool approval interface. Hosting through this controller assumes an explicitly trusted project and local user. The pinned ShellTool copies its process environment to an approved command, including the native server's Basic credential and the route-local relay token under the current launcher. Project plugins run inside the stock process. Public activation requires a decision about project configuration and an internal-credential boundary exercised across every enabled subprocess launcher. It also requires desktop and phone lifecycle and reconnect testing and review of the capability and setup flow. Additional verification should exercise `prompt_async` failures before a durable assistant record and the current bounded signal for ambiguous cancellation.

## Verification

Build the opt-in test executable.

```sh
cargo test --manifest-path desktop/Cargo.toml -p ox-host --test opencode_stock_runtime --no-run
```

Run it against a pinned official stock binary and an OX CLI containing the managed host helper.

```sh
python3 desktop/tests/opencode_host_runtime.py STOCK OX TEST_BINARY
```

The fixture uses a mock localhost Messages provider. Its fixture project allows Bash and file read, so a native approval appears only if the host session's permissions actually override the permissive defaults. The test exercises send receipt duplication, stream loss with transcript recovery, history pages, exact approval matching, tool allow and reject, stop during a tool request and immediately after send, session restart and a provider failure. Only fixture files are read or written.
