-- 공급사별 모델·가격 카탈로그. 승인할 때마다 전체 목록을 새 버전으로 남긴다(가장 큰 version 이 현재).
-- 요청마다 입장 시점의 가격이 장부에 따로 저장되므로, 버전이 바뀌어도 이미 기록된 금액은 바뀌지 않는다.
CREATE TABLE gw_model_catalog_proposals (
 id uuid PRIMARY KEY,
 base_version bigint NOT NULL CHECK(base_version>=0),
 created_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 payload jsonb NOT NULL CHECK(jsonb_typeof(payload)='object' AND octet_length(payload::text)<=2097152)
);
CREATE INDEX gw_model_catalog_proposals_created ON gw_model_catalog_proposals(created_at);
CREATE TABLE gw_model_catalog_versions (
 version bigint PRIMARY KEY CHECK(version>0),
 proposal_id uuid NOT NULL REFERENCES gw_model_catalog_proposals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 approved_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 approved_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 entries jsonb NOT NULL CHECK(jsonb_typeof(entries)='array' AND jsonb_array_length(entries)<=512)
);

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff','set_provider_credential','retire_provider_credential','sync_model_catalog','approve_model_catalog'));
ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_target_kind_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_target_kind_check CHECK(target_kind IN ('key','principal','policy','request','ledger','credential','catalog'));
