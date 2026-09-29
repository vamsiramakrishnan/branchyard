//! Declarative questions, their conditions and rules, and the batching that
//! turns partial answers into the next questions to ask.
//!
//! A topic lists every question it may ask, in order, as a function of the
//! detected facts and the answers so far. The engine evaluates each
//! question's `when` against earlier answers, normalizes and checks the
//! answers it was given, and returns the next batch: at most
//! [`BATCH_MAX`] askable questions, never one that depends on another
//! still unanswered, so a harness can put a whole batch in one
//! `AskUserQuestion` call.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Questions per batch: what one Claude Code `AskUserQuestion` call takes.
pub const BATCH_MAX: usize = 4;

/// Choices shown directly: `AskUserQuestion` takes 2 to 4 options, and
/// adds "Other" itself.
pub const CHOICES_MAX: usize = 4;

/// Answers by question id, normalized.
pub type Answers = BTreeMap<String, Value>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// One of the choices (or, with `allow_other`, any text the rules accept).
    Select,
    /// Any number of the choices: a JSON array, or a comma-separated string.
    Multiselect,
    /// Free text.
    Text,
    /// Yes or no: a JSON boolean, or `yes`/`no`.
    Confirm,
    /// A number: JSON number or numeric text.
    Number,
    /// A filesystem path.
    Path,
    /// Where a secret is: a variable name (`ANTHROPIC_API_KEY`) or `@file`.
    /// Never the secret itself.
    SecretRef,
}

/// An option of a question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Choice {
    /// The answer this choice gives; `null` for "skip".
    pub value: Value,
    /// Short, comma-free, unique within the question.
    pub label: String,
    pub description: String,
    /// The engine's suggestion; also the question's default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

impl Choice {
    pub fn new(
        value: impl Into<Value>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Choice {
        Choice {
            value: value.into(),
            label: label.into(),
            description: description.into(),
            recommended: false,
        }
    }

    /// The "skip" choice of an optional question.
    pub fn skip(description: impl Into<String>) -> Choice {
        Choice::new(Value::Null, "Skip", description)
    }

    pub fn recommended(mut self) -> Choice {
        self.recommended = true;
        self
    }
}

/// When a question applies, over earlier answers. A question skipped
/// because its own condition was false reads as `null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// The answer to `id` equals `value`.
    Equals {
        id: String,
        value: Value,
    },
    /// The answer to `id` is one of `values`.
    In {
        id: String,
        values: Vec<Value>,
    },
    /// The answer to `id`, a list, includes `value`.
    Includes {
        id: String,
        value: Value,
    },
    /// The answer to `id` is neither `null`, `false`, `""` nor `[]`.
    Truthy {
        id: String,
    },
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    pub fn equals(id: &str, value: impl Into<Value>) -> Condition {
        Condition::Equals {
            id: id.into(),
            value: value.into(),
        }
    }

    pub fn truthy(id: &str) -> Condition {
        Condition::Truthy { id: id.into() }
    }

    pub fn includes(id: &str, value: impl Into<Value>) -> Condition {
        Condition::Includes {
            id: id.into(),
            value: value.into(),
        }
    }

    pub fn negate(self) -> Condition {
        Condition::Not(Box::new(self))
    }

    /// Every question id this condition reads.
    pub fn ids(&self) -> Vec<&str> {
        match self {
            Condition::Equals { id, .. }
            | Condition::In { id, .. }
            | Condition::Includes { id, .. }
            | Condition::Truthy { id } => vec![id.as_str()],
            Condition::All(cs) | Condition::Any(cs) => cs.iter().flat_map(Condition::ids).collect(),
            Condition::Not(inner) => inner.ids(),
        }
    }

