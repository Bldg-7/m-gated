use m_gated::ledger::PgIdentityStore;
use sqlx::{
    Connection, PgConnection,
    postgres::{PgConnectOptions, PgSslMode},
};
use std::sync::Arc;

pub fn test_options(raw: &str) -> Result<PgConnectOptions, &'static str> {
    let url = url::Url::parse(raw).map_err(|_| "test URL invalid")?;
    if url.scheme() != "postgres"
        || url.host_str() != Some("127.0.0.1")
        || url.port() != Some(64603)
        || url.path() != "/m_gated_test"
        || url.username() != "m_gated_test"
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("dedicated loopback test URL required");
    }
    m_gated::ledger::explicit_database_options(
        "127.0.0.1",
        64603,
        "m_gated_test",
        "m_gated_test",
        "",
        PgSslMode::Disable,
    )
    .map_err(|_| "ambient PG settings forbidden")
}
// 진단은 고정 분류만 출력한다. DB 오류 원문/URL/options는 panic에 넣지 않는다.
fn pool_connect_error_code(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Configuration(_) => "configuration",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::Database(error) => match error.code().as_deref() {
            Some("53300") => "database_connections",
            _ => "database_other",
        },
        _ => "other",
    }
}
async fn cleanup_failed_pool_initialization(
    connection: &mut PgConnection,
    schema: &str,
    ownership: &std::path::Path,
) -> bool {
    let directory =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-owned-schemas");
    let expected = "m_gated_test/127.0.0.1:64603\n";
    if ownership != directory.join(schema)
        || std::fs::read_to_string(ownership).ok().as_deref() != Some(expected)
    {
        return false;
    }
    // 초기 CREATE에 사용한 자기 연결을 재사용해 자원 부족 때 새 연결을 요구하지 않는다.
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        sqlx::raw_sql("SET lock_timeout='500ms'; SET statement_timeout='2s'").execute(&mut *connection).await.map_err(|_| ())?;
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1 AND nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user))")
            .bind(schema).fetch_one(&mut *connection).await.map_err(|_| ())?;
        if !owned { return Err(()); }
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE")).execute(&mut *connection).await.map_err(|_| ())?;
        for _ in 0..2 {
            let absent: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
                .bind(schema).fetch_one(&mut *connection).await.map_err(|_| ())?;
            if !absent { return Err(()); }
        }
        if std::fs::read_to_string(ownership).ok().as_deref() != Some(expected) { return Err(()); }
        std::fs::remove_file(ownership).map_err(|_| ())
    }).await;
    matches!(result, Ok(Ok(())))
}
pub struct TestDb {
    pub store: Arc<PgIdentityStore>,
    pub schema: String,
    options: PgConnectOptions,
    closed: std::sync::atomic::AtomicBool,
    ownership: std::path::PathBuf,
}
impl TestDb {
    pub async fn new() -> Self {
        Self::initialize(format!("gw_test_{}", uuid::Uuid::new_v4().simple()), false).await
    }
    /// 실제 연결 장애 원인 재현이 아닌, 초기화 실패 정리 경로의 명시적 테스트 주입이다.
    #[allow(dead_code)]
    pub async fn injected_pool_connect_failure(schema: String) -> Self {
        let id = schema
            .strip_prefix("gw_test_")
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .expect("자기 UUID schema만 허용");
        assert_eq!(schema, format!("gw_test_{}", id.simple()));
        Self::initialize(schema, true).await
    }
    async fn initialize(schema: String, inject_pool_failure: bool) -> Self {
        let raw = std::env::var("TEST_DATABASE_URL").expect(
            "explicit TEST_DATABASE_URL required; product DATABASE_URL/DB_* fallback forbidden",
        );
        let options = test_options(&raw).expect("dedicated test URL required");
        let mut connection = PgConnection::connect_with(&options).await.unwrap();
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut connection)
            .await
            .unwrap();
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-owned-schemas");
        std::fs::create_dir_all(&directory).unwrap();
        let ownership = directory.join(&schema);
        std::fs::write(&ownership, "m_gated_test/127.0.0.1:64603\n").unwrap();
        let connected = if inject_pool_failure {
            Err(sqlx::Error::PoolTimedOut)
        } else {
            PgIdentityStore::pool_options()
                .connect_with(options.clone().options([("search_path", schema.as_str())]))
                .await
        };
        let pool = match connected {
            Ok(pool) => pool,
            Err(error) => {
                let code = pool_connect_error_code(&error);
                let cleaned =
                    cleanup_failed_pool_initialization(&mut connection, &schema, &ownership).await;
                panic!("TEST_POOL_INITIALIZATION_FAILED code={code} owned_cleanup={cleaned}");
            }
        };
        let store = Arc::new(PgIdentityStore::new(pool));
        let db = Self {
            store,
            schema,
            options,
            closed: std::sync::atomic::AtomicBool::new(false),
            ownership,
        };
        db.store.migrate().await.unwrap();
        db
    }
    // auth target도 공유하지만 wire fault target에서만 쓰는 테스트 조립이다.
    #[allow(dead_code)]
    pub async fn store_via_proxy(&self, port: u16) -> Arc<PgIdentityStore> {
        assert_ne!(port, 0);
        let pool = PgIdentityStore::pool_options()
            .connect_with(
                self.options
                    .clone()
                    .port(port)
                    .options([("search_path", self.schema.as_str())]),
            )
            .await
            .unwrap();
        Arc::new(PgIdentityStore::new(pool))
    }
    /// 자기 UUID namespace의 SQL 시각만 제어한다. 제품 코드/환경에는 시각 우회가 없다.
    #[allow(dead_code)]
    pub async fn clock_store(&self, port: Option<u16>) -> Arc<PgIdentityStore> {
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS gw_fixture_clock(observed timestamptz NOT NULL); INSERT INTO gw_fixture_clock SELECT '2025-03-31T23:59:00Z'::timestamptz WHERE NOT EXISTS(SELECT 1 FROM gw_fixture_clock); CREATE OR REPLACE FUNCTION clock_timestamp() RETURNS timestamptz LANGUAGE sql VOLATILE AS 'SELECT observed FROM gw_fixture_clock'; CREATE OR REPLACE FUNCTION transaction_timestamp() RETURNS timestamptz LANGUAGE sql VOLATILE AS 'SELECT observed FROM gw_fixture_clock'").execute(self.store.pool()).await.unwrap();
        let path = format!("{},pg_catalog", self.schema);
        let pool = PgIdentityStore::pool_options()
            .connect_with(
                self.options
                    .clone()
                    .port(port.unwrap_or(64603))
                    .options([("search_path", path.as_str())]),
            )
            .await
            .unwrap();
        Arc::new(PgIdentityStore::new(pool))
    }
    pub async fn close(self) {
        self.store.pool().close().await;
        let mut connection = PgConnection::connect_with(&self.options).await.unwrap();
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::remove_file(&self.ownership).unwrap();
    }
}
// 테스트 assertion panic도 자기 생성 UUID 하나만 정리한다. 제품 Drop/Task 수명과 무관하다.
impl Drop for TestDb {
    fn drop(&mut self) {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let options = self.options.clone();
        let schema = self.schema.clone();
        let cleaned = std::thread::Builder::new()
            .name("mgated-test-cleanup".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime
                    .block_on(async {
                        tokio::time::timeout(std::time::Duration::from_secs(3), async move {
                            let mut connection = PgConnection::connect_with(&options).await?;
                            sqlx::raw_sql("SET lock_timeout='500ms'; SET statement_timeout='2s'")
                                .execute(&mut connection)
                                .await?;
                            sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
                                .execute(&mut connection)
                                .await?;
                            connection.close().await
                        })
                        .await
                    })
                    .ok()?
                    .ok()
            })
            .ok()
            .and_then(|handle| handle.join().ok())
            .flatten()
            .is_some();
        if cleaned {
            let _ = std::fs::remove_file(&self.ownership);
        } else {
            eprintln!("TEST_SCHEMA_CLEANUP_FAILED: 자기 schema 소유 기록을 target에 보존했습니다.");
        }
    }
}
