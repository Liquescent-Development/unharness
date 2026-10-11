//! `elicitation/create` in form mode (ACP v1): the agent asks the user to
//! fill in a form, shown as questions (`PermissionKind::Question`, the TUI's
//! question modal). unharness declares `elicitation: {form: {}}` and not
//! `url`, so a URL elicitation is refused.
//!
//! Each property of `requestedSchema` is one question, in the order the
//! agent wrote them: a string with `oneOf` or `enum` a single choice, an
//! array whose `items` have `anyOf` or `enum` a multiple choice, a plain
//! string, a number or an integer a typed answer, a boolean Yes or No. A
//! form with no property is a confirmation. A choice takes a typed answer
//! where the agent says it does: claude-agent-acp 0.85.1 puts one beside
//! each question as `<key>_custom` (its "Other"), and vibe-acp 2.26.1's
//! `ask_user_question` call says `hideOther: false` and takes a value
//! outside the options (read in its source, and recorded).
//!
//! The answer is `{"action": "accept", "content": {key: value}}`; a
//! dismissed question (Esc, or `--print`, which has nobody to ask) is
//! `decline`, and a question still open when its turn is cancelled is
//! `cancel`.

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use crate::core::{PermissionDecision, PermissionKind, Question, QuestionOption};

/// What a field takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    /// One of its choices, or several.
    Choice {
        multi: bool,
    },
    Text,
    Number {
        integer: bool,
    },
    Boolean,
}

/// One choice of a choice field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What is sent (`const`, or the `enum` value).
    pub value: String,
    /// What is shown (`title`, else the value).
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    pub kind: FieldKind,
    pub choices: Vec<Choice>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The property a typed answer to this choice goes to instead
    /// (claude-agent-acp's `<key>_custom`).
    pub other_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub message: String,
    /// The tool call the agent ties the form to, as it names it.
    pub tool_call_id: Option<String>,
    pub fields: Vec<Field>,
}

/// Why an elicitation is not shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not a form (`url`, or a mode the spec does not have yet): answered
    /// with an error, since unharness never declared it.
    NotForm(String),
    /// A form with a field no question can stand for: declined.
    Unsupported(String),
}

