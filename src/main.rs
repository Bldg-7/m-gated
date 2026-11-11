use m_gated::{
    bootstrap::{self, Dependencies},
    config::{self, ConfigInputs, StartupError},
};
use std::{
    collections::BTreeMap, ffi::OsString, fs::File, io::Read, path::PathBuf, process::ExitCode,
};

fn read_bounded(path: PathBuf, max: usize) -> Result<String, StartupError> {
    let file = File::open(path).map_err(|_| StartupError::ConfigInput)?;
    let mut input = String::new();
    file.take((max + 1) as u64)
        .read_to_string(&mut input)
        .map_err(|_| StartupError::ConfigInput)?;
    if input.len() > max {
        return Err(StartupError::ConfigInput);
    }
    Ok(input)
}
fn load() -> Result<(config::ValidatedConfig, bool), StartupError> {
    let mut yaml_path = PathBuf::from("gateway.yaml");
    let mut env_path = None;
    let mut args = std::env::args_os().skip(1);
    let mut seen = std::collections::BTreeSet::new();
    let mut maintenance = false;
    while let Some(arg) = args.next() {
        if !seen.insert(arg.clone()) {
            return Err(StartupError::ConfigInput);
        }
        if arg == "--maintenance" {
            maintenance = true;
            continue;
        }
        let path = args.next().ok_or(StartupError::ConfigInput)?;
        match arg.to_str() {
            Some("--config") => yaml_path = path.into(),
            Some("--env-file") => env_path = Some(PathBuf::from(path)),
            _ => return Err(StartupError::ConfigInput),
        }
    }
    let yaml = read_bounded(yaml_path, 256 * 1024)?;
    let file_environment = env_path
        .map(|path| read_bounded(path, 64 * 1024))
        .transpose()?;
    let mut environment = BTreeMap::new();
    for key in config::ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            environment.insert(
                (*key).to_owned(),
                OsString::into_string(value).map_err(|_| StartupError::ConfigInput)?,
            );
        }
    }
    let config = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: file_environment.as_deref(),
        environment: &environment,
    })?;
    Ok((config, maintenance))
}
async fn start() -> Result<(), StartupError> {
    let (config, maintenance) = load()?;
    if maintenance {
        return run_maintenance(&config).await;
    }
    let ui = std::sync::Arc::new(m_gated::dashboard::UiBundle::load(
        std::path::Path::new("web/dist"),
        &config.auth().issuer.origin().ascii_serialization(),
    )?);
    let dependencies = Dependencies::assemble_backend(&config).await?.with_ui(ui);
    let app = bootstrap::build(config, dependencies)?;
    app.validate_bootstrap()?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| StartupError::RuntimeFailure)?;
    app.run(
        std::net::SocketAddr::from(([0, 0, 0, 0], 4000)),
        async move {
            terminate.recv().await;
        },
    )
    .await
}
fn maintenance_socket(fd: std::os::fd::OwnedFd) -> Result<tokio::net::UnixStream, StartupError> {
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream
        .set_nonblocking(true)
        .map_err(|_| StartupError::ConfigInput)?;
    tokio::net::UnixStream::from_std(stream).map_err(|_| StartupError::ConfigInput)
}
fn maintenance_input(
    fd: std::os::fd::OwnedFd,
) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>, StartupError> {
    use std::os::unix::fs::FileTypeExt;
    let file = File::from(fd.try_clone().map_err(|_| StartupError::ConfigInput)?);
    if file
        .metadata()
        .map_err(|_| StartupError::ConfigInput)?
        .file_type()
        .is_socket()
    {
        Ok(Box::new(maintenance_socket(fd)?))
    } else {
        Ok(Box::new(
            tokio::net::unix::pipe::Receiver::from_owned_fd(fd)
                .map_err(|_| StartupError::ConfigInput)?,
        ))
    }
}
fn maintenance_output(
    fd: std::os::fd::OwnedFd,
) -> Result<Box<dyn tokio::io::AsyncWrite + Unpin>, StartupError> {
    use std::os::unix::fs::FileTypeExt;
    let file = File::from(fd.try_clone().map_err(|_| StartupError::ConfigInput)?);
    if file
        .metadata()
        .map_err(|_| StartupError::ConfigInput)?
        .file_type()
        .is_socket()
    {
        Ok(Box::new(maintenance_socket(fd)?))
    } else {
        Ok(Box::new(
            tokio::net::unix::pipe::Sender::from_owned_fd(fd)
                .map_err(|_| StartupError::RuntimeFailure)?,
        ))
    }
}
async fn run_maintenance(config: &config::ValidatedConfig) -> Result<(), StartupError> {
    use std::os::fd::AsFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        // FIFO와 subprocess socketpair만 받는다. regular file/TTY나 blocking stdin task는 사용하지 않는다.
        let fd = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| StartupError::ConfigInput)?;
        let mut input = maintenance_input(fd)?;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            (&mut input).take(65537).read_to_end(&mut bytes),
        )
        .await
        .map_err(|_| StartupError::ConfigInput)?
        .map_err(|_| StartupError::ConfigInput)?;
        let result = m_gated::maintenance::execute(config, &bytes).await?;
        let mut output = serde_json::to_vec(&result).map_err(|_| StartupError::RuntimeFailure)?;
        if output.len() > 8192 {
            return Err(StartupError::RuntimeFailure);
        }
        output.push(b'\n');
        let fd = std::io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| StartupError::RuntimeFailure)?;
        let mut pipe = maintenance_output(fd)?;
        tokio::time::timeout(std::time::Duration::from_secs(5), pipe.write_all(&output))
            .await
            .map_err(|_| StartupError::RuntimeFailure)?
            .map_err(|_| StartupError::RuntimeFailure)?;
        Ok(())
    })
    .await;
    if matches!(result, Ok(Ok(()))) {
        Ok(())
    } else {
        Err(StartupError::MaintenanceFailure)
    }
}
#[tokio::main]
async fn main() -> ExitCode {
    m_gated::runtime::panic::install_redacting_hook();
    m_gated::telemetry::install_stderr();
    match start().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if error == StartupError::MaintenanceFailure {
                eprintln!("maintenance_failed: 유지보수 검증에 실패했습니다.");
            } else {
                eprintln!("{error}: 기동 검증에 실패했습니다.");
            }
            ExitCode::FAILURE
        }
    }
}
