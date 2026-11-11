import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

test('측정 collector와 소비자는 공유 파일 snapshot을 사용하지 않는다',()=>{
 const client=readFileSync(new URL('./client.mjs',import.meta.url),'utf8');
 const runner=readFileSync(new URL('../../scripts/test-product.py',import.meta.url),'utf8');
 assert(!client.includes("readFileSync('/fixture-data/metrics.json'"),'소비자의 공유mount snapshot 읽기 경계를 제거해야 한다');
 assert(!runner.includes("pending.replace(directory/'metrics.json')"),'collector의 공유mount 파일전달 경계를 제거해야 한다');
});

test('반복 wave는 DB finished뿐 아니라 실제 제품의 새 자원반환 관측을 기다린다',()=>{
 const client=readFileSync(new URL('./client.mjs',import.meta.url),'utf8');
 assert(client.includes("control('/await-idle')"),'실제 task/permit/body 반환 barrier가 필요하다');
 assert(client.includes('barrier_wait_ms'),'해제 대기시간을 처리량과 구분해 기록해야 한다');
 assert(client.includes('try{const initialBarrier=await awaitActualIdle();do{'),'첫 wave도 실제 새 idle 관측 뒤 시작해야 한다');
 for(const name of ['barrier_wait_max_ms','max_sample_gap_ms'])assert(client.includes(name),'최대 대기/표본 gap 보고가 필요하다');
 const runner=readFileSync(new URL('../../scripts/test-product.py',import.meta.url),'utf8');
 assert(runner.includes("['NetworkSettings']['Ports']['64604/tcp']"),'inspect의 실제 hostIP/port 교차검사가 필요하다');
 assert(runner.includes('RPC_BUDGET_MS'),'전달시간의 보수적 상한과 실제 whole-RPC 경과를 검사해야 한다');
});

import {NumericSnapshot} from './metrics.mjs';
import http from 'node:http';
const epoch='00000000-0000-4000-8000-000000000001';
const other='00000000-0000-4000-8000-000000000002';
const value=(sequence=1)=>({epoch,sequence,age_ms:0,barrier_generation:0,telemetry:{rss:123,counter:4}});
test('고정 숫자 latest 하나만 저장하며 epoch/sequence/schema/age를 재검사한다',()=>{
 let now=100;const snapshot=new NumericSnapshot(['counter','rss'],()=>now);
 assert.throws(()=>snapshot.read());snapshot.reset(epoch);assert.throws(()=>snapshot.read());
 const first=value();snapshot.update(first);first.telemetry.rss=999;assert.equal(snapshot.read().telemetry.rss,123);
 for(const invalid of [value(),{...value(2),epoch:other},{...value(2),sequence:0},{...value(2),sequence:1.5},{...value(2),age_ms:15001},{...value(2),age_ms:-1},{...value(2),extra:1},{...value(2),telemetry:{rss:1,counter:1,arbitrary_label:1}},{...value(2),telemetry:{rss:'123',counter:4}}])assert.throws(()=>snapshot.update(invalid));
 assert.equal(snapshot.read().sequence,1);now+=15000;assert.equal(snapshot.read().age_ms,15000);now++;assert.throws(()=>snapshot.read());
 snapshot.update(value(3));assert.equal(snapshot.read().sequence,3);assert.equal(snapshot.read().age_ms,0);
 snapshot.reset(other);assert.throws(()=>snapshot.read());assert.throws(()=>snapshot.update(value(4)));
});
test('전달 전 측정나이를 보존하고 stale/missing을 실제 loopback bridge에서 거부한다',async()=>{
 let now=1000;const snapshot=new NumericSnapshot(['counter','rss'],()=>now);snapshot.reset(epoch);
 const server=http.createServer(async(req,res)=>{try{let body='';for await(const chunk of req){body+=chunk;assert(body.length<=16384);}let result={};if(req.method==='POST')snapshot.update(JSON.parse(body));else result=snapshot.read();res.setHeader('content-type','application/json');res.end(JSON.stringify(result));}catch{res.writeHead(503);res.end('{}');}});
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));const url=`http://127.0.0.1:${server.address().port}`;
 try{
  assert.equal((await fetch(url,{signal:AbortSignal.timeout(2000)})).status,503);
  assert.equal((await fetch(url,{method:'POST',body:JSON.stringify({...value(),age_ms:500}),signal:AbortSignal.timeout(2000)})).status,200);
  const response=await fetch(url,{signal:AbortSignal.timeout(2000)});assert.equal(response.status,200);assert.equal((await response.json()).age_ms,500);
  now+=14501;assert.equal((await fetch(url,{signal:AbortSignal.timeout(2000)})).status,503);
  assert.equal((await fetch(url,{method:'POST',body:JSON.stringify(value()),signal:AbortSignal.timeout(2000)})).status,503);
 }finally{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
});

