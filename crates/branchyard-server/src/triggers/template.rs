//! `{{placeholder}}` templates in a trigger's prompt and branch name.
//!
//! | Placeholder | Value |
//! |---|---|
//! | `{{event.source}}`, `kind`, `id`, `repo`, `author`, `title`, `text`, `url`, `number`, `branch`, `channel` | The normalized event's field; empty when it has none |
//! | `{{event.labels}}` | Its labels, comma-separated |
//! | `{{event.from}}`, `to`, `cc`, `subject`, `message_id`, `attachments` | An email's sender, recipients (comma-separated), subject, `Message-ID`, and its attachments as `name (type, N bytes)`, comma-separated; empty for other events |
//! | `{{event.payload.a.b.0.c}}` | Any value of the delivery body, by path (a number indexes an array); a string as is, anything else as JSON |
//! | `{{trigger.name}}`, `{{trigger.id}}`, `{{trigger.repo}}` | The trigger's |
//! | `{{scheduled_at}}` | The run's scheduled time (or arrival time, for an event), RFC 3339 in UTC |
//! | `{{run.id}}` | The run's ID |
//!
//! Spaces inside the braces are allowed. A template is checked when the
//! trigger is created: an unknown placeholder, or an `event.*` one on a
//! schedule, is refused then rather than rendered empty later.

use branchyard_client::triggers::TriggerEvent;
use serde_json::Value;

const EVENT_FIELDS: &[&str] = &[
    "source",
    "kind",
    "id",
    "repo",
    "author",
    "title",
    "text",
    "url",
    "number",
    "branch",
    "channel",
    "labels",
    "from",
    "to",
    "cc",
    "subject",
    "message_id",
    "attachments",
];

/// The values a template is rendered with.
pub struct Context<'a> {
    pub event: Option<&'a TriggerEvent>,
    pub trigger_id: &'a str,
    pub trigger_name: &'a str,
    pub trigger_repo: &'a str,
    pub run_id: &'a str,
    pub scheduled_at_ms: u64,
}

/// The placeholders in `template`, in order, trimmed.
fn placeholders(template: &str) -> Result<Vec<(usize, usize, String)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(open) = template[at..].find("{{") {
        let start = at + open;
        let Some(close) = template[start + 2..].find("}}") else {
            return Err(format!(
                "a placeholder opened at character {start} is never closed with }}}}"
            ));
        };
        let end = start + 2 + close + 2;
        let name = template[start + 2..end - 2].trim().to_owned();
        out.push((start, end, name));
        at = end;
    }
    Ok(out)
}

/// Check every placeholder in `template` names a value; `event` says
/// whether the trigger has events.
pub fn check(template: &str, event: bool, what: &str) -> Result<(), String> {
    for (_, _, name) in placeholders(template).map_err(|e| format!("{what}: {e}"))? {
        let known = match name.split_once('.') {
            Some(("event", rest)) => {
                if !event {
                    return Err(format!(
                        "{what}: {{{{{name}}}}} needs an event, and a schedule has none"
                    ));
                }
                EVENT_FIELDS.contains(&rest)
                    || rest
                        .strip_prefix("payload.")
                        .is_some_and(|p| !p.is_empty() && p.split('.').all(|s| !s.is_empty()))
            }
            Some(("trigger", rest)) => matches!(rest, "name" | "id" | "repo"),
            Some(("run", rest)) => rest == "id",
            None => name == "scheduled_at",
            _ => false,
        };
        if !known {
            return Err(format!(
                "{what}: {{{{{name}}}}} is not a placeholder; use event.<{}>, \
                 event.payload.<path>, trigger.name, trigger.id, trigger.repo, run.id or \
                 scheduled_at",
                EVENT_FIELDS.join("|")
            ));
        }
    }
    Ok(())
}

