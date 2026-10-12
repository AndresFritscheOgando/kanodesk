//! The host runtime: the main window's requests (passwords, settings
//! reload, stop), the overview it shows, and the ID-server link settings.

use std::sync::Arc;
use std::time::Duration;

use kanodesk_config::{ApproveMode, Config, HostSettings};
use kanodesk_crypto::DeviceIdentity;
use kanodesk_crypto::keystore::MemoryKeyStore;
use kanodesk_crypto::password::{HostPasswords, TemporaryPolicy};
use kanodesk_crypto::throttle::{LoginThrottle, ThrottleConfig};
use kanodesk_host::control::Request;
use kanodesk_host::engine::{EngineParts, EngineSettings, HostEngine};
use kanodesk_host::runtime::presence_config;
use kanodesk_host::video::VideoHub;
use kanodesk_host::{AutoAccept, HostRuntime, RuntimeParts};
use kanodesk_input::{Permissions, RecordingSink};
use kanodesk_ipc::control::{IdServerState, UiToHost};

const KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kd-runtime-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn engine(mode: ApproveMode) -> HostEngine {
    let sink = RecordingSink::default();
    HostEngine::new(EngineParts {
        identity: DeviceIdentity::generate().unwrap(),
        passwords: HostPasswords::new(TemporaryPolicy::default()).unwrap(),
        throttle: LoginThrottle::new(ThrottleConfig::default()),
        throttle_path: None,
        settings: EngineSettings {
            approve_mode: mode,
            approve_timeout: Duration::from_secs(10),
            click_delay: Duration::ZERO,
            permissions: Permissions::ALL,
            display: None,
            agent: "runtime-test".into(),
            listen: None,
        },
        cm: Arc::new(AutoAccept),
        video: VideoHub::new(Arc::new(|_, _| Ok(()))),
        displays: Arc::new(|| Ok(Vec::new())),
        input: Arc::new(move || Ok(Box::new(sink.clone()))),
    })
}

fn runtime(tag: &str, interactive_cm: bool) -> (HostRuntime, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let config_path = dir.join("kanodesk.toml");
    Config::load(&config_path).unwrap().save().unwrap();
    let rt = HostRuntime::start(
        RuntimeParts {
            engine: engine(ApproveMode::Password),
            config_path: config_path.clone(),
            store: Arc::new(MemoryKeyStore::default()),
            identity: Arc::new(|| DeviceIdentity::generate().map_err(|e| e.to_string())),
            data_dir: dir.clone(),
            port: 0,
            version: "9.9.9".into(),
            interactive_cm,
        },
        &HostSettings::from_config(&Config::in_memory()),
    );
    (rt, config_path)
}

async fn ask(rt: &HostRuntime, command: UiToHost) -> Option<(bool, String)> {
    let (request, answer) = Request::new(command);
    rt.handle(request);
    answer.await.unwrap()
}

#[tokio::test]
async fn overview_and_password_requests() {
    let (rt, _) = runtime("passwords", true);
    let overview = rt.overview().borrow().clone();
    assert_eq!(overview.version, "9.9.9");
    assert_eq!(overview.id_server, IdServerState::NotConfigured);
    let before = overview.status.temporary_password.clone().unwrap();

    // A new temporary password shows in the overview.
    assert_eq!(ask(&rt, UiToHost::RefreshPassword).await, None);
    let after = rt.overview().borrow().status.temporary_password.clone();
    assert_ne!(after.as_deref(), Some(before.as_str()));
    assert_eq!(after, Some(rt.engine().temporary_password()));

    // Permanent password: too short is refused, a good one is set.
    let (ok, _) = ask(&rt, UiToHost::SetPermanentPassword(Some("short".into())))
        .await
        .unwrap();
    assert!(!ok);
    assert!(!rt.overview().borrow().status.has_permanent_password);
    let (ok, msg) = ask(
        &rt,
        UiToHost::SetPermanentPassword(Some("Correct Horse".into())),
    )
    .await
    .unwrap();
    assert!(ok, "{msg}");
    assert!(rt.overview().borrow().status.has_permanent_password);
    let (ok, _) = ask(&rt, UiToHost::SetPermanentPassword(None))
        .await
        .unwrap();
    assert!(ok);
    assert!(!rt.overview().borrow().status.has_permanent_password);
}

