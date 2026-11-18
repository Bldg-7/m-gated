//! Provider 회사 키 명령. 다른 관리 명령과 같은 명령 기록·감사·재전송 규칙을 따른다.
//!
//! - Provider마다 회사 키를 여러 개 두고, 키마다 관리자가 붙인 이름(label)으로 구별한다.
//! - Provider마다 기본 키가 하나 있다. 가상 키는 발급할 때 Provider별로 회사 키 하나에 연결되고,
//!   연결이 없는 Provider(키 발급 뒤에 처음 등록한 Provider 등)는 기본 키를 쓴다.
//! - 키 원문은 명령 기록·감사·응답 어디에도 남지 않는다. 재전송 비교용 입력 digest도 원문 대신
//!   root key로 만든 fingerprint로 계산한다. 등록 전 확인(실제 Provider 호출)은 transaction 밖에서 한다.
use super::{
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
    management_query::stamp,
};
use crate::{
    config::ProviderId,
    credentials::{self, CredentialService},
    providers::transport::VerifyError,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Provider 하나에 둘 수 있는 사용 중 회사 키 수.
pub const MAX_ACTIVE_PER_PROVIDER: i64 = 20;

pub enum CredentialCommand {
    /// 새 키를 이름과 함께 추가한다. 그 Provider의 첫 키면 기본 키가 된다.
    Add {
        provider: ProviderId,
        label: String,
        secret: Zeroizing<String>,
    },
    /// 키 원문만 바꾼다(유출 대응·만료 교체). 이름·기본 여부·가상 키 연결은 새 키로 옮긴다.
    Replace {
        provider: ProviderId,
        id: Uuid,
        expected: i64,
        secret: Zeroizing<String>,
    },
    /// 이름을 바꾸거나 기본 키로 지정한다.
    Update {
        provider: ProviderId,
        id: Uuid,
        expected: i64,
        label: Option<String>,
        make_default: bool,
    },
    /// 키를 해제한다. 기본 키이거나 가상 키가 연결돼 있으면 옮길 키(`move_to`)가 필요하다.
    Retire {
        provider: ProviderId,
        id: Uuid,
        expected: i64,
        move_to: Option<Uuid>,
    },
    /// 사용자 하나가 Provider에서 쓸 수 있는 회사 키와 그 사용자의 기본 키를 정한다.
    /// 목록이 비면 제한을 없앤다(그 Provider의 기본 키를 쓴다).
    Authorize {
        principal: Uuid,
        provider: ProviderId,
        credentials: Vec<Uuid>,
        default: Option<Uuid>,
        /// 지금 인가(회사 키 목록, 기본 키). 다른 관리자의 변경을 덮어쓰지 않게 한다.
        expected: (Vec<Uuid>, Option<Uuid>),
        /// 인가에서 빠지는 회사 키에 연결된 이 사용자의 가상 키를 어떻게 할지.
        bound: Option<BoundAction>,
    },
    /// 가상 키 하나의 Provider 연결을 다른 회사 키로 바꾼다.
    BindKey {
        key: Uuid,
        provider: ProviderId,
        credential: Uuid,
        /// 지금 연결된 회사 키. 연결이 없으면 None. 다른 관리자의 변경을 덮어쓰지 않게 한다.
        expected: Option<Uuid>,
    },
}
/// 인가에서 빠지는 회사 키에 연결된 가상 키의 처리.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundAction {
    /// 인가된 다른 회사 키로 옮긴다.
    Move(Uuid),
    /// 옮기지 않는다. 그 가상 키로 이 Provider에 보내는 요청은 거절된다.
    Block,
}
impl CredentialCommand {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add_provider_credential",
            Self::Replace { .. } => "replace_provider_credential",
            Self::Update { .. } => "update_provider_credential",
            Self::Retire { .. } => "retire_provider_credential",
            Self::BindKey { .. } => "bind_key_credential",
            Self::Authorize { .. } => "authorize_principal_credentials",
        }
    }
    pub fn provider(&self) -> ProviderId {
        match self {
            Self::Add { provider, .. }
            | Self::Replace { provider, .. }
            | Self::Update { provider, .. }
            | Self::Retire { provider, .. }
            | Self::BindKey { provider, .. }
            | Self::Authorize { provider, .. } => *provider,
        }
    }
    fn secret(&self) -> Option<&str> {
        match self {
            Self::Add { secret, .. } | Self::Replace { secret, .. } => Some(secret.as_str()),
            _ => None,
        }
    }
}

/// 이름 규칙: 앞뒤 공백 없이 1~60자, 제어 문자 없음. DB 제약과 같다.
pub fn valid_label(label: &str) -> bool {
    let n = label.chars().count();
    (1..=60).contains(&n)
        && label.len() <= 240
        && label.trim() == label
        && !label.chars().any(char::is_control)
}