fn payload_value(payload: &Value, path: &str) -> String {
    let mut v = payload;
    for segment in path.split('.') {
        let next = match v {
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
            other => other.get(segment),
        };
        match next {
            Some(n) => v = n,
            None => return String::new(),
        }
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn event_value(e: &TriggerEvent, field: &str) -> String {
    let some = |v: &Option<String>| v.clone().unwrap_or_default();
    match field {
        "source" => e.source.clone(),
        "kind" => e.kind.clone(),
        "id" => e.id.clone(),
        "repo" => some(&e.repo),
        "author" => some(&e.author),
        "title" => some(&e.title),
        "text" => some(&e.text),
        "url" => some(&e.url),
        "number" => some(&e.number),
        "branch" => some(&e.branch),
        "channel" => some(&e.channel),
        "labels" => e.labels.join(", "),
        "from" | "to" | "cc" | "subject" | "message_id" | "attachments" => e
            .email
            .as_ref()
            .map(|m| match field {
                "from" => m.from.clone(),
                "to" => m.to.join(", "),
                "cc" => m.cc.join(", "),
                "subject" => m.subject.clone(),
                "message_id" => m.message_id.clone().unwrap_or_default(),
                _ => m
                    .attachments
                    .iter()
                    .map(|a| {
                        let mut about = Vec::new();
                        if !a.content_type.is_empty() {
                            about.push(a.content_type.clone());
                        }
                        if let Some(size) = a.size {
                            about.push(format!("{size} bytes"));
                        }
                        match about.is_empty() {
                            true => a.name.clone(),
                            false => format!("{} ({})", a.name, about.join(", ")),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            })
            .unwrap_or_default(),
        _ => field
            .strip_prefix("payload.")
            .map(|p| payload_value(&e.payload, p))
            .unwrap_or_default(),
    }
}

/// `template` with each placeholder replaced; one [`check`] accepted. A
/// placeholder the event has no value for renders empty.
pub fn render(template: &str, cx: &Context<'_>) -> Result<String, String> {
    render_with(template, cx, false)
}

/// [`render`], refusing a placeholder that would render empty: for a
/// value that must be determined by the event, such as the branch a
/// trigger delivers to, where an empty field would pool unrelated events.
pub fn render_filled(template: &str, cx: &Context<'_>) -> Result<String, String> {
    render_with(template, cx, true)
}

fn render_with(template: &str, cx: &Context<'_>, filled: bool) -> Result<String, String> {
    let mut out = String::new();
    let mut at = 0;
    for (start, end, name) in placeholders(template)? {
        out.push_str(&template[at..start]);
        let value = match name.split_once('.') {
            Some(("event", field)) => cx.event.map(|e| event_value(e, field)).unwrap_or_default(),
            Some(("trigger", "name")) => cx.trigger_name.to_owned(),
            Some(("trigger", "id")) => cx.trigger_id.to_owned(),
            Some(("trigger", "repo")) => cx.trigger_repo.to_owned(),
            Some(("run", "id")) => cx.run_id.to_owned(),
            None if name == "scheduled_at" => {
                branchyard_support::time::rfc3339_secs(cx.scheduled_at_ms)
            }
            _ => return Err(format!("{{{{{name}}}}} is not a placeholder")),
        };
        if filled && value.is_empty() {
            return Err(format!("{{{{{name}}}}} is empty for this event"));
        }
        out.push_str(&value);
        at = end;
    }
    out.push_str(&template[at..]);
    Ok(out)
}

/// A branch name from free text: lowercase `a-z`, `0-9` and `-`, at most
/// 60 characters, never empty.
pub fn branch_name(text: &str) -> String {
    let mut name = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if matches!(c, '-' | '_' | '.' | '/' | ' ')
            && !name.ends_with('-')
            && !name.is_empty()
        {
            name.push('-');
        }
        if name.len() >= 60 {
            break;
        }
    }
    let name = name.trim_matches('-').to_owned();
    match name.is_empty() {
        true => "trigger".into(),
        false => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> TriggerEvent {
        TriggerEvent {
            source: "github".into(),
            kind: "issues.opened".into(),
            id: "d1".into(),
            title: Some("Parser crash".into()),
            number: Some("42".into()),
            labels: vec!["bug".into(), "agent".into()],
            payload: serde_json::json!({"issue": {"user": {"login": "alice"}, "assignees": [{"login": "bob"}], "n": 3}}),
            ..TriggerEvent::default()
        }
    }

    fn cx(e: Option<&TriggerEvent>) -> Context<'_> {
        Context {
            event: e,
            trigger_id: "trg_1",
            trigger_name: "triage",
            trigger_repo: "app",
            run_id: "run_1",
            scheduled_at_ms: 1_790_000_000_000,
        }
    }

    #[test]
    fn renders_event_fields_payload_paths_and_trigger_values() {
        let e = event();
        let out = render(
            "Fix #{{event.number}}: {{ event.title }} ({{event.labels}}) by \
             {{event.payload.issue.user.login}}, {{event.payload.issue.assignees.0.login}}, \
             {{event.payload.issue.n}}, [{{event.payload.missing.path}}] via {{trigger.name}} \
             at {{scheduled_at}}",
            &cx(Some(&e)),
        )
        .unwrap();
        assert_eq!(
            out,
            "Fix #42: Parser crash (bug, agent) by alice, bob, 3, [] via triage at \
             2026-09-21T14:13:20Z"
        );
    }

    #[test]
    fn checks_refuse_unknown_and_misplaced_placeholders() {
        assert!(check("{{event.title}}", true, "prompt").is_ok());
        assert!(check("{{event.payload.a.b}}", true, "prompt").is_ok());
        assert!(check(
            "{{trigger.name}} {{scheduled_at}} {{run.id}}",
            false,
            "prompt"
        )
        .is_ok());
        let err = check("{{event.title}}", false, "prompt").unwrap_err();
        assert!(err.contains("needs an event"), "{err}");
        let err = check("{{event.titel}}", true, "prompt").unwrap_err();
        assert!(err.contains("not a placeholder"), "{err}");
        let err = check("{{event.payload.}}", true, "prompt").unwrap_err();
        assert!(err.contains("not a placeholder"), "{err}");
        let err = check("hello {{event.title", true, "prompt").unwrap_err();
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn email_fields_render_and_are_empty_for_other_events() {
        use branchyard_client::triggers::{EmailAttachment, EmailMessage};
        let mut e = event();
        assert_eq!(render("[{{event.from}}]", &cx(Some(&e))).unwrap(), "[]");
        e.email = Some(EmailMessage {
            from: "alice@example.com".into(),
            to: vec!["ops@by.example".into(), "b@by.example".into()],
            subject: "Deploy failed".into(),
            message_id: Some("m1@example.com".into()),
            attachments: vec![
                EmailAttachment {
                    name: "log.txt".into(),
                    content_type: "text/plain".into(),
                    size: Some(12),
                },
                EmailAttachment {
                    name: "x".into(),
                    ..EmailAttachment::default()
                },
            ],
            ..EmailMessage::default()
        });
        assert!(check("{{event.from}} {{event.attachments}}", true, "prompt").is_ok());
        assert_eq!(
            render(
                "{{event.from}} -> {{event.to}}: {{event.subject}} <{{event.message_id}}> \
                 [{{event.attachments}}]",
                &cx(Some(&e))
            )
            .unwrap(),
            "alice@example.com -> ops@by.example, b@by.example: Deploy failed <m1@example.com> \
             [log.txt (text/plain, 12 bytes), x]"
        );
    }

    #[test]
    fn branch_names_are_sanitized() {
        assert_eq!(branch_name("Fix: Parser crash #42!"), "fix-parser-crash-42");
        assert_eq!(branch_name("triage-42"), "triage-42");
        assert_eq!(branch_name("///"), "trigger");
        assert!(branch_name(&"x".repeat(100)).len() <= 60);
    }
}
