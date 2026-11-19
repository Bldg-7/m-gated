-- 금액 검증 함수를 지운다. 금액의 범위와 소수 12자리는 서버 코드(src/ledger/amount.rs)가 확인한다.
-- 금액 열의 형식(numeric(28,12))과 CHECK 제약(0 이상, NaN·무한대 금지)은 그대로 둔다.
DROP FUNCTION gw_amount(numeric);
