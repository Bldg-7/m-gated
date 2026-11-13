//! 유지보수는 명시 SSO 헤더 인증과 현재 admin을 사용하며 HTTP listener를 열지 않는다.
use crate::{
    auth::{AccessSettings, AuthRoute, SsoAuthenticator},
    config::{StartupError, ValidatedConfig},
    ledger::{
        PgIdentityStore, PgLedger, commands::CommandContext, credentials::CredentialCommand,
        retention::RetentionDelete,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    access_token: String,
    command_id: uuid::Uuid,
    command: Command,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    RetentionDelete(RetentionDelete),
    Handoff(Box<crate::ledger::handoff::Handoff>),
    /// 등록 전에 실제 Provider 확인을 거친다. 키는 stdin envelope로만 받는다.
    AddProviderCredential {
        provider: String,
        label: String,
        secret: String,
    },
    /// 키 원문만 바꾼다. 이름·기본 여부·가상 키 연결은 새 키로 옮겨진다.
    ReplaceProviderCredential {
        provider: String,
        credential_id: uuid::Uuid,
        expected_version: i64,
        secret: String,
    },
    RetireProviderCredential {
        provider: String,
        credential_id: uuid::Uuid,
        expected_version: i64,
        #[serde(default)]
        move_to: Option<uuid::Uuid>,
    },
}
struct Syntax;
impl crate::json::Visitor for Syntax {
    fn value(
        &mut self,
        _: &[String],
        _: crate::json::Kind,
        _: &str,
    ) -> Result<(), crate::json::JsonError> {
        Ok(())
    }
}
pub async fn execute(config: &ValidatedConfig, input: &[u8]) -> Result<Value, StartupError> {
    if input.len() > 64 * 1024 {
        return Err(StartupError::ConfigInput);
    }
    crate::json::scan(input, &mut Syntax)
        .await
        .map_err(|_| StartupError::ConfigInput)?;
    let envelope: Envelope =
        serde_json::from_slice(input).map_err(|_| StartupError::ConfigInput)?;
    if envelope.access_token.len() > 8192 {
        return Err(StartupError::AuthenticationUnavailable);
    }
    let database = Arc::new(
        PgIdentityStore::connect(config.database())
            .await
            .map_err(|_| StartupError::SchemaUnavailable)?,
    );
    database
        .migrate()
        .await
        .map_err(|_| StartupError::SchemaUnavailable)?;
    let auth = SsoAuthenticator::from_config(config, database.clone())
        .map_err(|_| StartupError::AuthenticationUnavailable)?;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", envelope.access_token)
            .parse()
            .map_err(|_| StartupError::AuthenticationUnavailable)?,
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StartupError::RuntimeFailure)?
        .as_secs() as i64;
    let human = auth
        .authenticate(&headers, AuthRoute::Management, now)
        .await
        .map_err(|_| StartupError::AuthenticationUnavailable)?;
    if !human.current.admin {
        return Err(StartupError::AuthenticationUnavailable);
    }
    let settings = AccessSettings::from_config(config);
    let ledger = PgLedger::new(
        database,
        settings.clone(),
        crate::gateway::auth_config_digest(config),
    );
    // 정책에 줄 수 있는 목록은 설정 파일 profile 과 승인된 카탈로그를 합친 것이다(서버와 같은 규칙).
    let catalog = crate::catalog::CatalogService::new(config, ledger.database().clone())?;
    catalog
        .refresh()
        .await
        .map_err(|_| StartupError::SchemaUnavailable)?;
    let mut grants: Vec<Value> = config.providers().iter().flat_map(|p|p.profiles.iter().map(|profile|json!({"provider":crate::ledger::finance::provider_name(p.provider),"model":profile.model,"capabilities":[crate::ledger::finance::api_name(profile.api)]}))).collect();
    for (provider, profile) in &catalog.cache().snapshot().profiles {
        grants.push(json!({"provider":crate::ledger::finance::provider_name(*provider),"model":profile.model,"capabilities":[crate::ledger::finance::api_name(profile.api)]}));
    }
    let grants = Value::Array(grants);
    let path = match &envelope.command {
        Command::AddProviderCredential { .. } => "add_provider_credential",
        Command::ReplaceProviderCredential { .. } => "replace_provider_credential",
        Command::RetireProviderCredential { .. } => "retire_provider_credential",
        _ => "retention_delete",
    };
    let ctx = CommandContext {
        actor: human.current.id,
        command_id: envelope.command_id,
        method: "MAINTENANCE",
        path,
        settings: &settings,
        grants: &grants,
        digest: crate::gateway::auth_config_digest(config),
        ceiling: config.execution_ceiling(),
        execution_running: false,
    };
    match envelope.command {
        Command::RetentionDelete(input) => ledger
            .retention_delete(&ctx, config.retention(), &input)
            .await
            .map_err(|_| StartupError::RuntimeFailure),
        Command::Handoff(input) => ledger
            .handoff(&ctx, *input)
            .await
            .map_err(|_| StartupError::RuntimeFailure),
        Command::AddProviderCredential {
            provider,
            label,
            secret,
        } => {
            let provider =
                crate::config::ProviderId::from_name(&provider).ok_or(StartupError::ConfigInput)?;
            credential(
                config,
                ledger.database().clone(),
                &ctx,
                CredentialCommand::Add {
                    provider,
                    label,
                    secret: zeroize::Zeroizing::new(secret),
                },
            )
            .await
        }
        Command::ReplaceProviderCredential {
            provider,
            credential_id,
            expected_version,
            secret,
        } => {
            let provider =
                crate::config::ProviderId::from_name(&provider).ok_or(StartupError::ConfigInput)?;
            credential(
                config,
                ledger.database().clone(),
                &ctx,
                CredentialCommand::Replace {
                    provider,
                    id: credential_id,
                    expected: expected_version,
                    secret: zeroize::Zeroizing::new(secret),
                },
            )
            .await
        }
        Command::RetireProviderCredential {
            provider,
            credential_id,
            expected_version,
            move_to,
        } => {
            let provider =
                crate::config::ProviderId::from_name(&provider).ok_or(StartupError::ConfigInput)?;
            credential(
                config,
                ledger.database().clone(),
                &ctx,
                CredentialCommand::Retire {
                    provider,
                    id: credential_id,
                    expected: expected_version,
                    move_to,
                },
            )
            .await
        }
    }
}
async fn credential(
    config: &ValidatedConfig,
    store: Arc<PgIdentityStore>,
    ctx: &CommandContext<'_>,
    command: CredentialCommand,
) -> Result<Value, StartupError> {
    let service = crate::credentials::CredentialService::new(config, store)?;
    let result = service
        .execute(ctx, command)
        .await
        .map_err(|_| StartupError::MaintenanceFailure)?;
    Ok(json!({"status":result.status,"ack":result.body}))
}