impl Form {
    /// Read the request (`line` is the whole message, from which the order
    /// of the properties is taken: `params` has them sorted by name).
    pub fn from_request(line: &str, params: &Value) -> Result<Form, Refusal> {
        let mode = params.get("mode").and_then(Value::as_str).unwrap_or("");
        if mode != "form" {
            return Err(Refusal::NotForm(format!(
                "a {} elicitation, which unharness does not take",
                if mode.is_empty() { "modeless" } else { mode }
            )));
        }
        let schema = params.get("requestedSchema").unwrap_or(&Value::Null);
        let empty = Map::new();
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let mut keys = property_order(line);
        keys.retain(|k| properties.contains_key(k));
        if keys.len() != properties.len() {
            keys = properties.keys().cloned().collect();
            keys.sort_by_key(|k| natural(k));
        }
        let mut fields: Vec<Field> = Vec::new();
        for key in &keys {
            let prop = &properties[key];
            let kind = s(prop, "type");
            // claude-agent-acp 0.85.1: the free answer to `question_<n>`.
            if kind == "string"
                && let Some(base) = key.strip_suffix("_custom")
                && let Some(field) = fields.iter_mut().find(|f| f.key == base)
                && matches!(field.kind, FieldKind::Choice { .. })
                && field.other_key.is_none()
            {
                field.other_key = Some(key.clone());
                continue;
            }
            fields.push(field(key, prop)?);
        }
        Ok(Form {
            message: s(params, "message").to_string(),
            tool_call_id: params
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_string),
            fields,
        })
    }

    /// Whether `input` (a tool call's `rawInput`) holds these questions:
    /// as many, each with the field's choices and its text or header.
    pub fn asked_by(&self, input: &Value) -> bool {
        let Some(questions) = input.get("questions").and_then(Value::as_array) else {
            return false;
        };
        questions.len() == self.fields.len()
            && questions.iter().zip(&self.fields).all(|(q, f)| {
                let labels: Vec<&str> = q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|os| os.iter().map(|o| s(o, "label")).collect())
                    .unwrap_or_default();
                let values: Vec<&str> = f.choices.iter().map(|c| c.value.as_str()).collect();
                labels == values
                    && (f.description.as_deref() == Some(s(q, "question"))
                        || f.title.as_deref() == Some(s(q, "header")))
            })
    }

    /// What the user is asked. `input` is the `rawInput` of the tool call
    /// that asks (`asked_by`), when known: it gives the options their
    /// descriptions where the schema has none, and says whether a choice
    /// takes a typed answer (`hideOther`).
    pub fn kind(&self, input: Option<&Value>) -> PermissionKind {
        if self.fields.is_empty() {
            return PermissionKind::Confirm {
                title: self.message.clone(),
                message: None,
            };
        }
        let asked = input
            .filter(|i| self.asked_by(i))
            .and_then(|i| i.get("questions"))
            .and_then(Value::as_array);
        let questions = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let raw = asked.and_then(|qs| qs.get(i));
                let described = |c: &Choice| {
                    if !c.description.is_empty() {
                        return c.description.clone();
                    }
                    raw.and_then(|q| q.get("options"))
                        .and_then(Value::as_array)
                        .and_then(|os| os.iter().find(|o| s(o, "label") == c.value))
                        .map(|o| s(o, "description").to_string())
                        .unwrap_or_default()
                };
                let options = match f.kind {
                    FieldKind::Boolean => {
                        vec![
                            QuestionOption::new("Yes", ""),
                            QuestionOption::new("No", ""),
                        ]
                    }
                    _ => f
                        .choices
                        .iter()
                        .map(|c| QuestionOption::new(c.label.clone(), described(c)))
                        .collect(),
                };
                let hide_other = raw
                    .and_then(|q| q.get("hideOther"))
                    .and_then(Value::as_bool);
                let allow_other = match f.kind {
                    FieldKind::Choice { .. } => f.other_key.is_some() || hide_other == Some(false),
                    FieldKind::Boolean => false,
                    FieldKind::Text | FieldKind::Number { .. } => true,
                };
                let text = f
                    .description
                    .clone()
                    .filter(|d| !d.is_empty())
                    .or_else(|| (self.fields.len() == 1).then(|| self.message.clone()))
                    .filter(|t| !t.is_empty())
                    .or_else(|| f.title.clone())
                    .unwrap_or_else(|| f.key.clone());
                Question {
                    id: f.key.clone(),
                    header: f.title.clone().unwrap_or_else(|| f.key.clone()),
                    text,
                    options,
                    allow_other,
                    multi: f.kind == FieldKind::Choice { multi: true },
                }
            })
            .collect();
        PermissionKind::Question { questions }
    }

    /// The answer to the elicitation for the user's decision: their
    /// answers accepted, anything else declined.
    pub fn answer(&self, decision: &PermissionDecision) -> Value {
        match decision {
            PermissionDecision::Answer(Value::Object(answers)) => {
                json!({"action": "accept", "content": self.content(answers)})
            }
            // A confirmation (a form with nothing to fill in).
            PermissionDecision::Answer(Value::Bool(true)) if self.fields.is_empty() => {
                json!({"action": "accept", "content": {}})
            }
            _ => declined(),
        }
    }

    /// `{key: value}` from the question modal's `{question id: label |
    /// [labels] | typed text}`. An answer left empty is left out.
    fn content(&self, answers: &Map<String, Value>) -> Value {
        let mut content = Map::new();
        for f in &self.fields {
            let Some(answer) = answers.get(&f.key) else {
                continue;
            };
            let value_of = |label: &str| {
                f.choices
                    .iter()
                    .find(|c| c.label == label)
                    .map(|c| c.value.clone())
            };
            match (&f.kind, answer) {
                (_, Value::String(t)) if t.trim().is_empty() => {}
                (FieldKind::Choice { multi }, Value::String(t)) => match value_of(t) {
                    Some(v) if *multi => {
                        content.insert(f.key.clone(), json!([v]));
                    }
                    Some(v) => {
                        content.insert(f.key.clone(), json!(v));
                    }
                    // Typed: to its own property, else as the choice.
                    None => match &f.other_key {
                        Some(other) => {
                            content.insert(other.clone(), json!(t));
                        }
                        None if *multi => {
                            content.insert(f.key.clone(), json!([t]));
                        }
                        None => {
                            content.insert(f.key.clone(), json!(t));
                        }
                    },
                },
                (FieldKind::Choice { .. }, Value::Array(labels)) if !labels.is_empty() => {
                    let values: Vec<String> = labels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|l| value_of(l).unwrap_or_else(|| l.to_string()))
                        .collect();
                    content.insert(f.key.clone(), json!(values));
                }
                (FieldKind::Boolean, Value::String(t)) => {
                    content.insert(f.key.clone(), json!(t == "Yes"));
                }
                (FieldKind::Number { integer }, Value::String(t)) => {
                    let t = t.trim();
                    // One that does not read as a number goes as typed, for
                    // the agent to refuse.
                    let v = if *integer {
                        t.parse::<i64>().map(Value::from).ok()
                    } else {
                        t.parse::<f64>()
                            .ok()
                            .and_then(serde_json::Number::from_f64)
                            .map(Value::Number)
                    };
                    content.insert(f.key.clone(), v.unwrap_or_else(|| json!(t)));
                }
                (FieldKind::Text, Value::String(t)) => {
                    content.insert(f.key.clone(), json!(t));
                }
                _ => {}
            }
        }
        Value::Object(content)
    }
}

