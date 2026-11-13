-- Provider 회사 키. 원문은 저장하지 않고 AES-256-GCM 암호문만 둔다(root key는 DB 밖).
-- 무효화한 행은 암호문을 지워 복구할 수 없게 하고, 누가 언제 등록·해제했는지만 남긴다.
CREATE TABLE gw_provider_credentials (
 id uuid PRIMARY KEY,
 provider text NOT NULL CHECK(provider IN ('anthropic','openai','deepseek')),
 state text NOT NULL CHECK(state IN ('active','retired')),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0),
 kek_id bytea NOT NULL CHECK(octet_length(kek_id)=8),
 nonce bytea CHECK(octet_length(nonce)=12),
 ciphertext bytea CHECK(octet_length(ciphertext) BETWEEN 36 AND 528),
 fingerprint bytea NOT NULL CHECK(octet_length(fingerprint)=32),
 hint text NOT NULL CHECK(char_length(hint)=4 AND hint ~ '^[!-~]{4}$'),
 verified_at timestamptz NOT NULL,
 created_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 retired_by uuid REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 retired_at timestamptz,
 CHECK((state='active' AND nonce IS NOT NULL AND ciphertext IS NOT NULL AND retired_by IS NULL AND retired_at IS NULL)
    OR (state='retired' AND nonce IS NULL AND ciphertext IS NULL AND retired_by IS NOT NULL AND retired_at IS NOT NULL))
);
CREATE UNIQUE INDEX gw_provider_credentials_active ON gw_provider_credentials(provider) WHERE state='active';
CREATE INDEX gw_provider_credentials_history ON gw_provider_credentials(provider,created_at);

-- 어떤 회사 키로 나간 요청인지 남긴다(키 유출 시 범위 확인). 이관·테스트 고정 키 요청은 NULL이다.
ALTER TABLE gw_requests ADD COLUMN provider_credential_id uuid REFERENCES gw_provider_credentials(id) ON UPDATE RESTRICT ON DELETE RESTRICT;
CREATE INDEX gw_requests_provider_credential ON gw_requests(provider_credential_id,admitted_at) WHERE provider_credential_id IS NOT NULL;

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff','set_provider_credential','retire_provider_credential'));
ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_target_kind_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_target_kind_check CHECK(target_kind IN ('key','principal','policy','request','ledger','credential'));
