-- Provider마다 회사 키를 여러 개 두고, 키마다 관리자가 알아볼 이름(label)을 붙인다.
-- 공급사마다 기본 키가 하나 있고, 가상 키는 발급할 때 공급사별로 회사 키 하나에 연결된다.
ALTER TABLE gw_provider_credentials ADD COLUMN label text;
ALTER TABLE gw_provider_credentials ADD COLUMN is_default boolean NOT NULL DEFAULT false;
-- 이전 버전에서 등록한 키는 이름이 없으므로 'default'로 두고, 쓰고 있던 키를 기본 키로 둔다.
UPDATE gw_provider_credentials SET label='default';
UPDATE gw_provider_credentials SET is_default=true WHERE state='active';
ALTER TABLE gw_provider_credentials ALTER COLUMN label SET NOT NULL;
ALTER TABLE gw_provider_credentials ADD CONSTRAINT gw_provider_credentials_label_check
 CHECK(char_length(label) BETWEEN 1 AND 60 AND octet_length(label)<=240 AND label !~ '[[:cntrl:]]' AND label=btrim(label));
ALTER TABLE gw_provider_credentials ADD CONSTRAINT gw_provider_credentials_default_check CHECK(state='active' OR NOT is_default);
ALTER TABLE gw_provider_credentials ADD CONSTRAINT gw_provider_credentials_id_provider UNIQUE(id,provider);
DROP INDEX gw_provider_credentials_active;
-- 쓰고 있는 키끼리는 이름이 겹치지 않는다(대소문자 무시). 해제한 키의 이름은 다시 쓸 수 있다.
CREATE UNIQUE INDEX gw_provider_credentials_label ON gw_provider_credentials(provider,lower(label)) WHERE state='active';
CREATE UNIQUE INDEX gw_provider_credentials_default ON gw_provider_credentials(provider) WHERE is_default;

-- 가상 키가 공급사별로 어느 회사 키로 나가는지. 연결이 없는 공급사는 그 공급사의 기본 키를 쓴다.
CREATE TABLE gw_virtual_key_credentials (
 key_id uuid NOT NULL REFERENCES gw_virtual_keys(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 provider text NOT NULL CHECK(provider IN ('anthropic','openai','deepseek')),
 credential_id uuid NOT NULL,
 bound_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(key_id,provider),
 FOREIGN KEY(credential_id,provider) REFERENCES gw_provider_credentials(id,provider) ON UPDATE RESTRICT ON DELETE RESTRICT
);
CREATE INDEX gw_virtual_key_credentials_credential ON gw_virtual_key_credentials(credential_id);
-- 이미 발급된 가상 키는 지금 쓰고 있는 회사 키에 연결한다(동작은 이전과 같다).
INSERT INTO gw_virtual_key_credentials(key_id,provider,credential_id)
SELECT k.id,c.provider,c.id FROM gw_virtual_keys k CROSS JOIN gw_provider_credentials c WHERE c.state='active';

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff','set_provider_credential','retire_provider_credential','sync_model_catalog','approve_model_catalog','add_provider_credential','replace_provider_credential','update_provider_credential','bind_key_credential'));