    /// `None` while a referenced question is unresolved.
    pub fn eval(&self, resolved: &Answers) -> Option<bool> {
        let get = |id: &str| resolved.get(id);
        Some(match self {
            Condition::Equals { id, value } => get(id)? == value,
            Condition::In { id, values } => values.contains(get(id)?),
            Condition::Includes { id, value } => match get(id)? {
                Value::Array(items) => items.contains(value),
                _ => false,
            },
            Condition::Truthy { id } => truthy(get(id)?),
            Condition::All(all) => {
                for c in all {
                    if !c.eval(resolved)? {
                        return Some(false);
                    }
                }
                true
            }
            Condition::Any(any) => {
                let mut unknown = false;
                for c in any {
                    match c.eval(resolved) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => unknown = true,
                    }
                }
                if unknown {
                    return None;
                }
                false
            }
            Condition::Not(inner) => !inner.eval(resolved)?,
        })
    }
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// A check on an answer, beyond its kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Rule {
    /// A number at least `value`.
    Min { value: f64 },
    /// A number at most `value`.
    Max { value: f64 },
    /// A whole number.
    Integer,
    /// A name: lowercase letters, digits, `.`, `_`, `-`, starting with a
    /// letter or digit, at most `max` characters.
    Name { max: usize },
    /// A socket address such as `127.0.0.1:8421`.
    ListenAddress,
    /// A URL with one of these schemes.
    Url { schemes: Vec<String> },
    /// A PostgreSQL URL without a password in it.
    PostgresUrl,
    /// A variable name or `@file`, never a secret's value.
    SecretRef,
    /// An absolute executable path followed by its arguments.
    AbsoluteCommand,
    /// A command line that splits into words.
    CommandLine,
    /// Comma-separated names, each a [`Rule::Name`].
    NameList { max: usize },
    /// A `[workspace] copy` glob: relative, inside the repository.
    CopyGlob,
}

impl Rule {
    /// Check a normalized, non-null answer. Messages never quote a
    /// [`Rule::SecretRef`] answer.
    pub fn check(&self, value: &Value) -> Result<(), String> {
        let text = value.as_str().unwrap_or_default();
        match self {
            Rule::Min { value: min } => match value.as_f64() {
                Some(n) if n >= *min => Ok(()),
                _ => Err(format!("must be at least {min}")),
            },
            Rule::Max { value: max } => match value.as_f64() {
                Some(n) if n <= *max => Ok(()),
                _ => Err(format!("must be at most {max}")),
            },
            Rule::Integer => match value.as_f64() {
                Some(n) if n.fract() == 0.0 => Ok(()),
                _ => Err("must be a whole number".into()),
            },
            Rule::Name { max } => check_name(text, *max),
            Rule::NameList { max } => {
                for name in split_list(text) {
                    check_name(&name, *max)?;
                }
                Ok(())
            }
            Rule::ListenAddress => text
                .parse::<std::net::SocketAddr>()
                .map(|_| ())
                .map_err(|_| {
                    format!("{text:?} is not an address such as 127.0.0.1:8421 or [::1]:8421")
                }),
            Rule::Url { schemes } => match text.split_once("://") {
                Some((scheme, rest)) if schemes.iter().any(|s| s == scheme) && !rest.is_empty() => {
                    Ok(())
                }
                _ => Err(format!("must be a {} URL", schemes.join(" or "))),
            },
            Rule::PostgresUrl => {
                let rest = text
                    .strip_prefix("postgres://")
                    .or_else(|| text.strip_prefix("postgresql://"))
                    .ok_or("must be a postgres:// URL")?;
                if rest
                    .split_once('@')
                    .is_some_and(|(user, _)| user.contains(':'))
                {
                    return Err(
                        "must not hold a password: use a .pgpass file, or the deploy \
                                topic, which reads it from a secret file"
                            .into(),
                    );
                }
                Ok(())
            }
            Rule::SecretRef => crate::config::check_secret_reference(text),
            Rule::AbsoluteCommand => match text.split_whitespace().next() {
                Some(program) if program.starts_with('/') => Ok(()),
                _ => Err("must start with an absolute path to the executable".into()),
            },
            Rule::CopyGlob => crate::config::check_copy_glob(text),
            Rule::CommandLine => match crate::config::split_words(text) {
                Ok(words) if !words.is_empty() => Ok(()),
                Ok(_) => Err("needs a command".into()),
                Err(e) => Err(e),
            },
        }
    }
}

