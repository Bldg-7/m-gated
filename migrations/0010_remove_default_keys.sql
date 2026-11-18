-- 기본 키 개념을 없앤다. 가상 키는 발급할 때 Provider 키를 반드시 직접 고르고,
-- 사용자는 관리자가 인가한 Provider 키만 쓸 수 있다.
DROP INDEX gw_provider_credentials_default;
ALTER TABLE gw_provider_credentials DROP CONSTRAINT gw_provider_credentials_default_check;
ALTER TABLE gw_provider_credentials DROP COLUMN is_default;
DROP INDEX gw_principal_credentials_default;
ALTER TABLE gw_principal_credentials DROP COLUMN is_default;