fn sorted(ids: &[Uuid]) -> Vec<Uuid> {
    let mut v = ids.to_vec();
    v.sort();
    v
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const SELECT: &str = "SELECT c.*,{verified} AS verified,{created} AS created,{retired} AS retired,p.email AS created_email,r.email AS retired_email,(SELECT count(*) FROM gw_virtual_key_credentials b JOIN gw_virtual_keys k ON k.id=b.key_id WHERE b.credential_id=c.id AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())) AS bound_keys FROM gw_provider_credentials c JOIN gw_principals p ON p.id=c.created_by LEFT JOIN gw_principals r ON r.id=c.retired_by";
fn select(tail: &str) -> String {
    format!(
        "{} {tail}",
        SELECT
            .replace("{verified}", &stamp("c.verified_at"))
            .replace("{created}", &stamp("c.created_at"))
            .replace("{retired}", &stamp("c.retired_at"))
    )
}

/// 화면·감사에 쓰는 메타데이터. 원문·암호문·fingerprint는 없다.
pub async fn metadata(
    c: &mut PgConnection,
    id: Uuid,
    kek_id: &[u8],
) -> Result<Value, ManagementError> {
    let row = sqlx::query(&select("WHERE c.id=$1"))
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or(ManagementError::NotFound)?;
    Ok(row_metadata(&row, kek_id))
}
fn row_metadata(row: &PgRow, kek_id: &[u8]) -> Value {
    let state: String = row.get("state");
    let decryptable = state == "active" && row.get::<Vec<u8>, _>("kek_id") == kek_id;
    json!({
        "id": row.get::<Uuid, _>("id"),
        "provider": row.get::<String, _>("provider"),
        "label": row.get::<String, _>("label"),
        "default": row.get::<bool, _>("is_default"),
        "state": state,
        "decryptable": decryptable,
        "hint": row.get::<String, _>("hint"),
        "version": row.get::<i64, _>("version").to_string(),
        "bound_keys": row.get::<i64, _>("bound_keys").to_string(),
        "verified_at": row.get::<String, _>("verified"),
        "created_at": row.get::<String, _>("created"),
        "created_by": row.get::<Uuid, _>("created_by"),
        "created_by_email": row.get::<Option<String>, _>("created_email"),
        "retired_at": row.get::<Option<String>, _>("retired"),
        "retired_by_email": row.get::<Option<String>, _>("retired_email"),
    })
}

/// 가상 키의 Provider별 회사 키 연결. 키 목록·상세 응답에 붙는다.
pub async fn key_bindings(c: &mut PgConnection, key: Uuid) -> Result<Value, ManagementError> {
    // authorized: 키 주인에게 이 회사 키가 인가돼 있는지. 그 Provider에 인가가 없으면 제한이 없어 true다.
    let rows = sqlx::query("SELECT b.provider,c.id,c.label,c.hint,c.state,(NOT EXISTS(SELECT 1 FROM gw_principal_credentials a WHERE a.principal_id=k.principal_id AND a.provider=b.provider) OR EXISTS(SELECT 1 FROM gw_principal_credentials a WHERE a.principal_id=k.principal_id AND a.credential_id=b.credential_id)) AS authorized FROM gw_virtual_key_credentials b JOIN gw_virtual_keys k ON k.id=b.key_id JOIN gw_provider_credentials c ON c.id=b.credential_id WHERE b.key_id=$1 ORDER BY b.provider")
        .bind(key)
        .fetch_all(c)
        .await?;
    Ok(Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "provider": r.get::<String, _>("provider"),
                    "credential_id": r.get::<Uuid, _>("id"),
                    "label": r.get::<String, _>("label"),
                    "hint": r.get::<String, _>("hint"),
                    "state": r.get::<String, _>("state"),
                    "authorized": r.get::<bool, _>("authorized"),
                })
            })
            .collect(),
    ))
}

/// 고른 회사 키 중 그 사용자에게 인가가 없는 Provider의 것이 있는지. 그런 선택은 관리자만 할 수 있다.
pub async fn choice_needs_admin(
    c: &mut PgConnection,
    principal: Uuid,
    chosen: &std::collections::BTreeMap<String, Uuid>,
) -> Result<bool, ManagementError> {
    let providers: Vec<String> = chosen.keys().cloned().collect();
    let restricted: i64 = sqlx::query_scalar("SELECT count(DISTINCT provider) FROM gw_principal_credentials WHERE principal_id=$1 AND provider=ANY($2)")
        .bind(principal)
        .bind(&providers)
        .fetch_one(c)
        .await?;
    Ok(restricted < providers.len() as i64)
}