fn check_name(text: &str, max: usize) -> Result<(), String> {
    let ok = !text.is_empty()
        && text.len() <= max
        && text.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
    match ok {
        true => Ok(()),
        false => Err(format!(
            "{text:?} is not a usable name: lowercase letters, digits, '.', '_' and '-', \
             starting with a letter or digit, at most {max} characters"
        )),
    }
}

/// `a, b,c` as `["a", "b", "c"]`.
pub fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Question {
    /// Stable, dotted; the key of its answer.
    pub id: String,
    pub kind: Kind,
    /// A chip label of at most 12 characters (`AskUserQuestion`'s `header`).
    pub header: String,
    /// The question, ending in `?`.
    pub prompt: String,
    /// Why Branchyard asks: what the answer changes.
    pub why: String,
    /// At most [`CHOICES_MAX`] options, the recommended one first.
    pub choices: Vec<Choice>,
    /// Further options a wizard lists and a harness accepts as "Other".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more_choices: Vec<Choice>,
    /// Whether an answer outside the choices is accepted (checked by `rules`).
    pub allow_other: bool,
    /// What an unanswered question becomes with `--defaults`, and what
    /// `null` means for a required one.
    pub default: Value,
    /// Whether `null` (skip) is a valid answer.
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    /// For `secret_ref`: the secret's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl Question {
    pub fn new(id: &str, kind: Kind, header: &str, prompt: &str, why: &str) -> Question {
        Question {
            id: id.into(),
            kind,
            header: header.into(),
            prompt: prompt.into(),
            why: why.into(),
            choices: Vec::new(),
            more_choices: Vec::new(),
            allow_other: matches!(
                kind,
                Kind::Text | Kind::Number | Kind::Path | Kind::SecretRef
            ),
            default: Value::Null,
            optional: false,
            when: None,
            rules: Vec::new(),
            secret: None,
        }
    }

    /// Choices, the first [`CHOICES_MAX`] shown and the rest in
    /// `more_choices`; the default becomes the recommended choice.
    pub fn choices(mut self, mut choices: Vec<Choice>) -> Question {
        if let Some(i) = choices.iter().position(|c| c.value == self.default) {
            let choice = choices.remove(i).recommended();
            choices.insert(0, choice);
        }
        let more = choices.split_off(choices.len().min(CHOICES_MAX));
        self.choices = choices;
        self.more_choices = more;
        self
    }

    /// Set the default; call before [`Question::choices`].
    pub fn default(mut self, value: impl Into<Value>) -> Question {
        self.default = value.into();
        self
    }

    pub fn optional(mut self) -> Question {
        self.optional = true;
        self
    }

    pub fn when(mut self, condition: Condition) -> Question {
        self.when = Some(condition);
        self
    }

    pub fn rule(mut self, rule: Rule) -> Question {
        self.rules.push(rule);
        self
    }

    pub fn allow_other(mut self, allow: bool) -> Question {
        self.allow_other = allow;
        self
    }

    pub fn secret(mut self, name: &str) -> Question {
        self.secret = Some(name.into());
        self
    }

    fn all_choices(&self) -> impl Iterator<Item = &Choice> {
        self.choices.iter().chain(&self.more_choices)
    }

    /// A choice matching `text` by value or label, ignoring case.
    fn choice_for(&self, text: &str) -> Option<&Choice> {
        let text = text.trim();
        self.all_choices().find(|c| {
            c.label.eq_ignore_ascii_case(text)
                || c.value
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(text))
                || (c.value.is_boolean() || c.value.is_number())
                    && c.value == serde_json::from_str::<Value>(text).unwrap_or(Value::Null)
        })
    }

    /// The default as an answer (what `--defaults` gives an unanswered
    /// question).
    pub fn default_answer(&self) -> Result<Value, String> {
        match (&self.default, self.optional) {
            (Value::Null, true) => Ok(Value::Null),
            (Value::Null, false) => Err("needs an answer".into()),
            (default, _) => Ok(default.clone()),
        }
    }

    /// Normalize a raw answer: labels to values, text to numbers and
    /// booleans, lists from comma-separated text, `null` to the default,
    /// then check it against the choices and the rules.
    pub fn normalize(&self, raw: &Value) -> Result<Value, String> {
        // `null` is a skip choice's value: it skips an optional question;
        // a required one takes its default.
        if raw.is_null() {
            return match self.optional {
                true => Ok(Value::Null),
                false => self.default_answer(),
            };
        }
        if let Value::String(text) = raw {
            let t = text.trim();
            let skip = t.is_empty()
                || ["skip", "none", "no value"]
                    .iter()
                    .any(|s| t.eq_ignore_ascii_case(s));
            if skip && self.optional && self.kind != Kind::Confirm {
                return Ok(Value::Null);
            }
        }
        let value = match self.kind {
            Kind::Confirm => match raw {
                Value::Bool(b) => Value::Bool(*b),
                Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "yes" | "y" | "true" | "on" => Value::Bool(true),
                    "no" | "n" | "false" | "off" => Value::Bool(false),
                    _ => match self.choice_for(s) {
                        Some(c) => c.value.clone(),
                        None => return Err("answer yes or no".into()),
                    },
                },
                _ => return Err("answer yes or no".into()),
            },
            Kind::Number => match raw {
                Value::Number(_) => raw.clone(),
                Value::String(s) => match self.choice_for(s) {
                    Some(c) => c.value.clone(),
                    None => {
                        let n: f64 = s
                            .trim()
                            .trim_start_matches('$')
                            .parse()
                            .map_err(|_| "must be a number".to_owned())?;
                        number(n)
                    }
                },
                _ => return Err("must be a number".into()),
            },
            Kind::Multiselect => {
                let items: Vec<String> = match raw {
                    Value::Array(items) => items
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect(),
                    Value::String(s) => split_list(s),
                    _ => return Err("must be a list".into()),
                };
                let mut values = Vec::new();
                for item in items {
                    let value = match self.choice_for(&item) {
                        Some(c) => c.value.clone(),
                        None if self.allow_other => Value::String(item.trim().to_owned()),
                        None => return Err(self.not_a_choice(&item)),
                    };
                    if !value.is_null() && !values.contains(&value) {
                        values.push(value);
                    }
                }
                Value::Array(values)
            }
            Kind::Select | Kind::Text | Kind::Path | Kind::SecretRef => {
                let text = match raw {
                    Value::String(s) => s.trim().to_owned(),
                    Value::Number(_) | Value::Bool(_) => raw.to_string(),
                    _ => return Err("must be text".into()),
                };
                match self.choice_for(&text) {
                    Some(c) => c.value.clone(),
                    None if self.allow_other => Value::String(text),
                    None if self.kind == Kind::SecretRef => {
                        return Err("is not one of the choices".into())
                    }
                    None => return Err(self.not_a_choice(&text)),
                }
            }
        };
        if value.is_null() {
            return match self.optional {
                true => Ok(Value::Null),
                false => Err("needs an answer".into()),
            };
        }
        for rule in &self.rules {
            match &value {
                Value::Array(items) => {
                    for item in items {
                        rule.check(item)?;
                    }
                }
                single => rule.check(single)?,
            }
        }
        Ok(value)
    }

    fn not_a_choice(&self, text: &str) -> String {
        let names: Vec<String> = self
            .all_choices()
            .filter_map(|c| c.value.as_str().map(str::to_owned))
            .collect();
        format!("{text:?} is not one of {}", names.join(", "))
    }
}

