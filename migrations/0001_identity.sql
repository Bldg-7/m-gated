CREATE TABLE gw_principals (
 id uuid PRIMARY KEY,
 kind text NOT NULL CHECK (kind IN ('human','service')),
 issuer text CHECK (octet_length(issuer) BETWEEN 1 AND 2048),
 subject text CHECK (octet_length(subject) BETWEEN 1 AND 512),
 email text CHECK (octet_length(email) BETWEEN 1 AND 320 AND email=lower(email)),
 display_name text NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 100 AND octet_length(display_name)<=400 AND display_name !~ '[[:cntrl:]]'),
 owner_principal_id uuid REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 disabled_at timestamptz,
 disabled_reason text CHECK (octet_length(disabled_reason) BETWEEN 1 AND 128),
 security_fence_ref text CHECK (octet_length(security_fence_ref) BETWEEN 1 AND 2048),
 inference_paused_at timestamptz,
 inference_pause_reason text CHECK (inference_pause_reason IN ('cost_bound_violation','amount_overflow')),
 inference_pause_request_id uuid,
 inference_safety_version bigint NOT NULL DEFAULT 1 CHECK (inference_safety_version>0),
 version bigint NOT NULL DEFAULT 1 CHECK (version>0),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK (((kind='human' AND issuer IS NOT NULL AND subject IS NOT NULL AND email IS NOT NULL AND owner_principal_id IS NULL)
 OR (kind='service' AND issuer IS NULL AND subject IS NULL AND email IS NULL AND owner_principal_id IS NOT NULL AND owner_principal_id<>id)) IS TRUE),
 CHECK ((disabled_at IS NULL)=(disabled_reason IS NULL)),
 CHECK (security_fence_ref IS NULL OR disabled_at IS NOT NULL),
 CHECK (((inference_paused_at IS NULL AND inference_pause_reason IS NULL AND inference_pause_request_id IS NULL)
 OR (inference_paused_at IS NOT NULL AND inference_pause_reason IS NOT NULL AND inference_pause_request_id IS NOT NULL)) IS TRUE)
);
CREATE UNIQUE INDEX gw_human_identity ON gw_principals(issuer,subject) WHERE kind='human';
CREATE INDEX gw_principals_created ON gw_principals(created_at,id);
CREATE INDEX gw_service_owner ON gw_principals(owner_principal_id,id) WHERE kind='service';
CREATE INDEX gw_inference_paused ON gw_principals(inference_paused_at,id) WHERE inference_paused_at IS NOT NULL;
CREATE FUNCTION gw_check_principal() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND (NEW.kind IS DISTINCT FROM OLD.kind OR NEW.issuer IS DISTINCT FROM OLD.issuer OR NEW.subject IS DISTINCT FROM OLD.subject) THEN
  RAISE EXCEPTION 'immutable identity' USING ERRCODE='23514';
 END IF;
 IF NEW.kind='service' AND NOT EXISTS (SELECT 1 FROM gw_principals WHERE id=NEW.owner_principal_id AND kind='human') THEN
  RAISE EXCEPTION 'human owner required' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER gw_principal_identity_owner AFTER INSERT OR UPDATE ON gw_principals DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION gw_check_principal();
CREATE TABLE gw_policies (
 id uuid PRIMARY KEY,
 principal_id uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 key_id uuid,
 version bigint NOT NULL DEFAULT 1 CHECK (version>0),
 allow_rules jsonb NOT NULL CHECK (jsonb_typeof(allow_rules)='array' AND octet_length(allow_rules::text)<=32768),
 budget_mode text NOT NULL CHECK (budget_mode IN ('actual','reserved')),
 budget_limit_usd numeric(28,12) CHECK (budget_limit_usd>=0 AND budget_limit_usd<'Infinity'::numeric AND budget_limit_usd<>'NaN'::numeric),
 period_kind text NOT NULL CHECK (period_kind='calendar_month'),
 period_timezone text NOT NULL CHECK (octet_length(period_timezone) BETWEEN 1 AND 100),
 rpm_limit bigint CHECK (rpm_limit BETWEEN 1 AND 1000000000),
 tpm_limit bigint CHECK (tpm_limit BETWEEN 1 AND 1000000000),
 count_rpm_limit bigint CHECK (count_rpm_limit BETWEEN 1 AND 1000000000),
 updated_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE UNIQUE INDEX gw_principal_policy ON gw_policies(principal_id) WHERE key_id IS NULL;
CREATE UNIQUE INDEX gw_key_policy ON gw_policies(key_id) WHERE key_id IS NOT NULL;
CREATE FUNCTION gw_check_policy() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS (SELECT 1 FROM pg_timezone_names WHERE name=NEW.period_timezone) THEN
  RAISE EXCEPTION 'invalid timezone' USING ERRCODE='23514';
 END IF;
 IF TG_OP='UPDATE' AND (NEW.id IS DISTINCT FROM OLD.id OR NEW.principal_id IS DISTINCT FROM OLD.principal_id OR NEW.key_id IS DISTINCT FROM OLD.key_id) THEN
  RAISE EXCEPTION 'immutable scope' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER gw_policy_scope_timezone BEFORE INSERT OR UPDATE ON gw_policies FOR EACH ROW EXECUTE FUNCTION gw_check_policy();
