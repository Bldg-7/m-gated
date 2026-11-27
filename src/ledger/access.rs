//! 접근 목록(결정 0001). 누가 게이트웨이를 쓸 수 있고 누가 관리자인지 정한다.
//!
//! 목록은 이메일로 찾는다. 관리자는 아직 로그인한 적 없는 사람도 미리 더할 수 있다.
//! 설정의 고정 관리자(`admin_email`) 항목과 자기 자신의 항목은 바꾸거나 지울 수 없다.
use super::{
    PgIdentityStore,
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
    management_query::stamp,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::time::Duration;
use uuid::Uuid;

/// 접근 목록의 역할.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Admin,
}
impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
        }
    }
}

fn select_sql(filter: &str) -> String {
    format!(
        "SELECT a.id,a.email,a.role,a.version,{} AS added_at,b.email AS added_by_email,p.id AS principal_id,p.disabled_at IS NOT NULL AS disabled FROM gw_access a LEFT JOIN gw_principals b ON b.id=a.added_by LEFT JOIN gw_principals p ON p.kind='human' AND p.email=a.email {filter}",
        stamp("a.added_at")
    )
}

fn view(row: &PgRow, fixed_admin: &str) -> Value {
    let email: String = row.get("email");
    json!({
        "id": row.get::<Uuid, _>("id"),
        "email": email,
        "role": row.get::<String, _>("role"),
        "fixed": email == fixed_admin,
        "added_by_email": row.get::<Option<String>, _>("added_by_email"),
        "added_at": row.get::<String, _>("added_at"),
        "version": row.get::<i64, _>("version").to_string(),
        "principal": row.get::<Option<Uuid>, _>("principal_id").map(|id| json!({
            "id": id,
            "disabled": row.get::<bool, _>("disabled"),
        })),
    })
}

async fn entry(
    c: &mut PgConnection,
    id: Uuid,
    fixed_admin: &str,
) -> Result<Option<Value>, sqlx::Error> {
    Ok(sqlx::query(&select_sql("WHERE a.id=$1"))
        .bind(id)
        .fetch_optional(c)
        .await?
        .map(|row| view(&row, fixed_admin)))
}

/// 바꾸려는 항목을 잠그고 읽는다. 고정 관리자와 자기 자신의 항목은 바꿀 수 없다.
async fn lock_changeable(
    c: &mut PgConnection,
    ctx: &CommandContext<'_>,
    id: Uuid,
    expected: i64,
) -> Result<Value, ManagementError> {
    let row = sqlx::query("SELECT email,version FROM gw_access WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or(ManagementError::NotFound)?;
    let email: String = row.get("email");
    let actor_email: Option<String> =
        sqlx::query_scalar("SELECT email FROM gw_principals WHERE id=$1")
            .bind(ctx.actor)
            .fetch_one(&mut *c)
            .await?;
    if email == ctx.settings.fixed_admin() || actor_email.as_deref() == Some(email.as_str()) {
        return Err(ManagementError::AccessProtected);
    }
    management::cas(&expected.to_string(), row.get("version"))?;
    entry(c, id, ctx.settings.fixed_admin())
        .await?
        .ok_or(ManagementError::Invariant)
}

/// 명령 기록과 감사 기록을 남긴다.
#[allow(clippy::too_many_arguments)]
async fn record(
    c: &mut PgConnection,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
    operation: &str,
    id: Uuid,
    status: u16,
    version: Option<i64>,
    before: &Value,
    after: &Value,
) -> Result<CommandResult, ManagementError> {
    let ack = json!({"command_id":ctx.command_id,"operation":operation,"target":{"kind":"access","id":id},"result_version":version.map(|v|v.to_string()),"effect":"changed","access":after});
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,$4,'access',$5,$6,$7,$8,clock_timestamp())")
        .bind(ctx.actor)
        .bind(ctx.command_id)
        .bind(digest.as_slice())
        .bind(operation)
        .bind(id)
        .bind(status as i16)
        .bind(version)
        .bind(&ack)
        .execute(&mut *c)
        .await?;
    sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,$4,'access',$5,$6,$7)")
        .bind(ctx.actor)
        .bind(ctx.command_id)
        .bind(ctx.digest.as_slice())
        .bind(operation)
        .bind(id)
        .bind(before)
        .bind(after)
        .execute(&mut *c)
        .await?;
    Ok(CommandResult {
        status,
        body: Some(ack),
        replayed: false,
    })
}