/// 가상 키 발급 transaction 안에서 연결을 만든다.
/// 고른 Provider는 그 키로 잇는다. 사용자에게 인가가 있는 Provider면 인가된 키여야 한다.
/// 고르지 않은 Provider는 사용자의 기본 키로, 그것도 없으면 Provider의 기본 키로 잇는다.
pub async fn bind_new_key(
    c: &mut PgConnection,
    key: Uuid,
    principal: Uuid,
    chosen: &std::collections::BTreeMap<String, Uuid>,
) -> Result<(), ManagementError> {
    for (provider, id) in chosen {
        let allowed: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gw_principal_credentials WHERE principal_id=$1 AND provider=$2) OR EXISTS(SELECT 1 FROM gw_principal_credentials WHERE principal_id=$1 AND credential_id=$3)")
            .bind(principal)
            .bind(provider)
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
        if !allowed {
            return Err(ManagementError::CredentialNotAuthorized);
        }
        let n = sqlx::query("INSERT INTO gw_virtual_key_credentials(key_id,provider,credential_id) SELECT $1,provider,id FROM gw_provider_credentials WHERE id=$2 AND provider=$3 AND state='active'")
            .bind(key)
            .bind(id)
            .bind(provider)
            .execute(&mut *c)
            .await?
            .rows_affected();
        if n != 1 {
            return Err(ManagementError::Invalid);
        }
    }
    let chosen: Vec<String> = chosen.keys().cloned().collect();
    sqlx::query("INSERT INTO gw_virtual_key_credentials(key_id,provider,credential_id) SELECT $1,provider,credential_id FROM gw_principal_credentials WHERE principal_id=$3 AND is_default AND NOT (provider=ANY($2))")
        .bind(key)
        .bind(&chosen)
        .bind(principal)
        .execute(&mut *c)
        .await?;
    sqlx::query("INSERT INTO gw_virtual_key_credentials(key_id,provider,credential_id) SELECT $1,provider,id FROM gw_provider_credentials p WHERE is_default AND NOT (provider=ANY($2)) AND NOT EXISTS(SELECT 1 FROM gw_principal_credentials a WHERE a.principal_id=$3 AND a.provider=p.provider)")
        .bind(key)
        .bind(&chosen)
        .bind(principal)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 사용 중인 가상 키(회수·만료 전)의 연결만 옮긴다. 회수·만료된 키는 쓰던 키를 이력으로 남긴다.
async fn move_bindings(
    c: &mut PgConnection,
    from: Uuid,
    to: Option<Uuid>,
) -> Result<i64, ManagementError> {
    const ACTIVE: &str = "key_id IN(SELECT id FROM gw_virtual_keys WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()))";
    let n = match to {
        Some(to) => sqlx::query(&format!("UPDATE gw_virtual_key_credentials SET credential_id=$2,bound_at=clock_timestamp() WHERE credential_id=$1 AND {ACTIVE}"))
            .bind(from)
            .bind(to)
            .execute(c)
            .await?,
        // 마지막 키를 해제하면 연결을 지운다. 나중에 키를 새로 등록하면 그 Provider의 기본 키를 쓴다.
        None => sqlx::query(&format!("DELETE FROM gw_virtual_key_credentials WHERE credential_id=$1 AND {ACTIVE}"))
            .bind(from)
            .execute(c)
            .await?,
    }
    .rows_affected();
    Ok(n as i64)
}

