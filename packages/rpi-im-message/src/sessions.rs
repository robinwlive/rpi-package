//! Persistent session selection scoped to an IM profile and chat.
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static STORE_LOCK: Mutex<()> = Mutex::new(());

fn load(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({"version":1,"chats":{}}));
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() > 1_048_576 {
        return Err("IM session store exceeds 1 MiB".into());
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid IM session store: {error}"))?;
    if value["version"] != 1 || !value["chats"].is_object() {
        return Err("invalid IM session store schema".into());
    }
    Ok(value)
}

fn key(profile: &str, chat: &str, topic: Option<&str>) -> String {
    match topic {
        Some(topic) => format!("{profile}\n{chat}\n{topic}"),
        None => format!("{profile}\n{chat}"),
    }
}

/// The session id a conversation maps to.
///
/// A Feishu group can hold several **topics** (话题), and each topic is a
/// conversation of its own: what is said in one topic has nothing to do with
/// another. So the topic's root message id is part of the session id, and a
/// group without topics keeps the plain one.
pub fn topic_session_id(chat: &str, topic: Option<&str>) -> String {
    let base = super::conversation_session_id(chat);
    match topic {
        Some(topic) => {
            let suffix: String = topic
                .chars()
                .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
                .take(32)
                .collect();
            if suffix.is_empty() {
                base
            } else {
                format!("{base}-{suffix}")
            }
        }
        None => base,
    }
}

fn selection<'a>(
    value: &'a Value,
    profile: &str,
    chat: &str,
    topic: Option<&str>,
) -> Result<Option<&'a str>, String> {
    let entry = &value["chats"][key(profile, chat, topic)];
    if entry.is_null() {
        return Ok(None);
    }
    let selected = entry["selected"]
        .as_str()
        .ok_or("invalid IM session selection")?;
    let known = entry["sessions"]
        .as_array()
        .ok_or("invalid IM session list")?;
    let base = topic_session_id(chat, topic);
    // The session id is the base plus a suffix (`/new` appends a stamp), so it
    // must be the base itself or the base followed by a separator. A plain
    // prefix check would let one topic claim another's session: `root-ab`
    // starts with `root-a`.
    let owned = selected == base
        || selected
            .strip_prefix(&base)
            .is_some_and(|rest| rest.starts_with('-'));
    if !owned
        || !selected
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        || !known.iter().any(|id| id.as_str() == Some(selected))
    {
        return Err("IM session selection is not owned by this conversation".into());
    }
    Ok(Some(selected))
}

pub fn selected(
    path: &Path,
    profile: &str,
    chat: &str,
    topic: Option<&str>,
) -> Result<Option<String>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "IM session store lock is poisoned")?;
    selection(&load(path)?, profile, chat, topic).map(|selected| selected.map(str::to_owned))
}

/// Unknown slash commands remain normal user prompts.
pub fn command(
    path: &Path,
    profile: &str,
    chat: &str,
    topic: Option<&str>,
    text: &str,
) -> Option<Result<String, String>> {
    let words: Vec<_> = text.split_whitespace().collect();
    let command = *words.first()?;
    if !matches!(command, "/new" | "/sessions" | "/session") {
        return None;
    }
    Some(handle(path, profile, chat, topic, &words))
}

