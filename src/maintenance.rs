//! 유지보수는 명시 SSO 헤더 인증과 현재 admin을 사용하며 HTTP listener를 열지 않는다.
use crate::{
    auth::{AccessSettings, AuthRoute, SsoAuthenticator},
    config::{StartupError, ValidatedConfig},
    ledger::{PgIdentityStore, PgLedger, commands::CommandContext, retention::RetentionDelete},
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
    let grants = json!(config.providers().iter().flat_map(|p|p.profiles.iter().map(|profile|json!({"provider":crate::ledger::finance::provider_name(p.provider),"model":profile.model,"capabilities":[crate::ledger::finance::api_name(profile.api)]}))).collect::<Vec<_>>());
    let ctx = CommandContext {
        actor: human.current.id,
        command_id: envelope.command_id,
        method: "MAINTENANCE",
        path: "retention_delete",
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
    }
}
