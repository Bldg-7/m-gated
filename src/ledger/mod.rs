pub mod amount;
pub mod finance;
pub mod handoff;
pub mod policy;
pub mod recovery;
pub mod retention;
pub use finance::PgLedger;

use crate::{
    auth::{AccessSettings, AuthError, VerifiedIdentity},
    config::DatabaseConfig,
};
use sqlx::{
    PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;

const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_identity.sql")),
    (2, include_str!("../../migrations/0002_finance.sql")),
    (3, include_str!("../../migrations/0003_key_policy.sql")),
    (4, include_str!("../../migrations/0004_handoff.sql")),
    (5, include_str!("../../migrations/0005_alarm_totals.sql")),
    (
        6,
        include_str!("../../migrations/0006_provider_credentials.sql"),
    ),
    (7, include_str!("../../migrations/0007_model_catalog.sql")),
];

pub struct PgIdentityStore {
    pool: PgPool,
    gate: Arc<Semaphore>,
}
pub struct CurrentHuman {
    pub id: Uuid,
    pub version: i64,
    pub policy_id: Uuid,
    pub policy_version: i64,
    pub admin: bool,
}
impl PgIdentityStore {
    pub(crate) async fn begin_transaction(
        &self,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
        let mut timer = crate::telemetry::Timer::start(
            crate::telemetry::Metric::DbAcquire,
            crate::telemetry::Metric::DbAcquireMicros,
            crate::telemetry::Metric::DbAcquireFailed,
        );
        let result = self.pool.begin().await;
        if result.is_ok() {
            timer.success();
        }
        result
    }

    pub async fn connect(config: &DatabaseConfig) -> Result<Self, AuthError> {
        let mut options = explicit_database_options(
            &config.host,
            config.port,
            &config.name,
            &config.user,
            config.password.expose(),
            match config.ssl_mode {
                crate::config::DbSslMode::VerifyFull => PgSslMode::VerifyFull,
                crate::config::DbSslMode::Require => PgSslMode::Require,
            },
        )?;
        if let Some(pem) = &config.ca_certificate {
            options = options.ssl_root_cert_from_pem(pem.clone());
        }
        let pool = Self::pool_options()
            .connect_with(options)
            .await
            .map_err(|_| AuthError::DatabaseUnavailable)?;
        Ok(Self::new(pool))
    }
    pub fn pool_options() -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_millis(500))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET lock_timeout='500ms'")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET statement_timeout='2s'")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
    }
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            gate: Arc::new(Semaphore::new(6)),
        }
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
    pub async fn migrate(&self) -> Result<(), AuthError> {
        tokio::time::timeout(Duration::from_secs(120), self.migrate_transaction())
            .await
            .map_err(|_| AuthError::DatabaseUnavailable)?
            .map_err(|_| AuthError::DatabaseUnavailable)
    }
    async fn migrate_transaction(&self) -> Result<(), sqlx::Error> {
        use sha2::{Digest, Sha256};
        let mut tx = self.begin_transaction().await?;
        sqlx::raw_sql("SET LOCAL lock_timeout='30s'; SET LOCAL statement_timeout='120s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(7234638650625911)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS gw_schema_migrations(version bigint PRIMARY KEY,checksum bytea NOT NULL CHECK(octet_length(checksum)=32))").execute(&mut *tx).await?;
        for &(version, sql) in MIGRATIONS {
            let checksum = Sha256::digest(sql.as_bytes()).to_vec();
            let applied: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT checksum FROM gw_schema_migrations WHERE version=$1")
                    .bind(version)
                    .fetch_optional(&mut *tx)
                    .await?;
            match applied {
                Some(existing) if existing != checksum => {
                    return Err(sqlx::Error::Protocol("migration checksum".into()));
                }
                Some(_) => {}
                None => {
                    sqlx::raw_sql(sql).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO gw_schema_migrations VALUES($1,$2)")
                        .bind(version)
                        .bind(checksum)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        let future: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM gw_schema_migrations WHERE version NOT IN (1,2,3,4,5,6,7))",
        )
        .fetch_one(&mut *tx)
        .await?;
        if future {
            return Err(sqlx::Error::Protocol("migration version".into()));
        }
        tx.commit().await
    }
    pub async fn health(&self) -> (bool, bool) {
        use sha2::{Digest, Sha256};
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let _gate = self.gate.clone().try_acquire_owned().map_err(|_| sqlx::Error::PoolTimedOut)?;
            let mut connection = self.pool.acquire().await?;
            sqlx::query("SELECT 1").execute(&mut *connection).await?;
            let schema = async {
                let rows=sqlx::query("SELECT version,checksum FROM gw_schema_migrations ORDER BY version LIMIT 8").fetch_all(&mut *connection).await?;
                let versions=rows.len()==MIGRATIONS.len() && rows.iter().zip(MIGRATIONS).all(|(r,(version,sql))|r.get::<i64,_>("version")==*version&&r.get::<Vec<u8>,_>("checksum")==Sha256::digest(sql.as_bytes()).to_vec());
                let tables:bool=sqlx::query_scalar("SELECT count(*)=15 FROM unnest(ARRAY['gw_principals','gw_virtual_keys','gw_policies','gw_requests','gw_request_budgets','gw_usage_events','gw_usage_totals','gw_rate_windows','gw_commands','gw_audit_events','gw_handoffs','gw_alarm_totals','gw_provider_credentials','gw_model_catalog_proposals','gw_model_catalog_versions']::text[]) AS names(name) WHERE to_regclass(name) IS NOT NULL").fetch_one(&mut *connection).await?;
                Ok::<_,sqlx::Error>(versions && tables)
            }.await.unwrap_or(false);
            Ok::<_,sqlx::Error>((true,schema))
        }).await;
        result.ok().and_then(Result::ok).unwrap_or((false, false))
    }
    pub async fn current_human(
        &self,
        identity: &VerifiedIdentity,
        settings: &AccessSettings,
    ) -> Result<CurrentHuman, AuthError> {
        if !settings.allows(identity.email()) {
            return Err(AuthError::AccessDenied);
        }
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::DatabaseUnavailable)?;
        tokio::time::timeout(
            Duration::from_secs(3),
            self.upsert_human(identity, settings),
        )
        .await
        .map_err(|_| AuthError::DatabaseUnavailable)?
        .map_err(|_| AuthError::DatabaseUnavailable)?
        .ok_or(AuthError::AccessDenied)
    }

    async fn upsert_human(
        &self,
        identity: &VerifiedIdentity,
        settings: &AccessSettings,
    ) -> Result<Option<CurrentHuman>, sqlx::Error> {
        let mut tx = self.begin_transaction().await?;
        // identity만 conflict key다. 같은 email의 다른 sub를 조회하거나 갱신하지 않는다.
        sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human',$2,$3,$4,$5) ON CONFLICT(issuer,subject) WHERE kind='human' DO NOTHING")
                .bind(Uuid::new_v4()).bind(identity.issuer()).bind(identity.subject()).bind(identity.email()).bind(identity.display_name()).execute(&mut *tx).await?;
        let row=sqlx::query("SELECT id,version,disabled_at IS NOT NULL AS disabled,security_fence_ref IS NOT NULL AS fenced,email,display_name FROM gw_principals WHERE issuer=$1 AND subject=$2 AND kind='human' FOR UPDATE")
                .bind(identity.issuer()).bind(identity.subject()).fetch_one(&mut *tx).await?;
        if row.get::<bool, _>("disabled") || row.get::<bool, _>("fenced") {
            tx.rollback().await?;
            return Ok(None);
        }
        let id: Uuid = row.get("id");
        let mut version: i64 = row.get("version");
        if row.get::<String, _>("email") != identity.email()
            || row.get::<String, _>("display_name") != identity.display_name()
        {
            version=sqlx::query_scalar("UPDATE gw_principals SET email=$2,display_name=$3,version=version+1,updated_at=clock_timestamp() WHERE id=$1 RETURNING version")
                    .bind(id).bind(identity.email()).bind(identity.display_name()).fetch_one(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,'[]','actual','calendar_month','UTC',$2) ON CONFLICT(principal_id) WHERE key_id IS NULL DO NOTHING")
                .bind(Uuid::new_v4()).bind(id).execute(&mut *tx).await?;
        let policy = sqlx::query(
            "SELECT id,version FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let human = CurrentHuman {
            id,
            version,
            policy_id: policy.get("id"),
            policy_version: policy.get("version"),
            admin: settings.is_admin(identity.email()),
        };
        tx.commit().await?;
        Ok(Some(human))
    }
}