/// A JSON number, whole when it can be.
pub fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// An answer that was refused, and why. The message never quotes a secret
/// reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AnswerError {
    pub id: String,
    pub message: String,
}

/// Where an interview stands.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// Valid answers, normalized, by id; `null` for a question skipped.
    pub answers: Answers,
    /// Every question that applies and is unanswered, in order; the next
    /// batch is a prefix of it.
    pub pending: Vec<Question>,
    /// Questions that cannot be asked until a pending one is answered.
    pub deferred: usize,
    pub errors: Vec<AnswerError>,
    /// Answer ids no question has.
    pub unknown: Vec<String>,
    /// Every question that applied, answered or not, in order.
    pub asked: Vec<Question>,
}

impl State {
    pub fn done(&self) -> bool {
        self.pending.is_empty() && self.deferred == 0
    }

    /// The next questions to ask: pending ones, at most [`BATCH_MAX`].
    pub fn batch(&self) -> Vec<Question> {
        self.pending.iter().take(BATCH_MAX).cloned().collect()
    }
}

/// Resolve `raw` answers against the questions `questions` produces for
/// them, repeating until stable, since a topic's questions (and defaults)
/// may depend on earlier answers. With `defaults`, every pending question
/// takes its default, as if the answer were `null`.
pub fn resolve(
    questions: &dyn Fn(&Answers) -> Vec<Question>,
    raw: &BTreeMap<String, Value>,
    defaults: bool,
) -> State {
    let mut answers = Answers::new();
    // Questions may appear once earlier answers exist (one per chosen
    // secret, say), so repeat until the answers stop changing; a bound
    // keeps a topic whose questions oscillate from looping.
    for _ in 0..32 {
        let state = step(&questions(&answers), raw, defaults);
        if state.answers == answers {
            return state;
        }
        answers = state.answers;
    }
    step(&questions(&answers), raw, defaults)
}

