-- TypeSafe Provider를 더한다(결정 0008). API 이름은 systemone이다.
-- 끊긴 요청 정산 규칙(gw_settlement_rules)은 스트림이 없는 TypeSafe에 필요 없어 그대로 둔다.
ALTER TABLE gw_requests DROP CONSTRAINT gw_requests_provider_check;
ALTER TABLE gw_requests ADD CONSTRAINT gw_requests_provider_check CHECK(provider IN ('anthropic','openai','deepseek','typesafe'));
ALTER TABLE gw_requests DROP CONSTRAINT gw_requests_api_check;
ALTER TABLE gw_requests ADD CONSTRAINT gw_requests_api_check CHECK(api IN ('messages','count_tokens','responses','chat_completions','embeddings','systemone'));
ALTER TABLE gw_requests DROP CONSTRAINT gw_requests_check;
ALTER TABLE gw_requests ADD CONSTRAINT gw_requests_check CHECK((provider='anthropic' AND api IN ('messages','count_tokens')) OR (provider='openai' AND api IN ('responses','chat_completions','embeddings')) OR (provider='deepseek' AND api='chat_completions') OR (provider='typesafe' AND api='systemone'));
ALTER TABLE gw_provider_credentials DROP CONSTRAINT gw_provider_credentials_provider_check;
ALTER TABLE gw_provider_credentials ADD CONSTRAINT gw_provider_credentials_provider_check CHECK(provider IN ('anthropic','openai','deepseek','typesafe'));
ALTER TABLE gw_virtual_key_credentials DROP CONSTRAINT gw_virtual_key_credentials_provider_check;
ALTER TABLE gw_virtual_key_credentials ADD CONSTRAINT gw_virtual_key_credentials_provider_check CHECK(provider IN ('anthropic','openai','deepseek','typesafe'));
ALTER TABLE gw_principal_credentials DROP CONSTRAINT gw_principal_credentials_provider_check;
ALTER TABLE gw_principal_credentials ADD CONSTRAINT gw_principal_credentials_provider_check CHECK(provider IN ('anthropic','openai','deepseek','typesafe'));
