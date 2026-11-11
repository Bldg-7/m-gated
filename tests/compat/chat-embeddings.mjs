import OpenAI from 'openai';
import assert from 'node:assert/strict';
async function main() {
  assert.equal(process.versions.node, '22.22.2');
  assert.equal(process.env.DO_NOT_TRACK, '1');
  const base = new URL(process.env.MGATED_GATEWAY_BASE);
  assert.equal(base.protocol, 'http:'); assert.equal(base.hostname, '127.0.0.1'); assert.ok(base.port);
  const kind = process.env.MGATED_SDK_KIND;
  const embedding = kind.startsWith('embeddings');
  const expected = Number(process.env.MGATED_EXPECTED_STATUS);
  let attempts = 0; let wireStatus;
  const client = new OpenAI({apiKey:process.env.MGATED_GATEWAY_TOKEN,baseURL:base.href.replace(/\/$/,''),maxRetries:0,timeout:10000,logLevel:'off',fetch:async(input,init)=>{
    const target=new URL(input instanceof Request?input.url:input);assert.equal(target.origin,base.origin);assert.equal(target.pathname,embedding?'/openai/v1/embeddings':'/openai/v1/chat/completions');assert.equal(init.method,'POST');attempts++;
    const response=await fetch(input,{...init,redirect:'manual'});wireStatus=response.status;return response;
  }});
  const nativeError = kind.startsWith('chat_native_error');
  let observedNativeError = false;
  let actual=200;
  try {
    if(embedding){const request={model:'synthetic-text',input:['synthetic SDK'],dimensions:3};if(kind==='embeddings_float')request.encoding_format='float';if(kind==='embeddings_base64')request.encoding_format='base64';
      const result=await client.embeddings.create(request);assert.equal(result.model,'synthetic-text');assert.equal(result.usage.prompt_tokens,100);assert.equal(result.usage.total_tokens,100);assert.equal(result.data.length,1);
      if(kind==='embeddings_base64')assert.equal(result.data[0].embedding,Buffer.from(new Float32Array([0.5,0.5,0.5]).buffer).toString('base64'));
      else assert.deepEqual(Array.from(result.data[0].embedding),[0.5,0.5,0.5]);
    }else{const stream=kind==='chat_stream'||nativeError;const request={model:'synthetic-text',messages:[{role:'user',content:'synthetic SDK'}],max_completion_tokens:100,stream};if(stream)request.stream_options={include_usage:true};if(kind==='chat_bad_usage'){request.stream=true;}
      const result=await client.chat.completions.create(request);
      if(stream){let usage=0;let finished=0;for await(const chunk of result){if(chunk.choices.length===0){usage++;assert.equal(chunk.usage.prompt_tokens,100);assert.equal(chunk.usage.completion_tokens,20);assert.equal(chunk.usage.completion_tokens_details.reasoning_tokens,10);}else if(chunk.choices[0].finish_reason)finished++;}assert.equal(usage,1);assert.equal(finished,1);}
      else{assert.equal(result.model,'synthetic-text');assert.equal(result.usage.total_tokens,120);assert.equal(result.choices[0].message.role,'assistant');}
    }
  }catch(error){assert.ok(error instanceof OpenAI.APIError);
    if(nativeError){assert.equal(wireStatus,200);assert.equal(error.status,undefined);assert.equal(error.code,'synthetic_stream_error');assert.equal(error.type,'server_error');assert.equal(error.param,'synthetic_param');assert.equal(error.message,'x'.repeat(50000));observedNativeError=true;}
    else{actual=error.status;assert.equal(error.error?.code?.startsWith('gateway_'),true);assert.equal(error.error?.message?.includes('synthetic-company'),false);}}
  if(nativeError)assert.equal(observedNativeError,true);
  assert.equal(actual,expected);assert.equal(wireStatus,expected);assert.equal(attempts,1);
  console.log(`SDK_S7_OK status=${actual} attempts=1 maxRetries=0 kind=${kind}`);
}
main().catch(()=>{console.error('SDK_S7_FAILED');process.exitCode=1;});