const idleNames=['active_tasks','active_permits','body_bytes','input_bytes','queue_bytes','parser_bytes','queue_waiters'];
const idleValue=(sequence,generation=0,active=0)=>({epoch,sequence,age_ms:0,barrier_generation:generation,telemetry:Object.fromEntries(idleNames.map(name=>[name,name==='active_tasks'?active:0]))});
test('이전 idle의 늦은 도착과 실행/bytes 잔존은 대기를 해제하지 않으며 epoch 변경은 실패한다',async()=>{
 let now=10;const snapshot=new NumericSnapshot(idleNames,()=>now);snapshot.reset(epoch);snapshot.update(idleValue(1));
 let done=false;const wait=snapshot.waitIdle().then(result=>{done=true;return result;});
 now++;assert.equal(snapshot.update(idleValue(2)),1);await Promise.resolve();assert.equal(done,false,'대기 전 관측이 늦게 도착해도 안 된다');
 for(const field of idleNames){now++;const input=idleValue(snapshot.latest.sequence+1,1);input.telemetry[field]=1;snapshot.update(input);await Promise.resolve();assert.equal(done,false);}
 now++;snapshot.update(idleValue(snapshot.latest.sequence+1,1));const idle=await wait;assert.equal(idle.barrier_generation,1);assert.equal(idle.telemetry.active_tasks,0);
 const waiting=snapshot.waitIdle();const rejected=assert.rejects(waiting,/epoch/);snapshot.reset(other);await rejected;
});
test('실제 HTTP producer 왕복 뒤 새 관측만 idle longpoll을 완료한다',async()=>{
 let now=10;const snapshot=new NumericSnapshot(idleNames,()=>now);snapshot.reset(epoch);snapshot.update(idleValue(1));
 let entered;const requested=new Promise(resolve=>{entered=resolve;});
 const server=http.createServer(async(req,res)=>{try{let body='';for await(const chunk of req){body+=chunk;assert(body.length<=16384);}let result={};if(req.url==='/idle'){entered();result=await snapshot.waitIdle();}else if(req.method==='POST'){result={barrier_generation:snapshot.update(JSON.parse(body))};}else result=snapshot.read();res.end(JSON.stringify(result));}catch{res.writeHead(503);res.end('{}');}});
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));const url=`http://127.0.0.1:${server.address().port}`;
 try{
  let completed=false;const response=fetch(url+'/idle',{signal:AbortSignal.timeout(2000)}).then(r=>{completed=true;return r;});await requested;
  now++;const acknowledged=await fetch(url,{method:'POST',body:JSON.stringify(idleValue(2)),signal:AbortSignal.timeout(2000)});assert.equal((await acknowledged.json()).barrier_generation,1);assert.equal(completed,false);
  now++;await fetch(url,{method:'POST',body:JSON.stringify(idleValue(3,1,1)),signal:AbortSignal.timeout(2000)});assert.equal(completed,false);
  now++;await fetch(url,{method:'POST',body:JSON.stringify(idleValue(4,1)),signal:AbortSignal.timeout(2000)});const idle=await response;assert.equal(idle.status,200);assert.equal((await idle.json()).sequence,4);
 }finally{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
});

test('새 관측이 없으면 단일 idle waiter가 실제15초에 실패하고 정리된다',{timeout:17000},async()=>{
 const snapshot=new NumericSnapshot(idleNames);snapshot.reset(epoch);snapshot.update(idleValue(1));
 const started=performance.now();await assert.rejects(snapshot.waitIdle(),/15초/);
 assert(performance.now()-started>=15000);assert.equal(snapshot.waiter,null);
});
