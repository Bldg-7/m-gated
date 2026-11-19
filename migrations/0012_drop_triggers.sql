-- 남은 검사 트리거와 예산 기간 함수를 지운다. 규칙은 서버 코드가 맡는다.
-- - 사용자: 종류·발급자·식별자를 바꾸는 명령이 없고, 서비스 계정의 주인은 서버가 사람인지 확인한다.
-- - 정책: 범위(사용자·가상 키)를 바꾸는 명령이 없다.
-- - 요청: 접수할 때 정한 값과 전송 사실을 바꾸는 명령이 없고, 상위 요청은 서버가 접수할 때 확인한다.
-- - 예산 시간대: 서버가 저장하기 전에 시간대 이름을 확인한다.
-- - 예산 기간: 서버가 조회문 안에서 계산한다(src/ledger/settings.rs 의 budget_period_sql).
DROP TRIGGER gw_principal_identity_owner ON gw_principals;
DROP FUNCTION gw_check_principal();
DROP TRIGGER gw_policy_scope_timezone ON gw_policies;
DROP FUNCTION gw_check_policy();
DROP TRIGGER gw_request_immutable ON gw_requests;
DROP FUNCTION gw_check_request();
DROP TRIGGER gw_budget_timezone_valid ON gw_budget_timezones;
DROP FUNCTION gw_check_budget_timezone();
DROP FUNCTION gw_budget_period(timestamptz);
