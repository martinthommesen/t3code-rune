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
    Other(String),
}

impl From<&str> for FolderBrowseError {
    fn from(value: &str) -> Self {
        Self::Other(value.to_owned())
    }
}

impl super::RpcExit {
    pub(super) fn into_folder_result(self) -> Result<FolderListing, FolderBrowseError> {
        let missing = matches!(&self, Self::Failure { cause }
            if cause.iter().any(|reason| matches!(reason, super::RpcCause::Fail { error } if error.path_missing)));
        if missing {
            return Err(FolderBrowseError::Missing);
        }
        self.into_result()
            .map_err(FolderBrowseError::Other)
            .and_then(|value| {
                serde_json::from_value(value)
                    .map_err(|_| "The environment returned an invalid folder listing.".into())
            })
    }
}
