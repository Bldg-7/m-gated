-- numeric typmod에 들어가기 전에 정밀도를 검증한다. 모든 제품 금액 bind는 이 함수를 거친다.
CREATE FUNCTION gw_amount(v numeric) RETURNS numeric LANGUAGE plpgsql IMMUTABLE STRICT AS $$
BEGIN
 IF NOT (v >= 0 AND v < 10000000000000000 AND v = trunc(v,12)) OR v IN ('NaN'::numeric,'Infinity'::numeric,'-Infinity'::numeric) THEN
  RAISE EXCEPTION 'invalid amount' USING ERRCODE='23514';
 END IF;
 RETURN v::numeric(28,12);
END $$;
CREATE TABLE gw_commands (
 actor_principal_id uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 command_id uuid NOT NULL,
 input_digest bytea NOT NULL CHECK(octet_length(input_digest)=32),
 operation text NOT NULL CHECK(operation IN ('issue_key','revoke_key','create_service','update_service','replace_policy','settle_unknown','release_inference_pause','retention_delete','handoff')),
 target_kind text NOT NULL CHECK(target_kind IN ('key','principal','policy','request','ledger')),
 target_id uuid NOT NULL,
 result_status smallint NOT NULL CHECK(result_status IN (200,201,204)),
 result_version bigint CHECK(result_version>0),
 result_projection jsonb NOT NULL CHECK(jsonb_typeof(result_projection)='object' AND octet_length(result_projection::text)<=4096),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(actor_principal_id,command_id)
);
CREATE INDEX gw_commands_created ON gw_commands(created_at,actor_principal_id,command_id);
CREATE INDEX gw_commands_target ON gw_commands(target_kind,target_id,created_at);
CREATE TABLE gw_virtual_keys (
 id uuid PRIMARY KEY,
 principal_id uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 secret_hash bytea NOT NULL UNIQUE CHECK(octet_length(secret_hash)=32),
 hash_version smallint NOT NULL CHECK(hash_version=1),
 public_prefix text NOT NULL CHECK(octet_length(public_prefix) BETWEEN 1 AND 32),
 name text NOT NULL CHECK(char_length(name) BETWEEN 1 AND 100 AND octet_length(name)<=400 AND name !~ '[[:cntrl:]]'),
 grant_snapshot jsonb NOT NULL CHECK(jsonb_typeof(grant_snapshot)='array' AND jsonb_array_length(grant_snapshot)<=64 AND octet_length(grant_snapshot::text)<=32768),
 validity_preset text NOT NULL CHECK(validity_preset IN ('3_months','6_months','1_year','3_years','unlimited')),
 expires_at timestamptz,
 revoked_at timestamptz,
 created_by uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 created_command_id uuid NOT NULL,
 version bigint NOT NULL DEFAULT 1 CHECK(version>0),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE(id,principal_id),
 FOREIGN KEY(created_by,created_command_id) REFERENCES gw_commands(actor_principal_id,command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
 CHECK((validity_preset='unlimited')=(expires_at IS NULL)),
 CHECK(expires_at IS NULL OR expires_at>created_at),
 CHECK(revoked_at IS NULL OR revoked_at>=created_at)
);
CREATE INDEX gw_keys_principal_created ON gw_virtual_keys(principal_id,created_at,id);
CREATE INDEX gw_keys_created ON gw_virtual_keys(created_at,id);
ALTER TABLE gw_policies ADD CONSTRAINT gw_policy_key_owner FOREIGN KEY(key_id,principal_id) REFERENCES gw_virtual_keys(id,principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT;
CREATE FUNCTION gw_check_key_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF (NEW.id,NEW.principal_id,NEW.secret_hash,NEW.hash_version,NEW.grant_snapshot,NEW.validity_preset,NEW.expires_at,NEW.created_at,NEW.created_by,NEW.created_command_id) IS DISTINCT FROM (OLD.id,OLD.principal_id,OLD.secret_hash,OLD.hash_version,OLD.grant_snapshot,OLD.validity_preset,OLD.expires_at,OLD.created_at,OLD.created_by,OLD.created_command_id) THEN
  RAISE EXCEPTION 'immutable key' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER gw_key_immutable BEFORE UPDATE ON gw_virtual_keys FOR EACH ROW EXECUTE FUNCTION gw_check_key_immutable();
CREATE TABLE gw_requests (
 id uuid PRIMARY KEY,
 principal_id uuid NOT NULL REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 key_id uuid,
 parent_request_id uuid,
 purpose text NOT NULL CHECK(purpose IN ('root','internal_count')),
 provider text NOT NULL CHECK(provider IN ('anthropic','openai','deepseek')),
 requested_model text NOT NULL CHECK(octet_length(requested_model) BETWEEN 1 AND 200 AND requested_model !~ '[[:cntrl:]]'),
 api text NOT NULL CHECK(api IN ('messages','count_tokens','responses','chat_completions','embeddings')),
 admitted_at timestamptz NOT NULL,
 admit_expires_at timestamptz NOT NULL,
 transport_deadline timestamptz NOT NULL,
 execution_deadline timestamptz NOT NULL,
 dispatch_authorized_at timestamptz,
 lease_expires_at timestamptz,
 finished_at timestamptz,
 executor_id uuid NOT NULL,
 execution_stopped_at timestamptz,
 phase text NOT NULL CHECK(phase IN ('admitted','dispatching','finished')),
 outcome text CHECK(outcome IN ('completed','rejected','failed','cancelled')),
 accounting_state text NOT NULL CHECK(accounting_state IN ('pending','settled','unknown')),
 cost_usd numeric(28,12) CHECK(cost_usd>=0 AND cost_usd<'Infinity'::numeric AND cost_usd<>'NaN'::numeric),
 pricing_snapshot jsonb NOT NULL CHECK(jsonb_typeof(pricing_snapshot)='object' AND octet_length(pricing_snapshot::text)<=32768),
 execution_snapshot jsonb NOT NULL CHECK(jsonb_typeof(execution_snapshot)='object' AND octet_length(execution_snapshot::text)<=32768),
 auth_config_digest bytea NOT NULL CHECK(octet_length(auth_config_digest)=32),
 safety_violation_recorded boolean NOT NULL DEFAULT false,
 provider_request_id text CHECK(octet_length(provider_request_id) BETWEEN 1 AND 256 AND provider_request_id !~ '[[:cntrl:]]'),
 http_status smallint CHECK(http_status BETWEEN 100 AND 599),
 reason_code text CHECK(octet_length(reason_code) BETWEEN 1 AND 64 AND reason_code ~ '^[a-z_]+$'),
 result_digest bytea CHECK(octet_length(result_digest)=32),
 settlement_digest bytea CHECK(octet_length(settlement_digest)=32),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0),
 UNIQUE(id,principal_id),
 FOREIGN KEY(key_id,principal_id) REFERENCES gw_virtual_keys(id,principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 FOREIGN KEY(parent_request_id,principal_id) REFERENCES gw_requests(id,principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 CHECK((provider='anthropic' AND api IN ('messages','count_tokens')) OR (provider='openai' AND api IN ('responses','chat_completions','embeddings')) OR (provider='deepseek' AND api='chat_completions')),
 CHECK((purpose='root' AND parent_request_id IS NULL) OR (purpose='internal_count' AND parent_request_id IS NOT NULL AND provider='anthropic' AND api='count_tokens')),
 CHECK(admitted_at<admit_expires_at AND admit_expires_at<=transport_deadline AND transport_deadline<execution_deadline),
 CHECK(dispatch_authorized_at IS NULL OR (dispatch_authorized_at>=admitted_at AND dispatch_authorized_at<admit_expires_at)),
 CHECK(lease_expires_at IS NULL OR lease_expires_at=execution_deadline),
 CHECK(finished_at IS NULL OR finished_at>=admitted_at),
 CHECK(execution_stopped_at IS NULL OR execution_stopped_at>=admitted_at),
 CHECK(((phase='admitted' AND accounting_state='pending' AND outcome IS NULL AND finished_at IS NULL AND cost_usd IS NULL AND dispatch_authorized_at IS NULL AND lease_expires_at IS NULL)
 OR (phase='dispatching' AND accounting_state='pending' AND outcome IS NULL AND finished_at IS NULL AND cost_usd IS NULL AND dispatch_authorized_at IS NOT NULL AND lease_expires_at IS NOT NULL)
 OR (phase='finished' AND outcome IS NOT NULL AND finished_at IS NOT NULL
 AND ((accounting_state='settled' AND cost_usd IS NOT NULL AND cost_usd>=0) OR (accounting_state='unknown' AND cost_usd IS NULL))
 AND ((dispatch_authorized_at IS NOT NULL AND lease_expires_at IS NOT NULL)
 OR (dispatch_authorized_at IS NULL AND lease_expires_at IS NULL AND accounting_state='settled' AND cost_usd IS NOT NULL AND cost_usd=0 AND outcome IN ('rejected','failed','cancelled'))))) IS TRUE)
);
CREATE UNIQUE INDEX gw_one_internal_count ON gw_requests(parent_request_id) WHERE parent_request_id IS NOT NULL;
CREATE INDEX gw_requests_principal ON gw_requests(principal_id,admitted_at,id);
CREATE INDEX gw_requests_admitted ON gw_requests(admitted_at,id);
CREATE INDEX gw_requests_provider_id ON gw_requests(provider,provider_request_id) WHERE provider_request_id IS NOT NULL;
CREATE INDEX gw_requests_unknown ON gw_requests(principal_id,admitted_at,id) WHERE accounting_state='unknown';
CREATE INDEX gw_requests_principal_lease ON gw_requests(principal_id,lease_expires_at,id) WHERE phase='dispatching';
CREATE INDEX gw_requests_recover_admitted ON gw_requests(admit_expires_at,id) WHERE phase='admitted';
CREATE INDEX gw_requests_recover_dispatch ON gw_requests(lease_expires_at,id) WHERE phase='dispatching';
CREATE INDEX gw_requests_settled ON gw_requests(finished_at,id) WHERE phase='finished' AND accounting_state='settled';
ALTER TABLE gw_principals ADD CONSTRAINT gw_pause_request_owner FOREIGN KEY(inference_pause_request_id,id) REFERENCES gw_requests(id,principal_id) ON UPDATE RESTRICT ON DELETE RESTRICT;
CREATE FUNCTION gw_check_request() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND (NEW.id,NEW.principal_id,NEW.key_id,NEW.parent_request_id,NEW.purpose,NEW.provider,NEW.requested_model,NEW.api,NEW.admitted_at,NEW.admit_expires_at,NEW.transport_deadline,NEW.execution_deadline,NEW.executor_id,NEW.pricing_snapshot,NEW.execution_snapshot,NEW.auth_config_digest) IS DISTINCT FROM (OLD.id,OLD.principal_id,OLD.key_id,OLD.parent_request_id,OLD.purpose,OLD.provider,OLD.requested_model,OLD.api,OLD.admitted_at,OLD.admit_expires_at,OLD.transport_deadline,OLD.execution_deadline,OLD.executor_id,OLD.pricing_snapshot,OLD.execution_snapshot,OLD.auth_config_digest) THEN
  RAISE EXCEPTION 'immutable request' USING ERRCODE='23514';
 END IF;
 IF NEW.parent_request_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM gw_requests p WHERE p.id=NEW.parent_request_id AND p.purpose='root' AND p.principal_id=NEW.principal_id AND p.key_id IS NOT DISTINCT FROM NEW.key_id) THEN
  RAISE EXCEPTION 'invalid parent' USING ERRCODE='23514';
 END IF;
 IF TG_OP='UPDATE' AND ((OLD.dispatch_authorized_at IS NOT NULL AND (NEW.dispatch_authorized_at,NEW.lease_expires_at) IS DISTINCT FROM (OLD.dispatch_authorized_at,OLD.lease_expires_at)) OR (OLD.safety_violation_recorded AND NOT NEW.safety_violation_recorded)) THEN
  RAISE EXCEPTION 'immutable dispatch fact' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER gw_request_immutable BEFORE INSERT OR UPDATE ON gw_requests FOR EACH ROW EXECUTE FUNCTION gw_check_request();
CREATE TABLE gw_usage_totals (
 scope_id uuid NOT NULL REFERENCES gw_policies(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 period_start timestamptz NOT NULL,
 period_end timestamptz NOT NULL,
 timezone_snapshot text NOT NULL,
 cost_usd numeric(28,12) NOT NULL DEFAULT 0 CHECK(cost_usd>=0 AND cost_usd<'Infinity'::numeric AND cost_usd<>'NaN'::numeric),
 reserved_usd numeric(28,12) NOT NULL DEFAULT 0 CHECK(reserved_usd>=0 AND reserved_usd<'Infinity'::numeric AND reserved_usd<>'NaN'::numeric),
 carried_cost_usd numeric(28,12) NOT NULL DEFAULT 0 CHECK(carried_cost_usd BETWEEN 0 AND cost_usd),
 unknown_count bigint NOT NULL DEFAULT 0 CHECK(unknown_count>=0),
 settled_count bigint NOT NULL DEFAULT 0 CHECK(settled_count>=0),
 input_tokens bigint NOT NULL DEFAULT 0 CHECK(input_tokens>=0),
 output_tokens bigint NOT NULL DEFAULT 0 CHECK(output_tokens>=0),
 carried_settled_count bigint NOT NULL DEFAULT 0 CHECK(carried_settled_count BETWEEN 0 AND settled_count),
 carried_input_tokens bigint NOT NULL DEFAULT 0 CHECK(carried_input_tokens BETWEEN 0 AND input_tokens),
 carried_output_tokens bigint NOT NULL DEFAULT 0 CHECK(carried_output_tokens BETWEEN 0 AND output_tokens),
 settled_unknown_tokens_count bigint NOT NULL DEFAULT 0 CHECK(settled_unknown_tokens_count BETWEEN 0 AND settled_count),
 carried_settled_unknown_tokens_count bigint NOT NULL DEFAULT 0 CHECK(carried_settled_unknown_tokens_count BETWEEN 0 AND carried_settled_count AND carried_settled_unknown_tokens_count<=settled_unknown_tokens_count),
 detail_deleted_through timestamptz,
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(scope_id,period_start),
 CHECK(period_start<period_end),
 CHECK(detail_deleted_through IS NULL OR (period_start<=detail_deleted_through AND detail_deleted_through<period_end))
);
CREATE TABLE gw_rate_windows (
 scope_id uuid NOT NULL REFERENCES gw_policies(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 lane text NOT NULL CHECK(lane IN ('inference','token_count')),
 window_start timestamptz NOT NULL,
 window_end timestamptz NOT NULL,
 admitted_count bigint NOT NULL DEFAULT 0 CHECK(admitted_count>=0),
 reserved_tokens bigint NOT NULL DEFAULT 0 CHECK(reserved_tokens>=0),
 settled_tokens bigint NOT NULL DEFAULT 0 CHECK(settled_tokens>=0),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(scope_id,lane,window_start),
 CHECK(window_end=window_start+interval '60 seconds' AND extract(epoch FROM window_start)::numeric % 60=0)
);
CREATE INDEX gw_rate_expiry ON gw_rate_windows(window_end,scope_id,lane);
CREATE TABLE gw_request_budgets (
 request_id uuid NOT NULL REFERENCES gw_requests(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 scope_id uuid NOT NULL REFERENCES gw_policies(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 policy_version bigint NOT NULL CHECK(policy_version>0),
 policy_snapshot jsonb NOT NULL CHECK(jsonb_typeof(policy_snapshot)='object' AND octet_length(policy_snapshot::text)<=32768),
 period_start timestamptz NOT NULL,
 period_end timestamptz NOT NULL,
 timezone_snapshot text NOT NULL,
 rate_lane text NOT NULL CHECK(rate_lane IN ('inference','token_count')),
 rate_start timestamptz NOT NULL,
 rate_end timestamptz NOT NULL,
 budget_mode text NOT NULL CHECK(budget_mode IN ('actual','reserved')),
 reservation_usd numeric(28,12) NOT NULL CHECK(reservation_usd>=0 AND reservation_usd<'Infinity'::numeric AND reservation_usd<>'NaN'::numeric),
 remaining_reservation_usd numeric(28,12) NOT NULL CHECK(remaining_reservation_usd BETWEEN 0 AND reservation_usd),
 reserved_tokens bigint NOT NULL CHECK(reserved_tokens>=0),
 remaining_reserved_tokens bigint NOT NULL CHECK(remaining_reserved_tokens BETWEEN 0 AND reserved_tokens),
 applied_cost_usd numeric(28,12) CHECK(applied_cost_usd>=0 AND applied_cost_usd<'Infinity'::numeric AND applied_cost_usd<>'NaN'::numeric),
 applied_input_tokens bigint CHECK(applied_input_tokens>=0),
 applied_output_tokens bigint CHECK(applied_output_tokens>=0),
 state text NOT NULL CHECK(state IN ('admitted','active','unknown','settled')),
 PRIMARY KEY(request_id,scope_id),
 FOREIGN KEY(scope_id,period_start) REFERENCES gw_usage_totals(scope_id,period_start) ON UPDATE RESTRICT ON DELETE RESTRICT,
 FOREIGN KEY(scope_id,rate_lane,rate_start) REFERENCES gw_rate_windows(scope_id,lane,window_start) ON UPDATE RESTRICT ON DELETE RESTRICT,
 CHECK(period_start<period_end AND rate_end=rate_start+interval '60 seconds'),
 CHECK(((state='settled' AND remaining_reservation_usd=0 AND remaining_reserved_tokens=0 AND applied_cost_usd IS NOT NULL)
 OR (state<>'settled' AND applied_cost_usd IS NULL AND applied_input_tokens IS NULL AND applied_output_tokens IS NULL)) IS TRUE)
);
CREATE INDEX gw_budgets_period ON gw_request_budgets(scope_id,period_start,request_id);
CREATE INDEX gw_budgets_rate ON gw_request_budgets(scope_id,rate_lane,rate_start,request_id);
CREATE TABLE gw_usage_events (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 request_id uuid NOT NULL REFERENCES gw_requests(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 kind text NOT NULL CHECK(kind IN ('partial','final','settlement','late_final')),
 observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 actual_model text CHECK(octet_length(actual_model) BETWEEN 1 AND 200),
 provider_usage jsonb NOT NULL CHECK(jsonb_typeof(provider_usage)='object' AND octet_length(provider_usage::text)<=16384),
 normalized_usage jsonb NOT NULL CHECK(jsonb_typeof(normalized_usage)='object' AND octet_length(normalized_usage::text)<=16384),
 pricing_resolution jsonb NOT NULL CHECK(jsonb_typeof(pricing_resolution)='object' AND octet_length(pricing_resolution::text)<=16384),
 reason_code text CHECK(octet_length(reason_code) BETWEEN 1 AND 64 AND reason_code ~ '^[a-z_]+$'),
 content_digest bytea NOT NULL CHECK(octet_length(content_digest)=32),
 UNIQUE(request_id,kind)
);
CREATE TABLE gw_audit_events (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 actor_kind text NOT NULL CHECK(actor_kind IN ('human','system')),
 actor_principal_id uuid REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 command_id uuid,
 executor_id uuid,
 request_id uuid,
 actor_auth_config_digest bytea NOT NULL CHECK(octet_length(actor_auth_config_digest)=32),
 operation text NOT NULL CHECK(octet_length(operation) BETWEEN 1 AND 64),
 target_kind text NOT NULL CHECK(octet_length(target_kind) BETWEEN 1 AND 32),
 target_id uuid NOT NULL,
 before_snapshot jsonb NOT NULL CHECK(jsonb_typeof(before_snapshot)='object' AND octet_length(before_snapshot::text)<=32768),
 after_snapshot jsonb NOT NULL CHECK(jsonb_typeof(after_snapshot)='object' AND octet_length(after_snapshot::text)<=32768),
 reason text CHECK(char_length(reason) BETWEEN 1 AND 1000 AND octet_length(reason)<=4096 AND reason !~ '[[:cntrl:]]'),
 evidence_ref text CHECK(octet_length(evidence_ref) BETWEEN 1 AND 2048 AND evidence_ref !~ '[[:cntrl:]]'),
 FOREIGN KEY(actor_principal_id,command_id) REFERENCES gw_commands(actor_principal_id,command_id) ON UPDATE RESTRICT ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
 CHECK(((actor_kind='human' AND actor_principal_id IS NOT NULL AND executor_id IS NULL)
 OR (actor_kind='system' AND actor_principal_id IS NULL AND command_id IS NULL AND executor_id IS NOT NULL AND request_id IS NOT NULL AND operation='inference_auto_pause')) IS TRUE)
);
CREATE UNIQUE INDEX gw_audit_command ON gw_audit_events(actor_principal_id,command_id) WHERE command_id IS NOT NULL;
CREATE UNIQUE INDEX gw_audit_system ON gw_audit_events(operation,request_id) WHERE actor_kind='system';
CREATE INDEX gw_audit_occurred ON gw_audit_events(occurred_at,id);
CREATE INDEX gw_audit_target ON gw_audit_events(target_kind,target_id,occurred_at,id);
CREATE INDEX gw_audit_actor ON gw_audit_events(actor_principal_id,occurred_at,id);
