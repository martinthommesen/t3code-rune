use serde_json::{Map, Value, json};

use super::nonempty;

mod keys;

const STATES: &[&str] = &["open", "closed", "merged"];

pub(super) fn linked(raw: Option<&str>) -> Option<Value> {
    let raw: Value = serde_json::from_str(raw?).ok()?;
    decode_linked(&raw)
}

pub(super) fn decode_linked(raw: &Value) -> Option<Value> {
    Some(json!({
        "projectId": text(raw, "projectId")?,
        "repository": text(raw, "repository")?,
        "number": integer(raw.get("number")?, 1)?,
        "url": text(raw, "url")?,
    }))
}

pub(super) fn links(raw: &str, linked: Option<&Value>) -> Vec<Value> {
    let mut links: Vec<_> = serde_json::from_str::<Vec<Value>>(raw)
        .ok()
        .and_then(|values| values.iter().map(link).collect())
        .unwrap_or_default();
    if let Some(linked) = linked {
        let url = linked["url"]
            .as_str()
            .expect("validated linked pull request URL");
        let repository = linked["repository"].as_str().expect("validated repository");
        let number = linked["number"]
            .as_u64()
            .expect("validated pull request number");
        let (host, repository) = keys::legacy(repository, url, number);
        let key = keys::stored(&host, &repository, url, number);
        if !links.iter().any(|link| same_key(link, &key, number)) {
            links.push(json!({
                "host": host, "repository": repository, "number": number, "url": url,
                "source": "manual", "linkedAt": "1970-01-01T00:00:00.000Z",
                "snapshot": null, "stack": null,
            }));
        }
    }
    links
}

