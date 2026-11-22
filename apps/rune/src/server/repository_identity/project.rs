use std::collections::HashSet;
use std::path::Path;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::digest::{Context, SHA256};
use serde_json::{Value, json};

use super::Identities;

pub(in crate::server) fn enrich(project: &mut Value, identities: &Identities) -> Option<String> {
    let root = project["workspaceRoot"].as_str()?.to_owned();
    let available = identities.get_available(Path::new(&root));
    project["repositoryIdentity"] = json!(available.identity);
    available.resolved.then_some(root)
}

pub(in crate::server) fn enrich_projects(
    projects: &mut [Value],
    identities: &Identities,
) -> Vec<String> {
    let mut seen = HashSet::new();
    projects
        .iter_mut()
        .filter_map(|project| enrich(project, identities))
        .filter(|root| seen.insert(root.clone()))
        .collect()
}

pub(in crate::server) fn digest_projects(projects: &[Value]) -> String {
    let mut hash = Context::new(&SHA256);
    for project in projects {
        let mut identity = project["repositoryIdentity"].clone();
        if let Some(fields) = identity.as_object_mut()
            && let Some(web_url) = fields.shift_remove("webUrl")
        {
            fields.insert("webUrl".into(), web_url);
        }
        hash.update(json!([project["id"], identity]).to_string().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(hash.finish())
}
