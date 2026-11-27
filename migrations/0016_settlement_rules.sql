-- 끊긴 요청을 정산하는 규칙(결정 0005). Provider마다 하나이고, 이번 단계에서는 Anthropic 규칙만 쓴다.
-- - received: 끊기기 전에 받은 출력만큼 센다.
-- - max_tokens: 요청의 max_tokens만큼 센다.
-- 요청은 받을 때의 규칙을 실행 기록(execution_snapshot)에 남겨 끝까지 쓴다.
CREATE TABLE gw_settlement_rules (
 provider text PRIMARY KEY CHECK(provider IN ('anthropic','openai','deepseek')),
 cutoff_rule text NOT NULL CHECK(cutoff_rule IN ('received','max_tokens')),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0),
 updated_by uuid REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO gw_settlement_rules(provider,cutoff_rule) VALUES('anthropic','received'),('openai','received'),('deepseek','received');

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','update_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','retire_provider_credential','sync_model_catalog','approve_model_catalog','add_provider_credential','replace_provider_credential','update_provider_credential','bind_key_credential','authorize_principal_credentials','set_budget_timezone','set_settlement_rule','add_access','update_access','remove_access'));
