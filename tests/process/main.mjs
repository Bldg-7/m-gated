// 합성 CA·정상 서명 SSO·PG TLS만 제공하는 격리된 실제 제품 main 시험이다.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, rmSync, unlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID, randomBytes, generateKeyPairSync, createSign } from 'node:crypto';
import net from 'node:net';
import tls from 'node:tls';
import https from 'node:https';
import assert from 'node:assert/strict';
import pg from 'pg';
import { once } from 'node:events';
import { measureLoad } from './load.mjs';
const loadCase=process.env.TEST_S10_LOAD_CASE;assert(!loadCase||['all','baseline','boundary','mixed','soak'].includes(loadCase));
let loadActive=false;
const raw=process.env.TEST_DATABASE_URL;
assert.equal(raw,'postgres://m_gated_test@127.0.0.1:64603/m_gated_test');
const schema=`gw_test_${randomUUID().replaceAll('-','')}`;
const directory=mkdtempSync(join(tmpdir(),'mgated-u0-tls-'));
const db=new pg.Client({host:'host.docker.internal',port:64603,database:'m_gated_test',user:'m_gated_test',password:'',ssl:false,connectionTimeoutMillis:2000,query_timeout:2000});
let app=null,pgServer=null,sso=null;const sockets=new Set();const legacyChildren=new Set();
let created=false;const marker=join('/fixture-ownership',schema);
let expired=false;
const overall=setTimeout(()=>{expired=true;app?.child.kill('SIGKILL');for(const socket of sockets)socket.destroy();},loadCase?4200000:120000);
function bounded(promise,ms,label){let timer;return Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(label)),ms);})]).finally(()=>clearTimeout(timer));}
function openssl(args){const r=spawnSync('/usr/bin/openssl',args,{env:{},stdio:'pipe',timeout:10000});assert.equal(r.status,0,'합성 TLS 생성');}
const env={DB_PASSWORD:'synthetic-db-secret',CSRF_SIGNING_KEY:'000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f',ANTHROPIC_API_KEY:'synthetic-provider-secret',OPENAI_API_KEY:'synthetic-openai-company-key',DEEPSEEK_API_KEY:'synthetic-deepseek-company-key',TLS_CA_FILE:join(directory,'ca.pem')};
let readyResolve;let readySignal=new Promise(r=>{readyResolve=r;});
const metricNames=Object.freeze(["admission_authentication_total", "admission_principal_total", "admission_nonstream_total", "queue_waiters", "cgroup_cpu_usage_microseconds", "cgroup_cpu_user_microseconds", "cgroup_cpu_system_microseconds", "admission_rejected_total", "admission_not_ready_total", "admission_draining_total", "admission_capacity_total", "admission_invalid_total", "http_429_total", "http_413_total", "http_503_total", "authentication_total", "authentication_failed_total", "authentication_microseconds_total", "jwks_fetch_total", "jwks_failed_total", "jwks_fetch_microseconds_total", "jwks_cache_age_seconds", "db_acquire_total", "db_acquire_failed_total", "db_acquire_microseconds_total", "db_lock_total", "db_lock_failed_total", "db_lock_microseconds_total", "finalization_total", "finalization_failed_total", "finalization_microseconds_total", "active_tasks", "active_permits", "body_bytes", "queue_bytes", "queue_peak_bytes", "parser_peak_bytes", "parser_bytes", "input_bytes", "ttft_count", "ttft_microseconds_total", "stream_count", "stream_microseconds_total", "stream_cancelled_total", "upstream_2xx_total", "upstream_4xx_total", "upstream_5xx_total", "upstream_other_total", "unknown_requests", "unknown_blocked_principals", "oldest_unknown_seconds", "price_unresolved_total", "pricing_bound_violation_total", "reaper_transitions_total", "deleted_requests_total", "reaper_failed_total", "panic_total", "shutdown_remaining_milliseconds", "ready", "process_rss_bytes", "process_anon_bytes", "cgroup_anon_bytes", "cgroup_memory_current_bytes", "cgroup_memory_peak_bytes", "telemetry_dropped_total", "alarm_new_unknown_total", "alarm_finalization_latch_total", "alarm_company_key_error_total", "alarm_ready_degraded_total", "alarm_pricing_bound_total"]);
function start(extra={}){
 assert(!expired,'전체 프로세스 시험 기한');
 const child=spawn('/fixture/m-gated',['--config',join(directory,'config.yaml')],{cwd:'/fixture',env:{...env,...extra},stdio:['ignore','pipe','pipe']});
 let out='',err='',pending='';const telemetry={count:0,latest:null,peaks:{}};
 const capture=(name,data)=>{
  if(name==='out'){out+=data;assert(out.length<=4096,'bounded main stdout');return;}
  pending+=data;let newline;
  while((newline=pending.indexOf('\n'))>=0){const line=pending.slice(0,newline);pending=pending.slice(newline+1);assert(line.length<=8192,'bounded telemetry line');
   for(const secret of Object.values(env))assert(!line.includes(secret),'운영 출력에 합성 인증값 노출');
   assert(!line.includes('synthetic prompt sentinel'),'운영 출력에 대화본문 노출');
   if(line.startsWith('{"telemetry":')){const parsed=JSON.parse(line);assert.deepEqual(Object.keys(parsed),['telemetry']);assert.deepEqual(Object.keys(parsed.telemetry).sort(),[...metricNames].sort());for(const [key,value]of Object.entries(parsed.telemetry)){assert(Number.isSafeInteger(value)&&value>=0);telemetry.peaks[key]=Math.max(telemetry.peaks[key]??0,value);}telemetry.count++;telemetry.latest=parsed.telemetry;
   }else{err+=line+'\n';assert(err.length<=4096,'bounded main diagnostics');}
  }
  assert(pending.length<=8192,'bounded telemetry framing');
 };
 child.stdout.on('data',d=>capture('out',d));child.stderr.on('data',d=>capture('err',d));
 const exit=bounded(new Promise((resolve,reject)=>{child.on('error',reject);child.on('close',(code,signal)=>{assert.equal(pending,'','불완전 계측 출력');resolve({code,signal,out,err,telemetry});});}),loadActive?4200000:35000,'bounded main exit').catch(e=>{child.kill('SIGKILL');throw e;});
 return {child,exit,telemetry};
}
async function stop(){if(!app)return;app.child.kill('SIGTERM');const result=await app.exit;assert.equal(result.code,0);assert.equal(result.out,'');assert.equal(result.err,'');assert(result.telemetry.count>0,'실제 main collector snapshot');assert.equal(result.telemetry.latest.active_tasks,0);assert.equal(result.telemetry.latest.input_bytes,0);app=null;}

