# 0009. Run the DeepSeek adapter as a WASM component, as a trial

- Date: 2026-11-25
- Status: Implemented

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
3. The interface is written in WIT and follows the port. It is the package
   `m-gated:adapter@0.1.0` in `crates/adapter-kit/wit/adapter.wit`:
   - All functions and types are in one interface, `provider`. The component exports it. If the
     types were in a second interface, the component would import that interface, and rule 4
     refuses any import.
   - `describe()` is called once at start. It returns the fixed facts: provider name, upstream
     address, key check path, response headers to pass, and the APIs with their routes, upstream
     paths, error formats and token rules.
   - API keys never go into the component. `describe()` also returns the authentication header
     form (only `bearer` now), the key prefix and the prefixes to refuse. The host checks the key
     and builds the header.
   - `check-profile` checks the provider's rules for a model profile in the config. `validate`
     checks a request body. `approved-headers` gets the client headers as name and value pairs,
     and returns the names of the headers to forward. `read` reads a non-streamed response body
     and reports the usage. These are the port's methods. Each of them gets the API name that
     `describe()` gave, such as `deepseek_chat`.
   - A `stream-reader` resource is created for each streamed request. It has these methods:
     - `decoder` names the fields to extract from each event, as dotted paths such as
       `choices.*.delta.content`.
     - `accept` gets one event at a time, as those paths and their values. It says if the event
       ends the stream or is a provider error.
     - `provider-error` says if any event so far was a provider failure. The host asks for it
       during the stream.
     - `close` is called when the stream ends. It returns the usage to settle and whether the
       provider reported a failure, or an error if the stream ended without an end event.
     - `final-usage` returns the same usage as `close`. The host asks for it when it cannot wait
       for `close`, for example when the client disconnects after the end event but before the
       connection to the provider closes.
   - `SideRequest` is not in this interface. It comes later, for adapters that need it.
4. The host runs the component with wasmtime and the component model:
   - The component may have no imports, that is, no functions that the host lends to it. A
     component with any import is refused at start. So it cannot reach the network, files, the
     clock or random numbers.
   - The host uses wasmtime 48.0.9, with only the features that it needs: the runtime, the
     Cranelift compiler, the component model and async calls. wasmtime 48 is a long-term support
     release. The host turns off the wasmtime features for garbage collection (GC) and threads,
     because the adapters do not need them. Up to 48.0.8, a build without those features could
     stop the whole host process on some faults. 48.0.9 fixes this, so the host uses it although
     it is less than two weeks old. wasmtime 48 needs Rust 1.95, so the repository moves from Rust 1.93 to
     1.95.
   - In wasmtime, a store holds one copy of the component's memory and state. Each call gets its
     own store, created from an instance that was prepared once at start. A streamed request keeps
     one store until the stream ends. No state is shared between requests.
   - Each store has a memory limit (128 MiB). The memory limit is large enough to copy and read a
     16 MiB response body, the largest that a model profile may allow.
   - Each call also has a CPU limit. wasmtime counts the instructions that the component runs, in
     units called fuel. When a call has used its fuel, wasmtime stops it, and the call fails.
     Fuel counts instructions, not time, so the time that a limit allows depends on the
     instructions. The limit for a call depends on whether the call is async:
     - `validate` and `read` are async in the port. During these calls, the component gives
       control back to tokio after each 10 million units of fuel, so a long call does not hold a
       tokio worker thread. Each of these calls gets 20 billion units. This is 4 times the fuel
       that reading a 16 MiB response needs.
     - The other port methods are not async: `check-profile`, `approved-headers`, opening a stream
       reader, and the stream methods. The host runs each of these calls to the end at once, so it
       holds a tokio worker thread until the call ends. Each of these calls (for a stream, each
       call for one event) gets 10 million units. This is 100 times the fuel that these calls
       need, and the slowest instructions use it up in about 9 ms.
   - If the component traps (stops with a fault) or reaches a limit while checking a request, the
     gateway refuses the request with `500` and the error code `adapter_failed`, and does not send
     it. This is not the client's fault, so it is not a `400`. If the component fails while
     reading a response, the gateway treats it as a malformed response, so the request becomes
     `unknown`. If the component fails on one stream event, the host stops using that store. Every
     later event of that stream is then malformed, and the request becomes `unknown`.
   - `approved-headers` may return only names of headers that the client sent, and must include
     `content-type`. Other answers count as a failure of the component.
   - The config names the component file (`providers[].adapter`) and its SHA-256
     (`adapter_sha256`), as 64 lowercase hex digits. The gateway does not start if the file's hash
     differs, if the file is larger than 64 MiB, or if the component does not export
     `m-gated:adapter/provider@0.1.0`.
   - In this trial, the provider name and the API names that `describe()` returns must be ones
     that the built-in adapter has, and each API's token rule must match the built-in one. This is
     because the database checks, the ledger and the dashboard list these names, so a new name
     needs a host change. The other facts come from the component.
   - The built-in adapters are in a fixed list in the port. At start, the gateway loads the
     component and adds it to a second list for the whole process. This is called installing it.
     It happens before the model profiles are checked. Many places in the gateway find a provider
     by its name, and they find the installed component before the built-in adapter.
   - One process uses one implementation for each provider. The gateway reads its configuration
     once at start, but the tests load many configurations in one process. When a component is
     installed for a provider, each later configuration in the same process must name the same
     file hash for that provider. A configuration with another hash, or with no component, is
     refused.