fn handle(
    path: &Path,
    profile: &str,
    chat: &str,
    topic: Option<&str>,
    words: &[&str],
) -> Result<String, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "IM session store lock is poisoned")?;
    let mut value = load(path)?;
    let base = topic_session_id(chat, topic);
    let current = selection(&value, profile, chat, topic)?
        .unwrap_or(&base)
        .to_owned();
    let entry_key = key(profile, chat, topic);
    if value["chats"][&entry_key].is_null() {
        value["chats"][&entry_key] = json!({"selected":base,"sessions":[base]});
    }
    let entry = &mut value["chats"][&entry_key];
    let response = match words {
        ["/session"] => return Ok(format!("当前 session：{current}\n/new 新建会话\n/sessions 查看会话列表\n/session <ID> 切换会话")),
        ["/sessions"] => {
            let known = entry["sessions"].as_array().ok_or("invalid IM session list")?;
            let mut lines = vec!["当前聊天的 sessions（最近 20 个）：".to_owned()];
            for id in known.iter().rev().take(20).filter_map(Value::as_str) {
                lines.push(format!("{} {id}", if id == current { "当前 →" } else { "•" }));
            }
            lines.push("发送 /session <ID> 切换；/new 新建空白会话。".into());
            return Ok(lines.join("\n"));
        }
        ["/new"] => {
            let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| error.to_string())?.as_nanos();
            let id = format!("{base}-{stamp:x}");
            entry["selected"] = json!(id);
            entry["sessions"].as_array_mut().ok_or("invalid IM session list")?.push(json!(id));
            format!("已新建并切换 session：{id}\n后续消息使用空白上下文，原会话历史已保留。")
        }
        ["/session", id] => {
            if !entry["sessions"].as_array().ok_or("invalid IM session list")?.iter().any(|known| known.as_str() == Some(id)) {
                return Err("未找到当前聊天的这个 session。发送 /sessions 查看完整 ID。".into());
            }
            entry["selected"] = json!(id);
            format!("已切换 session：{id}\n后续消息使用该会话上下文。")
        }
        _ => return Err("指令用法：/new、/sessions、/session 或 /session <ID>。".into()),
    };
    let parent = path.parent().ok_or("invalid IM session store path")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let stage = path.with_extension("json.tmp");
    std::fs::write(
        &stage,
        serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    std::fs::rename(&stage, path)
        .map_err(|error| format!("failed saving IM session selection: {error}"))?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupted_selection_is_reported_instead_of_resetting_history() {
        let path = std::env::temp_dir().join(format!(
            "rpi-im-sessions-invalid-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"{invalid").unwrap();
        assert!(selected(&path, "p", "oc_a", None).is_err());
        assert!(command(&path, "p", "oc_a", None, "/new").unwrap().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{invalid");
        std::fs::remove_file(path).unwrap();
    }

    /// 话题是独立的会话：同一个群里两个话题不能共用一个 session，
    /// 否则一个话题里说的话会漏到另一个话题。
    #[test]
    fn topics_in_one_group_are_separate_sessions() {
        let path = std::env::temp_dir().join(format!(
            "rpi-im-sessions-topics-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // 主群（没有话题）有自己的 session。
        command(&path, "p", "oc_a", None, "/new").unwrap().unwrap();
        // 话题 A 和话题 B 各自有自己的 session。
        command(&path, "p", "oc_a", Some("root-a"), "/new")
            .unwrap()
            .unwrap();
        command(&path, "p", "oc_a", Some("root-b"), "/new")
            .unwrap()
            .unwrap();
        let topic_a = selected(&path, "p", "oc_a", Some("root-a")).unwrap().unwrap();
        let topic_b = selected(&path, "p", "oc_a", Some("root-b")).unwrap().unwrap();
        assert_ne!(topic_a, topic_b, "两个话题不能是同一个 session");
        assert!(
            topic_a.contains("root-a") && topic_b.contains("root-b"),
            "session id 要带话题: {topic_a} / {topic_b}"
        );
        // 选中互不影响。
        assert_eq!(
            selected(&path, "p", "oc_a", Some("root-a")).unwrap().unwrap(),
            topic_a
        );
        assert_eq!(
            selected(&path, "p", "oc_a", Some("root-b")).unwrap().unwrap(),
            topic_b
        );
        // 一个话题的 session 不能被另一个话题选中。
        assert!(command(
            &path,
            "p",
            "oc_a",
            Some("root-b"),
            &format!("/session {topic_a}")
        )
        .unwrap()
        .is_err());
        std::fs::remove_file(path).unwrap();
    }

    /// 话题 id 是前缀包含关系时（`root-a` 与 `root-ab`），一个话题
    /// 不能认领另一个话题的 session —— 否则隔离就漏了。
    #[test]
    fn a_topic_cannot_claim_a_session_that_only_shares_its_prefix() {
        let path = std::env::temp_dir().join(format!(
            "rpi-im-sessions-prefix-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        command(&path, "p", "oc_a", Some("root-ab"), "/new")
            .unwrap()
            .unwrap();
        let other = selected(&path, "p", "oc_a", Some("root-ab")).unwrap().unwrap();
        // 在 root-a 话题里选 root-ab 的 session：必须失败。
        assert!(
            command(&path, "p", "oc_a", Some("root-a"), &format!("/session {other}"))
                .unwrap()
                .is_err(),
            "root-a 不能认领 root-ab 的 session"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// 没有话题的普通群保持原来的 session id，不被话题机制影响。
    #[test]
    fn a_group_without_topics_keeps_the_plain_session_id() {
        let path = std::env::temp_dir().join(format!(
            "rpi-im-sessions-plain-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        command(&path, "p", "oc_a", None, "/new").unwrap().unwrap();
        let plain = selected(&path, "p", "oc_a", None).unwrap().unwrap();
        assert!(
            plain.starts_with("im-feishu-oc_a"),
            "没有话题时 session id 就是群: {plain}"
        );
        // 同一个群、没有话题：选中不受话题影响。
        assert_eq!(
            selected(&path, "p", "oc_a", None).unwrap().unwrap(),
            plain
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn creates_switches_persists_and_isolates_chat_sessions() {
        let path = std::env::temp_dir().join(format!(
            "rpi-im-sessions-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(selected(&path, "p", "oc_a", None).unwrap().is_none());
        assert!(command(&path, "p", "oc_a", None, "/session")
            .unwrap()
            .unwrap()
            .contains("im-feishu-oc_a"));
        command(&path, "p", "oc_a", None, "/new").unwrap().unwrap();
        let first = selected(&path, "p", "oc_a", None).unwrap().unwrap();
        command(&path, "p", "oc_a", None, "/new").unwrap().unwrap();
        assert_ne!(selected(&path, "p", "oc_a", None).unwrap().unwrap(), first);
        assert!(command(&path, "p", "oc_a", None, "/sessions")
            .unwrap()
            .unwrap()
            .contains(&first));
        command(&path, "p", "oc_a", None, &format!("/session {first}"))
            .unwrap()
            .unwrap();
        assert_eq!(selected(&path, "p", "oc_a", None).unwrap().unwrap(), first);
        assert!(command(&path, "p", "oc_b", None, &format!("/session {first}"))
            .unwrap()
            .is_err());
        assert!(selected(&path, "other-profile", "oc_a", None).unwrap().is_none());
        command(&path, "p", "oc_a", None, "/session im-feishu-oc_a")
            .unwrap()
            .unwrap();
        assert_eq!(
            selected(&path, "p", "oc_a", None).unwrap().unwrap(),
            "im-feishu-oc_a"
        );
        assert!(command(&path, "p", "oc_a", None, "/session ../../secret")
            .unwrap()
            .is_err());
        assert!(command(&path, "p", "oc_a", None, "/new extra").unwrap().is_err());
        assert!(command(&path, "p", "oc_a", None, "普通消息 /new").is_none());
        std::fs::remove_file(path).unwrap();
    }
}
