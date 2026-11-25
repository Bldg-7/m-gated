# 0009. Run the DeepSeek adapter as a WASM component, as a trial

- Date: 2026-11-25
- Status: Accepted, not implemented

## Context

Decision 0007 put all provider-specific code behind one port. The next goal is to let an operator
add a provider without changing and rebuilding the gateway. The operator would supply an adapter
as a file, and the gateway would load it at start.

An adapter decides how a request is checked and how the usage in a response is read, so a wrong
or hostile adapter could make the ledger wrong. A file that the gateway loads must therefore run
with no access to anything outside its inputs: no network, no files, and no clock. It must also
have limits on memory and CPU time. A WebAssembly (WASM) component meets these needs. Its host
decides what the component can reach, and the wasmtime runtime can limit its memory and CPU time.

Before the gateway accepts adapters from outside, one existing adapter must run as a WASM
component and pass the existing tests. The DeepSeek adapter is the trial:

- Production does not use DeepSeek.
- It has request checks, non-streamed reading and stream reading, and 35 Postgres tests.
- It does not need the extra count request (`SideRequest`) that the Anthropic adapter uses.

## Decision

As a trial, the DeepSeek adapter is built as a WASM component, and the gateway loads it at start
and puts it behind the port. In the rest of this record, "component" means this WASM file, and
"host" means the gateway that runs it.

1. The DeepSeek adapter is built as a WASM component. The gateway loads it at start and puts it
   behind the port. The provider name stays `deepseek`, so the database checks and the dashboard
   do not change. Only the implementation changes.
2. The boundary is the port of decision 0007:
   - The host (the gateway) keeps HTTP, size checks, settlement, user blocks and deadlines. It
     also keeps reading the stream bytes (`SseDecoder`).
   - The component gets the request body and the non-streamed response body whole. For a stream,
     it gets one event at a time, and each event holds only the fields that the component named
     in `decoder` (rule 3). The component is called once per event, not once per byte.
3. The interface is written in WIT and follows the port:
   - `describe()` is called once at start. It returns the fixed facts: provider name, upstream
     address, key check path, response headers to pass, and the APIs with their routes, upstream
     paths, error formats and token rules.
   - `check-profile` checks the provider's rules for a model profile in the config. `validate`
     checks a request body. `approved-headers` picks the client headers to forward. `read` reads
     a non-streamed response body and reports the usage. These are the port's methods.
   - A `stream-reader` resource is created for each streamed request. It has `decoder` (the
     fields to extract), `accept`, `final-usage`, `provider-error` and `close`.
   - `SideRequest` is not in this interface. It comes later, for adapters that need it.
4. The host runs the component with wasmtime and the component model:
   - The component may have no imports, that is, no functions that the host lends to it. A
     component with any import is refused at start. So it cannot reach the network, files, the
     clock or random numbers.
   - In wasmtime, a store holds one copy of the component's memory and state. Each request gets
     its own store, created from an instance that was prepared once at start. No state is shared
     between requests.
   - Each store has a memory limit and a CPU limit (fuel). Long calls yield to the async runtime.
   - If the component traps (stops with a fault) or reaches a limit while checking a request, the
     request is refused before it is sent. If that happens while reading a response, the gateway
     treats it as a malformed response, so the request becomes `unknown`.
   - The config names the component file (`providers[].adapter`) and its SHA-256
     (`adapter_sha256`). The gateway does not start if the file's hash differs.
5. The repository becomes a Cargo workspace:
   - `crates/adapter-kit` holds the code that both the host and the component need. It does not
     use tokio. In step 1 it gets the JSON request checker (`json.rs`) and the stream decoders
     (`stream.rs`): `JsonDecoder` reads selected fields from JSON, `SseDecoder` splits a stream
     into events, `Scalar` is a selected field's value, and `Event` is one stream event. The host
     keeps two thin async wrappers, for the JSON checker and for `SseDecoder::feed`, that give
     control back to tokio during long work. The types that an adapter returns, such as
     `FinalUsage`, move in step 2.
   - `adapters/deepseek` holds the DeepSeek logic. DeepSeek and OpenAI Chat now share the code that
     reads a Chat Completions response and an error body. That shared code moves to
     `adapter-kit`, and `openai.rs` uses it from there. `adapters/deepseek` is an ordinary Rust
     crate, so its unit tests run natively.
   - The component is built for `wasm32-unknown-unknown` with wit-bindgen and wrapped with
     `wasm-tools component new`. Building for a WASI target instead could add imports of WASI,
     the standard set of system functions for WASM.
6. The work goes in five steps. The full test suite runs after each step.
   1. Create the workspace and `adapter-kit`, and make the host use it. Behavior does not change.
   2. Move the DeepSeek logic into `adapters/deepseek`, and link it into the host directly.
      Behavior does not change.
   3. Write the WIT, and build the DeepSeek component.
   4. Write the host loader: wasmtime, the port implementation, the limits, the failure handling
      and the config.
   5. Run the DeepSeek Postgres tests against the component. Measure the added latency per
      request, the cost of each event call, and the growth of the image size and the build time.
7. After step 5, the user decides whether release images use the WASM DeepSeek adapter by
   default or only in tests.

## Rejected alternatives

**Run adapters in a separate process (a sidecar).** Any language would work. But each stream
event would need a call between processes, a stopped sidecar would leave requests `unknown` and
block users, and the prompts would go to another process.

**Embed a script language (Rhai or Lua).** No compile step would be needed. But the strict request
checks (300 to 1,000 lines for each adapter now) are hard to write and slow to run in a script
language.

**Describe adapters in the config file.** Nothing would run, so this would be the safest. But it
fits only OpenAI-compatible chat APIs. To describe each provider's usage fields, allowed fields and
error formats, the config would turn into a small language.

**Start with Anthropic.** Production uses only Anthropic, so a mistake would reach real
settlements. Anthropic also needs `SideRequest`.

## Consequences

- wasmtime is a large dependency. Step 5 measures its effect on the build time and the image size.
- The WIT package has a version. The host refuses a component whose major version differs.
- Steps 1 and 2 move much host code, but they do not change behavior, so the existing tests check
  them.
- Not in this trial:
  - Adding a provider with a new name. Today the database checks and the dashboard list the
    provider names, so a new name needs a migration and a dashboard change.
  - `SideRequest`.
  - How operators ship their own components.
