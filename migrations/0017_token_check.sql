-- 토큰 대조와 사후 보정(결정 0005 11번, 23~27번). Admin API 키(ANTHROPIC_ADMIN_API_KEY)가 있을 때만 채운다.

-- Provider 키와 Anthropic의 API 키 ID. 사용량 보고서는 키 원문이 아니라 이 ID로 키를 가리킨다.
-- 키 목록의 앞뒤 글자(partial_key_hint)로 찾는다. Provider 키를 해제해도 지난 사용량을 대조하려고 남겨 둔다.
-- 해제한 키를 다시 등록하면 Provider 키 두 개가 같은 API 키 ID를 가리킨다.
CREATE TABLE gw_provider_key_ids (
 credential_id uuid PRIMARY KEY REFERENCES gw_provider_credentials(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 api_key_id text NOT NULL CHECK(octet_length(api_key_id) BETWEEN 1 AND 200 AND api_key_id !~ '[[:cntrl:]]'),
 matched_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX gw_provider_key_ids_api ON gw_provider_key_ids(api_key_id);

-- API 키 ID마다 사용량 보고서를 받아 둔 시간. covered_from부터 covered_until 전까지의 1시간 칸만 대조한다.
CREATE TABLE gw_token_check_keys (
 api_key_id text PRIMARY KEY CHECK(octet_length(api_key_id) BETWEEN 1 AND 200 AND api_key_id !~ '[[:cntrl:]]'),
 covered_from timestamptz NOT NULL CHECK(extract(epoch FROM covered_from)::numeric % 3600=0),
 covered_until timestamptz NOT NULL CHECK(extract(epoch FROM covered_until)::numeric % 3600=0),
 CHECK(covered_from<covered_until)
);

-- Anthropic 사용량 보고서의 1시간 칸. 게이트웨이의 Provider 키에 해당하는 API 키만 저장한다.
-- 늦게 들어오는 값이 있어 조회할 때마다 최근 48시간을 덮어쓴다.
CREATE TABLE gw_provider_usage (
 api_key_id text NOT NULL CHECK(octet_length(api_key_id) BETWEEN 1 AND 200 AND api_key_id !~ '[[:cntrl:]]'),
 model text NOT NULL CHECK(octet_length(model) BETWEEN 1 AND 200 AND model !~ '[[:cntrl:]]'),
 hour_start timestamptz NOT NULL CHECK(extract(epoch FROM hour_start)::numeric % 3600=0),
 uncached_input_tokens bigint NOT NULL CHECK(uncached_input_tokens>=0),
 cache_write_5m_tokens bigint NOT NULL CHECK(cache_write_5m_tokens>=0),
 cache_write_1h_tokens bigint NOT NULL CHECK(cache_write_1h_tokens>=0),
 cache_read_tokens bigint NOT NULL CHECK(cache_read_tokens>=0),
 output_tokens bigint NOT NULL CHECK(output_tokens>=0),
 PRIMARY KEY(api_key_id,model,hour_start)
);
CREATE INDEX gw_provider_usage_hour ON gw_provider_usage(hour_start);

-- 같은 칸의 게이트웨이 쪽 합계. 사용량 보고서를 받을 때 같은 시간을 다시 계산해 둔다(화면은 이 표만 읽는다).
-- 정산한 messages 요청의 토큰이고, 사후 보정한 요청은 고친 출력이다.
-- 캐시 쓰기는 사용량 보고서처럼 5분과 1시간으로 나눠 따로 비교한다(결정 0012).
CREATE TABLE gw_gateway_usage (
 api_key_id text NOT NULL CHECK(octet_length(api_key_id) BETWEEN 1 AND 200 AND api_key_id !~ '[[:cntrl:]]'),
 model text NOT NULL CHECK(octet_length(model) BETWEEN 1 AND 200 AND model !~ '[[:cntrl:]]'),
 hour_start timestamptz NOT NULL CHECK(extract(epoch FROM hour_start)::numeric % 3600=0),
 input_tokens bigint NOT NULL CHECK(input_tokens>=0),
 cache_write_5m_tokens bigint NOT NULL CHECK(cache_write_5m_tokens>=0),
 cache_write_1h_tokens bigint NOT NULL CHECK(cache_write_1h_tokens>=0),
 cache_read_tokens bigint NOT NULL CHECK(cache_read_tokens>=0),
 output_tokens bigint NOT NULL CHECK(output_tokens>=0),
 cutoff_requests bigint NOT NULL CHECK(cutoff_requests>=0),
 corrected_requests bigint NOT NULL CHECK(corrected_requests>=0),
 PRIMARY KEY(api_key_id,model,hour_start)
);
CREATE INDEX gw_gateway_usage_hour ON gw_gateway_usage(hour_start);
-- 그 시간에 그 API 키로 보낸 요청 수와, 그 가운데 사용량을 모르는 요청(정산한 사용량 기록이 없는 요청) 수. 모델과 무관하다.
CREATE TABLE gw_gateway_activity (
 api_key_id text NOT NULL CHECK(octet_length(api_key_id) BETWEEN 1 AND 200 AND api_key_id !~ '[[:cntrl:]]'),
 hour_start timestamptz NOT NULL CHECK(extract(epoch FROM hour_start)::numeric % 3600=0),
 requests bigint NOT NULL CHECK(requests>0),
 unknown_requests bigint NOT NULL CHECK(unknown_requests>=0 AND unknown_requests<=requests),
 PRIMARY KEY(api_key_id,hour_start)
);
CREATE INDEX gw_gateway_activity_hour ON gw_gateway_activity(hour_start);

-- 마지막 조회 결과. 여러 서버 가운데 lease_until을 먼저 잡은 한 대만 조회한다.
-- 사용량 보고서가 1분 칸에 요청을 보낸 분과 끝난 분 가운데 어디에 넣는지는 문서에 없다.
-- 사용량을 모두 아는 분 가운데 한쪽 기준으로만 맞는 분을 세어(start_minutes, end_minutes) 기준을 정한다.
-- minutes_checked_until 전의 분은 이미 셌다.
CREATE TABLE gw_token_check_state (
 provider text PRIMARY KEY CHECK(provider='anthropic'),
 lease_until timestamptz,
 last_attempt_at timestamptz,
 last_success_at timestamptz,
 last_error text CHECK(octet_length(last_error) BETWEEN 1 AND 64 AND last_error ~ '^[a-z_]+$'),
 start_minutes bigint NOT NULL DEFAULT 0 CHECK(start_minutes>=0),
 end_minutes bigint NOT NULL DEFAULT 0 CHECK(end_minutes>=0),
 minutes_checked_until timestamptz CHECK(extract(epoch FROM minutes_checked_until)::numeric % 60=0)
);
INSERT INTO gw_token_check_state(provider) VALUES('anthropic');

-- 사후 보정 스위치. 기본은 끄기다.
ALTER TABLE gw_settlement_rules ADD COLUMN correction boolean NOT NULL DEFAULT false;

-- 보정은 요청마다 한 번, usage 기록(kind='correction')과 시스템 감사 기록(usage_correction)으로 남긴다.
ALTER TABLE gw_usage_events DROP CONSTRAINT gw_usage_events_kind_check;
ALTER TABLE gw_usage_events ADD CONSTRAINT gw_usage_events_kind_check CHECK(kind IN ('partial','final','settlement','late_final','correction'));
ALTER TABLE gw_audit_events DROP CONSTRAINT gw_audit_events_check;
ALTER TABLE gw_audit_events ADD CONSTRAINT gw_audit_events_check CHECK(((actor_kind='human' AND actor_principal_id IS NOT NULL AND executor_id IS NULL)
 OR (actor_kind='system' AND actor_principal_id IS NULL AND command_id IS NULL AND executor_id IS NOT NULL AND request_id IS NOT NULL AND operation IN ('inference_auto_pause','usage_correction'))) IS TRUE);
