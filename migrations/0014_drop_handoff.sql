-- 사용자 한 명의 장부를 다른 m-gated DB로 옮기던 handoff 유지보수 명령을 없앤다(결정 0006).
-- gw_commands 에 남은 operation 'handoff' 기록과 그 CHECK 값은 그대로 둔다.
DROP TABLE gw_handoffs;