5. The repository becomes a Cargo workspace:
   - `crates/adapter-kit` holds the code that both the host and the component need. It does not
     use tokio. In step 1 it gets the JSON request checker (`json.rs`) and the stream decoders
     (`stream.rs`): `JsonDecoder` reads selected fields from JSON, `SseDecoder` splits a stream
     into events, `Scalar` is a selected field's value, and `Event` is one stream event. The host
     keeps two thin async wrappers: one for the JSON checker and one for `SseDecoder::feed`. Both
     give control back to tokio during long work.
   - In step 2, `adapter-kit` also gets the values that pass between the port and an adapter.
     These values do not depend on HTTP, the config or the ledger:
     - `ApiError`: why a request check refused a request.
     - `RequestMetadata`: what a request check found in an accepted request: the model, the
       output limit and whether the client asked for a stream.
     - `ErrorFormat`: the shape of the error body that the gateway sends to the client.
     - `Step`: what one stream event told the reader. It says if the event ends the stream, if
       it is a provider error, and which output text it held.
     - `Usage`: the token counts that a response reports.
     - `ReadError`: why a response could not be read. There are three causes. The response has a
       wrong shape. A provider error event in a stream is larger than 64 KiB. The body is larger
       than the response size limit. If the response was a success, the request becomes
       `unknown`. If it was an error response, the request is settled at 0 USD (decision 0005).
     - `Profile`: the part of a model profile that an adapter sees. This is the model name, the
       provider features that the profile turns on (for example tools or thinking), the values
       of Anthropic's `anthropic-beta` header that it allows (other providers have no such
       header), the output limit, and the names of its prices (for
       example `input` and `cache_read`). An adapter checks which prices exist, not their amounts.
     - `Header`: one client header, as a name and a value.
   - The port uses `ApiError`, `RequestMetadata`, `ErrorFormat` and `Step` without change, so it
     re-exports them. The
     host converts the other values to its own types. `Usage` becomes the ledger's `FinalUsage`.
     `FinalUsage` stays in the ledger, because the ledger stores it as JSON and calculates prices
     from it.
   - `adapters/deepseek` holds the DeepSeek logic. The host file `src/providers/deepseek.rs` only
     connects that crate to the port.
   - Some code is shared between adapters. It moves to `adapter-kit`, and `openai.rs` uses it from
     there:
     - DeepSeek and OpenAI accept only keys that start with `sk-`.
     - DeepSeek and OpenAI refuse a model profile that allows `anthropic-beta` values, because
       they have no beta header. They also refuse a profile
       that turns on the cache but has no cache read price.
     - DeepSeek and OpenAI read a Chat Completions response and an error body in the same way.
     - DeepSeek, OpenAI and TypeSafe forward only one client header, a JSON `content-type`.
   - `adapters/deepseek` is an ordinary Rust crate, so its unit tests run natively.
   - The component is the crate `adapters/deepseek/component`. It only converts between the WIT
     types and the types of `adapters/deepseek`. Its code compiles only for wasm32, because on
     other targets the code that wit-bindgen makes has nothing to link to.
   - `scripts/build-adapter.sh` builds the component for `wasm32-unknown-unknown` with
     wit-bindgen 0.62.0 and wraps it with `wasm-tools component new`. It uses wasm-tools 1.259.0,
     which came out on the same day as wit-bindgen 0.62.0 and uses the same component encoding
     library (wit-component 0.259.0). The script refuses a component that
     imports anything. It writes the component's SHA-256 to a file next to the component.
   - The component contains the source file paths of some crates, in its error messages. The
     script replaces the absolute paths of the source directory and of the Cargo home directory
     (`~/.cargo`, where Cargo keeps the downloaded crates) with fixed names. So the file does not
     contain the builder's paths, and the same source on the same kind of machine gives the same
     file.
   - A build on another kind of machine (macOS, Linux amd64 or Linux arm64) gives a different
     file. Cargo adds a hash to the name of each function, and the kind of machine is part of
     that hash. When the names change, the compiler puts the functions in another order and
     makes other choices, so the code changes too.
   - A WASI target is not used. WASI is the standard set of system functions for WASM, so a
     build for it could add imports, and rule 4 refuses a component with any import.
