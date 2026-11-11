// 합성 프로세스 부하 전용이다. 제품 binary는 이 모듈/TEST_*에 의존하지 않는다.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
import { arch, cpus } from 'node:os';

export async function measureLoad({ caseName, token, adminToken, db, schema, startWave, sample, principalIds, finalized }) {
  assert(['baseline','boundary','mixed','soak'].includes(caseName));
  const count=16, seconds=caseName==='soak'?3600:caseName==='boundary'?5:60;
  const waveSeconds=caseName==='soak'?2:seconds;
  const targetBytes=caseName==='boundary'?8388608:300000;
  const seed={model:'synthetic-text',max_tokens:100,stream:true,messages:[{role:'user',content:''}]};
  const overhead=Buffer.byteLength(JSON.stringify(seed));seed.messages[0].content='x'.repeat(targetBytes-overhead);
  const body=JSON.stringify(seed);assert.equal(Buffer.byteLength(body),targetBytes);
  const started=performance.now();let completed=0,rejected=0,adminCompleted=0,bytes=0;
  const latency=[],ttft=[],memory=[];let peakRss=0,peakAnon=0,failed=false;
  const cgroup=()=>({current:Number(readFileSync('/sys/fs/cgroup/memory.current','utf8')),peak:Number(readFileSync('/sys/fs/cgroup/memory.peak','utf8')),anon:Number(readFileSync('/sys/fs/cgroup/memory.stat','utf8').match(/^anon (\d+)/m)[1])});
  const take=()=>{const m=sample();peakRss=Math.max(peakRss,m.rssKiB);peakAnon=Math.max(peakAnon,m.anonKiB);memory.push({seconds:Math.round((performance.now()-started)/1000),...m,...cgroup()});};
  const sampler=setInterval(take,1000);take();
  const before=(await db.query(`SELECT count(*)::int AS n FROM ${schema}.gw_requests WHERE principal_id=ANY($1::uuid[])`,[principalIds])).rows[0].n;
  try {
    do {
      const release=startWave(waveSeconds);
      const tasks=[],requestIds=[];
      for(let i=0;i<count;i++){
        const begin=performance.now();const response=await fetch('http://127.0.0.1:4000/anthropic/v1/messages',{method:'POST',headers:{authorization:`Bearer ${token(i)}`,'content-type':'application/json','anthropic-version':'2023-06-01'},body,signal:AbortSignal.timeout((waveSeconds+20)*1000)});
        if(response.status!==200){rejected++;await response.arrayBuffer();assert.equal(response.status,200,'보안/입장검사를 끄거나429로 덜 처리해서 통과하지 않는다');}
        requestIds.push(response.headers.get('x-gateway-request-id'));
        tasks.push((async()=>{let first=true;for await(const chunk of response.body){if(first){ttft.push(performance.now()-begin);first=false;}bytes+=chunk.length;}assert(!first);latency.push(performance.now()-begin);completed++;})());
      }
      release();
      if(caseName==='mixed'){
        const management=(async()=>{for(let i=0;i<60;i++){
          for(const path of ['/livez','/readyz','/ui/','/gateway/admin/principals?limit=100','/gateway/admin/usage']){
            const r=await fetch(`http://127.0.0.1:4000${path}`,{headers:{authorization:`Bearer ${adminToken}`},signal:AbortSignal.timeout(5000)});assert.equal(r.status,200);await r.arrayBuffer();adminCompleted++;
          }
          await new Promise(resolve=>setTimeout(resolve,1000));
        }})();tasks.push(management);
      }
      await Promise.all(tasks);
      await Promise.all(requestIds.map(finalized));
      const diagnostic=await db.query(`SELECT phase,accounting_state,CASE WHEN reason_code IN ('completed','usage_unresolved','price_unresolved','amount_overflow','lease_expired','upstream_protocol_error','stream_parse_error','stream_limit','client_cancelled','upstream_read_timeout') THEN reason_code ELSE 'other' END AS reason,count(*)::int AS n FROM ${schema}.gw_requests WHERE id=ANY($1::uuid[]) GROUP BY 1,2,3 ORDER BY 1,2,3`,[requestIds]);
      if(diagnostic.rows.some(r=>r.phase!=='finished'||r.accounting_state!=='settled'||r.reason!=='completed'))console.log(`S10_LOAD_DIAGNOSTIC ${JSON.stringify(diagnostic.rows)}`);
      if(caseName==='soak'&&Math.floor((performance.now()-started)/1000)%60<3){take();}
    }while(caseName==='soak'&&performance.now()-started<seconds*1000);
    const rows=await db.query(`SELECT count(*)::int AS n,count(*) FILTER(WHERE phase='finished' AND accounting_state='settled' AND cost_usd=0.00014)::int AS valid,sum(cost_usd)::text AS cost,(SELECT sum(t.cost_usd)::text FROM ${schema}.gw_usage_totals t JOIN ${schema}.gw_policies p ON p.id=t.scope_id WHERE p.principal_id=ANY($1::uuid[]) AND p.key_id IS NULL) AS total,(SELECT count(*)::int FROM ${schema}.gw_usage_events e JOIN ${schema}.gw_requests r ON r.id=e.request_id WHERE r.principal_id=ANY($1::uuid[]) AND e.kind='final' AND (e.normalized_usage->>'input_tokens')::bigint=100 AND (e.normalized_usage->>'output_tokens')::bigint=20) AS valid_final FROM ${schema}.gw_requests WHERE principal_id=ANY($1::uuid[])`,[principalIds]);
    const result=rows.rows[0];assert.equal(result.n-before,completed);assert.equal(result.valid,result.n);assert.equal(result.valid_final,result.n);assert.equal(result.cost,result.total);assert.equal(rejected,0);
    assert.equal(completed%16,0);assert(completed>=16);assert(peakRss<=512*1024,'실제 child RSS 상한');
    if(caseName==='soak')assert(performance.now()-started>=3600000,'실제60분을 축약하지 않는다');
    latency.sort((a,b)=>a-b);ttft.sort((a,b)=>a-b);take();
    const quantile=(values,p)=>Math.round(values[Math.min(values.length-1,Math.floor(values.length*p))]*1000)/1000;
    const report={case:caseName,cpu:cpus()[0].model,arch:arch(),cpus:cpus().length,cpu_quota:readFileSync('/sys/fs/cgroup/cpu.max','utf8').trim(),memory_limit:readFileSync('/sys/fs/cgroup/memory.max','utf8').trim(),duration_ms:Math.round(performance.now()-started),body_bytes:targetBytes,concurrency:count,completed,rejected,admin_completed:adminCompleted,received_bytes:bytes,usage:{input_per_request:100,output_per_request:20,cost_per_request:'0.000140000000',cumulative_rows:result.n,cumulative_cost_usd:result.cost},rss_peak_kib:peakRss,anon_peak_kib:peakAnon,latency_ms:{p50:quantile(latency,.5),p95:quantile(latency,.95)},ttft_ms:{p50:quantile(ttft,.5),p95:quantile(ttft,.95)},cgroup_scope:'제품 child+합성 Node Provider/측정기; 제품 단독 cgroup로 주장하지 않음',samples:memory.filter((_,i)=>i===0||i===memory.length-1||i%60===0)};
    console.log(`S10_LOAD_RESULT ${JSON.stringify(report)}`);
    return report;
  }catch(error){failed=true;throw error;}finally{clearInterval(sampler);if(failed)console.log(`S10_LOAD_FAILED case=${caseName} completed=${completed} rejected=${rejected}`);}
}