#[tokio::test]
async fn reload_applies_settings_and_the_id_server() {
    let (rt, path) = runtime("reload", true);
    let mut config = Config::load(&path).unwrap();
    config.set("approve-mode", "both").unwrap();
    config.set("temporary-password-length", "12").unwrap();
    config.set("enable-mouse", "N").unwrap();
    config
        .set("custom-rendezvous-server", "127.0.0.1:9")
        .unwrap();
    config.set("key", KEY).unwrap();
    config.save().unwrap();
    let (ok, msg) = ask(&rt, UiToHost::ReloadConfig).await.unwrap();
    assert!(ok && msg == "Settings applied.", "{msg}");
    let s = rt.engine().settings();
    assert_eq!(s.approve_mode, ApproveMode::Both);
    assert!(!s.permissions.mouse && s.permissions.keyboard);
    assert_eq!(rt.engine().default_permissions(), s.permissions);
    assert_eq!(rt.engine().temporary_password().len(), 12);
    let o = rt.overview().borrow().clone();
    assert_eq!(o.status.approve_mode, "both");
    assert_eq!(o.id_server_address.as_deref(), Some("127.0.0.1:9"));
    assert_ne!(
        o.id_server,
        IdServerState::NotConfigured,
        "the link started"
    );

    // A bad key stops the link and says why.
    config.set("key", "nothex").unwrap();
    config.save().unwrap();
    ask(&rt, UiToHost::ReloadConfig).await;
    assert!(matches!(
        rt.overview().borrow().id_server,
        IdServerState::Failed(ref e) if e.contains("key")
    ));
    // No server: not configured.
    config.set("key", "").unwrap();
    config.set("custom-rendezvous-server", "").unwrap();
    config.save().unwrap();
    ask(&rt, UiToHost::ReloadConfig).await;
    assert_eq!(
        rt.overview().borrow().id_server,
        IdServerState::NotConfigured
    );

    // An unreadable config is reported.
    std::fs::write(&path, "schema_version = \"x\"").unwrap();
    let (ok, _) = ask(&rt, UiToHost::ReloadConfig).await.unwrap();
    assert!(!ok);
}

#[tokio::test]
async fn click_approval_is_refused_without_a_connection_manager() {
    let (rt, _) = runtime("hidden-cm", false);
    let mut config = Config::in_memory();
    config.set("approve-mode", "click").unwrap();
    let msg = rt.apply_settings(&HostSettings::from_config(&config));
    assert!(msg.contains("restart"), "{msg}");
    assert_eq!(
        rt.engine().settings().approve_mode,
        ApproveMode::Password,
        "never auto-accept unasked"
    );
}

#[tokio::test]
async fn shutdown_request_stops_the_runtime() {
    let (rt, _) = runtime("stop", true);
    assert_eq!(ask(&rt, UiToHost::Shutdown).await, None);
    tokio::time::timeout(Duration::from_secs(2), rt.stopped())
        .await
        .expect("stopped");
}

#[test]
fn presence_config_from_settings() {
    let dir = std::path::Path::new("data");
    let mut config = Config::in_memory();
    assert_eq!(
        presence_config(&HostSettings::from_config(&config), dir, 0),
        Ok(None)
    );
    config
        .set("custom-rendezvous-server", "ids.example.com")
        .unwrap();
    assert_eq!(
        presence_config(&HostSettings::from_config(&config), dir, 0),
        Ok(None),
        "a server without a key is not usable"
    );
    config.set("key", KEY).unwrap();
    let p = presence_config(&HostSettings::from_config(&config), dir, 21120)
        .unwrap()
        .unwrap();
    assert_eq!(p.server, "ids.example.com");
    assert_eq!(p.port, 21120);
    assert_eq!(p.id_file.as_deref(), Some(dir.join("device-id").as_path()));
    config.set("key", "zz").unwrap();
    assert!(presence_config(&HostSettings::from_config(&config), dir, 0).is_err());
}
