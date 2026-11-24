-- 예약과 예산 방식을 없앤다(결정 0005).
-- - 예산은 이번 기간의 확정 비용만 보고 판단한다. 요청은 실행 중에 금액이나 토큰을 잡아 두지 않는다.
-- - 분당 토큰 한도는 요청이 정산된 1분 칸에 쌓인 토큰으로 판단한다.
-- 예약 열을 지우면 그 열을 쓰던 CHECK 제약도 함께 지워진다. 정산 상태 제약은 예약 없이 다시 둔다.
ALTER TABLE gw_usage_totals DROP COLUMN reserved_usd;
ALTER TABLE gw_rate_windows DROP COLUMN reserved_tokens;
ALTER TABLE gw_request_budgets
 DROP COLUMN budget_mode,
 DROP COLUMN reservation_usd,
 DROP COLUMN remaining_reservation_usd,
 DROP COLUMN reserved_tokens,
 DROP COLUMN remaining_reserved_tokens;
ALTER TABLE gw_request_budgets ADD CONSTRAINT gw_request_budgets_settlement_check
 CHECK(((state='settled' AND applied_cost_usd IS NOT NULL)
 OR (state<>'settled' AND applied_cost_usd IS NULL AND applied_input_tokens IS NULL AND applied_output_tokens IS NULL)) IS TRUE);
ALTER TABLE gw_policies DROP COLUMN budget_mode;