async function get(path,headers={}){return fetch(`http://127.0.0.1:4000${path}`,{headers,redirect:'manual',signal:AbortSignal.timeout(3000)});}
// 실제 probe 응답을 기다리는 유한 event-driven retry다. SQL 경합을 sleep으로 맞추지 않는다.
async function waitStatus(path,status,deadline=15000){const until=Date.now()+deadline;while(!expired&&Date.now()<until){try{const r=await get(path);if(r.status===status)return r;}catch{}await new Promise(r=>setTimeout(r,50));}throw new Error(`probe ${path} expected ${status}`);}
try{
 await db.connect();writeFileSync(marker,'m_gated_test/127.0.0.1:64603\n');await db.query(`CREATE SCHEMA ${schema}`);created=true;
 openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',join(directory,'ca.key'),'-out',join(directory,'ca.pem'),'-days','1','-subj','/CN=m-gated synthetic test CA']);
 openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout',join(directory,'server.key'),'-out',join(directory,'server.csr'),'-subj','/CN=auth.example.test']);
 writeFileSync(join(directory,'ext.cnf'),'subjectAltName=DNS:auth.example.test,DNS:api.anthropic.com,DNS:api.openai.com,DNS:api.deepseek.com,DNS:localhost\nextendedKeyUsage=serverAuth\n');
 openssl(['x509','-req','-in',join(directory,'server.csr'),'-CA',join(directory,'ca.pem'),'-CAkey',join(directory,'ca.key'),'-CAcreateserial','-out',join(directory,'server.pem'),'-days','1','-extfile',join(directory,'ext.cnf')]);
 openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout',join(directory,'wrong-ca.key'),'-out',join(directory,'wrong-ca.pem'),'-days','1','-subj','/CN=untrusted synthetic CA']);
 const tlsOptions={key:readFileSync(join(directory,'server.key')),cert:readFileSync(join(directory,'server.pem'))};
 const context=tls.createSecureContext(tlsOptions);
 // TLS PG 연결은 기존 부모 소유 PG에만 전달하고 합성 schema를 startup packet에 고정한다.
 pgServer=net.createServer(socket=>{sockets.add(socket);socket.on('close',()=>sockets.delete(socket));socket.once('data',packet=>{assert.equal(packet.readInt32BE(0),8);assert.equal(packet.readInt32BE(4),80877103);socket.write('S');const secure=new tls.TLSSocket(socket,{isServer:true,secureContext:context});secure.once('data',startup=>{const payload=Buffer.concat([startup.subarray(4,-1),Buffer.from(`options\0-c search_path=${schema}\0\0`)]);const length=Buffer.alloc(4);length.writeInt32BE(payload.length+4);const upstream=net.connect(64603,'host.docker.internal',()=>{upstream.write(Buffer.concat([length,payload]));secure.pipe(upstream);upstream.pipe(secure);});upstream.on('error',()=>secure.destroy());secure.on('error',()=>upstream.destroy());secure.on('close',()=>upstream.destroy());});secure.on('error',()=>socket.destroy());});});
 await new Promise(r=>pgServer.listen(64603,'127.0.0.1',r));
 const keys=generateKeyPairSync('rsa',{modulusLength:2048});const jwk=keys.publicKey.export({format:'jwk'});Object.assign(jwk,{kid:'key1',alg:'RS256',use:'sig'});
 let loadWave=null;
 let failJwks=false;let calls=0;let providerCalls=0;let providerMode='large';let responsesCalls=0;let responseStatus='completed';let responsesStarted;let responsesRelease;let providerStoppedResolve;let providerStopped=new Promise(r=>{providerStoppedResolve=r;});
 const startWire='event: message_start\ndata: {"type":"message_start","message":{"type":"message","role":"assistant","model":"synthetic-text","usage":{"input_tokens":100,"output_tokens":0}}}\n\n';
 const endWire='event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}\n\nevent: message_stop\ndata: {"type":"message_stop"}\n\n';
 function sampleMemory(){const stats=readFileSync(`/proc/${app.child.pid}/smaps_rollup`,'utf8');return {rssKiB:Number(stats.match(/^Rss:\s+(\d+)/m)[1]),anonKiB:Number(stats.match(/^Anonymous:\s+(\d+)/m)[1])};}
 async function provider(req,res){providerCalls++;assert(['/v1/messages','/v1/messages/count_tokens'].includes(req.url));assert.equal(req.headers['x-api-key'],'synthetic-provider-secret');assert.equal(req.headers.authorization,undefined);let n=0;for await(const chunk of req){n+=chunk.length;assert(n<=(providerMode==='load'?8388608:65535));}if(req.url.endsWith('/count_tokens')){res.writeHead(200,{'content-type':'application/json'});res.end('{"input_tokens":321}');return;}if(providerMode==='strict'){assert(n<=4096,'합성 native context 계약');res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({type:'message',id:'msg_strict',role:'assistant',model:'synthetic-text',content:[{type:'text',text:'synthetic strict'}],stop_reason:'end_turn',usage:{input_tokens:100,output_tokens:20}}));return;}res.writeHead(200,{'content-type':'text/event-stream','request-id':'synthetic-main-provider'});res.on('close',()=>providerStoppedResolve());res.write(startWire);if(providerMode==='stalled')return;
  if(providerMode==='load'){assert(loadWave);const wave=loadWave;await wave.gate;const until=Date.now()+wave.seconds*1000;while(Date.now()<until){if(!res.write('event: future_event\ndata: '+JSON.stringify({type:'future_event',text:'x'.repeat(16384)})+'\n\n'))await once(res,'drain');await new Promise(r=>setTimeout(r,1000));}res.end(endWire);return;}

  res.write('event: future_event\ndata: {"type":"future_event","text":"');const chunk='x'.repeat(16384);for(let i=0;i<640;i++){if(!res.write(chunk))await once(res,'drain');}res.write('"}\n\n');res.end(endWire);
 }

 async function responsesProvider(req,res){
  responsesCalls++;assert.equal(req.url,'/v1/responses');assert.equal(req.headers.authorization,'Bearer synthetic-openai-company-key');assert.equal(req.headers['x-api-key'],undefined);assert.equal(req.headers['anthropic-version'],undefined);
  const chunks=[];let length=0;for await(const chunk of req){length+=chunk.length;assert(length<=4096,'합성 Responses context 전체 입력/overhead 계약');chunks.push(chunk);}const input=JSON.parse(Buffer.concat(chunks));assert.equal(input.store,false);assert(input.max_output_tokens>0&&input.max_output_tokens<=1024);
  if(responsesStarted){responsesStarted();await new Promise(r=>{responsesRelease=r;});}
  const native={id:'resp_main_fixture',object:'response',model:'synthetic-text',status:responseStatus,output:[],incomplete_details:responseStatus==='incomplete'?{reason:'max_output_tokens'}:null,usage:{input_tokens:100,output_tokens:20,total_tokens:120,input_tokens_details:{cached_tokens:40},output_tokens_details:{reasoning_tokens:10}}};
  if(!input.stream){res.writeHead(200,{'content-type':'application/json','x-request-id':'synthetic-main-openai'});res.end(JSON.stringify(native));return;}
  res.writeHead(200,{'content-type':'text/event-stream','x-request-id':'synthetic-main-openai'});
  res.write(`event: response.created\ndata: ${JSON.stringify({type:'response.created',sequence_number:0,response:{...native,status:'in_progress',error:null,incomplete_details:null,usage:null}})}\n\n`);
  if(providerMode==='responses_large'){native.output=[{type:'message',role:'assistant',content:[{type:'output_text',text:'x'.repeat(3*1024*1024),annotations:[]}]}];native.instructions='i'.repeat(3*1024*1024);native.tools=[{type:'function',name:'f',parameters:{description:'t'.repeat(3*1024*1024)}}];}
  const wire=`event: response.${responseStatus}\ndata: ${JSON.stringify({type:`response.${responseStatus}`,sequence_number:1,response:native})}\n\n`;
  for(let offset=0;offset<wire.length;offset+=16384){if(!res.write(wire.slice(offset,offset+16384)))await once(res,'drain');}res.end();
 }
 let s7Calls=0;let s7Started;let s7Release;
 async function chatEmbeddingsProvider(req,res){
  s7Calls++;assert(['/v1/chat/completions','/v1/embeddings'].includes(req.url));assert.equal(req.headers.authorization,'Bearer synthetic-openai-company-key');assert.equal(req.headers['x-api-key'],undefined);assert.equal(req.headers['anthropic-version'],undefined);
  const chunks=[];let length=0;for await(const chunk of req){length+=chunk.length;assert(length<=4096,'합성 Chat context 전체 입력/overhead 계약');chunks.push(chunk);}const input=JSON.parse(Buffer.concat(chunks));
  if(s7Started){s7Started();await new Promise(r=>{s7Release=r;});}
  if(req.url==='/v1/embeddings'){const encoded=input.encoding_format==='base64';const vector=encoded?Buffer.from(new Float32Array([0.5,0.5,0.5]).buffer).toString('base64'):[0.5,0.5,0.5];res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({object:'list',model:'synthetic-text',data:[{object:'embedding',index:0,embedding:vector}],usage:{prompt_tokens:100,total_tokens:100}}));return;}
  assert(input.max_completion_tokens>0&&input.max_completion_tokens<=1024);const usage={prompt_tokens:100,completion_tokens:20,total_tokens:120,prompt_tokens_details:{cached_tokens:40},completion_tokens_details:{reasoning_tokens:10}};
  if(!input.stream){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({id:'c_s7',object:'chat.completion',model:'synthetic-text',choices:[{index:0,message:{role:'assistant',content:'synthetic'},finish_reason:'length'}],usage}));return;}
  assert.equal(input.stream_options.include_usage,true);res.writeHead(200,{'content-type':'text/event-stream'});res.write(`data: ${JSON.stringify({id:'c_s7',object:'chat.completion.chunk',model:'synthetic-text',choices:[{index:0,delta:{role:'assistant',content:'synthetic'},finish_reason:'length'}],usage:null})}\n\n`);res.write(`data: ${JSON.stringify({id:'c_s7',object:'chat.completion.chunk',model:'synthetic-text',choices:[],usage})}\n\n`);res.end('data: [DONE]\n\n');
 }
 let s8Calls=0;let s8Started;let s8Release;
 async function deepseekProvider(req,res){
  s8Calls++;assert.equal(req.url,'/chat/completions');assert.equal(req.headers.authorization,'Bearer synthetic-deepseek-company-key');assert.equal(req.headers['x-api-key'],undefined);assert.equal(req.headers['anthropic-version'],undefined);assert.equal(req.headers.cookie,undefined);
  const chunks=[];let length=0;for await(const chunk of req){length+=chunk.length;assert(length<=4096,'합성 DeepSeek context 전체입력/overhead 계약');chunks.push(chunk);}const input=JSON.parse(Buffer.concat(chunks));assert(input.max_tokens>0&&input.max_tokens<=1024);assert.equal(input.max_completion_tokens,undefined);
  if(s8Started){s8Started();await new Promise(r=>{s8Release=r;});}
  const usage={prompt_tokens:100,completion_tokens:20,total_tokens:120,prompt_cache_hit_tokens:40,prompt_cache_miss_tokens:60,completion_tokens_details:{reasoning_tokens:10}};
  const content=input.response_format?.type==='json_object'?'{\"ok\":true}':'synthetic';
  if(!input.stream){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({id:'ds_main',object:'chat.completion',model:'synthetic-text',choices:[{index:0,message:{role:'assistant',content,reasoning_content:'synthetic thought'},finish_reason:'stop'}],usage}));return;}
  res.writeHead(200,{'content-type':'text/event-stream'});res.write(`data: ${JSON.stringify({id:'ds_main',object:'chat.completion.chunk',model:'synthetic-text',choices:[{index:0,delta:{role:'assistant',content},finish_reason:null}],...(input.stream_options?.include_usage?{usage:null}:{})})}\n\n`);res.write(`data: ${JSON.stringify({id:'ds_main',object:'chat.completion.chunk',model:'synthetic-text',choices:[{index:0,delta:{},finish_reason:'stop'}],usage})}\n\n`);res.end('data: [DONE]\n\n');
 }
 sso=https.createServer(tlsOptions,(req,res)=>{if(req.headers.host==='api.deepseek.com'){deepseekProvider(req,res).catch(()=>res.destroy());return;}if(req.headers.host==='api.anthropic.com'){provider(req,res).catch(()=>res.destroy());return;}if(req.headers.host==='api.openai.com'){(req.url==='/v1/responses'?responsesProvider:chatEmbeddingsProvider)(req,res).catch(()=>res.destroy());return;}if(req.url==='/.well-known/jwks.json'){calls++;if(failJwks){res.writeHead(503);res.end();}else{res.setHeader('content-type','application/json');res.end(JSON.stringify({keys:[jwk]}));}readyResolve();}else{res.writeHead(404);res.end();}});
 await new Promise(r=>sso.listen(443,'127.0.0.1',r));
 let config=readFileSync('/fixture/synthetic-config.yaml','utf8').replace('host: 127.0.0.1','host: localhost').replace('user@example.invalid','alice@example.test').replace('admin@example.invalid','alice@example.test');writeFileSync(join(directory,'config.yaml'),config);
 // 잘못된 CA와 hostname은 인증을 우회하지 않고 실제 main의 migration 전에 실패한다.
 app=start({TLS_CA_FILE:join(directory,'missing.pem')});let result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/config_secret_invalid/);app=null;
 app=start({TLS_CA_FILE:join(directory,'wrong-ca.pem')});result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/schema_unavailable/);app=null;
 config=config.replace('host: localhost','host: 127.0.0.1');writeFileSync(join(directory,'config.yaml'),config);app=start();result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/schema_unavailable/);app=null;
 writeFileSync(join(directory,'config.yaml'),config.replace('host: 127.0.0.1','host: localhost'));
 app=start();await waitStatus('/readyz',200);assert.equal((await get('/livez')).status,200);assert.equal((await get('/ui/')).status,200);
 const migrations=await db.query(`SELECT version FROM ${schema}.gw_schema_migrations ORDER BY version`);assert.deepEqual(migrations.rows.map(r=>r.version),['1','2','3','4','5']);
 const header=Buffer.from(JSON.stringify({alg:'RS256',kid:'key1'})).toString('base64url');const payload=Buffer.from(JSON.stringify({iss:'https://auth.example.test',aud:'https://example.test',sub:'main-human',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200,name:'Synthetic main'})).toString('base64url');const signed=`${header}.${payload}`;const signature=createSign('RSA-SHA256').update(signed).sign(keys.privateKey).toString('base64url');const cookie=`mgated_access=${signed}.${signature}`;
 assert.equal((await get('/gateway/me',{cookie})).status,200);assert.equal((await get('/gateway/usage?group_by=provider',{cookie})).status,200);
 // canonical Provider도 /etc/hosts의 명시 loopback mapping 없이는 절대 호출하지 않는다.
 assert.match(readFileSync('/etc/hosts','utf8'),/127\.0\.0\.1\s+api\.anthropic\.com(?:\s|$)/m);
 const principal=(await db.query(`SELECT id FROM ${schema}.gw_principals WHERE subject='main-human'`)).rows[0].id;
 // JWT는 argv/env/파일 대신 bounded stdin pipe에만 전달한다. 현재 SSO admin·미설정삭제0·replay를 실제 binary로 관측한다.
 async function maintenance(commandId, command, token=`${signed}.${signature}`){
  const child=spawn('/fixture/m-gated',['--maintenance','--config',join(directory,'config.yaml')],{cwd:'/fixture',env:{...env},stdio:['pipe','pipe','pipe']});
  let out='',err='';child.stdout.on('data',d=>{out+=d;if(out.length>8192)child.kill('SIGKILL');});child.stderr.on('data',d=>{err+=d;if(err.length>4096)child.kill('SIGKILL');});
  child.stdin.end(JSON.stringify({access_token:token,command_id:commandId,command}));
  const [code]=await bounded(once(child,'exit'),15000,'bounded maintenance exit');
  for(const secret of [token,...Object.values(env)]){assert(!out.includes(secret));assert(!err.includes(secret));}
  return {code,out,err};
 }
 const maintenanceId=randomUUID();const retentionCommand={operation:'retention_delete',after:null,limit:50,deletion_approved:true,approval_ref:'synthetic-approval-local-only',reason:'합성 로컬 보관'};
 for(const replayed of [false,true]){const result=await maintenance(maintenanceId,retentionCommand);assert.equal(result.code,0);assert.equal(result.err,'');const decoded=JSON.parse(result.out);assert.equal(decoded.ack.replayed,replayed);assert.equal(decoded.result.deleted_requests,0);}
 const deniedMaintenance=await maintenance(randomUUID(),{...retentionCommand,deletion_approved:false});assert.equal(deniedMaintenance.code,1);assert.equal(deniedMaintenance.out,'');assert.equal(deniedMaintenance.err,'maintenance_failed: 유지보수 검증에 실패했습니다.\n');
 console.log('S10_ACTUAL_MAIN_MAINTENANCE_SSO_ADMIN_REPLAY_AND_UNCONFIGURED_DELETE_ZERO_OK');
 await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2 WHERE principal_id=$1`,[principal,JSON.stringify([{provider:'anthropic',model:'synthetic-text',capabilities:['messages']}])]);
 const authorization=`Bearer ${signed}.${signature}`;const requestBody=JSON.stringify({model:'synthetic-text',max_tokens:100,stream:true,messages:[{role:'user',content:'synthetic main lifecycle'}]});
 async function inference(){return fetch('http://127.0.0.1:4000/v1/messages',{method:'POST',headers:{authorization,'content-type':'application/json','anthropic-version':'2023-06-01'},body:requestBody,signal:AbortSignal.timeout(33000)});}
 const idleMemory=sampleMemory();let peakMemory={...idleMemory};let response=await inference();assert.equal(response.status,200);const largeId=response.headers.get('x-gateway-request-id');let received=0;for await(const chunk of response.body){received+=chunk.length;const memory=sampleMemory();peakMemory.rssKiB=Math.max(peakMemory.rssKiB,memory.rssKiB);peakMemory.anonKiB=Math.max(peakMemory.anonKiB,memory.anonKiB);}assert(received>10*1024*1024);await bounded(providerStopped,2000,'provider large closed');
 // 다음 요청의 dispatch는 직전 finalization commit과 principal 잠금으로 직렬화된다.
 providerMode='stalled';providerStopped=new Promise(r=>{providerStoppedResolve=r;});response=await inference();assert.equal(response.status,200);const cancelledId=response.headers.get('x-gateway-request-id');const reader=response.body.getReader();assert((await reader.read()).value.length>0);
 const interrupted=reader.read().then(r=>r.done).catch(()=>true);const shutdownAt=Date.now();await stop();await bounded(interrupted,1000,'stream interrupted after shutdown');await bounded(providerStopped,1000,'provider stopped before main exit');assert(Date.now()-shutdownAt<31000);assert.equal(providerCalls,2);
 const outcomes=await db.query(`SELECT id,accounting_state,cost_usd::text,execution_stopped_at IS NOT NULL AS stopped FROM ${schema}.gw_requests ORDER BY admitted_at`);assert.equal(outcomes.rows.length,2);assert.deepEqual(outcomes.rows.map(r=>[r.id,r.accounting_state,r.cost_usd,r.stopped]),[[largeId,'settled','0.000140000000',true],[cancelledId,'unknown',null,true]]);
 console.log(`S3_ACTUAL_MAIN_LARGE_STREAM_AND_SIGTERM_OK bytes=${received} idle_rss_kib=${idleMemory.rssKiB} idle_anon_kib=${idleMemory.anonKiB} peak_rss_kib=${peakMemory.rssKiB} peak_anon_kib=${peakMemory.anonKiB} provider_calls=${providerCalls}`);


 // 최신 실제 binary에서도 strict/내부count/실효ceiling/현재잔액이 같은 DB 장부에 연결된다.
 const strictConfig=config.replace('host: 127.0.0.1','host: localhost').replace('api: messages','api: messages\n        internal_count: true').replace('        max_input_tokens: 4096','        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n        max_input_tokens: 4096')+`      - model: synthetic-text
        api: count_tokens
        profile_version: count-fixture-v1
        pricing_version: free-price-v1
        fixture_version: free-count-v1
        actual_models: [synthetic-text]
        billing_class: guaranteed_free
        max_input_tokens: 4096
        max_body_bytes: 8388608
        max_response_bytes: 262144
        rates:
          input: '0.000000000000'