/// The user would not answer (or nobody could be asked).
pub fn declined() -> Value {
    json!({"action": "decline"})
}

/// The answer to an elicitation still open when its turn is cancelled.
pub fn cancelled() -> Value {
    json!({"action": "cancel"})
}

fn field(key: &str, prop: &Value) -> Result<Field, Refusal> {
    let text = |k: &str| {
        prop.get(k)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let unsupported = |what: &str| {
        Err(Refusal::Unsupported(format!(
            "its field '{key}' is {what}, which unharness cannot ask for"
        )))
    };
    let (kind, choices) = match s(prop, "type") {
        "string" => match choices(prop) {
            Some(choices) => (FieldKind::Choice { multi: false }, choices),
            None => (FieldKind::Text, Vec::new()),
        },
        "array" => match prop.get("items").and_then(choices) {
            Some(choices) => (FieldKind::Choice { multi: true }, choices),
            None => return unsupported("a list without choices"),
        },
        "number" => (FieldKind::Number { integer: false }, Vec::new()),
        "integer" => (FieldKind::Number { integer: true }, Vec::new()),
        "boolean" => (FieldKind::Boolean, Vec::new()),
        other => return unsupported(&format!("of type '{other}'")),
    };
    if matches!(kind, FieldKind::Choice { .. }) && choices.is_empty() {
        return unsupported("a choice with no options");
    }
    Ok(Field {
        key: key.to_string(),
        kind,
        choices,
        title: text("title"),
        description: text("description"),
        other_key: None,
    })
}

/// `oneOf` / `anyOf` (titled) or `enum` choices of a property or its items.
fn choices(schema: &Value) -> Option<Vec<Choice>> {
    if let Some(titled) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
    {
        return Some(
            titled
                .iter()
                .filter_map(|o| {
                    let value = o.get("const")?.as_str()?.to_string();
                    let label = match s(o, "title") {
                        "" => value.clone(),
                        t => t.to_string(),
                    };
                    Some(Choice {
                        value,
                        label,
                        description: s(o, "description").to_string(),
                    })
                })
                .collect(),
        );
    }
    let values = schema.get("enum")?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(|v| Choice {
                value: v.to_string(),
                label: v.to_string(),
                description: String::new(),
            })
            .collect(),
    )
}

/// The names under `params.requestedSchema.properties` in the order the
/// line has them; empty when the line does not have that shape.
fn property_order(line: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Message {
        params: Params,
    }
    #[derive(Deserialize)]
    struct Params {
        #[serde(rename = "requestedSchema")]
        schema: Schema,
    }
    #[derive(Deserialize)]
    struct Schema {
        properties: Keys,
    }
    struct Keys(Vec<String>);
    impl<'de> Deserialize<'de> for Keys {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Keys;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some((k, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? {
                        keys.push(k);
                    }
                    Ok(Keys(keys))
                }
            }
            d.deserialize_map(V)
        }
    }
    serde_json::from_str::<Message>(line)
        .map(|m| m.params.schema.properties.0)
        .unwrap_or_default()
}

