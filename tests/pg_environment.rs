mod support;
use m_gated::{
    auth::AuthError,
    config::{self, ConfigInputs},
    ledger::{AMBIENT_PG_VARIABLES, PgIdentityStore, reject_ambient_pg},
};

#[test]
fn ambient_pg_presence_is_a_pure_fail_closed_input() {
    assert!(reject_ambient_pg([false; 13]).is_ok());
    for i in 0..AMBIENT_PG_VARIABLES.len() {
        let mut presence = vec![false; AMBIENT_PG_VARIABLES.len()];
        presence[i] = true;
        assert_eq!(
            reject_ambient_pg(presence).unwrap_err(),
            AuthError::DatabaseUnavailable
        );
    }
}
#[tokio::test]
async fn ambient_pg_is_rejected_before_network_or_personal_file_access() {
    if std::env::var_os("MGATED_TEST_PG_CHILD").is_some() {
        let mut env = support::environment();
        env.insert("DB_PORT".into(), std::env::var("MGATED_TEST_PORT").unwrap());
        let cfg = config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env,
        })
        .unwrap();
        assert!(matches!(
            PgIdentityStore::connect(cfg.database()).await,
            Err(AuthError::DatabaseUnavailable)
        ));
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    for name in AMBIENT_PG_VARIABLES {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("MGATED_TEST_PG_CHILD", "1")
            .env(
                "MGATED_TEST_PORT",
                listener.local_addr().unwrap().port().to_string(),
            )
            .env(name, "synthetic-PG-sentinel-never-open")
            .args([
                "--exact",
                "ambient_pg_is_rejected_before_network_or_personal_file_access",
                "--nocapture",
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "guard child must pass");
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("synthetic-PG-sentinel"));
        }
        assert!(
            matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock),
            "ambient PG guard must reject before network"
        );
    }
}
