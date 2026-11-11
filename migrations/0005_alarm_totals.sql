-- ACK 수신 여부와 무관한 DB 범위 경보 누계다. 파드별 합산하지 않는다.
CREATE TABLE gw_alarm_totals (
    code text PRIMARY KEY CHECK (code IN ('unknown','bound')),
    total bigint NOT NULL CHECK (total >= 0)
);
-- 적용 전에 남은 원자 금융/감사 사실도 보존한다. 상세 삭제 전 이미 확정된
-- 누계는 이후 이 테이블이 소유하며 retention으로 감소시키지 않는다.
INSERT INTO gw_alarm_totals(code,total)
SELECT 'unknown',count(*) FROM gw_requests r
WHERE accounting_state='unknown'
   OR EXISTS (SELECT 1 FROM gw_usage_events e WHERE e.request_id=r.id AND e.kind IN ('settlement','late_final'));
INSERT INTO gw_alarm_totals(code,total)
SELECT 'bound',count(*) FROM gw_audit_events WHERE actor_kind='system' AND operation='inference_auto_pause';
