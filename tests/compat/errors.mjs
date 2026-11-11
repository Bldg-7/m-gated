import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import OpenAI from 'openai';

async function main() {
  assert.equal(process.versions.node, '22.22.2');
  assert.equal(process.env.DO_NOT_TRACK, '1');
  const base = new URL(process.env.MGATED_GATEWAY_BASE);
  assert.equal(base.protocol, 'http:');
  assert.equal(base.hostname, '127.0.0.1');
  assert.ok(base.port);
  const api = process.env.MGATED_ERROR_API;
  const expected = Number(process.env.MGATED_EXPECTED_STATUS);
  assert.ok([401,403,429,502,503,504].includes(expected));
  assert.ok(['anthropic','responses','chat','embeddings','deepseek'].includes(api));
  let attempts = 0, refresh = 0, status;
  const paths = {anthropic:'/anthropic/v1/messages',responses:'/openai/v1/responses',chat:'/openai/v1/chat/completions',embeddings:'/openai/v1/embeddings',deepseek:'/deepseek/v1/chat/completions'};
  const options = {
    apiKey: process.env.MGATED_GATEWAY_TOKEN,
    baseURL: `${base.origin}${api==='anthropic'?'/anthropic':api==='deepseek'?'/deepseek/v1':'/openai/v1'}`,
    maxRetries: 0, timeout: 130_000,
    fetch: async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : input);
      if (url.pathname.includes('refresh')) refresh++;
      assert.equal(url.origin, base.origin);
      assert.equal(url.pathname, paths[api]);
      assert.equal(init.method, 'POST');
      attempts++;
      const r = await fetch(input, {...init, redirect:'manual'});
      status = r.status;
      assert.equal(r.headers.get('x-gateway-dispatch-state'), process.env.MGATED_EXPECT_DISPATCH==='N'?'not_dispatched':null);
      assert.equal(r.headers.get('x-should-retry'), process.env.MGATED_EXPECT_DISPATCH==='N'?null:'false');
      if (process.env.MGATED_EXPECT_ERROR_CODE) assert.equal(r.headers.get('x-gateway-error-code'), process.env.MGATED_EXPECT_ERROR_CODE);
      return r;
    },
  };
  const client = api==='anthropic'?new Anthropic(options):new OpenAI({...options,logLevel:'off'});
  const stream=process.env.MGATED_ERROR_STREAM==='1';
  let caught = false;
  try {
    if(api==='anthropic') await client.messages.create({model:'synthetic-text',max_tokens:100,stream,messages:[{role:'user',content:'synthetic SDK error fixture'}]});
    else if(api==='responses') await client.responses.create({model:'synthetic-text',store:false,input:'synthetic SDK error fixture',max_output_tokens:100,stream});
    else if(api==='embeddings') await client.embeddings.create({model:'synthetic-text',input:'synthetic SDK error fixture',dimensions:3,encoding_format:'float'});
    else await client.chat.completions.create({model:'synthetic-text',messages:[{role:'user',content:'synthetic SDK error fixture'}],stream,...(api==='chat'&&stream?{stream_options:{include_usage:true}}:{}),...(api==='deepseek'?{max_tokens:100}:{max_completion_tokens:100})});
  } catch(e) {
    assert.ok(e instanceof (api==='anthropic'?Anthropic.APIError:OpenAI.APIError));
    assert.equal(e.status, expected);
    caught = true;
  }
  assert.equal(caught, true);
  assert.equal(status, expected);
  assert.equal(attempts, 1);
  assert.equal(refresh, 0);
  process.stdout.write(`SDK_ERROR_MATRIX_OK api=${api} status=${status} attempts=1 refresh=0 maxRetries=0\n`);
}
main().catch(()=>{process.stderr.write('SDK_ERROR_MATRIX_FAILED\n');process.exitCode=1;});