/// Sort key that puts `q2` before `q10`.
fn natural(key: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut n = c.to_digit(10).unwrap_or(0) as u64;
            while let Some(d) = chars.peek().and_then(|d| d.to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(d as u64);
                chars.next();
            }
            parts.push((std::mem::take(&mut text), n));
        } else {
            text.push(c);
        }
    }
    parts.push((text, 0));
    parts
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: Value) -> (String, Value) {
        let line = json!({"jsonrpc": "2.0", "id": 0, "method": "elicitation/create",
            "params": params})
        .to_string();
        (line, params)
    }

    fn form(params: Value) -> Form {
        let (line, params) = request(params);
        Form::from_request(&line, &params).unwrap()
    }

    fn questions(kind: PermissionKind) -> Vec<Question> {
        match kind {
            PermissionKind::Question { questions } => questions,
            other => panic!("{other:?}"),
        }
    }

    /// vibe-acp 2.26.1 (`fixtures/vibe_acp_question.jsonl`): one property
    /// per question, all required; the option descriptions only in the
    /// tool call's `rawInput`.
    fn vibe() -> (Form, Value) {
        let f = form(json!({"sessionId": "s", "toolCallId": "chatcmpl-tool-1",
            "message": "User input required", "mode": "form",
            "requestedSchema": {"properties": {
                "q0": {"title": "Drink", "description": "Tea or coffee?", "type": "string",
                    "oneOf": [{"const": "Tea", "title": "Tea"}, {"const": "Coffee", "title": "Coffee"}]},
                "q1": {"title": "Colors", "description": "Which colours?", "type": "array",
                    "items": {"anyOf": [{"const": "Red", "title": "Red"}, {"const": "Blue", "title": "Blue"}]}}},
                "required": ["q0", "q1"]}}));
        let input = json!({"questions": [
            {"question": "Tea or coffee?", "header": "Drink", "multiSelect": false, "hideOther": false,
             "options": [{"label": "Tea", "description": "leaves"}, {"label": "Coffee", "description": "beans"}]},
            {"question": "Which colours?", "header": "Colors", "multiSelect": true, "hideOther": false,
             "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]}]});
        (f, input)
    }

    #[test]
    fn vibe_questions_take_descriptions_and_free_answers_from_the_call() {
        let (f, input) = vibe();
        assert!(f.asked_by(&input));
        let qs = questions(f.kind(Some(&input)));
        assert_eq!(qs.len(), 2);
        assert_eq!(
            (
                qs[0].id.as_str(),
                qs[0].header.as_str(),
                qs[0].text.as_str()
            ),
            ("q0", "Drink", "Tea or coffee?")
        );
        assert_eq!(qs[0].options[1], QuestionOption::new("Coffee", "beans"));
        assert!(!qs[0].multi && qs[0].allow_other);
        assert!(qs[1].multi && qs[1].allow_other);
        // Untied, the schema alone: no descriptions, no typed answer.
        let qs = questions(f.kind(None));
        assert_eq!(qs[0].options[1], QuestionOption::new("Coffee", ""));
        assert!(!qs[0].allow_other && !qs[1].allow_other);
        // Another call's questions are not these.
        let mut other = input.clone();
        other["questions"][0]["options"][0]["label"] = json!("Water");
        assert!(!f.asked_by(&other));
        assert!(!questions(f.kind(Some(&other)))[0].allow_other);
    }

    #[test]
    fn answers_are_accepted_with_the_schemas_values() {
        let (f, _) = vibe();
        let answer = |a: Value| f.answer(&PermissionDecision::Answer(a));
        assert_eq!(
            answer(json!({"q0": "Coffee", "q1": ["Red", "Blue"]})),
            json!({"action": "accept", "content": {"q0": "Coffee", "q1": ["Red", "Blue"]}})
        );
        // A typed answer: as it is for a single choice, a list of one for
        // a multiple one.
        assert_eq!(
            answer(json!({"q0": "Milk", "q1": "Teal"})),
            json!({"action": "accept", "content": {"q0": "Milk", "q1": ["Teal"]}})
        );
        // Esc, and `--print`, which has nobody to ask.
        assert_eq!(answer(Value::Null), json!({"action": "decline"}));
        assert_eq!(
            f.answer(&PermissionDecision::Deny {
                reason: "no".into()
            }),
            json!({"action": "decline"})
        );
    }

    /// claude-agent-acp 0.85.1 (`fixtures/claude_agent_acp_question.jsonl`):
    /// the question in `message`, the option descriptions in the schema,
    /// nothing required, and an "Other" property beside each question.
    #[test]
    fn claude_agent_acps_other_property_is_the_choices_typed_answer() {
        let f = form(
            json!({"mode": "form", "sessionId": "s", "toolCallId": "toolu_1",
            "message": "Which colours?",
            "requestedSchema": {"type": "object", "properties": {
                "question_0": {"type": "array", "title": "Colours", "items": {"anyOf": [
                    {"const": "Red", "title": "Red", "description": "Warm"},
                    {"const": "Blue", "title": "Blue", "description": "Cool"}]}},
                "question_0_custom": {"type": "string", "title": "Other",
                    "description": "Type your own answer (optional)."}}}}),
        );
        assert_eq!(f.fields.len(), 1);
        assert_eq!(f.fields[0].other_key.as_deref(), Some("question_0_custom"));
        let qs = questions(f.kind(None));
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].text, "Which colours?");
        assert_eq!(qs[0].options[0], QuestionOption::new("Red", "Warm"));
        assert!(qs[0].multi && qs[0].allow_other);
        let answer = |a: Value| f.answer(&PermissionDecision::Answer(a));
        assert_eq!(
            answer(json!({"question_0": ["Blue"]})),
            json!({"action": "accept", "content": {"question_0": ["Blue"]}})
        );
        assert_eq!(
            answer(json!({"question_0": "Teal"})),
            json!({"action": "accept", "content": {"question_0_custom": "Teal"}})
        );
    }

    #[test]
    fn properties_keep_the_order_the_agent_wrote() {
        let params = json!({"mode": "form", "message": "m", "requestedSchema": {"properties": {
            "zeta": {"type": "string"}, "alpha": {"type": "boolean"}, "mid": {"type": "integer"}}}});
        // `json!` sorts its keys: write the line by hand.
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"elicitation/create","params":{"mode":"form","message":"m","requestedSchema":{"properties":{"zeta":{"type":"string"},"alpha":{"type":"boolean"},"mid":{"type":"integer"}}}}}"#;
        let f = Form::from_request(line, &params).unwrap();
        let keys: Vec<&str> = f.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
        // Without the line, names with numbers sort as numbers.
        let params = json!({"mode": "form", "message": "m", "requestedSchema": {"properties": {
            "q10": {"type": "string"}, "q2": {"type": "string"}, "q1": {"type": "string"}}}});
        let f = Form::from_request("", &params).unwrap();
        let keys: Vec<&str> = f.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["q1", "q2", "q10"]);
    }

    #[test]
    fn typed_fields_and_confirmations() {
        let f = form(
            json!({"mode": "form", "message": "Details", "requestedSchema": {"properties": {
            "name": {"type": "string", "title": "Name"},
            "age": {"type": "integer"},
            "ratio": {"type": "number"},
            "ok": {"type": "boolean", "description": "Agree?"},
            "size": {"type": "string", "enum": ["S", "M"]}}}}),
        );
        let qs = questions(f.kind(None));
        let by_id = |id: &str| qs.iter().find(|q| q.id == id).unwrap().clone();
        assert!(by_id("name").options.is_empty() && by_id("name").allow_other);
        assert_eq!(by_id("ok").options.len(), 2);
        assert!(!by_id("ok").allow_other);
        assert_eq!(by_id("size").options[1].label, "M");
        assert_eq!(
            f.answer(&PermissionDecision::Answer(json!({
                "name": "Ann", "age": "42", "ratio": "0.5", "ok": "No", "size": "S"}))),
            json!({"action": "accept", "content": {
                "name": "Ann", "age": 42, "ratio": 0.5, "ok": false, "size": "S"}})
        );
        // An empty answer is left out; a number that is not one goes as typed.
        assert_eq!(
            f.answer(&PermissionDecision::Answer(
                json!({"name": " ", "age": "x"})
            ))["content"],
            json!({"age": "x"})
        );

        let confirm = form(json!({"mode": "form", "message": "Go on?",
            "requestedSchema": {"type": "object", "properties": {}}}));
        assert_eq!(
            confirm.kind(None),
            PermissionKind::Confirm {
                title: "Go on?".into(),
                message: None
            }
        );
        assert_eq!(
            confirm.answer(&PermissionDecision::Answer(json!(true))),
            json!({"action": "accept", "content": {}})
        );
        assert_eq!(
            confirm.answer(&PermissionDecision::Answer(json!(false))),
            json!({"action": "decline"})
        );
    }

    #[test]
    fn what_cannot_be_asked_is_refused() {
        let (line, params) = request(json!({"mode": "url", "sessionId": "s",
            "elicitationId": "e", "url": "https://example.com", "message": "Sign in"}));
        assert!(matches!(
            Form::from_request(&line, &params),
            Err(Refusal::NotForm(_))
        ));
        for prop in [
            json!({"type": "object"}),
            json!({"type": "array", "items": {"type": "string"}}),
            json!({"type": "string", "oneOf": []}),
        ] {
            let (line, params) = request(json!({"mode": "form", "message": "m",
                "requestedSchema": {"properties": {"x": prop}}}));
            assert!(
                matches!(
                    Form::from_request(&line, &params),
                    Err(Refusal::Unsupported(_))
                ),
                "{prop}"
            );
        }
    }
}