6. The work goes in five steps. The full test suite runs after each step. All five steps are done.
   1. Create the workspace and `adapter-kit`, and make the host use it. Behavior does not change.
   2. Move the DeepSeek logic into `adapters/deepseek`, and link it into the host directly.
      Behavior does not change.
   3. Write the WIT, and build the DeepSeek component.
   4. Write the host loader: wasmtime, the port implementation, the limits, the failure handling
      and the config.
   5. Run the DeepSeek Postgres tests against the component. Measure the added latency per
      request, the cost of each event call, and the growth of the image size and the build time.
      The tests use the component when `TEST_DEEPSEEK_ADAPTER` names its file. The measurement
      suite is `tests/wasm_adapter_cost.rs`.
7. Release images contain the DeepSeek component, but the gateway uses it only when the config
   names it. Without `adapter`, the built-in adapter runs:
   - The component is at `/app/adapters/deepseek.wasm`. Its SHA-256 is in
     `/app/adapters/deepseek.wasm.sha256` and in the image label
     `m-gated.adapters.deepseek.sha256`. The files belong to root, so the gateway cannot change
     them. The image check (`scripts/test-package.sh`) makes sure that the file matches its
     SHA-256.
   - The release workflow builds the component once, on Linux amd64, in its first job
     (`verify`). The jobs that build the amd64 and arm64 images get that file. They give it to
     Docker with `--build-context adapter=<directory>`, so Docker does not run the Dockerfile's
     step that builds the component, and uses the given file. So both images contain the same
     file, and one `adapter_sha256` fits both. A local `docker build` runs that step and builds
     the component itself.
   - An operator can use another component in the same way: put the file in the container, with
     a volume or with an image built on the release image, and name its path and SHA-256 in the
     config.
   - The gateway does not download components from the network. If it did, it could not start
     while the download source was down. The SHA-256 in the config already makes sure that the
     file is the one the operator chose.
   - Components are not signed. The SHA-256 in the config names the file. A signature would not
     help if the config could not be trusted.

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

- All 35 DeepSeek Postgres tests pass with the component.
- Step 5 measured these costs, with release builds on one Apple silicon laptop. Times are
  medians:
  - Each call to the component takes about 20 µs more than the built-in adapter, because each
    call gets a new store and instance. A request check takes 24 µs instead of 1.4 µs for a small
    request, and 19 ms instead of 15 ms for an 8 MiB request. Reading a 4 KiB response takes
    82 µs instead of 33 µs.
  - Opening a stream reader takes 17 µs. Each stream event takes 2.7 µs instead of 0.6 µs.
  - Loading the component at start takes 0.13 s.
  - At the limit of a non-async call, the component holds a tokio worker thread for at most
    about 9 ms. An async call can use about 16 to 17 s of CPU time at its limit, but it does not hold a
    thread.
  - wasmtime is a large dependency. The gateway binary grows from 15.9 MB to 29.9 MB, and the
    release image from 171.7 MB to 190.3 MB. The Rust build in the release image takes 115 s
    instead of 67 s. Building the component from nothing takes 8 s. The component itself adds
    0.3 MB to the image.
  - The binary contains wasmtime whether a configuration names a component or not.
- The WIT package has a version. The host accepts only a component that exports version 0.1.0.
  A change to the interface needs a new version, and the host and the components change together.
- Steps 1 and 2 move much host code, but they do not change behavior, so the existing tests check
  them.
- Not in this trial:
  - Adding a provider with a new name. Today the database checks and the dashboard list the
    provider names, so a new name needs a migration and a dashboard change.
  - `SideRequest`.
  - Instructions and a template for operators who build their own components. Until a provider
    with a new name can be added, an operator's component can only replace a built-in adapter.
