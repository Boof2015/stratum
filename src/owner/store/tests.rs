use super::*;

const PASSWORD: &str = "a long test-only password";
fn fixture() -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = Store::initialize(directory.path(), &Config::default()).unwrap();
    (directory, store)
}

#[test]
fn restart_and_reinitialization_preserve_identity_config_and_password() {
    let (dir, store) = fixture();
    let before = store.identity().unwrap();
    let config_before = fs::read(dir.path().join("config.json")).unwrap();
    let certificate_before = fs::read(dir.path().join("private.json")).unwrap();
    let other = Config {
        listen: "127.0.0.1:9999".parse().unwrap(),
        ..Config::default()
    };
    Store::initialize(dir.path(), &other).unwrap();
    assert_eq!(
        fs::read(dir.path().join("private.json")).unwrap(),
        certificate_before
    );
    assert_eq!(
        fs::read(dir.path().join("config.json")).unwrap(),
        config_before
    );
    store.set_password(PASSWORD, false).unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.identity().unwrap().spki_sha256, before.spki_sha256);
    assert_eq!(
        reopened.identity().unwrap().installation_id,
        before.installation_id
    );
    assert!(reopened.login(PASSWORD).unwrap().is_some());
    assert!(!fs::read_to_string(dir.path().join("private.json"))
        .unwrap()
        .contains(PASSWORD));
}

#[test]
fn codes_rotate_expire_are_hashed_and_are_single_use() {
    let (dir, store) = fixture();
    let first = store.setup_code().unwrap();
    let code = store.setup_code().unwrap();
    assert_ne!(code, first);
    assert!(!fs::read_to_string(dir.path().join("private.json"))
        .unwrap()
        .contains(&code));
    assert!(store.claim(&first, PASSWORD).unwrap().is_none());
    let mut state = store.read_state().unwrap();
    state.bootstrap.as_mut().unwrap().expires_at = now().unwrap();
    store.write("private.json", &state).unwrap();
    assert!(store.claim(&code, PASSWORD).unwrap().is_none());
    let code = store.setup_code().unwrap();
    assert!(store.claim(&code, PASSWORD).unwrap().is_some());
    assert!(store
        .claim(&code, "different long password")
        .unwrap()
        .is_none());
    assert!(store.setup_code().is_err());
    assert!(store.read_state().unwrap().bootstrap.is_none());
    assert!(store.login(PASSWORD).unwrap().is_some());
}

#[test]
fn concurrent_enrollment_has_one_winner() {
    let (_dir, store) = fixture();
    let code = store.setup_code().unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let code = code.clone();
            std::thread::spawn(move || store.claim(&code, PASSWORD).unwrap().is_some())
        })
        .collect();
    let winners = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().unwrap()))
        .sum::<usize>();
    assert_eq!(winners, 1);
}

#[test]
fn recovery_changes_generation_and_preserves_identity_and_configuration() {
    let (dir, store) = fixture();
    assert!(store.set_password(PASSWORD, true).is_err());
    store.set_password(PASSWORD, false).unwrap();
    assert!(store.set_password("a different password", false).is_err());
    let generation = store.owner_generation().unwrap();
    let identity = store.identity().unwrap();
    let config = fs::read(dir.path().join("config.json")).unwrap();
    store.set_password("a different password", true).unwrap();
    assert_ne!(store.owner_generation().unwrap(), generation);
    assert!(store.login(PASSWORD).unwrap().is_none());
    assert!(store.login("a different password").unwrap().is_some());
    assert_eq!(store.identity().unwrap().spki_sha256, identity.spki_sha256);
    assert_eq!(
        store.identity().unwrap().certificate_sha256,
        identity.certificate_sha256
    );
    assert_eq!(fs::read(dir.path().join("config.json")).unwrap(), config);
}

#[test]
fn renewal_keeps_the_spki_identity() {
    let (dir, store) = fixture();
    let identity = store.identity().unwrap();
    let mut config = store.config().unwrap();
    config
        .admin_origins
        .push("https://music.home.lan:7843".into());
    store.write("config.json", &config).unwrap();
    store.renew_certificate().unwrap();
    let after = Store::open(dir.path()).unwrap().identity().unwrap();
    assert_eq!(after.spki_sha256, identity.spki_sha256);
    assert_eq!(after.installation_id, identity.installation_id);
    assert_ne!(after.certificate_sha256, identity.certificate_sha256);
}

#[test]
fn partial_initialization_does_not_replace_identity_and_missing_state_is_not_reset() {
    let (dir, store) = fixture();
    let identity = store.identity().unwrap();
    fs::remove_file(dir.path().join("config.json")).unwrap();
    assert!(Store::open(dir.path()).is_err());
    let store = Store::initialize(dir.path(), &Config::default()).unwrap();
    assert_eq!(store.identity().unwrap().spki_sha256, identity.spki_sha256);
    fs::remove_file(dir.path().join("private.json")).unwrap();
    assert!(Store::initialize(dir.path(), &Config::default()).is_err());
    assert!(!dir.path().join("private.json").exists());
}

