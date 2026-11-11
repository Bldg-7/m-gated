// 테스트 지원 전용: 마지막 고정 숫자 snapshot 하나만 보유한다. 제품/금융 경로에서는 사용하지 않는다.
import assert from 'node:assert/strict';

export class NumericSnapshot {
 constructor(names, now=()=>performance.now()) {
  assert(names.length>0&&names.length<100&&new Set(names).size===names.length);
  this.names=Object.freeze([...names].sort());this.now=now;this.epoch=null;this.latest=null;this.generation=0;this.waiter=null;
 }
 reset(epoch) {
  assert.equal(typeof epoch,'string');assert(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(epoch));
  if(this.waiter!==null){clearTimeout(this.waiter.timer);this.waiter.reject(new Error('대기 중 제품 epoch 변경'));this.waiter=null;}
  this.epoch=epoch;this.latest=null;this.generation=0;
 }
 update(input) {
  assert.deepEqual(Object.keys(input).sort(),['age_ms','barrier_generation','epoch','sequence','telemetry']);
  assert(this.epoch!==null&&input.epoch===this.epoch);
  assert(Number.isSafeInteger(input.sequence)&&input.sequence>(this.latest?.sequence??0));
  assert(Number.isSafeInteger(input.age_ms)&&input.age_ms>=0&&input.age_ms<=15000);
  assert(Number.isSafeInteger(input.barrier_generation)&&input.barrier_generation>=0&&input.barrier_generation<=this.generation);
  assert(input.barrier_generation>=(this.latest?.generation??0));
  assert(input.telemetry&&typeof input.telemetry==='object'&&!Array.isArray(input.telemetry));
  assert.deepEqual(Object.keys(input.telemetry).sort(),this.names);
  assert(Object.values(input.telemetry).every(value=>Number.isSafeInteger(value)&&value>=0));
  this.latest=Object.freeze({sequence:input.sequence,generation:input.barrier_generation,telemetry:Object.freeze({...input.telemetry}),observedAt:this.now()-input.age_ms});
  const idleFields=['active_tasks','active_permits','body_bytes','input_bytes','queue_bytes','parser_bytes','queue_waiters'];
  if(this.waiter!==null&&this.latest.generation===this.generation&&this.latest.observedAt>this.waiter.after&&idleFields.every(name=>this.latest.telemetry[name]===0)){
   const waiter=this.waiter;this.waiter=null;clearTimeout(waiter.timer);waiter.resolve(this.read());
  }
  return this.generation;
 }
 // generation 왕복은 producer가 대기 요청을 확인한 뒤에 실제 읽은 새 stderr 관측만 허용한다.
 // 네트워크 도착 시각이나 보관 중인 idle snapshot을 실제 관측 시각으로 오인하지 않는다.
 waitIdle() {
  this.read();assert(this.waiter===null,'idle 대기는 하나만 허용');assert(Number.isSafeInteger(this.generation+1));this.generation++;
  return new Promise((resolve,reject)=>{
   const timer=setTimeout(()=>{this.waiter=null;reject(new Error('실제 자원반환 관측15초 상한'));},15000);
   this.waiter={after:this.now(),resolve,reject,timer};
  });
 }
 read() {
  assert(this.latest!==null,'고정 숫자 snapshot 누락');
  const age_ms=Math.max(0,Math.floor(this.now()-this.latest.observedAt));
  assert(age_ms<=15000,'고정 숫자 snapshot 15초 초과');
  return {epoch:this.epoch,sequence:this.latest.sequence,barrier_generation:this.latest.generation,age_ms,telemetry:this.latest.telemetry};
 }
}
