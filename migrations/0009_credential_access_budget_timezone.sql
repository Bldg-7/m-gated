-- 사용자(주체)별로 쓸 수 있는 회사 키를 정한다.
-- 주체가 어떤 Provider에 행을 하나라도 가지면, 그 Provider에서는 이 행에 있는 회사 키만 쓸 수 있다.
-- 행이 없는 Provider는 이전과 같다(그 Provider의 기본 키를 쓰고, 다른 키는 관리자만 고른다).
CREATE TABLE gw_principal_credentials (
 principal_id uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 provider text NOT NULL CHECK(provider IN ('anthropic','openai','deepseek')),
 credential_id uuid NOT NULL,
 is_default boolean NOT NULL DEFAULT false,
 granted_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 granted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(principal_id,credential_id),
 FOREIGN KEY(credential_id,provider) REFERENCES gw_provider_credentials(id,provider) ON UPDATE RESTRICT ON DELETE RESTRICT
);
-- 주체·Provider마다 기본 키(SSO 직접 요청과 키를 고르지 않은 발급이 쓰는 키)는 하나다.
CREATE UNIQUE INDEX gw_principal_credentials_default ON gw_principal_credentials(principal_id,provider) WHERE is_default;
CREATE INDEX gw_principal_credentials_credential ON gw_principal_credentials(credential_id);

-- 예산 기간을 정하는 시간대. 관리자가 한 곳에서 정하고, 바꾸면 다음 기간부터 적용한다.
-- effective_from 부터 그 행의 시간대를 쓴다. 첫 행은 -infinity 다.
-- gw_policies.period_timezone 열은 0.9.0부터 예산 기간 계산에 쓰지 않는다(이전 기록으로만 남는다).
CREATE TABLE gw_budget_timezones (
 effective_from timestamptz PRIMARY KEY,
 timezone text NOT NULL CHECK(octet_length(timezone) BETWEEN 1 AND 100),
 set_by uuid REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 set_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE FUNCTION gw_check_budget_timezone() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name=NEW.timezone) THEN
  RAISE EXCEPTION 'invalid timezone' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER gw_budget_timezone_valid BEFORE INSERT OR UPDATE ON gw_budget_timezones FOR EACH ROW EXECUTE FUNCTION gw_check_budget_timezone();
-- 지금까지 예산에 가장 많이 쓰던 시간대로 시작한다(없으면 UTC). 이번 달 사용액이 그대로 이어진다.
INSERT INTO gw_budget_timezones(effective_from,timezone)
SELECT '-infinity',COALESCE((SELECT period_timezone FROM gw_policies WHERE budget_limit_usd IS NOT NULL GROUP BY period_timezone ORDER BY count(*) DESC,period_timezone LIMIT 1),'UTC');

-- 시각 t가 속한 예산 기간과 그 기간의 시간대.
-- 보통은 그 시간대의 달력 한 달이다. 시간대가 바뀐 직후의 기간 하나만, 이전 기간이 끝난 시각부터
-- 새 시간대의 다음 달 1일 0시까지로 센다(짧거나 길어질 수 있다).
CREATE FUNCTION gw_budget_period(t timestamptz, OUT period_start timestamptz, OUT period_end timestamptz, OUT timezone text)
LANGUAGE plpgsql STABLE AS $$
DECLARE
 since timestamptz;
 boundary timestamptz;
 next_change timestamptz;
BEGIN
 SELECT z.effective_from,z.timezone INTO since,timezone FROM gw_budget_timezones z WHERE z.effective_from<=t ORDER BY z.effective_from DESC LIMIT 1;
 IF timezone IS NULL THEN
  RAISE EXCEPTION 'budget timezone missing' USING ERRCODE='P0002';
 END IF;
 period_start := date_trunc('month',t AT TIME ZONE timezone) AT TIME ZONE timezone;
 period_end := (date_trunc('month',t AT TIME ZONE timezone)+interval '1 month') AT TIME ZONE timezone;
 IF since>'-infinity'::timestamptz THEN
  boundary := (date_trunc('month',(since+interval '2 days') AT TIME ZONE timezone)+interval '1 month') AT TIME ZONE timezone;
  IF t<boundary THEN
   period_start := since;
   period_end := boundary;
  END IF;
 END IF;
 SELECT min(z.effective_from) INTO next_change FROM gw_budget_timezones z WHERE z.effective_from>t;
 IF next_change IS NOT NULL AND next_change<period_end THEN
  period_end := next_change;
 END IF;
END $$;

ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_operation_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_operation_check CHECK(operation IN ('issue_key','revoke_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff','set_provider_credential','retire_provider_credential','sync_model_catalog','approve_model_catalog','add_provider_credential','replace_provider_credential','update_provider_credential','bind_key_credential','authorize_principal_credentials','set_budget_timezone'));
ALTER TABLE gw_commands DROP CONSTRAINT gw_commands_target_kind_check;
ALTER TABLE gw_commands ADD CONSTRAINT gw_commands_target_kind_check CHECK(target_kind IN ('key','principal','policy','request','ledger','credential','catalog','settings','access','management_token'));
