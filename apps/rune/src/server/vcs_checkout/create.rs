use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{GitCommandError, checkout_lease, git, idle, switch_in_checkout, trim_js};

const SERVICE: &str = "GitWorkflowService.createRef";
const CREATE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CreateRefInput {
    pub(in crate::server) cwd: String,
    ref_name: String,
    #[serde(default, deserialize_with = "optional_switch")]
    pub(in crate::server) switch_ref: bool,
}

fn optional_switch<'de, D: Deserializer<'de>>(input: D) -> Result<bool, D::Error> {
    Option::<bool>::deserialize(input).map(Option::unwrap_or_default)
}

impl CreateRefInput {
    pub(in crate::server) fn parse(payload: Value) -> Result<Self, String> {
        let mut input: Self = serde_json::from_value(payload)
            .map_err(|_| "A Git working directory and reference name are required.".to_owned())?;
        input.cwd = trim_js(&input.cwd).to_owned();
        input.ref_name = trim_js(&input.ref_name).to_owned();
        if input.cwd.is_empty() || input.ref_name.is_empty() {
            return Err("A Git working directory and reference name are required.".into());
        }
        Ok(input)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct CreateRefResult {
    ref_name: String,
}

pub(in crate::server) async fn create_ref(
    database: &Path,
    input: CreateRefInput,
) -> Result<CreateRefResult, GitCommandError> {
    let database = database.to_owned();
    let cwd = input.cwd.clone();
    tokio::spawn(async move { create(&database, input).await })
        .await
        .map_err(|_| {
            GitCommandError::new(SERVICE, "git branch", &cwd, "Branch creation task stopped.")
        })?
}

async fn create(
    database: &Path,
    input: CreateRefInput,
) -> Result<CreateRefResult, GitCommandError> {
    let cwd = input.cwd.as_str();
    let root = git::repository_root(Path::new(cwd))
        .await
        .ok_or_else(|| GitCommandError::not_a_repository(SERVICE, cwd))?;
    let _checkout = if input.switch_ref {
        let checkout = checkout_lease(&root).await;
        idle::assert_idle(SERVICE, database, cwd, &root).await?;
        Some(checkout)
    } else {
        None
    };

    {
        let git = git::Git::new(cwd);
        let _config = git.config_lease().await;
        git.checked_with_timeout(
            "GitVcsDriver.createRef",
            &["branch", "--", &input.ref_name],
            Some("git branch create failed"),
            CREATE_TIMEOUT,
        )
        .await?;
    }
    if input.switch_ref {
        switch_in_checkout(cwd, &input.ref_name).await?;
    }
    Ok(CreateRefResult {
        ref_name: input.ref_name,
    })
}
