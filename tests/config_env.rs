//! config.json loading and saving: ATLAS_NNTP_* env overlays and the
//! `usenet_servers` layout.
//!
//! One test, no threads: it changes the environment, which is only sound
//! while nothing else can be reading it.

#[test]
fn env_overlay_and_usenet_servers() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary and it never spawns threads
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
    }

    // docker style env creds overlay config.json but never get written into it
    let cfg_path = home.path().join("config.json");
    std::fs::write(
        &cfg_path,
        r#"{"host": "file.host", "username": "fileuser", "port": 119, "groups": ["a.b.c"], "api_key": "KEEP"}"#,
    )
    .unwrap();
    // SAFETY: single test, single thread, see the module docs
    unsafe {
        std::env::set_var("ATLAS_NNTP_HOST", "env.host");
        std::env::set_var("ATLAS_NNTP_USER", "envuser");
        std::env::set_var("ATLAS_NNTP_PASS", "envsecret");
    }

    let mut cfg = atlas::config::load_config().unwrap();
    let env_server = &cfg.servers[0];
    assert_eq!(
        (env_server.host.as_str(), env_server.password.as_str(), env_server.port),
        ("env.host", "envsecret", 563)
    );
    assert_eq!(cfg.servers[1].host, "file.host", "file servers stay as fallbacks behind the env one");
    assert_eq!(cfg.groups, vec!["a.b.c".to_string()]);
    assert_eq!(cfg.api_key.as_deref(), Some("KEEP"));

    cfg.groups.push("x.y.z".into());
    atlas::config::save_config(&cfg).unwrap();

    let saved = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(!saved.contains("envsecret") && !saved.contains("env.host") && !saved.contains("envuser"), "{saved}");
    assert!(saved.contains("x.y.z") && saved.contains("KEEP") && saved.contains("file.host"));
    assert_eq!(atlas::config::load_config().unwrap().groups, vec!["a.b.c".to_string(), "x.y.z".to_string()]);

    for k in ["ATLAS_NNTP_HOST", "ATLAS_NNTP_USER", "ATLAS_NNTP_PASS"] {
        // SAFETY: single test, single thread, see the module docs
        unsafe { std::env::remove_var(k) };
    }

    // usenet_servers layout: order by priority, saving keeps passwords and unknown keys
    std::fs::write(
        &cfg_path,
        r#"{
            "usenet_servers": [
                {"host": "four.example", "username": "u4", "password": "p4", "port": 563, "ssl": true, "connections": 10, "priority": 4},
                {"host": "two.example", "username": "u2", "password": "p2", "port": 563, "ssl": true, "connections": 10, "priority": 2},
                {"host": "one.example", "username": "u1", "password": "p1", "port": 563, "ssl": true, "priority": 1}
            ],
            "group": "alt.binaries.boneless",
            "groups": ["alt.binaries.boneless"],
            "my_note": "keep me"
        }"#,
    )
    .unwrap();

    let mut cfg = atlas::config::load_config().unwrap();
    let hosts: Vec<_> = cfg.servers.iter().map(|s| s.host.as_str()).collect();
    assert_eq!(hosts, vec!["one.example", "two.example", "four.example"]);
    assert_eq!(cfg.servers[0].connections(), atlas::config::DEFAULT_CONNECTIONS);
    assert_eq!(cfg.servers[1].connections(), 10);

    cfg.groups.push("alt.binaries.new".into());
    atlas::config::save_config(&cfg).unwrap();

    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(saved["my_note"], "keep me");
    assert!(saved.get("host").is_none());
    let servers = saved["usenet_servers"].as_array().unwrap();
    assert_eq!(servers.len(), 3);
    assert!(servers.iter().all(|s| s["password"].as_str().is_some_and(|p| p.starts_with('p'))), "{servers:?}");
    assert_eq!(atlas::config::load_config().unwrap(), cfg);
}