#[test]
fn corrupt_or_unknown_state_is_not_overwritten() {
    for bytes in [b"broken".as_slice(), b"{\"version\":999}"] {
        let (dir, store) = fixture();
        fs::write(dir.path().join("private.json"), bytes).unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert!(Store::initialize(dir.path(), &Config::default()).is_err());
        assert!(store.set_password(PASSWORD, false).is_err());
        assert_eq!(fs::read(dir.path().join("private.json")).unwrap(), bytes);
    }
}

#[test]
fn only_one_server_owns_a_directory_but_recovery_can_open_it() {
    let (dir, store) = fixture();
    let first = store.server_lock().unwrap();
    let reopened = Store::open(dir.path()).unwrap();
    assert!(reopened.server_lock().is_err());
    reopened.set_password(PASSWORD, false).unwrap();
    drop(first);
    assert!(reopened.server_lock().is_ok());
}

#[cfg(unix)]
#[test]
fn permissions_and_symlinks_are_checked() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (dir, _store) = fixture();
    for name in ["private.json", "config.json", "state.lock"] {
        assert_eq!(
            fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::set_permissions(
        dir.path().join("private.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(Store::open(dir.path()).is_err());
    fs::set_permissions(
        dir.path().join("private.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::rename(
        dir.path().join("private.json"),
        external.path().join("state"),
    )
    .unwrap();
    symlink(
        external.path().join("state"),
        dir.path().join("private.json"),
    )
    .unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert!(Store::initialize(dir.path(), &Config::default()).is_err());
}

#[test]
fn rejects_bad_origins_passwords_and_forged_hash_cost() {
    for origin in [
        "http://localhost:7843",
        "https://u:p@localhost",
        "https://localhost/path",
        "https://localhost/?x=y",
        "https://localhost#x",
        "https://localhost/",
    ] {
        assert!(Config {
            admin_origins: vec![origin.into()],
            ..Config::default()
        }
        .validate()
        .is_err());
    }
    assert!(Config {
        admin_origins: vec!["https://[::1]:7843".into()],
        ..Config::default()
    }
    .validate()
    .is_ok());
    for password in ["short", "            ", "contains\nnewline!"] {
        assert!(validate_password(password).is_err());
    }
    let (_dir, store) = fixture();
    store.set_password(PASSWORD, false).unwrap();
    let mut state = store.read_state().unwrap();
    state.owner.as_mut().unwrap().password_hash = state
        .owner
        .as_ref()
        .unwrap()
        .password_hash
        .replace("m=19456", "m=999999999");
    store.write("private.json", &state).unwrap();
    assert!(store.login(PASSWORD).is_err());
}

#[test]
fn setup_is_backwards_compatible_resumable_and_shared_between_writers() {
    let (dir, store) = fixture();
    let mut raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
    raw.as_object_mut().unwrap().remove("setup");
    fs::write(
        dir.path().join("config.json"),
        serde_json::to_vec(&raw).unwrap(),
    )
    .unwrap();
    assert_eq!(
        Store::open(dir.path())
            .unwrap()
            .config()
            .unwrap()
            .setup
            .stage(),
        "navidrome"
    );
    let settings = Settings {
        navidrome_url: Some("http://navi:4533/music/".into()),
        listeners: vec![],
    };
    assert!(store.save_setup(&settings, "initial", None).is_err());
    store.set_password(PASSWORD, false).unwrap();
    let generation = store.owner_generation().unwrap().unwrap();
    let first = store
        .save_setup(&settings, "initial", Some(&generation))
        .unwrap();
    assert_eq!(first.stage(), "listeners");
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.config().unwrap().setup.revision, first.revision);
    assert_eq!(
        reopened
            .config()
            .unwrap()
            .setup
            .settings
            .navidrome_url
            .as_deref(),
        Some("http://navi:4533/music")
    );
    let mut full = first.settings.clone();
    full.listeners.push(super::super::setup::Endpoint {
        url: "https://sidecar.lan:7843".into(),
        trust: super::super::setup::EndpointTrust::SidecarKey,
    });
    let next = reopened.save_setup(&full, &first.revision, None).unwrap();
    assert_eq!(next.stage(), "connections-saved");
    assert_eq!(
        store
            .save_setup(&settings, &first.revision, Some(&generation))
            .err()
            .unwrap()
            .downcast_ref::<SetupWriteError>(),
        Some(&SetupWriteError::Conflict)
    );
    // A -> B -> A still invalidates the first revision.
    let last = store
        .save_setup(&first.settings, &next.revision, None)
        .unwrap();
    assert_ne!(last.revision, first.revision);
    store
        .set_password("a replacement test password", true)
        .unwrap();
    assert_eq!(
        store
            .save_setup(&full, &last.revision, Some(&generation))
            .err()
            .unwrap()
            .downcast_ref::<SetupWriteError>(),
        Some(&SetupWriteError::OwnerChanged)
    );
    assert_eq!(store.config().unwrap().setup.revision, last.revision);
}
