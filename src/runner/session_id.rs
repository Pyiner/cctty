use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) struct SessionIdAlias {
    external: Option<String>,
    claude: Option<String>,
}

impl SessionIdAlias {
    pub(super) fn new(external: Option<String>) -> Self {
        let claude = external.as_deref().map(claude_safe_session_id);
        Self { external, claude }
    }

    pub(super) fn claude_session_id(&self) -> Option<String> {
        self.claude.clone()
    }

    pub(super) fn rewrite_args_for_claude(&self, args: &[String]) -> Vec<String> {
        rewrite_session_args_for_claude(args)
    }

    pub(super) fn externalize_value(&self, value: &mut Value) {
        let Some(external) = self.external.as_deref() else {
            return;
        };
        let Some(claude) = self.claude.as_deref() else {
            return;
        };
        if external == claude {
            return;
        }
        replace_session_field(value, "session_id", claude, external);
        replace_session_field(value, "sessionId", claude, external);
    }
}

fn rewrite_session_args_for_claude(args: &[String]) -> Vec<String> {
    let mut rewritten = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if is_session_value_flag(arg) {
            rewritten.push(arg.clone());
            if let Some(value) = args.get(index + 1) {
                rewritten.push(claude_safe_session_id(value));
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some((flag, value)) = arg.split_once('=')
            && is_session_value_flag(flag)
        {
            rewritten.push(format!("{flag}={}", claude_safe_session_id(value)));
            index += 1;
            continue;
        }
        rewritten.push(arg.clone());
        index += 1;
    }
    rewritten
}

fn is_session_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--session-id" | "--resume" | "-r" | "--parent-session-id"
    )
}

fn claude_safe_session_id(value: &str) -> String {
    if Uuid::parse_str(value).is_ok() {
        value.to_owned()
    } else {
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("cctty-session:{value}").as_bytes(),
        )
        .to_string()
    }
}

fn replace_session_field(value: &mut Value, field: &str, claude: &str, external: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.get(field).and_then(Value::as_str) == Some(claude) {
        object.insert(field.to_owned(), Value::String(external.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_non_uuid_session_args_for_claude() {
        let args = vec![
            "--session-id".to_owned(),
            "conductor-session-1".to_owned(),
            "--resume=existing-session".to_owned(),
            "--resume-session-at".to_owned(),
            "message-1".to_owned(),
        ];

        let rewritten = rewrite_session_args_for_claude(&args);
        assert_eq!(rewritten[0], "--session-id");
        assert!(Uuid::parse_str(&rewritten[1]).is_ok());
        assert_ne!(rewritten[1], "conductor-session-1");
        assert!(rewritten[2].starts_with("--resume="));
        assert!(Uuid::parse_str(rewritten[2].split_once('=').unwrap().1).is_ok());
        assert_eq!(rewritten[3], "--resume-session-at");
        assert_eq!(rewritten[4], "message-1");
    }

    #[test]
    fn externalizes_claude_session_id_for_sdk_output() {
        let alias = SessionIdAlias::new(Some("conductor-session-1".to_owned()));
        let mut value = json!({
            "type": "result",
            "session_id": alias.claude_session_id().unwrap(),
        });

        alias.externalize_value(&mut value);

        assert_eq!(value["session_id"], "conductor-session-1");
    }
}
