// 이전 writer 합성 프로세스다. 제품 이미지/기본 build에는 포함되지 않는다.
import http from 'node:http';
import https from 'node:https';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import pg from 'pg';
let raw='';for await(const chunk of process.stdin){raw+=chunk;if(raw.length>65536)process.exit(2);}
const config=JSON.parse(raw);
if(config.ledger_clock!=='2025-03-20T00:00:00Z')process.exit(2);
if(!/^gw_test_[a-f0-9]{32}$/.test(config.schema)||!/^[-a-f0-9]{36}$/.test(config.source_id))process.exit(2);
const database=new pg.Client({host:'host.docker.internal',port:64603,user:'m_gated_test',database:'m_gated_test',password:'',ssl:false,connectionTimeoutMillis:2000,query_timeout:2000});
await database.connect();
const table=`${config.schema}.gw_fixture_legacy_writer`;
await database.query(`CREATE TABLE IF NOT EXISTS ${table}(source_id uuid PRIMARY KEY,blocked boolean NOT NULL,cost_usd numeric(28,12) NOT NULL,settled_count bigint NOT NULL,key_count bigint NOT NULL,period_start timestamptz NOT NULL,period_end timestamptz NOT NULL,last_admitted_at timestamptz NOT NULL)`);
await database.query(`INSERT INTO ${table} VALUES($1,false,0,0,0,date_trunc('month',$2::timestamptz),date_trunc('month',$2::timestamptz)+interval '1 month',$2::timestamptz) ON CONFLICT DO NOTHING`,[config.source_id,config.ledger_clock]);
const hash=value=>createHash('sha256').update(value).digest('hex');
const sso=hash(config.access_token),key=hash(config.old_key);delete config.access_token;delete config.old_key;
const active=new Set();let draining=false;
function provider(body){return new Promise((resolve,reject)=>{
 const request=https.request('https://api.anthropic.com/v1/messages',{method:'POST',ca:readFileSync(config.ca_file),rejectUnauthorized:true,headers:{'content-type':'application/json','anthropic-version':'2023-06-01','x-api-key':'synthetic-provider-secret'}},response=>{let bytes='';response.on('data',chunk=>{bytes+=chunk;if(bytes.length>65536)response.destroy();});response.on('end',()=>{try{if(response.statusCode!==200)throw new Error('synthetic provider status');const parsed=JSON.parse(bytes);if(parsed.usage.input_tokens!==100||parsed.usage.output_tokens!==20)throw new Error('synthetic usage');resolve(parsed);}catch{reject(new Error('synthetic upstream invalid'));}});response.on('error',()=>reject(new Error('synthetic upstream closed')));});request.on('error',()=>reject(new Error('synthetic upstream failed')));request.setTimeout(5000,()=>request.destroy());request.end(body);
});}
const server=http.createServer((request,response)=>{
 const task=(async()=>{
  if(request.method!=='POST'||request.url!=='/infer'){response.writeHead(404).end();return;}
  const ledger=(await database.query(`SELECT blocked FROM ${table} WHERE source_id=$1`,[config.source_id])).rows[0];
  if(draining||ledger.blocked){response.writeHead(503).end('fenced');return;}
  const token=(request.headers.authorization??'').replace(/^Bearer /,'');const credential=hash(token);
  if(credential!==sso&&credential!==key){response.writeHead(401).end();return;}
  let body='';for await(const part of request){body+=part;if(body.length>65536){response.writeHead(413).end();return;}}
  const parsed=await provider(body);
  // 검증된 합성 usage100/20, input1/output2 USD/백만token의 고정소수 가격0.00014다.
  await database.query(`UPDATE ${table} SET cost_usd=cost_usd+0.000140000000,settled_count=settled_count+1,key_count=key_count+$2,last_admitted_at=$3::timestamptz WHERE source_id=$1`,[config.source_id,credential===key?1:0,config.ledger_clock]);
  response.writeHead(200,{'content-type':'application/json'}).end(JSON.stringify(parsed));
 })();active.add(task);task.catch(()=>{if(!response.headersSent)response.writeHead(502);response.end();}).finally(()=>active.delete(task));
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
process.send({kind:'ready',port:server.address().port});
process.on('message',async message=>{
 if(message?.kind!=='fence')return;
 draining=true;await database.query(`UPDATE ${table} SET blocked=true WHERE source_id=$1`,[config.source_id]);process.send({kind:'fenced',active:active.size});
});
process.on('SIGTERM',async()=>{draining=true;await new Promise(resolve=>server.close(resolve));await Promise.allSettled([...active]);await database.end();process.exit(0);});