/// 회사 키를 해제할 때 그 키의 사용자 인가를 옮길 키로 넘긴다. 옮길 키가 없으면(마지막 키) 지운다.
async fn move_authorizations(
    c: &mut PgConnection,
    from: Uuid,
    to: Option<Uuid>,
) -> Result<(), ManagementError> {
    let Some(to) = to else {
        sqlx::query("DELETE FROM gw_principal_credentials WHERE credential_id=$1")
            .bind(from)
            .execute(&mut *c)
            .await?;
        return Ok(());
    };
    // 이미 옮길 키도 인가된 사용자는 해제하는 키의 행만 지우고, 기본 키였다면 옮길 키를 기본으로 한다.
    let defaults: Vec<Uuid> = sqlx::query_scalar::<_, Option<Uuid>>("DELETE FROM gw_principal_credentials o WHERE o.credential_id=$1 AND EXISTS(SELECT 1 FROM gw_principal_credentials n WHERE n.principal_id=o.principal_id AND n.credential_id=$2) RETURNING CASE WHEN o.is_default THEN o.principal_id END")
        .bind(from)
        .bind(to)
        .fetch_all(&mut *c)
        .await?
        .into_iter()
        .flatten()
        .collect();
    sqlx::query("UPDATE gw_principal_credentials SET is_default=true WHERE credential_id=$1 AND principal_id=ANY($2)")
        .bind(to)
        .bind(&defaults)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE gw_principal_credentials SET credential_id=$2 WHERE credential_id=$1")
        .bind(from)
        .bind(to)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// 사용자에게 인가된 회사 키. 인가가 있는 Provider만 담는다(없는 Provider는 그 Provider의 기본 키를 쓴다).
pub async fn principal_authorization(
    c: &mut PgConnection,
    principal: Uuid,
) -> Result<Value, sqlx::Error> {
    let rows = sqlx::query("SELECT a.provider,a.is_default,c.id,c.label,c.hint FROM gw_principal_credentials a JOIN gw_provider_credentials c ON c.id=a.credential_id WHERE a.principal_id=$1 ORDER BY a.provider,a.is_default DESC,c.label,c.id")
        .bind(principal)
        .fetch_all(c)
        .await?;
    let mut providers: Vec<Value> = Vec::new();
    for r in &rows {
        let provider: String = r.get("provider");
        let item = json!({"id":r.get::<Uuid,_>("id"),"label":r.get::<String,_>("label"),"hint":r.get::<String,_>("hint"),"default":r.get::<bool,_>("is_default")});
        match providers
            .iter_mut()
            .find(|p| p["provider"] == provider.as_str())
        {
            Some(p) => p["credentials"].as_array_mut().expect("목록").push(item),
            None => providers.push(json!({"provider":provider,"credentials":[item]})),
        }
    }
    Ok(Value::Array(providers))
}

impl CredentialService {
    /// 설정된 Provider마다 사용 중인 키 전체와 최근 해제 이력(최대 10건)을 돌려준다.
    pub async fn list(&self) -> Result<Value, ManagementError> {
        let mut c = self.store().pool().acquire().await?;
        let kek = self.cipher().kek_id();
        let mut providers = Vec::new();
        for provider in self.configured() {
            let active = sqlx::query(&select(
                "WHERE c.provider=$1 AND c.state='active' ORDER BY c.is_default DESC,c.created_at,c.id",
            ))
            .bind(provider.name())
            .fetch_all(&mut *c)
            .await?;
            let history = sqlx::query(&select(
                "WHERE c.provider=$1 AND c.state='retired' ORDER BY c.retired_at DESC,c.id LIMIT 10",
            ))
            .bind(provider.name())
            .fetch_all(&mut *c)
            .await?;
            let credentials: Vec<Value> = active.iter().map(|r| row_metadata(r, &kek)).collect();
            let default = credentials.iter().find(|m| m["default"] == json!(true));
            let status = match default {
                None => "missing",
                Some(d) if d["decryptable"] == json!(true) => "active",
                Some(_) => "undecryptable",
            };
            providers.push(json!({
                "provider": provider.name(),
                "status": status,
                "credentials": credentials,
                "history": history.iter().map(|r| row_metadata(r, &kek)).collect::<Vec<_>>(),
            }));
        }
        Ok(json!({ "providers": providers }))
    }

    pub async fn execute(
        &self,
        ctx: &CommandContext<'_>,
        command: CredentialCommand,
    ) -> Result<CommandResult, ManagementError> {
        let provider = command.provider();
        if self.verifier(provider).is_none() {
            return Err(ManagementError::NotFound);
        }
        let fingerprint = match command.secret() {
            Some(secret) if credentials::valid_secret(provider, secret) => {
                Some(hex(&self.cipher().fingerprint(provider, secret)))
            }
            Some(_) => return Err(ManagementError::Invalid),
            None => None,
        };
        let input = match &command {
            CredentialCommand::Add { label, .. } => {
                if !valid_label(label) {
                    return Err(ManagementError::Invalid);
                }
                json!({"provider":provider.name(),"label":label,"fingerprint":fingerprint})
            }
            CredentialCommand::Replace { id, expected, .. } => {
                json!({"provider":provider.name(),"id":id,"expected_version":expected,"fingerprint":fingerprint})
            }
            CredentialCommand::Update {
                id,
                expected,
                label,
                make_default,
                ..
            } => {
                if label.as_deref().is_some_and(|l| !valid_label(l))
                    || (label.is_none() && !make_default)
                {
                    return Err(ManagementError::Invalid);
                }
                json!({"provider":provider.name(),"id":id,"expected_version":expected,"label":label,"default":make_default})
            }
            CredentialCommand::Retire {
                id,
                expected,
                move_to,
                ..
            } => {
                if *move_to == Some(*id) {
                    return Err(ManagementError::Invalid);
                }
                json!({"provider":provider.name(),"id":id,"expected_version":expected,"move_to":move_to})
            }
            CredentialCommand::BindKey {
                key,
                credential,
                expected,
                ..
            } => {
                json!({"provider":provider.name(),"key_id":key,"credential_id":credential,"expected_credential_id":expected})
            }
            CredentialCommand::Authorize {
                principal,
                credentials,
                default,
                expected,
                bound,
                ..
            } => {
                let unique: std::collections::BTreeSet<_> = credentials.iter().collect();
                if unique.len() != credentials.len()
                    || credentials.len() as i64 > MAX_ACTIVE_PER_PROVIDER
                    || default.is_some() == credentials.is_empty()
                    || default.is_some_and(|d| !credentials.contains(&d))
                    || matches!(bound, Some(BoundAction::Move(to)) if !credentials.contains(to))
                {
                    return Err(ManagementError::Invalid);
                }
                let bound = match bound {
                    None => json!(null),
                    Some(BoundAction::Block) => json!("block"),
                    Some(BoundAction::Move(to)) => json!({"move_to":to}),
                };
                json!({"provider":provider.name(),"principal_id":principal,"credential_ids":sorted(credentials),"default_credential_id":default,"expected":{"credential_ids":sorted(&expected.0),"default_credential_id":expected.1},"bound_keys":bound})
            }
        };
        let digest = management::input_digest(ctx.method, ctx.path, &input);
        // 이미 처리한 명령은 Provider를 다시 호출하지 않고 기록된 결과를 돌려준다.
        if let Some(result) = self.replay(ctx, &digest).await? {
            return Ok(result);
        }
        if let Some(secret) = command.secret() {
            let verifier = self.verifier(provider).ok_or(ManagementError::NotFound)?;
            verifier
                .verify_credential(secret)
                .await
                .map_err(|e| match e {
                    VerifyError::Rejected => ManagementError::CredentialRejected,
                    VerifyError::Unavailable => ManagementError::CredentialUnverified,
                })?;
        }
        let _gate = self
            .store()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.transaction(ctx, &command, &digest),
        )
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    async fn replay(
        &self,
        ctx: &CommandContext<'_>,
        digest: &[u8; 32],
    ) -> Result<Option<CommandResult>, ManagementError> {
        let mut c = self.store().pool().acquire().await?;
        replay_on(&mut c, ctx, digest).await
    }

    fn seal(
        &self,
        provider: ProviderId,
        id: Uuid,
        secret: &str,
    ) -> Result<([u8; credentials::NONCE_LEN], Vec<u8>), ManagementError> {
        self.cipher()
            .seal(provider, id, secret)
            .ok_or(ManagementError::Invariant)
    }

    async fn transaction(
        &self,
        ctx: &CommandContext<'_>,
        command: &CredentialCommand,
        digest: &[u8; 32],
    ) -> Result<CommandResult, ManagementError> {
        let provider = command.provider();
        let kek = self.cipher().kek_id();
        let mut tx = self.store().begin_transaction().await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        if let Some(result) = replay_on(&mut tx, ctx, digest).await? {
            return Ok(result);
        }
        if let CredentialCommand::BindKey { key, .. } = command {
            sqlx::query("SELECT id FROM gw_virtual_keys WHERE id=$1 FOR UPDATE")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ManagementError::NotFound)?;
        }
        if let CredentialCommand::Authorize { principal, .. } = command {
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(principal)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ManagementError::NotFound)?;
        }
        // Provider 하나의 사용 중 키 전체를 잠가 기본 키·이름·연결 변경을 한 줄로 세운다.
        let active = sqlx::query(
            "SELECT id,label,is_default,fingerprint,kek_id,version FROM gw_provider_credentials WHERE provider=$1 AND state='active' ORDER BY id FOR UPDATE",
        )
        .bind(provider.name())
        .fetch_all(&mut *tx)
        .await?;
        let find = |id: Uuid| active.iter().find(|r| r.get::<Uuid, _>("id") == id);
        let label_taken = |label: &str, except: Option<Uuid>| {
            active.iter().any(|r| {
                Some(r.get::<Uuid, _>("id")) != except
                    && r.get::<String, _>("label").to_lowercase() == label.to_lowercase()
            })
        };
        let fingerprint = command
            .secret()
            .map(|s| self.cipher().fingerprint(provider, s));
        let duplicate = |except: Option<Uuid>| {
            active.iter().find_map(|r| {
                let id = r.get::<Uuid, _>("id");
                (Some(id) != except
                    && Some(r.get::<Vec<u8>, _>("fingerprint")) == fingerprint.map(|f| f.to_vec())
                    && r.get::<Vec<u8>, _>("kek_id") == kek)
                    .then_some(id)
            })
        };
        let mut moved = 0_i64;
        let mut authorization_counts = None;
        let (status, target_kind, target, before, changed) = match command {
            CredentialCommand::Add { label, secret, .. } => {
                if let Some(id) = duplicate(None) {
                    return Err(ManagementError::CredentialDuplicate(id));
                }
                if label_taken(label, None) {
                    return Err(ManagementError::CredentialLabelTaken);
                }
                if active.len() as i64 >= MAX_ACTIVE_PER_PROVIDER {
                    return Err(ManagementError::Quota);
                }
                let id = Uuid::new_v4();
                let (nonce, sealed) = self.seal(provider, id, secret)?;
                sqlx::query("INSERT INTO gw_provider_credentials(id,provider,state,kek_id,nonce,ciphertext,fingerprint,hint,verified_at,created_by,label,is_default) VALUES($1,$2,'active',$3,$4,$5,$6,$7,clock_timestamp(),$8,$9,$10)")
                    .bind(id).bind(provider.name()).bind(kek.as_slice()).bind(nonce.as_slice()).bind(&sealed).bind(fingerprint.expect("등록 키").as_slice()).bind(credentials::hint(secret)).bind(ctx.actor).bind(label).bind(active.is_empty())
                    .execute(&mut *tx).await?;
                (
                    201,
                    "credential",
                    id,
                    json!({"provider":provider.name(),"state":"missing"}),
                    true,
                )
            }
            CredentialCommand::Replace {
                id,
                expected,
                secret,
                ..
            } => {
                check(&mut tx, &active, provider, *id, *expected).await?;
                let before = metadata(&mut tx, *id, &kek).await?;
                let old = find(*id).expect("확인한 키");
                if duplicate(None) == Some(*id) {
                    (200, "credential", *id, before, false)
                } else {
                    if let Some(other) = duplicate(Some(*id)) {
                        return Err(ManagementError::CredentialDuplicate(other));
                    }
                    let new = Uuid::new_v4();
                    let (nonce, sealed) = self.seal(provider, new, secret)?;
                    retire(&mut tx, *id, ctx.actor).await?;
                    sqlx::query("INSERT INTO gw_provider_credentials(id,provider,state,kek_id,nonce,ciphertext,fingerprint,hint,verified_at,created_by,label,is_default) VALUES($1,$2,'active',$3,$4,$5,$6,$7,clock_timestamp(),$8,$9,$10)")
                        .bind(new).bind(provider.name()).bind(kek.as_slice()).bind(nonce.as_slice()).bind(&sealed).bind(fingerprint.expect("교체 키").as_slice()).bind(credentials::hint(secret)).bind(ctx.actor).bind(old.get::<String, _>("label")).bind(old.get::<bool, _>("is_default"))
                        .execute(&mut *tx).await?;
                    moved = move_bindings(&mut tx, *id, Some(new)).await?;
                    // 사용자 인가도 새 키로 옮긴다(이름·기본 여부와 같이 원문만 바뀐 같은 키로 본다).
                    sqlx::query("UPDATE gw_principal_credentials SET credential_id=$2 WHERE credential_id=$1")
                        .bind(id)
                        .bind(new)
                        .execute(&mut *tx)
                        .await?;
                    (201, "credential", new, before, true)
                }
            }
            CredentialCommand::Update {
                id,
                expected,
                label,
                make_default,
                ..
            } => {
                check(&mut tx, &active, provider, *id, *expected).await?;
                let before = metadata(&mut tx, *id, &kek).await?;
                let row = find(*id).expect("확인한 키");
                let rename = label
                    .as_deref()
                    .filter(|l| *l != row.get::<String, _>("label"));
                let promote = *make_default && !row.get::<bool, _>("is_default");
                if let Some(l) = rename
                    && label_taken(l, Some(*id))
                {
                    return Err(ManagementError::CredentialLabelTaken);
                }
                if promote {
                    sqlx::query("UPDATE gw_provider_credentials SET is_default=false,version=version+1 WHERE provider=$1 AND is_default")
                        .bind(provider.name())
                        .execute(&mut *tx)
                        .await?;
                }
                if rename.is_some() || promote {
                    sqlx::query("UPDATE gw_provider_credentials SET label=coalesce($2,label),is_default=is_default OR $3,version=version+1 WHERE id=$1")
                        .bind(id)
                        .bind(rename)
                        .bind(promote)
                        .execute(&mut *tx)
                        .await?;
                }
                (200, "credential", *id, before, rename.is_some() || promote)
            }
            CredentialCommand::Retire {
                id,
                expected,
                move_to,
                ..
            } => {
                check(&mut tx, &active, provider, *id, *expected).await?;
                let before = metadata(&mut tx, *id, &kek).await?;
                let row = find(*id).expect("확인한 키");
                let default = row.get::<bool, _>("is_default");
                let others = active.len() > 1;
                if let Some(to) = move_to
                    && find(*to).is_none()
                {
                    return Err(ManagementError::Invalid);
                }
                let bound: i64 = before["bound_keys"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or(ManagementError::Invariant)?;
                if move_to.is_none() && others && (default || bound > 0) {
                    return Err(ManagementError::CredentialInUse {
                        bound_keys: bound,
                        default,
                    });
                }
                retire(&mut tx, *id, ctx.actor).await?;
                moved = move_bindings(&mut tx, *id, *move_to).await?;
                move_authorizations(&mut tx, *id, *move_to).await?;
                if default && let Some(to) = move_to {
                    sqlx::query("UPDATE gw_provider_credentials SET is_default=true,version=version+1 WHERE id=$1")
                        .bind(to)
                        .execute(&mut *tx)
                        .await?;
                }
                (200, "credential", *id, before, true)
            }
            CredentialCommand::Authorize {
                principal,
                credentials,
                default,
                expected,
                bound,
                ..
            } => {
                if credentials.iter().any(|id| find(*id).is_none()) {
                    return Err(ManagementError::Invalid);
                }
                let current = sqlx::query("SELECT credential_id,is_default FROM gw_principal_credentials WHERE principal_id=$1 AND provider=$2 ORDER BY credential_id FOR UPDATE")
                    .bind(principal)
                    .bind(provider.name())
                    .fetch_all(&mut *tx)
                    .await?;
                let current_ids: Vec<Uuid> =
                    current.iter().map(|r| r.get("credential_id")).collect();
                let current_default: Option<Uuid> = current
                    .iter()
                    .find(|r| r.get::<bool, _>("is_default"))
                    .map(|r| r.get("credential_id"));
                if (sorted(&expected.0), expected.1) != (current_ids.clone(), current_default) {
                    return Err(ManagementError::AuthorizationConflict(
                        current_ids,
                        current_default,
                    ));
                }
                let before = json!({"provider":provider.name(),"credential_ids":current_ids,"default_credential_id":current_default});
                // 새 인가 밖의 회사 키에 연결된, 쓰고 있는 가상 키. 제한을 없애면(빈 목록) 막힐 키가 없다.
                let outside: i64 = if credentials.is_empty() {
                    0
                } else {
                    sqlx::query_scalar("SELECT count(*) FROM gw_virtual_key_credentials b JOIN gw_virtual_keys k ON k.id=b.key_id WHERE k.principal_id=$1 AND b.provider=$2 AND NOT (b.credential_id=ANY($3)) AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())")
                        .bind(principal)
                        .bind(provider.name())
                        .bind(credentials)
                        .fetch_one(&mut *tx)
                        .await?
                };
                let mut blocked = 0_i64;
                if outside > 0 {
                    match bound {
                        None => return Err(ManagementError::AuthorizationInUse(outside)),
                        Some(BoundAction::Block) => blocked = outside,
                        Some(BoundAction::Move(to)) => {
                            moved = sqlx::query("UPDATE gw_virtual_key_credentials b SET credential_id=$4,bound_at=clock_timestamp() FROM gw_virtual_keys k WHERE k.id=b.key_id AND k.principal_id=$1 AND b.provider=$2 AND NOT (b.credential_id=ANY($3)) AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())")
                                .bind(principal)
                                .bind(provider.name())
                                .bind(credentials)
                                .bind(to)
                                .execute(&mut *tx)
                                .await?
                                .rows_affected() as i64;
                        }
                    }
                }
                let wanted = sorted(credentials);
                let changed = wanted != current_ids || *default != current_default;
                if changed {
                    sqlx::query("DELETE FROM gw_principal_credentials WHERE principal_id=$1 AND provider=$2")
                        .bind(principal)
                        .bind(provider.name())
                        .execute(&mut *tx)
                        .await?;
                    for id in &wanted {
                        sqlx::query("INSERT INTO gw_principal_credentials(principal_id,provider,credential_id,is_default,granted_by) VALUES($1,$2,$3,$4,$5)")
                            .bind(principal)
                            .bind(provider.name())
                            .bind(id)
                            .bind(Some(*id) == *default)
                            .bind(ctx.actor)
                            .execute(&mut *tx)
                            .await?;
                    }
                }
                authorization_counts = Some((moved, blocked));
                (200, "principal", *principal, before, changed || moved > 0)
            }
            CredentialCommand::BindKey {
                key,
                credential,
                expected,
                ..
            } => {
                if find(*credential).is_none() {
                    return Err(ManagementError::Invalid);
                }
                let active_key: bool = sqlx::query_scalar("SELECT revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()) FROM gw_virtual_keys WHERE id=$1")
                    .bind(key)
                    .fetch_one(&mut *tx)
                    .await?;
                if !active_key {
                    return Err(ManagementError::Invalid);
                }
                // 키 주인에게 이 Provider의 인가가 있으면, 인가된 회사 키로만 옮길 수 있다.
                let allowed: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gw_principal_credentials a JOIN gw_virtual_keys k ON k.principal_id=a.principal_id WHERE k.id=$1 AND a.provider=$2) OR EXISTS(SELECT 1 FROM gw_principal_credentials a JOIN gw_virtual_keys k ON k.principal_id=a.principal_id WHERE k.id=$1 AND a.credential_id=$3)")
                    .bind(key)
                    .bind(provider.name())
                    .bind(credential)
                    .fetch_one(&mut *tx)
                    .await?;
                if !allowed {
                    return Err(ManagementError::CredentialNotAuthorized);
                }
                let current: Option<Uuid> = sqlx::query_scalar(
                    "SELECT credential_id FROM gw_virtual_key_credentials WHERE key_id=$1 AND provider=$2",
                )
                .bind(key)
                .bind(provider.name())
                .fetch_optional(&mut *tx)
                .await?;
                if current != *expected {
                    return Err(ManagementError::BindingConflict(current));
                }
                let before = json!({"provider":provider.name(),"credential_id":current});
                if current == Some(*credential) {
                    (200, "key", *key, before, false)
                } else {
                    sqlx::query("INSERT INTO gw_virtual_key_credentials(key_id,provider,credential_id) VALUES($1,$2,$3) ON CONFLICT(key_id,provider) DO UPDATE SET credential_id=EXCLUDED.credential_id,bound_at=clock_timestamp()")
                        .bind(key)
                        .bind(provider.name())
                        .bind(credential)
                        .execute(&mut *tx)
                        .await?;
                    (200, "key", *key, before, true)
                }
            }
        };
        let (after, version) = if let Some((moved, blocked)) = authorization_counts {
            let version: i64 = sqlx::query_scalar("SELECT version FROM gw_principals WHERE id=$1")
                .bind(target)
                .fetch_one(&mut *tx)
                .await?;
            let mut after = json!({"principal_id":target,"provider":provider.name()});
            let current = principal_authorization(&mut tx, target).await?;
            after["provider_credentials"] = current;
            after["moved_keys"] = json!(moved.to_string());
            after["blocked_keys"] = json!(blocked.to_string());
            (after, version)
        } else if target_kind == "key" {
            let version: i64 =
                sqlx::query_scalar("SELECT version FROM gw_virtual_keys WHERE id=$1")
                    .bind(target)
                    .fetch_one(&mut *tx)
                    .await?;
            (
                json!({"key_id":target,"provider_credentials":key_bindings(&mut tx, target).await?}),
                version,
            )
        } else {
            let mut after = metadata(&mut tx, target, &kek).await?;
            after["moved_keys"] = json!(moved.to_string());
            let version = after["version"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or(ManagementError::Invariant)?;
            (after, version)
        };
        let projection = if target_kind == "principal" {
            json!({"authorization":after})
        } else if target_kind == "key" {
            json!({"key":after})
        } else {
            json!({"credential":after})
        };
        let mut ack = json!({"command_id":ctx.command_id,"operation":command.operation(),"target":{"kind":target_kind,"id":target},"result_version":version.to_string(),"effect":if changed{"changed"}else{"noop"}});
        if let (Some(a), Some(p)) = (ack.as_object_mut(), projection.as_object()) {
            a.extend(p.clone());
        }
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,clock_timestamp())")
            .bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(command.operation()).bind(target_kind).bind(target).bind(status as i16).bind(version).bind(&ack)
            .execute(&mut *tx).await?;
        if changed {
            sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(command.operation()).bind(target_kind).bind(target).bind(before).bind(&after)
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let mut body = ack;
        body["replayed"] = json!(false);
        Ok(CommandResult {
            status,
            body: Some(body),
            replayed: false,
        })
    }
}