/// SQLx 고정 버전이 읽는 ambient PG 설정은 앱의 명시 DB 설정 소스가 아니다.
pub const AMBIENT_PG_VARIABLES: &[&str] = &[
    "PGPORT",
    "PGHOSTADDR",
    "PGHOST",
    "PGUSER",
    "PGDATABASE",
    "PGPASSWORD",
    "PGSSLROOTCERT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLMODE",
    "PGAPPNAME",
    "PGOPTIONS",
    "PGPASSFILE",
];
pub fn reject_ambient_pg(present: impl IntoIterator<Item = bool>) -> Result<(), AuthError> {
    if present.into_iter().any(|is_present| is_present) {
        Err(AuthError::DatabaseUnavailable)
    } else {
        Ok(())
    }
}
pub fn explicit_database_options(
    host: &str,
    port: u16,
    database: &str,
    user: &str,
    password: &str,
    ssl_mode: PgSslMode,
) -> Result<PgConnectOptions, AuthError> {
    // 이름별 존재 여부만 사용하며 값/파일 경로를 보관·출력하지 않는다. 빈 값도 거부한다.
    reject_ambient_pg(
        AMBIENT_PG_VARIABLES
            .iter()
            .map(|name| std::env::var_os(name).is_some()),
    )?;
    Ok(PgConnectOptions::new_without_pgpass()
        .host(host)
        .port(port)
        .database(database)
        .username(user)
        .password(password)
        .application_name("m-gated")
        .ssl_mode(ssl_mode))
}

pub mod dashboard;

pub mod management;
pub mod management_query;

pub mod commands;
pub mod credentials;

mod settlement;