pub(super) fn decode_links(raw: &Value) -> Option<Value> {
    raw.as_array()?
        .iter()
        .map(link)
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn same_key(link: &Value, key: &(String, String), number: u64) -> bool {
    if link["number"].as_u64() != Some(number) {
        return false;
    }
    let link_host = link["host"].as_str().expect("validated pull request host");
    let link_repository = link["repository"].as_str().expect("validated repository");
    let url = link["url"].as_str().expect("validated pull request URL");
    keys::stored(link_host, link_repository, url, number) == *key
}

fn link(raw: &Value) -> Option<Value> {
    let mut value = json!({
        "host": text(raw, "host")?, "repository": text(raw, "repository")?,
        "number": integer(raw.get("number")?, 1)?, "url": text(raw, "url")?,
        "source": literal(raw.get("source")?, &["manual", "created", "agent", "stack", "stack-dismissed"])?,
        "linkedAt": raw.get("linkedAt")?.as_str()?,
        "snapshot": nullable(raw.get("snapshot")?, snapshot)?,
        "stack": nullable(raw.get("stack")?, stack)?,
    });
    let fields = value.as_object_mut().expect("pull request fields");
    optional(raw, fields, "repositoryWebUrl", nonblank)?;
    optional(raw, fields, "watch", watch)?;
    optional(raw, fields, "endedWatch", watch)?;
    Some(value)
}

fn watch(raw: &Value) -> Option<Value> {
    let mut fields = Map::from_iter([
        ("startedAt".to_owned(), string(raw.get("startedAt")?)?),
        (
            "headSha".to_owned(),
            nullable(raw.get("headSha")?, nonblank)?,
        ),
        (
            "failedChecks".to_owned(),
            nonblank_array(raw.get("failedChecks")?)?,
        ),
        ("passed".to_owned(), boolean(raw.get("passed")?)?),
        (
            "passedChecks".to_owned(),
            match raw.get("passedChecks") {
                Some(value) => nonblank_array(value)?,
                None => Value::Array(Vec::new()),
            },
        ),
        (
            "remarksThrough".to_owned(),
            string(raw.get("remarksThrough")?)?,
        ),
        (
            "remarkIds".to_owned(),
            nonblank_array(raw.get("remarkIds")?)?,
        ),
        ("conflicting".to_owned(), boolean(raw.get("conflicting")?)?),
        ("wakes".to_owned(), integer(raw.get("wakes")?, 0)?.into()),
    ]);
    for name in ["owedChecks", "awaitingApproval"] {
        optional(raw, &mut fields, name, nonblank_array)?;
    }
    for name in ["approvalAnnounced", "rebaseRequired", "checksPending"] {
        optional(raw, &mut fields, name, boolean)?;
    }
    optional(raw, &mut fields, "mergeQueueRemovalId", |v| {
        nullable(v, nonblank)
    })?;
    optional(raw, &mut fields, "checksAwaitedSince", |v| {
        nullable(v, string)
    })?;
    optional(raw, &mut fields, "awaitingBaseline", |v| {
        literal(v, &["all", "checks"])
    })?;
    optional(raw, &mut fields, "until", |v| literal(v, &["merged"]))?;
    Some(Value::Object(fields))
}

fn snapshot(raw: &Value) -> Option<Value> {
    let mut fields = Map::from_iter([
        ("state".to_owned(), literal(raw.get("state")?, STATES)?),
        ("title".to_owned(), text(raw, "title")?.into()),
        ("headBranch".to_owned(), text(raw, "headBranch")?.into()),
        ("baseBranch".to_owned(), text(raw, "baseBranch")?.into()),
        ("isDraft".to_owned(), raw.get("isDraft")?.as_bool()?.into()),
        (
            "updatedAt".to_owned(),
            nullable(raw.get("updatedAt")?, string)?,
        ),
        ("syncedAt".to_owned(), string(raw.get("syncedAt")?)?),
    ]);
    for name in ["closedAt", "mergedAt"] {
        optional(raw, &mut fields, name, |value| nullable(value, string))?;
    }
    for name in ["additions", "deletions", "changedFiles"] {
        optional(raw, &mut fields, name, |value| {
            integer(value, 0).map(Value::from)
        })?;
    }
    optional(raw, &mut fields, "author", |value| nullable(value, actor))?;
    optional(raw, &mut fields, "reviewDecision", |value| {
        nullable(value, |value| {
            literal(value, &["approved", "changes-requested", "review-required"])
        })
    })?;
    optional(raw, &mut fields, "checksState", |value| {
        nullable(value, |value| {
            literal(value, &["passing", "failing", "pending"])
        })
    })?;
    optional(raw, &mut fields, "mergeability", |value| {
        literal(value, &["mergeable", "conflicting", "unknown"])
    })?;
    optional(raw, &mut fields, "rebaseRequired", boolean)?;
    Some(Value::Object(fields))
}

fn actor(raw: &Value) -> Option<Value> {
    let mut fields = Map::from_iter([
        ("login".to_owned(), text(raw, "login")?.into()),
        ("name".to_owned(), nullable(raw.get("name")?, string)?),
        (
            "avatarUrl".to_owned(),
            nullable(raw.get("avatarUrl")?, string)?,
        ),
    ]);
    optional(raw, &mut fields, "isBot", boolean)?;
    Some(Value::Object(fields))
}

fn stack(raw: &Value) -> Option<Value> {
    let layers = raw
        .get("layers")?
        .as_array()?
        .iter()
        .map(|layer| {
            Some(json!({
                "number": integer(layer.get("number")?, 1)?,
                "headBranch": text(layer, "headBranch")?,
                "state": literal(layer.get("state")?, STATES)?,
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(json!({
        "kind": literal(raw.get("kind")?, &["native"])?,
        "id": text(raw, "id")?, "number": integer(raw.get("number")?, 1)?,
        "url": text(raw, "url")?, "base": text(raw, "base")?, "layers": layers,
    }))
}

fn text<'a>(raw: &'a Value, name: &str) -> Option<&'a str> {
    nonempty(raw.get(name)?.as_str())
}

fn nonblank(raw: &Value) -> Option<Value> {
    nonempty(raw.as_str()).map(Value::from)
}

fn nonblank_array(raw: &Value) -> Option<Value> {
    raw.as_array()?
        .iter()
        .map(nonblank)
        .collect::<Option<Vec<_>>>()
        .map(Value::Array)
}

fn string(raw: &Value) -> Option<Value> {
    raw.as_str().map(Value::from)
}

fn boolean(raw: &Value) -> Option<Value> {
    raw.as_bool().map(Value::from)
}

fn integer(raw: &Value, minimum: u64) -> Option<u64> {
    let number = raw.as_f64()?;
    (number.fract() == 0.0 && number >= minimum as f64 && number <= 9_007_199_254_740_991.0)
        .then_some(number as u64)
}

fn literal(raw: &Value, values: &[&str]) -> Option<Value> {
    let value = raw.as_str()?;
    values.contains(&value).then(|| value.into())
}

fn nullable(raw: &Value, decode: impl FnOnce(&Value) -> Option<Value>) -> Option<Value> {
    if raw.is_null() {
        Some(Value::Null)
    } else {
        decode(raw)
    }
}

fn optional(
    raw: &Value,
    fields: &mut Map<String, Value>,
    name: &str,
    decode: impl FnOnce(&Value) -> Option<Value>,
) -> Option<()> {
    if let Some(value) = raw.get(name) {
        fields.insert(name.to_owned(), decode(value)?);
    }
    Some(())
}
