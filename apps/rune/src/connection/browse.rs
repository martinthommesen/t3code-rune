use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderListing {
    pub(crate) parent_path: String,
    pub(crate) entries: Vec<FolderEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderEntry {
    pub(crate) name: String,
    pub(crate) full_path: String,
}

#[derive(Debug)]
pub(crate) enum FolderBrowseError {
    Missing,
    NotDirectory(String),
    Other(String),
}

impl From<&str> for FolderBrowseError {
    fn from(value: &str) -> Self {
        Self::Other(value.to_owned())
    }
}

impl super::RpcExit {
    pub(super) fn is_project_folder_collision(&self) -> bool {
        matches!(self, Self::Failure { cause } if cause.iter().any(|reason| {
            let super::RpcCause::Fail { error } = reason else { return false; };
            let cause = &error.defect["cause"];
            error.defect["name"] == "ProjectOperationError"
                && (cause["name"] == "WorkspaceRootNotDirectoryError"
                    || cause["name"] == "WorkspaceRootStatFailedError"
                        && is_not_directory(&cause["cause"]["cause"]))
        }))
    }

    pub(super) fn into_folder_result(self) -> Result<FolderListing, FolderBrowseError> {
        let missing = matches!(&self, Self::Failure { cause }
            if cause.iter().any(|reason| matches!(reason, super::RpcCause::Fail { error } if error.path_missing)));
        if missing {
            return Err(FolderBrowseError::Missing);
        }
        let not_directory = matches!(&self, Self::Failure { cause } if cause.iter().any(|reason| {
            let super::RpcCause::Fail { error } = reason else { return false; };
            error.defect["name"] == "WorkspaceEntriesReadDirectoryError"
                && is_not_directory(&error.defect["cause"])
        }));
        self.into_result()
            .map_err(if not_directory {
                FolderBrowseError::NotDirectory
            } else {
                FolderBrowseError::Other
            })
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|_| "The environment returned an invalid folder listing.".into())
            })
    }
}

fn is_not_directory(cause: &serde_json::Value) -> bool {
    cause["name"] == "Error"
        && cause["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("ENOTDIR:"))
}
