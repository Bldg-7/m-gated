import OpenAI from 'openai';
import assert from 'node:assert/strict';

async function main() {
  assert.equal(process.versions.node, '22.22.2');
  assert.equal(process.env.DO_NOT_TRACK, '1');
  const base = new URL(process.env.MGATED_GATEWAY_BASE);
  assert.equal(base.protocol, 'http:');
  assert.equal(base.hostname, '127.0.0.1');
  assert.ok(base.port);
  const kind = process.env.MGATED_SDK_KIND;
  const expected = Number(process.env.MGATED_EXPECTED_STATUS);
  let attempts = 0;
  let wireStatus;
  const feature = kind.startsWith('tool_') || kind.startsWith('json_');
  const tool = kind.startsWith('tool_');
  const client = new OpenAI({
    apiKey: process.env.MGATED_GATEWAY_TOKEN,
    baseURL: base.href.replace(/\/$/, ''),
    maxRetries: 0,
    timeout: 10000,
    logLevel: 'off',
    fetch: async (input, init) => {
      const target = new URL(input instanceof Request ? input.url : input);
      assert.equal(target.origin, base.origin);
      assert.equal(target.pathname, '/deepseek/v1/chat/completions');
      assert.equal(init.method, 'POST');
      attempts++;
      const r = await fetch(input, { ...init, redirect: 'manual' });
      wireStatus = r.status;
      return r;
    },
  });
  const stream = kind.startsWith('stream') || kind === 'native_error' || kind.includes('_stream');
  const body = { model: 'synthetic-text', messages: [{ role: 'user', content: 'synthetic SDK' }], max_tokens: 100, stream };
  if (kind === 'stream_true') body.stream_options = { include_usage: true };
  if (kind === 'stream_false') body.stream_options = { include_usage: false };
  if (feature) {
    body.thinking = { type: kind.endsWith('_enabled') ? 'enabled' : 'disabled' };
    if (tool) {
      body.tools = [{ type: 'function', function: { name: 'lookup', parameters: { type: 'object', properties: { city: { type: 'string' } }, required: ['city'] } } }];
      body.tool_choice = kind.endsWith('_enabled') ? 'auto' : { type: 'function', function: { name: 'lookup' } };
    } else {
      body.messages[0].content = 'Return a JSON object';
      body.response_format = { type: 'json_object' };
    }
  }
  let actual = 200;
  let nativeError = false;
  try {
    const result = await client.chat.completions.create(body);
    let message;
    if (stream) {
      let finals = 0;
      let usage = 0;
      let content = '';
      let reasoning = '';
      let functionName = '';
      let argumentsText = '';
      let callId;
      let toolFragments = 0;
      let finishReason;
      let afterFinal = false;
      for await (const chunk of result) {
        assert.equal(afterFinal, false);
        assert.equal(chunk.choices.length, 1);
        const choice = chunk.choices[0];
        if (chunk.usage) {
          usage++;
          assert.equal(chunk.usage.prompt_cache_hit_tokens, 40);
          assert.equal(chunk.usage.prompt_cache_miss_tokens, 60);
          assert.equal(chunk.usage.total_tokens, 120);
          assert.ok(choice.finish_reason, 'usage는 마지막 일반 choices chunk');
        }
        if (choice.finish_reason) { finals++; finishReason = choice.finish_reason; afterFinal = true; }
        if (feature) {
          content += choice.delta.content ?? '';
          reasoning += choice.delta.reasoning_content ?? '';
          for (const call of choice.delta.tool_calls ?? []) {
            assert.equal(call.index, 0);
            toolFragments++;
            if (call.id) callId = call.id;
            if (call.type) assert.equal(call.type, 'function');
            functionName += call.function?.name ?? '';
            argumentsText += call.function?.arguments ?? '';
          }
        }
      }
      assert.equal(finals, 1);
      assert.equal(usage, 1);
      if (feature) {
        assert.equal(finishReason, tool ? 'tool_calls' : 'stop');
        message = { role: 'assistant', content: tool ? null : content };
        if (tool) {
          assert.equal(toolFragments, 2);
          message.tool_calls = [{ id: callId, type: 'function', function: { name: functionName, arguments: argumentsText } }];
        }
        if (reasoning) message.reasoning_content = reasoning;
      }
    } else {
      assert.equal(result.model, 'synthetic-text');
      assert.equal(result.usage.total_tokens, 120);
      assert.equal(result.usage.prompt_cache_hit_tokens, 40);
      message = result.choices[0].message;
      if (feature) assert.equal(result.choices[0].finish_reason, tool ? 'tool_calls' : 'stop');
    }
    if (feature) {
      if (tool) {
        assert.deepEqual(message.tool_calls, [{ id: 'call_ds1', type: 'function', function: { name: 'lookup', arguments: '{"city":"Seoul"}' } }]);
        assert.deepEqual(JSON.parse(message.tool_calls[0].function.arguments), { city: 'Seoul' });
        const next = await client.chat.completions.create({
          model: body.model,
          max_tokens: 100,
          thinking: body.thinking,
          messages: [...body.messages, message, { role: 'tool', content: JSON.stringify({ weather: 'clear' }), tool_call_id: message.tool_calls[0].id }],
        });
        assert.equal(next.choices[0].message.content, 'synthetic');
        assert.equal(next.choices[0].finish_reason, 'stop');
        assert.equal(next.usage.total_tokens, 120);
      } else {
        assert.deepEqual(JSON.parse(message.content), { ok: true });
      }
    }
  } catch (e) {
    assert.ok(e instanceof OpenAI.APIError);
    if (kind === 'native_error') {
      assert.equal(wireStatus, 200);
      assert.equal(e.status, undefined);
      assert.equal(e.code, 'synthetic_stream_error');
      assert.equal(e.type, 'server_error');
      nativeError = true;
    } else {
      actual = e.status;
      assert.equal(e.error?.code?.startsWith('gateway_'), true);
      assert.equal(e.error?.message?.includes('synthetic-deepseek-company'), false);
    }
  }
  if (kind === 'native_error') assert.equal(nativeError, true);
  assert.equal(actual, expected);
  assert.equal(wireStatus, expected);
  assert.equal(attempts, tool ? 2 : 1);
  console.log(`SDK_S8_OK status=${actual} attempts=${attempts} maxRetries=0 kind=${kind}`);
}
main().catch(() => { console.error('SDK_S8_FAILED'); process.exitCode = 1; });