fn step(questions: &[Question], raw: &BTreeMap<String, Value>, defaults: bool) -> State {
    let mut state = State::default();
    // A condition may name a question this topic did not ask here (one
    // that exists only on some machines): it reads as null.
    let ids: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
    let absent: Answers = questions
        .iter()
        .filter_map(|q| q.when.as_ref())
        .flat_map(Condition::ids)
        .filter(|id| !ids.contains(id))
        .map(|id| (id.to_owned(), Value::Null))
        .collect();
    for q in questions {
        // Conditions name earlier questions only: each is answered,
        // skipped (null) or pending, and pending leaves this one deferred.
        let when = q.when.as_ref().map_or(Some(true), |c| {
            let mut view = absent.clone();
            view.extend(state.answers.iter().map(|(k, v)| (k.clone(), v.clone())));
            c.eval(&view)
        });
        match when {
            None => {
                state.deferred += 1;
                continue;
            }
            Some(false) => {
                state.answers.insert(q.id.clone(), Value::Null);
                continue;
            }
            Some(true) => {}
        }
        state.asked.push(q.clone());
        let given = match raw.get(&q.id) {
            Some(value) => Some(q.normalize(value)),
            None if defaults => Some(q.default_answer()),
            None => None,
        };
        match given {
            Some(Ok(value)) => {
                state.answers.insert(q.id.clone(), value);
            }
            Some(Err(message)) => {
                state.errors.push(AnswerError {
                    id: q.id.clone(),
                    message,
                });
                state.pending.push(q.clone());
            }
            None => state.pending.push(q.clone()),
        }
    }
    state.unknown = raw
        .keys()
        .filter(|k| !ids.contains(&k.as_str()))
        .cloned()
        .collect();
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn questions(_: &Answers) -> Vec<Question> {
        vec![
            Question::new("a", Kind::Select, "A", "A?", "why")
                .default("x")
                .choices(vec![Choice::new("x", "X", "x"), Choice::new("y", "Y", "y")]),
            Question::new("b", Kind::Confirm, "B", "B?", "why")
                .default(false)
                .when(Condition::equals("a", "y")),
            Question::new("c", Kind::Number, "C", "C?", "why")
                .default(3)
                .rule(Rule::Min { value: 1.0 }),
            Question::new("d", Kind::Text, "D", "D?", "why").when(Condition::truthy("b")),
        ]
    }

    fn raw(value: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn conditions_defer_dependents_to_a_later_batch() {
        let state = resolve(&questions, &BTreeMap::new(), false);
        let ids: Vec<String> = state.batch().into_iter().map(|q| q.id).collect();
        assert_eq!(ids, ["a", "c"], "b waits for a, d for b");
        assert_eq!(state.deferred, 2);

        let state = resolve(&questions, &raw(json!({"a": "Y", "c": "2"})), false);
        assert_eq!(state.answers["a"], json!("y"), "labels map to values");
        assert_eq!(state.answers["c"], json!(2));
        let ids: Vec<String> = state.batch().into_iter().map(|q| q.id).collect();
        assert_eq!(ids, ["b"]);

        let state = resolve(&questions, &raw(json!({"a": "x", "c": 2})), false);
        assert!(state.done(), "b and d do not apply: {state:?}");
        assert_eq!(state.answers["b"], Value::Null);
        assert_eq!(state.answers["d"], Value::Null);
    }

    #[test]
    fn invalid_answers_are_reported_and_asked_again() {
        let state = resolve(&questions, &raw(json!({"a": "z", "c": 0, "zz": 1})), false);
        assert_eq!(state.errors.len(), 2, "{:?}", state.errors);
        assert!(state.errors[0].message.contains("not one of x, y"));
        assert!(state.errors[1].message.contains("at least 1"));
        assert_eq!(state.unknown, ["zz"]);
        assert_eq!(state.batch().len(), 2);
    }

    #[test]
    fn defaults_fill_every_question() {
        let state = resolve(&questions, &BTreeMap::new(), true);
        assert!(state.done(), "{state:?}");
        assert_eq!(state.answers["a"], json!("x"));
        assert_eq!(state.answers["c"], json!(3));
    }

    #[test]
    fn kinds_normalize() {
        let q = Question::new("m", Kind::Multiselect, "M", "M?", "w").choices(vec![
            Choice::new("p", "Pee", ""),
            Choice::new("q", "Queue", ""),
        ]);
        assert_eq!(q.normalize(&json!("Pee, q")).unwrap(), json!(["p", "q"]));
        assert!(q.normalize(&json!(["r"])).is_err());
        // null skips an optional question, even one with a default, and
        // takes a required question's default.
        let limit = Question::new("n", Kind::Number, "N", "N?", "w")
            .optional()
            .default(5)
            .choices(vec![
                Choice::new(5, "$5", ""),
                Choice::new(Value::Null, "No limit", ""),
            ]);
        assert_eq!(limit.normalize(&Value::Null).unwrap(), Value::Null);
        assert_eq!(limit.normalize(&json!("No limit")).unwrap(), Value::Null);
        assert_eq!(limit.default_answer().unwrap(), json!(5));
        let required = Question::new("r", Kind::Number, "R", "R?", "w").default(8);
        assert_eq!(required.normalize(&Value::Null).unwrap(), json!(8));
        let q = Question::new("c", Kind::Confirm, "C", "C?", "w");
        assert_eq!(q.normalize(&json!("Yes")).unwrap(), json!(true));
        let q = Question::new("s", Kind::SecretRef, "S", "S?", "w")
            .rule(Rule::SecretRef)
            .optional();
        assert_eq!(q.normalize(&json!("skip")).unwrap(), Value::Null);
        let error = q.normalize(&json!("sk-ant-api03-SECRET")).unwrap_err();
        assert!(!error.contains("SECRET"), "{error}");
        let q = Question::new("u", Kind::Text, "U", "U?", "w").rule(Rule::PostgresUrl);
        assert!(q
            .normalize(&json!("postgres://u:pw@db/x"))
            .unwrap_err()
            .contains("password"));
        assert!(q.normalize(&json!("postgres://u@db/x")).is_ok());
    }

    #[test]
    fn choices_put_the_default_first_and_overflow() {
        let q = Question::new("h", Kind::Select, "H", "H?", "w")
            .default("e")
            .choices(
                ["a", "b", "c", "d", "e", "f"]
                    .iter()
                    .map(|v| Choice::new(*v, v.to_uppercase(), ""))
                    .collect(),
            );
        assert_eq!(q.choices.len(), 4);
        assert_eq!(q.choices[0].value, json!("e"));
        assert!(q.choices[0].recommended);
        assert_eq!(q.more_choices.len(), 2);
        assert_eq!(
            q.normalize(&json!("F")).unwrap(),
            json!("f"),
            "more_choices count"
        );
    }
}