impl PgIdentityStore {
    /// `GET /api/admin/access`. 이메일 순서로 모두 돌려준다.
    pub async fn access_list(&self, ctx: &CommandContext<'_>) -> Result<Value, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            let rows = sqlx::query(&select_sql("ORDER BY a.email LIMIT 10000"))
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            let items: Vec<Value> = rows
                .iter()
                .map(|row| view(row, ctx.settings.fixed_admin()))
                .collect();
            Ok(json!({ "items": items }))
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    /// `POST /api/admin/access`. 사람을 목록에 더한다. 이미 있으면 `access_exists`다.
    pub async fn add_access(
        &self,
        ctx: &CommandContext<'_>,
        email: &str,
        role: Role,
    ) -> Result<CommandResult, ManagementError> {
        let email = crate::config::email_value(email).map_err(|_| ManagementError::Invalid)?;
        let digest = management::input_digest(
            ctx.method,
            ctx.path,
            &json!({"email":email,"role":role.name()}),
        );
        let _gate = self.access_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = match self.begin_access(ctx, &digest).await? {
                Begin::Run(tx) => tx,
                Begin::Replayed(result) => return Ok(result),
            };
            let id = Uuid::new_v4();
            let inserted = sqlx::query("INSERT INTO gw_access(id,email,role,added_by) VALUES($1,$2,$3,$4) ON CONFLICT(email) DO NOTHING")
                .bind(id)
                .bind(&email)
                .bind(role.name())
                .bind(ctx.actor)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if inserted == 0 {
                return Err(ManagementError::AccessDuplicate);
            }
            let after = entry(&mut tx, id, ctx.settings.fixed_admin())
                .await?
                .ok_or(ManagementError::Invariant)?;
            let result =
                record(&mut tx, ctx, &digest, "add_access", id, 201, Some(1), &json!({}), &after)
                    .await?;
            tx.commit().await?;
            Ok(result)
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    /// `PATCH /api/admin/access/{id}`. 역할을 바꾼다.
    pub async fn update_access(
        &self,
        ctx: &CommandContext<'_>,
        id: Uuid,
        role: Role,
        expected: i64,
    ) -> Result<CommandResult, ManagementError> {
        let digest = management::input_digest(
            ctx.method,
            ctx.path,
            &json!({"role":role.name(),"expected_version":expected.to_string()}),
        );
        let _gate = self.access_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = match self.begin_access(ctx, &digest).await? {
                Begin::Run(tx) => tx,
                Begin::Replayed(result) => return Ok(result),
            };
            let before = lock_changeable(&mut tx, ctx, id, expected).await?;
            let version: i64 = sqlx::query_scalar(
                "UPDATE gw_access SET role=$2,version=version+1 WHERE id=$1 RETURNING version",
            )
            .bind(id)
            .bind(role.name())
            .fetch_one(&mut *tx)
            .await?;
            let after = entry(&mut tx, id, ctx.settings.fixed_admin())
                .await?
                .ok_or(ManagementError::Invariant)?;
            let result = record(
                &mut tx,
                ctx,
                &digest,
                "update_access",
                id,
                200,
                Some(version),
                &before,
                &after,
            )
            .await?;
            tx.commit().await?;
            Ok(result)
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    /// `DELETE /api/admin/access/{id}`. 사람을 목록에서 지운다. 그 사람의 계정과 키는 남지만
    /// 다음 요청부터 쓸 수 없다.
    pub async fn remove_access(
        &self,
        ctx: &CommandContext<'_>,
        id: Uuid,
        expected: i64,
    ) -> Result<CommandResult, ManagementError> {
        let digest = management::input_digest(
            ctx.method,
            ctx.path,
            &json!({"expected_version":expected.to_string()}),
        );
        let _gate = self.access_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = match self.begin_access(ctx, &digest).await? {
                Begin::Run(tx) => tx,
                Begin::Replayed(result) => return Ok(result),
            };
            let before = lock_changeable(&mut tx, ctx, id, expected).await?;
            sqlx::query("DELETE FROM gw_access WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let result = record(
                &mut tx,
                ctx,
                &digest,
                "remove_access",
                id,
                200,
                None,
                &before,
                &json!({}),
            )
            .await?;
            tx.commit().await?;
            Ok(result)
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    fn access_gate(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ManagementError> {
        self.gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)
    }

    /// 트랜잭션을 열고 관리자인지 확인한다. 같은 명령 ID를 다시 보냈으면 이전 결과를 돌려준다.
    async fn begin_access(
        &self,
        ctx: &CommandContext<'_>,
        digest: &[u8; 32],
    ) -> Result<Begin<'_>, ManagementError> {
        let mut tx = self.begin_transaction().await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        if let Some(result) = super::credentials::replay_on(&mut tx, ctx, digest).await? {
            return Ok(Begin::Replayed(result));
        }
        Ok(Begin::Run(tx))
    }
}

enum Begin<'c> {
    Run(sqlx::Transaction<'c, sqlx::Postgres>),
    Replayed(CommandResult),
}
