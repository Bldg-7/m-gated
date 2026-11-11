import Anthropic from '@anthropic-ai/sdk';
import assert from 'node:assert/strict';

let phase = 'initial';
let diagnosis = 'none';
// 합성 loopback fixture만 사용한다. SDK/예외 원문·JWT·본문은 출력하지 않는다.
async function main() {
  assert.equal(process.versions.node, '22.22.2');
  const base = new URL(process.env.MGATED_GATEWAY_BASE);
  assert.equal(base.protocol, 'http:');
  assert.equal(base.hostname, '127.0.0.1');
  assert.ok(base.port);
  const expected = Number(process.env.MGATED_EXPECTED_STATUS);
  assert.ok([200, 400, 401, 403, 429, 502, 503, 504].includes(expected));
  const kind = process.env.MGATED_SDK_KIND;
  assert.ok(['messages', 'count_tokens', 'messages_stream', 'messages_stream_error', 'messages_stream_features', 'messages_stream_error_bounded', 'messages_stream_interrupted'].includes(kind));
  assert.equal(process.env.DO_NOT_TRACK, '1');
  let attempts = 0;
  let wireStatus;
  const client = new Anthropic({
    apiKey: process.env.MGATED_GATEWAY_TOKEN,
    baseURL: base.href.replace(/\/$/, ''),
    maxRetries: 0,
    timeout: 10_000,
    fetch: async (input, init) => {
      const target = new URL(input instanceof Request ? input.url : input);
      assert.equal(target.origin, base.origin);
      assert.ok(['/v1/messages', '/v1/messages/count_tokens', '/anthropic/v1/messages', '/anthropic/v1/messages/count_tokens'].includes(target.pathname));
      assert.equal(init.method, 'POST');
      attempts += 1;
      const response = await fetch(input, { ...init, redirect: 'manual' });
      wireStatus = response.status;
      return response;
    },
  });
  let actual = 200;
  let streamError = false;
  let starts=0,stops=0,events=0,iterationEnded=false;
  try {
    const request = { model: 'synthetic-text', messages: [{ role: 'user', content: 'synthetic SDK prompt' }] };
    if (kind === 'messages_stream_features') {
      const stream = client.messages.stream({ ...request, max_tokens: 100 });
      const result = await stream.finalMessage();
      assert.equal(result.id, 'msg_fixture');
      assert.equal(result.model, 'synthetic-text');
      assert.equal(result.stop_reason, 'tool_use');
      assert.equal(result.usage.input_tokens, 100);
      assert.equal(result.usage.output_tokens, 20);
      assert.equal(result.usage.cache_read_input_tokens, 4);
      assert.equal(result.usage.cache_creation_input_tokens, 8);
      assert.deepEqual(result.content, [
        { type: 'thinking', thinking: 'synthetic thinking', signature: 'synthetic-signature' },
        { type: 'text', text: '{"answer":"synthetic"}', citations: null },
        { type: 'tool_use', id: 'tool_2', name: 'synthetic_tool', input: { value: 1 } },
      ]);
    } else if (kind === 'messages_stream_interrupted') {
      // SDK0.78.0의 low-level iteration은 일부 AbortError에서 terminal 없이 끝난다.
      // high-level 누적기는 동일 HTTP 응답의 message_stop 없는 finalMessage를 거부해야 한다.
      const stream = client.messages.stream({ ...request, max_tokens: 100 });
      stream.on('streamEvent', (event) => {
        events++;
        if (event.type === 'message_start') starts++;
        if (event.type === 'message_stop') stops++;
      });
      let rejected = false;
      try {
        await stream.finalMessage();
      } catch (error) {
        assert.ok(error instanceof Anthropic.AnthropicError);
        rejected = true;
      }
      assert.equal(rejected, true);
      assert.equal(stops, 0);
      streamError = true;
    } else if (kind.startsWith('messages_stream')) {
      const stream = await client.messages.create({ ...request, max_tokens: 100, stream: true });
      for await(const event of stream){events++;if(event.type==='message_start'){starts++;assert.equal(event.message.model,'synthetic-text');assert.equal(event.message.usage.input_tokens,100);}if(event.type==='message_delta')assert.equal(event.usage.output_tokens,20);if(event.type==='message_stop')stops++;}
      iterationEnded=true;
      assert.notEqual(kind, 'messages_stream_interrupted');
      assert.equal(starts,1);assert.equal(stops,1);
    } else {
    const result = kind === 'count_tokens'
      ? await client.messages.countTokens(request)
      : await client.messages.create({ ...request, max_tokens: 100 });
    if (kind === 'messages') {
      assert.equal(result.model, 'synthetic-text');
      assert.equal(result.usage.input_tokens, 100);
      assert.equal(result.usage.output_tokens, 20);
    } else {
      assert.equal(result.input_tokens, 12);
    }
    }
  } catch (error) {
    phase = 'stream_error_received';
    const classification = error instanceof assert.AssertionError ? 'assertion' : error instanceof Anthropic.APIError ? 'api' : error instanceof TypeError ? 'type' : error.name === 'AbortError' ? 'abort' : 'other';
    diagnosis = `class=${classification} starts=${starts} stops=${stops} events=${events} ended=${Number(iterationEnded)} status=${wireStatus ?? -1} attempts=${attempts}`;
    if (kind === 'messages_stream_interrupted') {
      throw error;
    } else if (kind === 'messages_stream_error_bounded') {
      phase = 'bounded_api_error';
      assert.ok(error instanceof Anthropic.APIError);
      phase = 'bounded_status';
      assert.equal(error.status, undefined);
      phase = 'bounded_shape';
      assert.equal(error.error.type, 'error');
      assert.equal(error.error.error.type, 'overloaded_error');
      phase = 'bounded_message_length';
      assert.equal(Buffer.byteLength(error.error.error.message), Number(process.env.MGATED_EXPECTED_NATIVE_ERROR_BYTES));
      streamError = true;
    } else {
    if (!(error instanceof Anthropic.APIError)) throw new Error('SDK assertion or network failure');
    if(kind==='messages_stream_error'){assert.equal(error.status,undefined);assert.equal(error.error.type,'error');assert.equal(error.error.error.type,'overloaded_error');streamError=true;}else{actual = error.status;}
    }
  }
  phase = 'stream_error_flag';
  if(['messages_stream_error', 'messages_stream_error_bounded', 'messages_stream_interrupted'].includes(kind))assert.equal(streamError,true);
  phase = 'final_status';
  assert.equal(actual, expected);
  phase = 'attempts';
  assert.equal(attempts, 1);
  phase = 'wire_status';
  assert.equal(wireStatus, expected);
  process.stdout.write(`SDK_CASE_OK kind=${kind} status=${actual} attempts=${attempts} maxRetries=0\n`);
}
main().catch((error) => {
  const actual = typeof error.actual === 'number' ? error.actual : -1;
  const expected = typeof error.expected === 'number' ? error.expected : -1;
  process.stderr.write(`SDK_DIAGNOSTIC phase=${phase} actual=${actual} expected=${expected} ${diagnosis}\n`);
  process.stderr.write('SDK_CASE_FAILED: 합성 SDK 계약 검증에 실패했습니다.\n');
  process.exitCode = 1;
});
