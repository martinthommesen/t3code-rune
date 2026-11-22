use std::collections::{HashMap, HashSet, hash_map::Entry};

use rune::{Image, ImageCache};

use crate::connection::{Connection, FaviconBytes, FaviconTarget, Project, ProjectIcon};

#[derive(Default)]
pub(super) struct ProjectFavicons {
    targets: HashMap<String, FaviconTarget>,
    previews: HashSet<FaviconTarget>,
    images: HashMap<FaviconTarget, Favicon>,
}

enum Favicon {
    Loading,
    Decoding(Image),
    Ready(Image),
    Missing,
}

impl ProjectFavicons {
    pub(super) fn sync(&mut self, projects: &[Project], connection: &Connection) {
        self.targets = projects
            .iter()
            .filter(|project| matches!(project.icon, None | Some(ProjectIcon::Unknown)))
            .map(|project| {
                let target = FaviconTarget {
                    cwd: project.path.clone(),
                    path: project.favicon_path.clone().filter(|path| !path.is_empty()),
                };
                (project.id.clone(), target)
            })
            .collect();
        self.reconcile(connection);
    }

    pub(super) fn previews(&mut self, targets: Vec<FaviconTarget>, connection: &Connection) {
        self.previews = targets.into_iter().collect();
        self.reconcile(connection);
    }

    fn reconcile(&mut self, connection: &Connection) {
        let targets: HashSet<&FaviconTarget> =
            self.targets.values().chain(&self.previews).collect();
        self.images.retain(|target, _| targets.contains(target));
        connection.load_project_favicons(
            targets
                .into_iter()
                .filter_map(|target| match self.images.entry(target.clone()) {
                    Entry::Vacant(slot) => {
                        slot.insert(Favicon::Loading);
                        Some(target.clone())
                    }
                    Entry::Occupied(_) => None,
                })
                .collect(),
        );
    }

    pub(super) fn connection_lost(&mut self) {
        self.images
            .retain(|_, image| !matches!(image, Favicon::Loading));
    }

    pub(super) fn loaded(
        &mut self,
        target: FaviconTarget,
        image: Option<FaviconBytes>,
        cache: &ImageCache,
    ) {
        if let Some(slot) = self.images.get_mut(&target) {
            *slot = match image {
                Some(image) => {
                    Favicon::Decoding(cache.load_mime(image.bytes, &image.mime, 16.0, 2.0))
                }
                None => Favicon::Missing,
            };
        }
    }

    pub(super) fn poll(&mut self) -> bool {
        let mut changed = false;
        for slot in self.images.values_mut() {
            if let Favicon::Decoding(image) = slot
                && image.is_ready()
            {
                *slot = if image.size().is_some() {
                    Favicon::Ready(image.clone())
                } else {
                    Favicon::Missing
                };
                changed = true;
            }
        }
        changed
    }

    pub(super) fn image(&self, project: &Project) -> Option<&Image> {
        self.preview(self.targets.get(&project.id)?)
    }

    pub(super) fn preview(&self, target: &FaviconTarget) -> Option<&Image> {
        match self.images.get(target)? {
            Favicon::Ready(image) => Some(image),
            _ => None,
        }
    }
}
