import OpenAI from 'openai';
import assert from 'node:assert/strict';

async function main() {
  assert.equal(process.versions.node, '22.22.2');
  assert.equal(process.env.DO_NOT_TRACK, '1');
  const base = new URL(process.env.MGATED_GATEWAY_BASE);
  assert.equal(base.protocol, 'http:');
  assert.equal(base.hostname, '127.0.0.1');
  assert.ok(base.port);
  const expected = Number(process.env.MGATED_EXPECTED_STATUS);
  assert.ok([200, 400, 401, 403, 429, 502, 503, 504].includes(expected));
  const failedStream = process.env.MGATED_SDK_KIND === 'responses_stream_failed';
  const stream = failedStream || process.env.MGATED_SDK_KIND === 'responses_stream';
  const incomplete = process.env.MGATED_NATIVE_STATUS === 'incomplete';
  let attempts = 0;
  let wireStatus;
  const client = new OpenAI({
    apiKey: process.env.MGATED_GATEWAY_TOKEN,
    baseURL: base.href.replace(/\/$/, ''),
    maxRetries: 0,
    timeout: 10_000,
    logLevel: 'off',
    fetch: async (input, init) => {
      const target = new URL(input instanceof Request ? input.url : input);
      assert.equal(target.origin, base.origin);
      assert.equal(target.pathname, '/openai/v1/responses');
      assert.equal(init.method, 'POST');
      attempts += 1;
      const response = await fetch(input, { ...init, redirect: 'manual' });
      wireStatus = response.status;
      return response;
    },
  });
  let actual = 200;
  try {
    const request = { model: 'synthetic-text', input: 'synthetic SDK request', store: false, max_output_tokens: 100, stream };
    if (process.env.MGATED_SDK_KIND === 'responses_bad_store') delete request.store;
    if (stream) {
      let created = 0;
      let terminal = 0;
      let failures = 0;
      const responseStream=await client.responses.create(request);
      for await (const event of responseStream) {
        if(failedStream && ['response.failed','error'].includes(event.type)) {
          failures++;
          assert.equal(event.type,process.env.MGATED_NATIVE_STATUS);
          responseStream.controller.abort();
          break;
        }
        if (event.type === 'response.created') created += 1;
        if (['response.completed', 'response.incomplete'].includes(event.type)) {
          terminal += 1;
          assert.equal(event.type, incomplete ? 'response.incomplete' : 'response.completed');
          assert.equal(event.response.status, incomplete ? 'incomplete' : 'completed');
          assert.equal(event.response.usage.input_tokens, 100);
          assert.equal(event.response.usage.input_tokens_details.cached_tokens, 40);
          assert.equal(event.response.usage.output_tokens, 20);
          assert.equal(event.response.usage.output_tokens_details.reasoning_tokens, 10);
          if (incomplete) assert.equal(event.response.incomplete_details.reason, 'max_output_tokens');
        }
      }
      assert.equal(created, 1);
      assert.equal(terminal, failedStream ? 0 : 1);
      assert.equal(failures, failedStream ? 1 : 0);
    } else {
      const result = await client.responses.create(request);
      assert.equal(result.model, 'synthetic-text');
      assert.equal(result.status, incomplete ? 'incomplete' : 'completed');
      assert.equal(result.usage.total_tokens, 120);
      if (incomplete) assert.equal(result.incomplete_details.reason, 'max_output_tokens');
    }
  } catch (error) {
    assert.ok(error instanceof OpenAI.APIError);
    actual = error.status;
    assert.equal(error.error?.code?.startsWith('gateway_'), true);
    assert.equal(error.error?.message?.includes('synthetic-company'), false);
  }
  assert.equal(actual, expected);
  assert.equal(wireStatus, expected);
  assert.equal(attempts, 1);
  console.log(`SDK_RESPONSES_OK status=${actual} attempts=1 maxRetries=0 stream=${stream} incomplete=${incomplete}`);
}
main().catch(() => {
  // SDK 오류 원문·JWT·대화 본문은 어떤 실패 경로에서도 출력하지 않는다.
  console.error('SDK_RESPONSES_FAILED');
  process.exitCode = 1;
});