execution_ceiling:
  budget: {mode: reserved, limit_usd: '1.000000000000', period: {kind: calendar_month, timezone: UTC}}
  rpm: 5
  tpm: 5000
  count_rpm: 5
`;
 writeFileSync(join(directory,'config.yaml'),strictConfig);providerMode='strict';app=start();await waitStatus('/readyz',200);
 const strictPayload=Buffer.from(JSON.stringify({iss:'https://auth.example.test',aud:'https://example.test',sub:'strict-main-human',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200,name:'Synthetic strict main'})).toString('base64url');const strictSigned=`${header}.${strictPayload}`;const strictSignature=createSign('RSA-SHA256').update(strictSigned).sign(keys.privateKey).toString('base64url');const strictToken=`${strictSigned}.${strictSignature}`;
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${strictToken}`})).status,200);
 const strictPrincipal=(await db.query(`SELECT id FROM ${schema}.gw_principals WHERE subject='strict-main-human'`)).rows[0].id;
 await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2,budget_mode='reserved',budget_limit_usd='2.000000000000',tpm_limit=5000 WHERE principal_id=$1`,[strictPrincipal,JSON.stringify([{provider:'anthropic',model:'synthetic-text',capabilities:['messages','count_tokens']}])]);
 const beforeStrict=providerCalls;const strictResponse=await fetch('http://127.0.0.1:4000/v1/messages',{method:'POST',headers:{authorization:`Bearer ${strictToken}`,'content-type':'application/json','anthropic-version':'2023-06-01'},body:JSON.stringify({model:'synthetic-text',max_tokens:100,messages:[{role:'user',content:'synthetic strict context'}]}),signal:AbortSignal.timeout(5000)});assert.equal(strictResponse.status,200);assert.equal((await strictResponse.json()).usage.input_tokens,100);assert.equal(providerCalls-beforeStrict,2);
 const balanceResponse=await get('/gateway/usage',{authorization:`Bearer ${strictToken}`});assert.equal(balanceResponse.status,200);const balance=await balanceResponse.json();assert.equal(balance.budget_balances[0].limit_usd,'1.000000000000');assert.equal(balance.budget_balances[0].remaining_usd,'0.999860000000');
 const strictRows=await db.query(`SELECT r.purpose,r.cost_usd::text,b.reservation_usd::text,b.reserved_tokens::text,e.normalized_usage->>'source' AS source FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id JOIN ${schema}.gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.principal_id=$1 ORDER BY purpose`,[strictPrincipal]);assert.deepEqual(strictRows.rows.map(r=>[r.purpose,r.cost_usd,r.reservation_usd,r.reserved_tokens,r.source]),[['internal_count','0.000000000000','0.000000000000','0','profile_no_charge'],['root','0.000140000000','0.004296000000','4196','provider']]);
 console.log('S4_ACTUAL_MAIN_STRICT_INTERNAL_COUNT_CURRENT_BALANCE_OK provider_calls=2 rows=2');
 // S5 실제 제품 main: 키 원문 한 번/서비스·관리 명령/수동 처리/현재 owner를 동일 TLS·DB에 연결한다.
 async function api(method,path,input,command=randomUUID(),cookieOnly=false,extra={}){
  const headers={...(cookieOnly?{cookie:`mgated_access=${strictToken}`}:{authorization:`Bearer ${strictToken}`}),...extra};
  if(method!=='GET')headers['x-gateway-command-id']=command;
  if(input!==undefined)headers['content-type']='application/json';
  const r=await fetch(`http://127.0.0.1:4000${path}`,{method,headers,body:input===undefined?undefined:JSON.stringify(input),signal:AbortSignal.timeout(5000)});
  const status=r.status;const responseHeaders=r.headers;const body=status===204?null:await r.json();return {status,headers:responseHeaders,body};
 }
 // S10 handoff의 첫 경계: 실제 별도 identity를 현재 admin 명령으로 fence한다.
 const handoffClaims={iss:'https://auth.example.test',aud:'https://example.test',sub:'handoff-target',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200};
 const handoffPart=`${header}.${Buffer.from(JSON.stringify(handoffClaims)).toString('base64url')}`;
 const handoffToken=`${handoffPart}.${createSign('RSA-SHA256').update(handoffPart).sign(keys.privateKey).toString('base64url')}`;
 const handoffMe=await get('/gateway/me',{authorization:`Bearer ${handoffToken}`});assert.equal(handoffMe.status,200);
 const handoffTarget=(await handoffMe.json()).principal;
 const handoffSource=randomUUID();
 const fenced=await maintenance(randomUUID(),{operation:'handoff',action:'fence',target_id:handoffTarget.id,expected_version:handoffTarget.policy.version,source_id:handoffSource,grants:handoffTarget.policy.grants,limits:handoffTarget.policy.limits});
 assert.equal(fenced.code,0,'handoff 명시 fence가 현재 admin/실제 DB에서 동작해야 한다');assert.equal(JSON.parse(fenced.out).state,'fenced');
 const handoffBlocked=await get('/gateway/me',{authorization:`Bearer ${handoffToken}`});assert.equal(handoffBlocked.status,403);
 console.log('S10_HANDOFF_BINARY_FENCE_AND_EXPLICIT_SSO_BLOCK_OK');

 // 합성 이전 writer는 별도 process·실제 loopback ingress·실제 합성 Provider·PG 비용을 사용한다.
 async function legacyStart(oldKey){
  const child=spawn(process.execPath,['/fixture/tests/process/legacy-writer.mjs'],{env:{},stdio:['pipe','pipe','pipe','ipc']});legacyChildren.add(child);
  let out='',err='';child.stdout.on('data',d=>{out+=d;if(out.length>4096)child.kill('SIGKILL');});child.stderr.on('data',d=>{err+=d;if(err.length>4096)child.kill('SIGKILL');});
  const exited=new Promise(resolve=>child.once('exit',(code,signal)=>resolve({code,signal,out,err})));
  const ready=bounded(new Promise(resolve=>{const listener=m=>{if(m.kind==='ready'){child.off('message',listener);resolve(m);}};child.on('message',listener);}),5000,'legacy ready');
  child.stdin.end(JSON.stringify({schema,source_id:handoffSource,access_token:handoffToken,old_key:oldKey,ca_file:join(directory,'ca.pem'),ledger_clock:'2025-03-20T00:00:00Z'}));
  const {port}=await ready;
  return {child,port,exited,async stop(){child.kill('SIGTERM');const r=await bounded(exited,7000,'legacy joined');legacyChildren.delete(child);assert.equal(r.code,0);assert.equal(r.out,'');assert.equal(r.err,'');}};
 }
 const oldKeyId=randomUUID();const oldKey=`mgw_v1_${oldKeyId.replaceAll('-','')}_${randomBytes(32).toString('base64url')}`;
 let oldWriter=await legacyStart(oldKey);
 const oldBody=JSON.stringify({model:'synthetic-text',max_tokens:100,messages:[{role:'user',content:'synthetic handoff'}]});
 async function oldCall(writer,credential){const r=await fetch(`http://127.0.0.1:${writer.port}/infer`,{method:'POST',headers:{authorization:`Bearer ${credential}`,'content-type':'application/json'},body:oldBody,signal:AbortSignal.timeout(7000)});await r.arrayBuffer();return r.status;}
 for(const credential of [handoffToken,handoffToken,oldKey])assert.equal(await oldCall(oldWriter,credential),200);
 const oldFenced=bounded(new Promise(resolve=>{const listener=m=>{if(m.kind==='fenced'){oldWriter.child.off('message',listener);resolve(m);}};oldWriter.child.on('message',listener);}),5000,'legacy fence ack');oldWriter.child.send({kind:'fence'});assert.equal((await oldFenced).active,0);
 for(const credential of [handoffToken,oldKey])assert.equal(await oldCall(oldWriter,credential),503);
 await oldWriter.stop();
 const oldCost=(await db.query(`SELECT cost_usd::text,settled_count::text,key_count::text,blocked,to_char(period_start AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS period_start,to_char(period_end AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS period_end FROM ${schema}.gw_fixture_legacy_writer WHERE source_id=$1`,[handoffSource])).rows[0];assert.deepEqual(oldCost,{cost_usd:'0.000420000000',settled_count:'3',key_count:'1',blocked:true,period_start:'2025-03-01T00:00:00Z',period_end:'2025-04-01T00:00:00Z'});
 const primaryScope=randomUUID(),oldKeyScope=randomUUID();const migratedGrants=[{provider:'anthropic',model:'synthetic-text',capabilities:['count_tokens','messages']}];
 const migratedLimits={budget:{mode:'reserved',limit_usd:'1.000000000000',period:{kind:'calendar_month',timezone:'UTC'}},rpm:5,tpm:5000,count_rpm:5};
 function carry(scope,cost,n,input,output){return {scope_id:scope,period_start:oldCost.period_start,period_end:oldCost.period_end,timezone_snapshot:'UTC',cost_usd:cost,reserved_usd:'0.000000000000',carried_cost_usd:cost,unknown_count:'0',settled_count:n,input_tokens:input,output_tokens:output,carried_settled_count:n,carried_input_tokens:input,carried_output_tokens:output,settled_unknown_tokens_count:'0',carried_settled_unknown_tokens_count:'0',detail_deleted_through:null};}
 const handoffInput={operation:'handoff',action:'import',target_id:handoffTarget.id,expected_version:JSON.parse(fenced.out).ack.result_version,source_id:handoffSource,source_ref:'synthetic-legacy-process-ledger',previous_writer_fenced_ref:'actual-legacy-fence-ack',previous_executors_stopped_ref:'actual-legacy-process-exit0',cutoff:oldCost.period_end,bundle:{identity:{kind:'human',issuer:handoffClaims.iss,subject:handoffClaims.sub,owner_principal_id:null},original_disabled:false,original_disabled_reason:null,original_security_fence_ref:null,source_principal_version:'1',safety:{inference_paused_at:null,inference_pause_reason:null,inference_pause_request_id:null,inference_safety_version:'1'},keys:[{id:oldKeyId,public_prefix:'legacy-public-id',name:'Synthetic old key',grant_snapshot:migratedGrants,validity_preset:'unlimited',expires_at:null,revoked_at:null,created_at:'2025-03-20T00:00:00Z',version:'1',original_created_by:handoffSource,original_created_command_id:randomUUID()}],scopes:[{budget_mode:'reserved',period_kind:'calendar_month',period_timezone:'UTC',id:primaryScope,key_id:null,version:'1',grants:migratedGrants,limits:migratedLimits,created_at:'2025-03-01T00:00:00Z',updated_at:'2025-03-01T00:00:00Z'},{budget_mode:'reserved',period_kind:'calendar_month',period_timezone:'UTC',id:oldKeyScope,key_id:oldKeyId,version:'1',grants:migratedGrants,limits:migratedLimits,created_at:'2025-03-01T00:00:00Z',updated_at:'2025-03-01T00:00:00Z'}],periods:[carry(primaryScope,oldCost.cost_usd,'3','300','60'),carry(oldKeyScope,'0.000140000000','1','100','20')],windows:[],requests:[],budgets:[],events:[]}};
 const importedCommand=randomUUID();const handoffImported=await maintenance(importedCommand,handoffInput);assert.equal(handoffImported.code,0);assert.equal(JSON.parse(handoffImported.out).state,'imported');
 const replayedImport=await maintenance(importedCommand,handoffInput);assert.equal(replayedImport.code,0);assert.equal(JSON.parse(replayedImport.out).ack.replayed,true);
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${handoffToken}`})).status,403,'import는 자동 개방하지 않는다');
 const importVersion=JSON.parse(handoffImported.out).ack.result_version;
 const premature=await maintenance(randomUUID(),{operation:'handoff',action:'activate',target_id:handoffTarget.id,expected_version:importVersion,source_id:handoffSource,previous_executors_stopped_ref:'not-the-joined-process'});assert.equal(premature.code,1);
 const activated=await maintenance(randomUUID(),{operation:'handoff',action:'activate',target_id:handoffTarget.id,expected_version:importVersion,source_id:handoffSource,previous_executors_stopped_ref:'actual-legacy-process-exit0'});assert.equal(activated.code,0);
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${handoffToken}`})).status,200);
 async function newHandoffCall(credential,stream=false){return fetch('http://127.0.0.1:4000/anthropic/v1/messages',{method:'POST',headers:{authorization:`Bearer ${credential}`,'content-type':'application/json','anthropic-version':'2023-06-01'},body:JSON.stringify({model:'synthetic-text',max_tokens:100,stream,messages:[{role:'user',content:'synthetic handoff'}]}),signal:AbortSignal.timeout(10000)});}
 const oldKeyOnNew=await newHandoffCall(oldKey);assert.equal(oldKeyOnNew.status,401);await oldKeyOnNew.arrayBuffer();
 const handoffFinished=new Set(),handoffStopped=new Set(),handoffWaiters=new Map(),handoffStopWaiters=new Map();const handoffChannel=`handoff_${randomUUID().replaceAll('-','')}`;
 db.on('notification',m=>{if(m.channel!==handoffChannel)return;const [kind,id]=m.payload.split(':');const set=kind==='finished'?handoffFinished:handoffStopped;const waiters=kind==='finished'?handoffWaiters:handoffStopWaiters;set.add(id);waiters.get(id)?.();waiters.delete(id);});await db.query(`LISTEN ${handoffChannel}`);
 await db.query(`CREATE FUNCTION ${schema}.gw_fixture_handoff_notify() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.principal_id='${handoffTarget.id}'::uuid THEN IF NEW.phase='finished' THEN PERFORM pg_notify('${handoffChannel}','finished:'||NEW.id::text); END IF; IF NEW.execution_stopped_at IS NOT NULL THEN PERFORM pg_notify('${handoffChannel}','stopped:'||NEW.id::text); END IF; END IF; RETURN NEW; END $$; CREATE TRIGGER gw_fixture_handoff_notify AFTER UPDATE ON ${schema}.gw_requests FOR EACH ROW EXECUTE FUNCTION ${schema}.gw_fixture_handoff_notify()`);
 async function handoffWait(id,stopped=false){const set=stopped?handoffStopped:handoffFinished,waiters=stopped?handoffStopWaiters:handoffWaiters;if(set.has(id))return;await bounded(new Promise(resolve=>waiters.set(id,resolve)),6000,'handoff durable lifecycle');}
 const newResponse=await newHandoffCall(handoffToken);assert.equal(newResponse.status,200);const handoffPaidId=newResponse.headers.get('x-gateway-request-id');await newResponse.arrayBuffer();await handoffWait(handoffPaidId,true);
 const freshKey=await fetch('http://127.0.0.1:4000/gateway/keys',{method:'POST',headers:{authorization:`Bearer ${handoffToken}`,'content-type':'application/json','x-gateway-command-id':randomUUID()},body:JSON.stringify({name:'Synthetic post-handoff key',validity:'unlimited',grants:migratedGrants,limits:{budget:null,rpm:null,tpm:null,count_rpm:null}})});assert.equal(freshKey.status,201);const postHandoffKey=(await freshKey.json()).key;
 providerMode='stalled';providerStopped=new Promise(r=>{providerStoppedResolve=r;});const unknownStream=await newHandoffCall(handoffToken,true);assert.equal(unknownStream.status,200);const handoffUnknownId=unknownStream.headers.get('x-gateway-request-id');const unknownReader=unknownStream.body.getReader();assert((await unknownReader.read()).value.length>0);
 const targetState=await api('GET',`/gateway/admin/principals/${handoffTarget.id}`);const targetPolicy=targetState.body.principal.policy;
 const newFence=await maintenance(randomUUID(),{operation:'handoff',action:'fence',target_id:handoffTarget.id,expected_version:targetPolicy.version,source_id:handoffSource,grants:targetPolicy.grants,limits:targetPolicy.limits});assert.equal(newFence.code,0);
 const fenceVersion=(await api('GET',`/gateway/admin/principals/${handoffTarget.id}`)).body.principal.version;
 const runningHandoff=await db.query(`SELECT phase FROM ${schema}.gw_requests WHERE id=$1`,[handoffUnknownId]);assert.equal(runningHandoff.rows[0].phase,'dispatching');
 const rollbackBase={operation:'handoff',action:'rollback',target_id:handoffTarget.id,expected_version:fenceVersion,source_id:handoffSource,new_executors_stopped_ref:'actual-new-request-joined',legacy_represents_all_state:false,legacy_all_credentials_blocked:true};
 const whileRunning=await maintenance(randomUUID(),rollbackBase);assert.equal(whileRunning.code,1,'fence/lease는 실제 송신 종료가 아니다');
 await unknownReader.cancel();await bounded(providerStopped,2000,'handoff actual upstream dropped');await handoffWait(handoffUnknownId,true);providerMode='strict';
 const currentTarget=(await api('GET',`/gateway/admin/principals/${handoffTarget.id}`)).body.principal;rollbackBase.expected_version=currentTarget.version;
 assert.equal((await maintenance(randomUUID(),{...rollbackBase,legacy_all_credentials_blocked:false})).code,1);
 assert.equal((await maintenance(randomUUID(),rollbackBase)).code,0);
 for(const credential of [handoffToken,postHandoffKey,oldKey]){const rejected=await newHandoffCall(credential);assert([401,403].includes(rejected.status));await rejected.arrayBuffer();}
 oldWriter=await legacyStart(oldKey);for(const credential of [handoffToken,oldKey,postHandoffKey])assert.equal(await oldCall(oldWriter,credential),503);await oldWriter.stop();
 const retained=await db.query(`SELECT r.accounting_state,r.cost_usd,b.remaining_reservation_usd::text,b.remaining_reserved_tokens::text,r.execution_stopped_at IS NOT NULL AS stopped FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id WHERE r.id=$1`,[handoffUnknownId]);assert.deepEqual(retained.rows.map(r=>Object.values(r)),[['unknown',null,'0.004296000000','4196',true]]);
 const carried=await db.query(`SELECT cost_usd::text,carried_cost_usd::text FROM ${schema}.gw_usage_totals WHERE scope_id=$1 AND period_start='2025-03-01T00:00:00Z'`,[primaryScope]);assert.deepEqual(carried.rows.map(r=>Object.values(r)),[['0.000420000000','0.000420000000']]);
 console.log('S10_ACTUAL_TWO_WRITER_HANDOFF_ROLLBACK_REPLAY_AND_ALL_CREDENTIAL_BLOCK_OK');

 const csrfResponse=await api('GET','/gateway/csrf',undefined,undefined,true);assert.equal(csrfResponse.status,200);const csrf=csrfResponse.body.token;
 const keyInput={name:'Synthetic main key',validity:'3_months',grants:[{provider:'anthropic',model:'synthetic-text',capabilities:['count_tokens','messages']}],limits:{budget:null,rpm:null,tpm:null,count_rpm:null}};
 const issueCommand=randomUUID();const rejected=await api('POST','/gateway/keys',keyInput,issueCommand,true);assert.equal(rejected.status,403);
 const issued=await api('POST','/gateway/keys',keyInput,issueCommand,true,{origin:'https://m-gated.example.test','x-csrf-token':csrf});assert.equal(issued.status,201);assert(/^mgw_v1_[a-f0-9]{32}_[A-Za-z0-9_-]{43}$/.test(issued.body.key),'one-time key shape');const personalKey=issued.body.key;const personalId=issued.body.metadata.id;
 const replay=await api('POST','/gateway/keys',keyInput,issueCommand);assert.equal(replay.status,409);assert.equal(replay.body.error.code,'key_secret_unavailable');assert.equal(replay.body.error.details.key_id,personalId);assert.equal(replay.headers.get('x-gateway-command-replayed'),'true');assert(!Object.hasOwn(replay.body,'key'));
 const hashOnly=await db.query(`SELECT octet_length(secret_hash) AS bytes,hash_version FROM ${schema}.gw_virtual_keys WHERE id=$1`,[personalId]);assert.equal(hashOnly.rows[0].bytes,32);assert.equal(hashOnly.rows[0].hash_version,1);
 async function virtualCall(key){return fetch('http://127.0.0.1:4000/v1/messages',{method:'POST',headers:{authorization:`Bearer ${key}`,'content-type':'application/json','anthropic-version':'2023-06-01'},body:JSON.stringify({model:'synthetic-text',max_tokens:100,messages:[{role:'user',content:'synthetic virtual key'}]}),signal:AbortSignal.timeout(5000)});}
 let beforeKeyCalls=providerCalls;let keyed=await virtualCall(personalKey);assert.equal(keyed.status,200);await keyed.arrayBuffer();assert.equal(providerCalls-beforeKeyCalls,2);
 let r=await get('/gateway/me',{authorization:`Bearer ${personalKey}`});assert.equal(r.status,403);await r.arrayBuffer();
 const selected=await api('GET',`/gateway/admin/usage?principal_id=${strictPrincipal}&key_id=${personalId}`);assert.equal(selected.status,200);assert.equal(selected.body.budget_balances.length,2);assert.equal(selected.body.budget_balances[0].settled_usd,'0.000280000000');assert.equal(selected.body.budget_balances[1].settled_usd,'0.000140000000');
 const serviceCommand=randomUUID();const service=await api('POST','/gateway/admin/principals',{display_name:'Synthetic service',management_owner_id:strictPrincipal},serviceCommand);assert.equal(service.status,201);const serviceId=service.body.target.id;assert.equal(service.body.operation,'create_service');const serviceReplay=await api('POST','/gateway/admin/principals',{display_name:'Synthetic service',management_owner_id:strictPrincipal},serviceCommand);assert.equal(serviceReplay.status,201);assert.equal(serviceReplay.body.target.id,serviceId);assert.equal(serviceReplay.body.replayed,true);
 const servicePath=`/gateway/admin/principals/${serviceId}`;let serviceDetail=await api('GET',servicePath);assert.equal(serviceDetail.status,200);assert.equal(serviceDetail.body.principal.policy.grants.length,0);
 const servicePolicy={expected_version:serviceDetail.body.principal.policy.version,disabled:false,grants:keyInput.grants,limits:keyInput.limits,reason:'Synthetic service grants'};r=await api('PUT',`${servicePath}/policy`,servicePolicy);assert.equal(r.status,200);assert.equal(r.body.result_version,'2');
 const serviceIssued=await api('POST','/gateway/keys',{...keyInput,service_principal_id:serviceId});assert.equal(serviceIssued.status,201);const serviceKey=serviceIssued.body.key;beforeKeyCalls=providerCalls;keyed=await virtualCall(serviceKey);assert.equal(keyed.status,200);await keyed.arrayBuffer();assert.equal(providerCalls-beforeKeyCalls,2);
 // S3에서 남긴 unknown을 runtime 종료 출처의 관리자 입력으로 한 번만 처리한다.
 const unknownDetail=await api('GET',`/gateway/admin/requests/${cancelledId}`);assert.equal(unknownDetail.status,200);assert.equal(unknownDetail.body.accounting_state,'unknown');assert(unknownDetail.body.execution_stopped_at);
 const manual={expected_version:unknownDetail.body.version,decision:'charged',cost_usd:'0.010000000000',reason:'Synthetic manual entry'};const settlementCommand=randomUUID();r=await api('PUT',`/gateway/admin/requests/${cancelledId}/settlement`,manual,settlementCommand);assert.equal(r.status,200);assert.equal(r.body.effect,'changed');r=await api('PUT',`/gateway/admin/requests/${cancelledId}/settlement`,manual,settlementCommand);assert.equal(r.status,200);assert.equal(r.body.replayed,true);
 r=await api('PUT',`/gateway/admin/requests/${cancelledId}/settlement`,{...manual,expected_version:'1'});assert.equal(r.status,200);assert.equal(r.body.effect,'noop');const manualDetail=await api('GET',`/gateway/admin/requests/${cancelledId}`);assert.equal(manualDetail.body.cost_usd,'0.010000000000');assert.equal(manualDetail.body.settlement.source,'manual_entry');assert.equal(manualDetail.body.settlement.execution_stop_source,'runtime');assert.equal(manualDetail.body.settlement.actor_principal_id,strictPrincipal);
 const usage=await api('GET','/gateway/admin/usage?group_by=provider');assert.equal(usage.status,200);assert.equal(usage.body.balance_context,'aggregate_no_balance');assert.equal(usage.body.budget_balances.length,0);assert.equal(usage.body.block_state,null);
 r=await api('GET','/gateway/admin/principals?limit=1');assert.equal(r.status,200);const summary=r.body.items[0];assert(!Object.hasOwn(summary,'policy'));assert(Object.hasOwn(summary,'policy_version'));assert(Object.hasOwn(summary,'disabled'));
 const cursor=r.body.next_cursor;assert(cursor);r=await api('GET',`/gateway/admin/keys?cursor=${encodeURIComponent(cursor)}`);assert.equal(r.status,400);
 // owner 변경은 장부를 이동하지 않고 새 owner의 관리 정지가 서비스 송신을 차단한다.
 r=await api('PATCH',servicePath,{expected_version:'1',management_owner_id:principal});assert.equal(r.status,200);r=await api('PUT',`/gateway/admin/principals/${principal}/policy`,{...servicePolicy,expected_version:'1',disabled:true,reason:'Synthetic owner stop'});assert.equal(r.status,200);beforeKeyCalls=providerCalls;keyed=await virtualCall(serviceKey);assert.equal(keyed.status,403);assert.equal(keyed.headers.get('x-gateway-error-code'),'owner_inactive');await keyed.arrayBuffer();assert.equal(providerCalls,beforeKeyCalls);
 r=await api('PUT',`/gateway/admin/principals/${principal}/policy`,{...servicePolicy,expected_version:'2',disabled:false,reason:'Synthetic owner restore'});assert.equal(r.status,200);
 const revokeCommand=randomUUID();r=await api('DELETE',`/gateway/keys/${personalId}?expected_version=1`,undefined,revokeCommand);assert.equal(r.status,204);r=await api('DELETE',`/gateway/keys/${personalId}?expected_version=1`);assert.equal(r.status,204);keyed=await virtualCall(personalKey);assert.equal(keyed.status,401);await keyed.arrayBuffer();
 const safety=await api('GET',servicePath);r=await api('POST',`${servicePath}/inference-pause/release`,{expected_version:safety.body.principal.inference_safety.version,reason:'Synthetic explicit safety check'});assert.equal(r.status,200);assert.equal(r.body.effect,'noop');
 await stop();console.log('S5_ACTUAL_MAIN_KEYS_SERVICE_COMMAND_SETTLEMENT_OWNER_OK');

 // S6의 새 Provider도 실제 release main/검증 TLS·공유 client/비영 예약에 연결한다.
 assert.match(readFileSync('/etc/hosts','utf8'),/127\.0\.0\.1\s+api\.openai\.com(?:\s|$)/m);
 const responsesProfile=`  - provider: openai
    base_url: https://api.openai.com
    profiles:
      - model: synthetic-text
        api: responses
        profile_version: responses-context-fixture-v1
        pricing_version: responses-synthetic-price-v1
        fixture_version: responses-native-v1
        actual_models: [synthetic-text]
        features: [cache, thinking, client_tools, json_schema, inline_png]
        input_bound_contract:
          kind: openai_responses_context_v1
          context_tokens: 4096
          input_categories: [input]
        max_input_tokens: 4096
        max_output_tokens: 1024
        max_body_bytes: 8388608
        max_response_bytes: 16777216
        rates:
          input: '1.000000000000'
          output: '2.000000000000'
          cache_read: '0.500000000000'
`;
 const s7Profiles=`      - model: synthetic-text
        api: openai_chat
        chat_output_limit_field: max_completion_tokens
        profile_version: chat-context-fixture-v1
        pricing_version: chat-price-v1
        fixture_version: chat-native-v1
        actual_models: [synthetic-text]
        features: [cache, thinking]
        input_bound_contract:
          kind: openai_chat_context_v1
          context_tokens: 4096
          input_categories: [messages]
        max_input_tokens: 4096
        max_output_tokens: 1024
        max_body_bytes: 8388608
        max_response_bytes: 16777216
        rates:
          input: '1.000000000000'
          output: '2.000000000000'
          cache_read: '0.500000000000'
      - model: synthetic-text
        api: embeddings
        profile_version: embeddings-fixture-v1
        pricing_version: embeddings-price-v1
        fixture_version: embeddings-native-v1
        actual_models: [synthetic-text]
        embedding_dimensions: {default: 3, max: 4096}
        embedding_encodings: [float, base64]
        max_input_tokens: 4096
        max_body_bytes: 8388608
        max_response_bytes: 16777216
        rates:
          input: '1.000000000000'
`;
 const s8Profile=`  - provider: deepseek
    base_url: https://api.deepseek.com
    profiles:
      - model: synthetic-text
        api: deepseek_chat
        billing_time_rule: fixed_admission_v1
        profile_version: deepseek-context-fixture-v1
        pricing_version: deepseek-price-v1
        fixture_version: deepseek-native-v1
        actual_models: [synthetic-text]
        features: [cache, thinking, client_tools, json_schema]
        input_bound_contract:
          kind: deepseek_chat_context_v1
          context_tokens: 4096
          input_categories: [messages]
        max_input_tokens: 4096
        max_output_tokens: 1024
        max_body_bytes: 8388608
        max_response_bytes: 16777216
        rates:
          input: '1.000000000000'
          output: '2.000000000000'
          cache_read: '0.500000000000'
`;
 const responsesConfig=strictConfig.replace('execution_ceiling:',responsesProfile+s7Profiles+s8Profile+'execution_ceiling:');writeFileSync(join(directory,'config.yaml'),responsesConfig);app=start();await waitStatus('/readyz',200);
 const rp=Buffer.from(JSON.stringify({iss:'https://auth.example.test',aud:'https://example.test',sub:'responses-main-human',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200})).toString('base64url');const rs=`${header}.${rp}`;const rt=`${rs}.${createSign('RSA-SHA256').update(rs).sign(keys.privateKey).toString('base64url')}`;
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${rt}`})).status,200);const rid=(await db.query(`SELECT id FROM ${schema}.gw_principals WHERE subject='responses-main-human'`)).rows[0].id;
 await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2,budget_mode='reserved',budget_limit_usd=1,tpm_limit=5000 WHERE principal_id=$1`,[rid,JSON.stringify([{provider:'openai',model:'synthetic-text',capabilities:['responses']}])]);
 const finishChannel=`${schema}_s6`;
 const finished=new Set();const finishWaiters=new Map();db.on('notification',message=>{if(message.channel===finishChannel){finished.add(message.payload);finishWaiters.get(message.payload)?.();finishWaiters.delete(message.payload);}});
 await db.query(`LISTEN ${finishChannel}`);await db.query(`CREATE FUNCTION ${schema}.s6_finished() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='finished' THEN PERFORM pg_notify('${finishChannel}',NEW.id::text); END IF; RETURN NEW; END $$`);await db.query(`CREATE TRIGGER s6_finished AFTER UPDATE ON ${schema}.gw_requests FOR EACH ROW EXECUTE FUNCTION ${schema}.s6_finished()`);
 async function finalized(id){if(finished.has(id))return;await bounded(new Promise(resolve=>finishWaiters.set(id,resolve)),5000,'Responses actual finalization commit');}
 const responsesIdle=sampleMemory();let responsesPeak={...responsesIdle};const beforeResponses=responsesCalls;
 for(const stream of [false,true])for(const status of ['completed','incomplete']){
  responseStatus=status;let started;const startedSignal=new Promise(r=>{started=r;});responsesStarted=started;providerMode=stream&&status==='incomplete'?'responses_large':'responses';
  const send=fetch('http://127.0.0.1:4000/openai/v1/responses',{method:'POST',headers:{authorization:`Bearer ${rt}`,'content-type':'application/json'},body:JSON.stringify({model:'synthetic-text',input:'synthetic native Responses',store:false,max_output_tokens:100,stream}),signal:AbortSignal.timeout(10000)});
  await bounded(startedSignal,3000,'Responses actual dispatch barrier');const held=await db.query(`SELECT b.reservation_usd::text,b.remaining_reservation_usd::text,b.reserved_tokens::text FROM ${schema}.gw_request_budgets b JOIN ${schema}.gw_requests r ON r.id=b.request_id WHERE r.principal_id=$1 ORDER BY r.admitted_at DESC LIMIT 1`,[rid]);assert.deepEqual(held.rows.map(r=>Object.values(r)),[['0.004296000000','0.004296000000','4196']]);responsesStarted=null;responsesRelease();
  const result=await send;assert.equal(result.status,200);const requestId=result.headers.get('x-gateway-request-id');let text='';let bytes=0;for await(const chunk of result.body){bytes+=chunk.length;if(bytes<65536)text+=Buffer.from(chunk).toString();const memory=sampleMemory();responsesPeak.rssKiB=Math.max(responsesPeak.rssKiB,memory.rssKiB);responsesPeak.anonKiB=Math.max(responsesPeak.anonKiB,memory.anonKiB);}
  if(!stream)assert.equal(JSON.parse(text).status,status);else if(status==='incomplete')assert(bytes>9*1024*1024);else assert(text.includes('response.completed'));
  await finalized(requestId);const rows=await db.query(`SELECT r.accounting_state,r.outcome,r.cost_usd::text,b.remaining_reservation_usd::text,b.remaining_reserved_tokens::text,e.normalized_usage->>'input_tokens' AS input,e.normalized_usage->>'reasoning_tokens' AS reasoning FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id JOIN ${schema}.gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1`,[requestId]);assert.deepEqual(rows.rows.map(r=>Object.values(r)),[['settled','completed','0.000120000000','0.000000000000','0','100','10']]);
 }
 assert.equal(responsesCalls-beforeResponses,4);const responseTotals=await db.query(`SELECT t.cost_usd::text,t.reserved_usd::text,t.unknown_count::text,t.settled_count::text,t.input_tokens::text,t.output_tokens::text FROM ${schema}.gw_usage_totals t JOIN ${schema}.gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1`,[rid]);assert.deepEqual(responseTotals.rows.map(r=>Object.values(r)),[['0.000480000000','0.000000000000','0','4','400','80']]);await stop();
 console.log(`S6_ACTUAL_MAIN_RESPONSES_TLS_INCOMPLETE_RESERVATION_OK provider_calls=4 idle_rss_kib=${responsesIdle.rssKiB} idle_anon_kib=${responsesIdle.anonKiB} sampled_peak_rss_kib=${responsesPeak.rssKiB} sampled_peak_anon_kib=${responsesPeak.anonKiB}`);

 // S7 실제 release main: Chat 비영 예약/usage-only/DONE와 bounded Embeddings actual.
 app=start();await waitStatus('/readyz',200);
 const sp=Buffer.from(JSON.stringify({iss:'https://auth.example.test',aud:'https://example.test',sub:'s7-main-human',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200})).toString('base64url');const ss=`${header}.${sp}`;const st=`${ss}.${createSign('RSA-SHA256').update(ss).sign(keys.privateKey).toString('base64url')}`;
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${st}`})).status,200);const sid=(await db.query(`SELECT id FROM ${schema}.gw_principals WHERE subject='s7-main-human'`)).rows[0].id;
 await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2,budget_mode='reserved',budget_limit_usd=1,tpm_limit=5000 WHERE principal_id=$1`,[sid,JSON.stringify([{provider:'openai',model:'synthetic-text',capabilities:['chat_completions','embeddings']}])]);
 for(const stream of [false,true]){
  let signal;const startedSignal=new Promise(r=>{signal=r;});s7Started=signal;
  const send=fetch('http://127.0.0.1:4000/openai/v1/chat/completions',{method:'POST',headers:{authorization:`Bearer ${st}`,'content-type':'application/json'},body:JSON.stringify({model:'synthetic-text',messages:[{role:'user',content:'synthetic Chat'}],max_completion_tokens:100,stream,...(stream?{stream_options:{include_usage:true}}:{})}),signal:AbortSignal.timeout(10000)});
  await bounded(startedSignal,3000,'Chat actual dispatch barrier');const held=await db.query(`SELECT b.reservation_usd::text,b.remaining_reservation_usd::text,b.reserved_tokens::text FROM ${schema}.gw_request_budgets b JOIN ${schema}.gw_requests r ON r.id=b.request_id WHERE r.principal_id=$1 ORDER BY r.admitted_at DESC LIMIT 1`,[sid]);assert.deepEqual(held.rows.map(r=>Object.values(r)),[['0.004296000000','0.004296000000','4196']]);s7Started=null;s7Release();const result=await send;assert.equal(result.status,200);const id=result.headers.get('x-gateway-request-id');const native=await result.text();if(stream)assert(native.endsWith('data: [DONE]\n\n'));else assert.equal(JSON.parse(native).choices[0].finish_reason,'length');await finalized(id);
  const row=await db.query(`SELECT r.accounting_state,r.cost_usd::text,b.remaining_reservation_usd::text,e.normalized_usage->>'reasoning_tokens' AS reasoning FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id JOIN ${schema}.gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1`,[id]);assert.deepEqual(row.rows.map(r=>Object.values(r)),[['settled','0.000120000000','0.000000000000','10']]);
 }
 const strictEmbeddingRejection=await fetch('http://127.0.0.1:4000/openai/v1/embeddings',{method:'POST',headers:{authorization:`Bearer ${st}`,'content-type':'application/json'},body:JSON.stringify({model:'synthetic-text',input:'synthetic'})});assert.equal(strictEmbeddingRejection.status,400);assert.equal(strictEmbeddingRejection.headers.get('x-gateway-error-code'),'unsupported_strict_policy');await strictEmbeddingRejection.arrayBuffer();await stop();
 const actualConfig=responsesConfig.replace("budget: {mode: reserved,", "budget: {mode: actual,").replace('  tpm: 5000','  tpm: null');writeFileSync(join(directory,'config.yaml'),actualConfig);await db.query(`UPDATE ${schema}.gw_policies SET budget_mode='actual',tpm_limit=NULL WHERE principal_id=$1`,[sid]);app=start();await waitStatus('/readyz',200);
 for(const encoding of ['float','base64']){const result=await fetch('http://127.0.0.1:4000/openai/v1/embeddings',{method:'POST',headers:{authorization:`Bearer ${st}`,'content-type':'application/json'},body:JSON.stringify({model:'synthetic-text',input:['synthetic'],dimensions:3,encoding_format:encoding}),signal:AbortSignal.timeout(10000)});if(result.status!==200){const rawCode=result.headers.get('x-gateway-error-code');const code=['unsupported_strict_policy','configuration_conflict','rate_limited','policy_changed','inference_paused','unsettled_usage_block','upstream_protocol_error','upstream_auth_error','not_ready','invalid_request'].includes(rawCode)?rawCode:'other';const observed=await db.query(`SELECT budget_mode='actual' AS mode_actual,tpm_limit IS NULL AS tpm_null,(SELECT count(*)::int FROM ${schema}.gw_requests WHERE principal_id=$1) AS rows FROM ${schema}.gw_policies WHERE principal_id=$1 AND key_id IS NULL`,[sid]);const row=observed.rows[0];console.log(`S7_ACTUAL_MAIN_DIAGNOSTIC encoding=${encoding} status=${result.status} code=${code} provider_calls=${s7Calls} rows=${row.rows} mode_actual=${row.mode_actual} tpm_null=${row.tpm_null} ceiling_actual=${actualConfig.includes('budget: {mode: actual,')} ceiling_tpm_null=${!actualConfig.includes('  tpm: 5000')}`);}assert.equal(result.status,200);const id=result.headers.get('x-gateway-request-id');const native=await result.json();assert.equal(native.usage.total_tokens,100);assert.equal(native.data.length,1);await finalized(id);const accounted=await db.query(`SELECT accounting_state,cost_usd::text,reason_code FROM ${schema}.gw_requests WHERE id=$1`,[id]);assert.deepEqual(accounted.rows.map(r=>Object.values(r)),[['settled','0.000100000000','completed']]);}
 const s7Totals=await db.query(`SELECT t.cost_usd::text,t.reserved_usd::text,t.unknown_count::text,t.input_tokens::text,t.output_tokens::text FROM ${schema}.gw_usage_totals t JOIN ${schema}.gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1`,[sid]);assert.deepEqual(s7Totals.rows.map(r=>Object.values(r)),[['0.000440000000','0.000000000000','0','400','40']]);assert.equal(s7Calls,4);await stop();console.log('S7_ACTUAL_MAIN_CHAT_EMBEDDINGS_TLS_USAGE_RESERVATION_OK provider_calls=4');

 // S8 실제 release main: 세 Provider를 같은 client/pool로 조립해 자격/경로의 교차를 직접 검사한다.
 assert.match(readFileSync('/etc/hosts','utf8'),/127\.0\.0\.1\s+api\.deepseek\.com(?:\s|$)/m);
 const s8Config=responsesConfig.replace('  rpm: 5','  rpm: 20').replace('  tpm: 5000','  tpm: 10000');assert(s8Config.includes('  rpm: 20'));assert(s8Config.includes('  tpm: 10000'));
 writeFileSync(join(directory,'config.yaml'),s8Config);app=start();await waitStatus('/readyz',200);
 const dp=Buffer.from(JSON.stringify({iss:'https://auth.example.test',aud:'https://example.test',sub:'s8-main-human',email:'alice@example.test',email_verified:true,exp:Math.floor(Date.now()/1000)+7200})).toString('base64url');const ds=`${header}.${dp}`;const dt=`${ds}.${createSign('RSA-SHA256').update(ds).sign(keys.privateKey).toString('base64url')}`;
 assert.equal((await get('/gateway/me',{authorization:`Bearer ${dt}`})).status,200);const did=(await db.query(`SELECT id FROM ${schema}.gw_principals WHERE subject='s8-main-human'`)).rows[0].id;
 const dsGrants=[{provider:'deepseek',model:'synthetic-text',capabilities:['chat_completions']},{provider:'openai',model:'synthetic-text',capabilities:['chat_completions']},{provider:'anthropic',model:'synthetic-text',capabilities:['messages','count_tokens']}];
 await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2,budget_mode='reserved',budget_limit_usd=1,tpm_limit=10000 WHERE principal_id=$1`,[did,JSON.stringify(dsGrants)]);
 for(const include of [null,undefined,false,true]){
  const stream=include!==null;let signal;const startedSignal=new Promise(r=>{signal=r;});s8Started=signal;
  const body={model:'synthetic-text',messages:[{role:'user',content:'synthetic DeepSeek'}],max_tokens:100,stream,thinking:{type:'enabled'},response_format:{type:'json_object'},...(include!==null&&include!==undefined?{stream_options:{include_usage:include}}:{})};
  const send=fetch('http://127.0.0.1:4000/deepseek/v1/chat/completions',{method:'POST',headers:{authorization:`Bearer ${dt}`,'content-type':'application/json'},body:JSON.stringify(body),signal:AbortSignal.timeout(10000)});
  await bounded(startedSignal,3000,'DeepSeek actual dispatch barrier');const held=await db.query(`SELECT b.reservation_usd::text,b.remaining_reservation_usd::text,b.reserved_tokens::text FROM ${schema}.gw_request_budgets b JOIN ${schema}.gw_requests r ON r.id=b.request_id WHERE r.principal_id=$1 ORDER BY r.admitted_at DESC LIMIT 1`,[did]);assert.deepEqual(held.rows.map(r=>Object.values(r)),[['0.004296000000','0.004296000000','4196']]);s8Started=null;s8Release();const result=await send;assert.equal(result.status,200);const id=result.headers.get('x-gateway-request-id');const native=await result.text();if(stream){assert(native.endsWith('data: [DONE]\n\n'));assert(!native.includes('"choices":[]'));const content=native.split('\n').filter(l=>l.startsWith('data: {')).map(l=>JSON.parse(l.slice(6)).choices[0].delta.content??'').join('');assert.deepEqual(JSON.parse(content),{ok:true});}else{assert.equal(JSON.parse(native).usage.prompt_cache_miss_tokens,60);assert.deepEqual(JSON.parse(JSON.parse(native).choices[0].message.content),{ok:true});}await finalized(id);
  const row=await db.query(`SELECT r.accounting_state,r.cost_usd::text,b.remaining_reservation_usd::text,e.normalized_usage->>'input_tokens' AS input,e.normalized_usage->>'output_tokens' AS output,e.normalized_usage->>'reasoning_tokens' AS reasoning,e.provider_usage->>'prompt_cache_hit_tokens' AS hit,e.provider_usage->>'prompt_cache_miss_tokens' AS miss FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id JOIN ${schema}.gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1`,[id]);assert.deepEqual(row.rows.map(r=>Object.values(r)),[['settled','0.000120000000','0.000000000000','100','20','10','40','60']]);
 }
 assert.equal(s8Calls,4);const s8Totals=await db.query(`SELECT t.cost_usd::text,t.unknown_count::text,t.input_tokens::text,t.output_tokens::text FROM ${schema}.gw_usage_totals t JOIN ${schema}.gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1`,[did]);assert.deepEqual(s8Totals.rows.map(r=>Object.values(r)),[['0.000480000000','0','400','80']]);await stop();
 // 시간근거미확정: actual token-proof는독립유지하되금액은추정하지않는다. 새키/다른Provider로도우회없음.
 const unknownConfig=s8Config.replace("budget: {mode: reserved,", "budget: {mode: actual,").replace('  tpm: 10000','  tpm: null').replace('billing_time_rule: fixed_admission_v1','billing_time_rule: deepseek_time_unresolved_v1');writeFileSync(join(directory,'config.yaml'),unknownConfig);await db.query(`UPDATE ${schema}.gw_policies SET budget_mode='actual' WHERE principal_id=$1`,[did]);app=start();await waitStatus('/readyz',200);
 const unknownResponse=await fetch('http://127.0.0.1:4000/deepseek/v1/chat/completions',{method:'POST',headers:{authorization:`Bearer ${dt}`,'content-type':'application/json'},body:JSON.stringify({model:'synthetic-text',messages:[{role:'user',content:'synthetic time unknown'}],max_tokens:100}),signal:AbortSignal.timeout(10000)});assert.equal(unknownResponse.status,200);const unknownId=unknownResponse.headers.get('x-gateway-request-id');assert.equal((await unknownResponse.json()).usage.total_tokens,120);await finalized(unknownId);
 const unresolved=await db.query(`SELECT r.accounting_state,r.cost_usd,r.pricing_snapshot->>'billing_time_rule' AS time,b.remaining_reserved_tokens::text FROM ${schema}.gw_requests r JOIN ${schema}.gw_request_budgets b ON b.request_id=r.id WHERE r.id=$1`,[unknownId]);assert.deepEqual(unresolved.rows.map(r=>Object.values(r)),[['unknown',null,'deepseek_time_unresolved_v1','4196']]);
 const s8Issued=await fetch('http://127.0.0.1:4000/gateway/keys',{method:'POST',headers:{authorization:`Bearer ${dt}`,'content-type':'application/json','x-gateway-command-id':randomUUID()},body:JSON.stringify({name:'Synthetic S8 blocked key',validity:'3_months',grants:dsGrants,limits:{budget:{mode:'actual',limit_usd:'1.000000000000',period:{kind:'calendar_month',timezone:'UTC'}},rpm:null,tpm:null,count_rpm:null}})});assert.equal(s8Issued.status,201);const blockedKey=(await s8Issued.json()).key;
 const s8BeforeFreeCount=providerCalls;let probeOrdinal=0;
 for(const [path,token,body] of [['/deepseek/v1/chat/completions',blockedKey,{model:'synthetic-text',messages:[{role:'user',content:'synthetic blocked'}],max_tokens:100}],['/openai/v1/chat/completions',blockedKey,{model:'synthetic-text',messages:[{role:'user',content:'synthetic blocked'}],max_completion_tokens:100}],['/anthropic/v1/messages',dt,{model:'synthetic-text',messages:[{role:'user',content:'synthetic blocked'}],max_tokens:100}]]){const r=await fetch(`http://127.0.0.1:4000${path}`,{method:'POST',headers:{authorization:`Bearer ${token}`,'content-type':'application/json','anthropic-version':'2023-06-01'},body:JSON.stringify(body)});probeOrdinal++;if(r.status!==403){const code=r.headers.get('x-gateway-error-code');console.log(`S8_PROBE_DIAGNOSTIC ordinal=${probeOrdinal} status=${r.status} code=${['rate_limited','concurrency_limited','unsupported_strict_policy','access_denied','unsettled_usage_block'].includes(code)?code:'other'}`);}assert.equal(r.status,403);assert.equal(r.headers.get('x-gateway-error-code'),'unsettled_usage_block');await r.arrayBuffer();}
 assert.equal(providerCalls-s8BeforeFreeCount,1); // 마지막 Anthropic부모만미송신이고독립무료count1은정상이다.
 assert.equal(s8Calls,5);assert.equal(s7Calls,4);assert.equal(responsesCalls,4);await stop();console.log('S8_ACTUAL_MAIN_DEEPSEEK_TLS_LAST_CHUNK_TIME_UNKNOWN_CROSS_PROVIDER_KEY_OK provider_calls=5 fixed_cost=0.000480000000');



 if(loadCase){
  loadActive=true;
  const allowed=Array.from({length:16},(_,i)=>`load${i}@example.test`);
  const loadConfig=s8Config.split('execution_ceiling:')[0].replace('  - alice@example.test\nadmin_emails:',allowed.map(e=>'  - '+e).join('\n')+'\n  - alice@example.test\nadmin_emails:').replace('        internal_count: true\n','').replaceAll('context_tokens: 4096','context_tokens: 8388608').replaceAll('max_input_tokens: 4096','max_input_tokens: 8388608')+'execution_ceiling:\n  budget: null\n  rpm: null\n  tpm: null\n  count_rpm: null\n';
  writeFileSync(join(directory,'config.yaml'),loadConfig);providerMode='load';app=start();await waitStatus('/readyz',200);
  function loadToken(i){const claims={iss:'https://auth.example.test',aud:'https://example.test',sub:i===-1?'main-human':`load-human-${i}`,email:i===-1?'alice@example.test':allowed[i],email_verified:true,exp:Math.floor(Date.now()/1000)+7200,name:'Synthetic load'};const part=`${header}.${Buffer.from(JSON.stringify(claims)).toString('base64url')}`;return `${part}.${createSign('RSA-SHA256').update(part).sign(keys.privateKey).toString('base64url')}`;}
  const ids=[];
  for(let i=0;i<16;i++){const me=await get('/gateway/me',{authorization:`Bearer ${loadToken(i)}`});assert.equal(me.status,200);const id=(await me.json()).principal.id;ids.push(id);await db.query(`UPDATE ${schema}.gw_policies SET allow_rules=$2 WHERE principal_id=$1`,[id,JSON.stringify([{provider:'anthropic',model:'synthetic-text',capabilities:['messages']}])]);}
  const cases=loadCase==='all'?['baseline','boundary','mixed','soak']:[loadCase];
  for(const caseName of cases){await measureLoad({caseName,token:loadToken,adminToken:loadToken(-1),db,schema,principalIds:ids,finalized,sample:sampleMemory,startWave(seconds){let release;const gate=new Promise(r=>{release=r;});loadWave={seconds,gate};return release;}});}
  await stop();loadActive=false;writeFileSync(join(directory,'config.yaml'),unknownConfig);
 }

 failJwks=true;readySignal=new Promise(r=>{readyResolve=r;});app=start();await bounded(readySignal,8000,'bounded initial JWKS request');await waitStatus('/readyz',503);assert.equal((await get('/livez')).status,200);assert.equal((await get('/gateway/me',{cookie})).status,503);
 failJwks=false;await waitStatus('/readyz',200,20000);assert.equal((await get('/gateway/me',{cookie})).status,200);assert(calls>=3);await stop();
 await db.query(`ALTER TABLE ${schema}.gw_usage_events RENAME TO gw_hidden_usage_events`);app=start();result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/schema_unavailable/);app=null;await db.query(`ALTER TABLE ${schema}.gw_hidden_usage_events RENAME TO gw_usage_events`);
 await db.query(`INSERT INTO ${schema}.gw_schema_migrations(version,checksum) VALUES(6,decode(repeat('00',32),'hex'))`);app=start();result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/schema_unavailable/);app=null;await db.query(`DELETE FROM ${schema}.gw_schema_migrations WHERE version=6`);
 await db.query(`UPDATE ${schema}.gw_schema_migrations SET checksum=decode(repeat('00',32),'hex') WHERE version=2`);app=start();result=await app.exit;assert.equal(result.code,1);assert.match(result.err,/schema_unavailable/);app=null;
 console.log('U0_ACTUAL_MAIN_TLS_MIGRATION_HEALTH_OK');
}finally{
 for(const child of legacyChildren){child.kill('SIGKILL');}
 if(app){app.child.kill('SIGKILL');await app.exit;}
 if(sso){sso.closeAllConnections();await new Promise(r=>sso.close(r));}
 for(const s of sockets)s.destroy();if(pgServer)await new Promise(r=>pgServer.close(r));
 if(db._connected){if(created){await db.query(`DROP SCHEMA ${schema} CASCADE`);unlinkSync(marker);}await db.end();}
 clearTimeout(overall);
 rmSync(directory,{recursive:true,force:true});
}