/// 대상 키가 사용 중이고 버전이 맞는지 본다. 해제된 키도 지금 버전으로 충돌을 알린다.
async fn check(
    c: &mut PgConnection,
    active: &[PgRow],
    provider: ProviderId,
    id: Uuid,
    expected: i64,
) -> Result<(), ManagementError> {
    match active
        .iter()
        .find(|r| r.get::<Uuid, _>("id") == id)
        .map(|r| r.get::<i64, _>("version"))
    {
        Some(v) if v == expected => Ok(()),
        Some(v) => Err(ManagementError::Version(v)),
        None => {
            let v: Option<i64> = sqlx::query_scalar(
                "SELECT version FROM gw_provider_credentials WHERE id=$1 AND provider=$2",
            )
            .bind(id)
            .bind(provider.name())
            .fetch_optional(c)
            .await?;
            Err(v.map_or(ManagementError::NotFound, ManagementError::Version))
        }
    }
}

async fn retire(c: &mut PgConnection, id: Uuid, actor: Uuid) -> Result<(), ManagementError> {
    // 해제와 동시에 암호문을 지운다. 메타데이터만 이력으로 남는다.
    let n = sqlx::query("UPDATE gw_provider_credentials SET state='retired',is_default=false,nonce=NULL,ciphertext=NULL,retired_by=$2,retired_at=clock_timestamp(),version=version+1 WHERE id=$1 AND state='active'")
        .bind(id)
        .bind(actor)
        .execute(c)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(ManagementError::Invariant);
    }
    Ok(())
}

pub(super) async fn replay_on(
    c: &mut PgConnection,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
) -> Result<Option<CommandResult>, ManagementError> {
    let Some(prior) = sqlx::query(
        "SELECT input_digest,operation,result_status,result_projection FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2",
    )
    .bind(ctx.actor)
    .bind(ctx.command_id)
    .fetch_optional(c)
    .await?
    else {
        return Ok(None);
    };
    if prior.get::<Vec<u8>, _>("input_digest") != digest {
        return Err(ManagementError::CommandConflict);
    }
    let status = prior.get::<i16, _>("result_status") as u16;
    let mut ack: Value = prior.get("result_projection");
    ack["replayed"] = json!(true);
    Ok(Some(CommandResult {
        status,
        body: Some(ack),
        replayed: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::valid_label;
    #[test]
    fn label_rules_match_db_constraint() {
        for ok in ["default", "팀A 운영", "a", &"가".repeat(60)] {
            assert!(valid_label(ok), "{ok}");
        }
        for bad in ["", " lead", "trail ", "tab\there", &"가".repeat(61)] {
            assert!(!valid_label(bad), "{bad:?}");
        }
    }
}
