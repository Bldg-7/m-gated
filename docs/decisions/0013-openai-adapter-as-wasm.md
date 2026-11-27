# 0013. Offer the OpenAI adapter as a WASM component, next to the built-in adapter

- Date: 2026-11-27
- Status: Implemented

## Context

This decision builds the OpenAI adapter as a WebAssembly (WASM) component, as decision 0009 did for
DeepSeek. The built-in OpenAI adapter stays. The component interface changes to version 0.3.0, so
that the component can use the model catalog and gets the values that the OpenAI checks need.

Decision 0009 built the DeepSeek adapter as a WASM component, as a trial. The gateway loads a
component at start, and the component runs with no access to the network, files or the clock, with
limits on memory and CPU time. In the rest of this record, "component" means such a WASM file, and
"host" means the gateway that runs it. The trial passed: all 35 DeepSeek Postgres tests pass with
the component. Decision 0012 then changed the interface to `m-gated:adapter@0.2.0`.

The interface (`crates/adapter-kit/wit/adapter.wit`) has two records that describe a provider. The
host calls `describe()` once at start, and it returns a `description` record: the provider name,
the upstream address, the key rules and the list of APIs. Each API in that list is an `api-info`
record: its routes, upstream path, error format, usage categories and whether it streams.

OpenAI is the next provider to try, because it tests more of the interface than DeepSeek:

- It has three APIs: `responses`, `openai_chat` (Chat Completions) and `embeddings`. One process
  uses one implementation for each provider (decision 0009, rule 4), so a component must serve
  all three.
- The Responses stream has more event types than the Chat Completions stream.
- Its models and prices can come from the model catalog that admins approve. Two facts about a
  provider decide this: each API says whether it can use the catalog (`catalog`), and the provider
  gives the name that the models.dev price list uses for it (`models_dev_name`). The interface does
  not carry these two facts, so a component cannot use the catalog today.
- Its checks use values that the interface does not pass to a component. Three come from the model
  profile: the field that a Chat Completions request uses for its output limit (`max_tokens` or
  `max_completion_tokens`), and the vector dimensions and encodings (`float`, `base64`) that an
  Embeddings profile allows. And reading an Embeddings response needs the request body, to check
  that the response has one vector of the right size for each input.

The OpenAI logic is in host files now (`src/providers/openai.rs`, `openai_chat.rs` and
`embeddings.rs`, about 2,800 lines). Code that DeepSeek and OpenAI share is already in
`crates/adapter-kit`. One part of the OpenAI checks is not: `responses` and `openai_chat` accept
images in the request body, and the gateway checks that each one is a valid PNG within the size
limits. This PNG check is host code in the Anthropic adapter. The host runs requests on tokio, its
async runtime, which runs many requests on a few threads. A long check must pause now and then so
that other requests on the same thread can run, and the PNG check does this. Inside a component,
the fuel limit of decision 0009 keeps a long call from holding a thread.

## Decision

1. **The OpenAI logic moves to its own crate.** `adapters/openai` holds the request checks, the
   response reading and the stream reading of the three APIs. It depends only on `adapter-kit`,
   like `adapters/deepseek`. The host files only connect that crate to the port. Behavior does not
   change. The built-in adapter stays, and it runs when the config names no component.
2. **The PNG check moves to `adapter-kit`.** Anthropic and OpenAI use the same check. The host
   keeps a thin async wrapper that pauses for tokio during long work, as it already does for the
   JSON request checker in `adapter-kit`.
3. **The interface becomes `m-gated:adapter@0.3.0`.**
   - `api-info` gets `catalog`: whether the API can use the model catalog.
   - `description` gets `models-dev-name`: the provider's name in the models.dev price list, or
     none.
   - `profile` gets `chat-output-limit-field`, `embedding-dimensions` (the default and the maximum)
     and `embedding-encodings`. They are empty or none for the other APIs.
   - `read`, the function that reads a non-streamed response, also gets the client's request body.
     So for each non-streamed response, the host copies the request body into the component a
     second time (the first time is the request check). This applies to DeepSeek too.
   - Like the usage categories (decision 0012), the values of `catalog` and `models-dev-name` must
     equal the built-in adapter's values, or the gateway does not start. Today the catalog sync and
     the dashboard handle only the built-in providers. The interface carries the two values anyway,
     so that a later decision that allows new providers can drop this check without another
     interface change.
   - The host accepts only version 0.3.0. The DeepSeek component is rebuilt for it.
4. **The OpenAI component.** The crate `adapters/openai/component` only converts between the WIT
   types and the types of `adapters/openai`, like the DeepSeek component. `scripts/build-adapter.sh`
   builds both components with the same tools and checks: no imports, and the export
   `m-gated:adapter/provider@0.3.0`. It writes `target/adapters/openai.wasm` and its SHA-256 next to
   the DeepSeek files.
5. **Configuration.** An operator turns on the component as for DeepSeek: `providers[].adapter` and
   `adapter_sha256` on the `openai` provider. The limits of decision 0009 (128 MiB memory, fuel per
   call, `500 adapter_failed` when a request check fails) apply unchanged.
6. **Tests.** The Postgres test files for the OpenAI APIs (`responses_postgres` and
   `chat_embeddings_postgres`) run against the component when the environment variable
   `TEST_OPENAI_ADAPTER` names its file. `TEST_DEEPSEEK_ADAPTER` does the same for DeepSeek.
   `tests/wasm_adapter.rs` gets OpenAI cases: the component gives the same results as the built-in
   adapter, and a component with a wrong `catalog` or `models-dev-name` is refused.
7. **Release images.** The release workflow publishes Docker images of the gateway, one for Linux
   amd64 and one for Linux arm64. They contain the OpenAI component next to the DeepSeek component:
   `/app/adapters/openai.wasm`, its SHA-256 in `/app/adapters/openai.wasm.sha256`, and the image
   label `m-gated.adapters.openai.sha256`. The workflow builds both components once, on Linux
   amd64, and gives the same files to both images, as decision 0009 describes for DeepSeek.
8. **Steps.** The full test suite runs after each step.
   1. Move the PNG check to `adapter-kit`, and the OpenAI logic to `adapters/openai`. Link the crate
      into the host directly. Behavior does not change.
   2. Change the interface to 0.3.0, and rebuild the DeepSeek component.
   3. Build the OpenAI component, and run the OpenAI tests against it.
   4. Add the component to the release images.

## Rejected alternatives

**Take `catalog` and `models-dev-name` from the built-in adapter, and leave the interface at
0.2.0.** No interface change would be needed now. But the component would not describe its
provider fully, and a later decision that allows new providers would have to change the interface
then.

**Build only `openai_chat` as a component.** It is the API closest to DeepSeek's. But the built-in
adapter would still serve `responses` and `embeddings`, so one provider would have two
implementations, which rule 4 of decision 0009 does not allow. The trial would also not test the
Responses stream.

**Move Anthropic instead.** Production uses Anthropic, so a mistake would reach real settlements.
Anthropic also needs `SideRequest`, which the interface does not have (decision 0009).

## Consequences

- The WASM interface changes in a breaking way. A component built for version 0.2.0 is refused at
  start. The DeepSeek component's SHA-256 in release images changes.
- Step 1 moves much host code but does not change behavior, so the existing tests check it. Step 2
  changes the interface as rule 3 describes, and the host code that reads it. It does not change how
  the built-in adapters check requests or read responses.
- The release images grow by the size of the OpenAI component.
- `README.md` describes the OpenAI component, the interface version and `TEST_OPENAI_ADAPTER`.
