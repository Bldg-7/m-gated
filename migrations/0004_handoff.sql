-- 이전 기준점은 주체당 한 번만 인수한다. 다른 명령 ID로 재실행해도 원장을 재가산하지 않는다.
CREATE TABLE gw_handoffs (
 principal_id uuid PRIMARY KEY REFERENCES gw_principals(id) ON UPDATE RESTRICT ON DELETE RESTRICT,
 source_id uuid NOT NULL,
 source_digest bytea NOT NULL CHECK(octet_length(source_digest)=32),
 source_ref text NOT NULL CHECK(octet_length(source_ref) BETWEEN 1 AND 2048 AND source_ref !~ '[[:cntrl:]]'),
 cutoff timestamptz NOT NULL,
 previous_executors_stopped_ref text NOT NULL CHECK(octet_length(previous_executors_stopped_ref) BETWEEN 1 AND 2048 AND previous_executors_stopped_ref !~ '[[:cntrl:]]'),
 source_disabled boolean NOT NULL,
 phase text NOT NULL CHECK(phase IN ('imported','active','rollback_blocked')),
 version bigint NOT NULL DEFAULT 1 CHECK(version>0),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
