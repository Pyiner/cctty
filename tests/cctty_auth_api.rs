use cctty::auth::{
    AuthLoginEvent, AuthLoginOptions, AuthLoginSession, AuthStatusOptions, auth_status_json,
};
use std::collections::HashMap;

mod support;
use support::FakeClaude;

#[tokio::test]
async fn auth_login_session_returns_url_prompt_success_and_exit() {
    let fixture = FakeClaude::new();
    let mut session = AuthLoginSession::start(AuthLoginOptions {
        passthrough_args: ["auth", "login", "--claudeai"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        claude_path: Some(fixture.path().to_path_buf()),
        ..AuthLoginOptions::default()
    })
    .unwrap();
    let input = session.input();
    let mut events = session.take_events();
    let mut seen = Vec::new();
    let mut submitted = false;

    while let Some(event) = events.recv().await {
        if matches!(event, AuthLoginEvent::AuthorizationUrl { .. }) && !submitted {
            submitted = true;
            input.submit_code("test-code#fake-state").await.unwrap();
        }
        let is_exit = matches!(event, AuthLoginEvent::Exit { .. });
        seen.push(event);
        if is_exit {
            break;
        }
    }

    let code = session.wait().await.unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        seen.first(),
        Some(&AuthLoginEvent::Started {
            command: fixture
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            args: ["auth", "login", "--claudeai"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
    );
    assert!(seen.iter().any(|event| matches!(
        event,
        AuthLoginEvent::AuthorizationUrl { url }
            if url.starts_with("https://claude.test/oauth/authorize")
    )));
    assert!(seen.iter().any(|event| matches!(
        event,
        AuthLoginEvent::InputRequested { input, .. } if input == "authorization_code"
    )));
    assert!(
        seen.iter()
            .any(|event| matches!(event, AuthLoginEvent::Success { .. }))
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, AuthLoginEvent::Exit { exit_code: 0 }))
    );

    let json = seen
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        !json.contains("test-code"),
        "auth code leaked in events:\n{json}"
    );
}

#[tokio::test]
async fn auth_login_session_reports_bad_code_as_error_and_exit() {
    let fixture = FakeClaude::new();
    let mut session = AuthLoginSession::start(AuthLoginOptions {
        passthrough_args: ["auth", "login", "--claudeai"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        claude_path: Some(fixture.path().to_path_buf()),
        ..AuthLoginOptions::default()
    })
    .unwrap();
    let input = session.input();
    let mut events = session.take_events();
    let mut seen = Vec::new();
    let mut submitted = false;

    while let Some(event) = events.recv().await {
        if matches!(event, AuthLoginEvent::AuthorizationUrl { .. }) && !submitted {
            submitted = true;
            input.submit_code("bad-code").await.unwrap();
        }
        let is_exit = matches!(event, AuthLoginEvent::Exit { .. });
        seen.push(event);
        if is_exit {
            break;
        }
    }

    let code = session.wait().await.unwrap();
    assert_eq!(code, 1);
    assert!(seen.iter().any(|event| matches!(
        event,
        AuthLoginEvent::Error { message } if message.contains("exited with code 1")
    )));
    assert!(
        seen.iter()
            .any(|event| matches!(event, AuthLoginEvent::Exit { exit_code: 1 }))
    );
}

#[tokio::test]
async fn auth_status_json_returns_parsed_claude_status() {
    let fixture = FakeClaude::new();
    let value = auth_status_json(AuthStatusOptions {
        claude_path: Some(fixture.path().to_path_buf()),
        ..AuthStatusOptions::default()
    })
    .await
    .unwrap();

    assert_eq!(value["loggedIn"], true);
    assert_eq!(value["authMethod"], "claude.ai");
    assert_eq!(value["orgName"], "Test Org");
}

#[tokio::test]
async fn auth_login_session_applies_per_session_environment() {
    let fixture = FakeClaude::new();
    let dir = tempfile::tempdir().unwrap();
    let env_path = dir.path().join("login-env.json");
    let config_dir = dir.path().join("managed-claude");
    let mut session = AuthLoginSession::start(AuthLoginOptions {
        passthrough_args: ["auth", "login", "--claudeai"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        claude_path: Some(fixture.path().to_path_buf()),
        env: HashMap::from([
            (
                "FAKE_CLAUDE_ENV_PATH".to_owned(),
                env_path.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_owned(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ]),
        ..AuthLoginOptions::default()
    })
    .unwrap();
    let input = session.input();
    let mut events = session.take_events();
    while let Some(event) = events.recv().await {
        if matches!(event, AuthLoginEvent::AuthorizationUrl { .. }) {
            input.submit_code("test-code#fake-state").await.unwrap();
        }
        if matches!(event, AuthLoginEvent::Exit { .. }) {
            break;
        }
    }
    assert_eq!(session.wait().await.unwrap(), 0);

    let env: serde_json::Value = serde_json::from_slice(&std::fs::read(env_path).unwrap()).unwrap();
    assert_eq!(
        env["CLAUDE_CONFIG_DIR"],
        config_dir.to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn auth_status_json_applies_per_call_environment() {
    let fixture = FakeClaude::new();
    let dir = tempfile::tempdir().unwrap();
    let env_path = dir.path().join("status-env.json");
    let config_dir = dir.path().join("managed-claude");
    auth_status_json(AuthStatusOptions {
        claude_path: Some(fixture.path().to_path_buf()),
        env: HashMap::from([
            (
                "FAKE_CLAUDE_ENV_PATH".to_owned(),
                env_path.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_owned(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ]),
        ..AuthStatusOptions::default()
    })
    .await
    .unwrap();

    let env: serde_json::Value = serde_json::from_slice(&std::fs::read(env_path).unwrap()).unwrap();
    assert_eq!(
        env["CLAUDE_CONFIG_DIR"],
        config_dir.to_string_lossy().as_ref()
    );
}
