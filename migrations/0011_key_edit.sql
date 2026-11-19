-- 가상 키의 이름·허용 API·Provider 키 연결을 발급 뒤에도 바꿀 수 있게 한다.
-- "발급한 가상 키는 바꿀 수 없다"는 규칙은 서버 코드가 맡는다. 유효기간·만료일·한도를 바꾸는 명령은 없다.
DROP TRIGGER gw_key_immutable ON gw_virtual_keys;
DROP FUNCTION gw_check_key_immutable();
DROP TRIGGER gw_key_policy_immutable ON gw_policies;
DROP FUNCTION gw_check_key_policy_immutable();

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','update_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff','set_provider_credential','retire_provider_credential','sync_model_catalog','approve_model_catalog','add_provider_credential','replace_provider_credential','update_provider_credential','bind_key_credential','authorize_principal_credentials','set_budget_timezone'));
