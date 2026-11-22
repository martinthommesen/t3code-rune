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
