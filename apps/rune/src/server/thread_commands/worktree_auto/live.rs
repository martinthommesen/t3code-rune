use std::collections::{BTreeMap, HashMap, HashSet};

use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::server::store;

use super::*;

const SESSION_CHECKOUTS: &str = "
    SELECT json_extract(payload_json, '$.cwd') FROM orchestration_v2_projection_provider_sessions
    WHERE status <> 'stopped' AND json_valid(payload_json) AND json_type(payload_json, '$.cwd') = 'text'";
const PENDING_DELETION_CHECKOUTS: &str = "
    SELECT json_extract(thread.payload_json, '$.worktreePath')
    FROM orchestration_v2_projection_threads AS thread
    WHERE thread.deleted_at IS NOT NULL AND json_type(thread.payload_json, '$.worktreePath') = 'text'
      AND EXISTS (SELECT 1 FROM orchestration_v2_effect_outbox AS effect
                  WHERE effect.thread_id = thread.thread_id AND effect.status NOT IN ('succeeded', 'cancelled'))";

#[derive(Clone, PartialEq, Eq)]
struct Plan {
    candidate: Candidate,
    reason: Result<String, String>,
    activity: OffsetDateTime,
}

#[derive(Clone, Copy)]
struct Rules {
    inactive_days: Option<i64>,
    settled_days: Option<i64>,
    keep: KeepWhen,
}

impl Rules {
    fn resolve(settings: &Value, project: &str) -> Self {
        let environment = &settings["worktreeCleanup"];
        let policy = settings["projectSettingsOverrides"][project]
            .get("worktreeCleanup")
            .unwrap_or(environment);
        let rules = match policy["mode"].as_str() {
            Some("off") => &Value::Null,
            Some("custom") => &policy["rules"],
            _ => &settings["storageCleanup"],
        };
        Self {
            inactive_days: rules["worktreeAfterDays"]
                .as_i64()
                .filter(|days| (1..=3650).contains(days)),
            settled_days: rules["worktreeSettledAfterDays"]
                .as_i64()
                .filter(|days| (0..=3650).contains(days)),
            keep: match rules["worktreeKeepWhen"].as_str() {
                Some("any-local-files") => KeepWhen::AnyLocalFiles,
                Some("tracked-changes") => KeepWhen::TrackedChanges,
                _ => KeepWhen::UncommittedChanges,
            },
        }
    }

    fn enabled(self) -> bool {
        self.inactive_days.is_some() || self.settled_days.is_some()
    }

    fn eligibility(
        self,
        now: OffsetDateTime,
        activity: OffsetDateTime,
        settled: Option<OffsetDateTime>,
    ) -> Result<String, String> {
        let idle = now - activity;
        if self
            .inactive_days
            .is_some_and(|days| idle > Duration::days(days))
        {
            return Ok(format!("Inactive for {} days", days_since(idle)));
        }
        let settled = settled.map(|when| now - when.max(activity));
        if let (Some(elapsed), Some(days)) = (settled, self.settled_days)
            && elapsed >= Duration::days(days)
        {
            return Ok(format!("Settled for {} days", days_since(elapsed)));
        }
        if let Some(days) = self.inactive_days {
            return Err(format!("Inactive for {} of {days} days", days_since(idle)));
        }
        match (settled, self.settled_days) {
            (Some(elapsed), Some(days)) => Err(format!(
                "Settled for {} of {days} days",
                days_since(elapsed)
            )),
            _ => Err("Not settled".to_owned()),
        }
    }
}

pub(super) async fn sweep(
    commands: &ThreadCommands,
    settings: &SettingsFile,
    terminals: &Arc<TerminalManager>,
    default_root: &Path,
) -> rusqlite::Result<Vec<Entry>> {
    let now = OffsetDateTime::now_utc();
    let plans = check_plans(commands, settings, default_root, now).await?;
    let mut entries = Vec::with_capacity(plans.len());
    for plan in plans {
        let (outcome, reason, bytes) = match &plan.reason {
            Err(reason) => (Outcome::Kept, reason.clone(), None),
            Ok(reason) => {
                match remove(&plan, commands, settings, terminals, default_root, now).await {
                    Ok(bytes) => (Outcome::Removed, reason.clone(), Some(bytes)),
                    Err(AutomaticInspectionError::Kept(reason)) => (Outcome::Kept, reason, None),
                    Err(AutomaticInspectionError::Failed(reason)) => {
                        (Outcome::Failed, reason, None)
                    }
                }
            }
        };
        entries.push(Entry {
            kind: cleanup::Kind::Worktree,
            outcome,
            reason,
            path: plan.candidate.path.to_str().map(str::to_owned),
            project_path: Some(plan.candidate.project_path),
            thread_id: Some(plan.candidate.thread_id),
            thread_title: Some(plan.candidate.title),
            bytes,
            files: None,
        });
    }
    Ok(entries)
}

async fn check_plans(
    commands: &ThreadCommands,
    settings: &SettingsFile,
    default_root: &Path,
    now: OffsetDateTime,
) -> rusqlite::Result<Vec<Plan>> {
    let commands = commands.clone();
    let settings = settings.clone();
    let default_root = default_root.to_owned();
    tokio::task::spawn_blocking(move || plans(&commands, &settings, &default_root, now))
        .await
        .map_err(|_| rusqlite::Error::InvalidQuery)?
}

async fn remove(
    plan: &Plan,
    commands: &ThreadCommands,
    settings: &SettingsFile,
    terminals: &Arc<TerminalManager>,
    default_root: &Path,
    now: OffsetDateTime,
) -> Result<u64, AutomaticInspectionError> {
    use AutomaticInspectionError::{Failed, Kept};

    let Candidate {
        path,
        branch,
        keep,
        project_path,
        ..
    } = &plan.candidate;
    if terminals.has_running_terminal_under(path) {
        return Err(Kept("Open terminal".to_owned()));
    }
    let removal = vcs_checkout::inspect_automatic(path, branch, *keep).await?;
    if !vcs_checkout::worktree_can_restore(path, Path::new(project_path), branch).await? {
        return Err(Kept("Repository or branch changed".to_owned()));
    }
    let bytes = vcs_checkout::measure_worktree(path).await?;
    let _lease = vcs_checkout::checkout_lease(path).await;
    if terminals.has_running_terminal_under(path) {
        return Err(Kept("Open terminal".to_owned()));
    }
    let latest = check_plans(commands, settings, default_root, now)
        .await
        .map_err(|error| Failed(format!("Could not confirm worktree eligibility: {error}")))?;
    if !latest.contains(plan) {
        return Err(Kept(
            "Thread activity or cleanup settings changed since check".to_owned(),
        ));
    }
    vcs_checkout::remove_automatic(removal, branch, *keep).await?;
    Ok(bytes)
}

fn plans(
    commands: &ThreadCommands,
    settings: &SettingsFile,
    default_root: &Path,
    now: OffsetDateTime,
) -> rusqlite::Result<Vec<Plan>> {
    let Ok(snapshot) = settings.read() else {
        return Ok(Vec::new());
    };
    let snapshot = snapshot.for_client();
    let roots = physical_roots(settings, &snapshot, default_root);
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut database = commands.connect()?;
    let read = database.transaction()?;
    let projects = read
        .prepare(
            "SELECT project_id, workspace_root FROM projection_projects WHERE deleted_at IS NULL",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    if !projects
        .keys()
        .any(|id| Rules::resolve(&snapshot, id).enabled())
    {
        return Ok(Vec::new());
    }
    let project_checkouts = projects
        .values()
        .map(|path| physical_path(PathBuf::from(path)))
        .collect::<Vec<_>>();
    let sessions = paths(&read, SESSION_CHECKOUTS)?
        .into_iter()
        .map(physical_path)
        .collect::<Vec<_>>();
    let deleting = paths(&read, PENDING_DELETION_CHECKOUTS)?
        .into_iter()
        .map(physical_path)
        .collect::<HashSet<_>>();
    let threads = store::storage_threads(&read)?;
    let by_id = threads
        .iter()
        .filter_map(|thread| Some((id(thread)?, thread)))
        .collect::<HashMap<_, _>>();
    let mut groups = BTreeMap::<PathBuf, Vec<&Value>>::new();
    for thread in &threads {
        if let Some(path) = thread["worktreePath"].as_str().map(PathBuf::from)
            && let Some(path) = live_checkout(&physical_path(path), &roots)
            && roots.iter().any(|root| {
                path.strip_prefix(root)
                    .is_ok_and(|relative| relative.components().count() == 2)
            })
        {
            groups.entry(path).or_default().push(thread);
        }
    }
    Ok(groups
        .into_iter()
        .filter_map(|(path, group)| {
            let owner = owner(&group, &by_id);
            let thread = owner.unwrap_or(group[0]);
            let project_id = thread["projectId"].as_str()?;
            let rules = Rules::resolve(&snapshot, project_id);
            if !rules.enabled() {
                return None;
            }
            let project_path = projects.get(project_id)?;
            let branch = thread["branch"].as_str()?;
            let activity = group.iter().filter_map(|thread| activity(thread)).max()?;
            let settled = settlement(thread, &by_id, now, &snapshot);
            let reason = if owner.is_none() {
                Err(format!("Shared by {} threads", group.len()))
            } else if group.iter().any(|member| !idle(member, now, &snapshot)) {
                Err("Thread is running or has pending work".to_owned())
            } else if group.iter().any(|member| member["branch"] != branch) {
                Err("A subagent thread's branch differs from this worktree's branch".to_owned())
            } else if thread["worktreePath"]
                .as_str()
                .is_some_and(|stored| Path::new(stored).is_symlink())
            {
                Err("Worktree is a symbolic link".to_owned())
            } else if project_checkouts.iter().any(|cwd| cwd.starts_with(&path)) {
                Err("Contains a project checkout".to_owned())
            } else if sessions.iter().any(|cwd| cwd.starts_with(&path)) {
                Err("Provider session is still open".to_owned())
            } else if deleting.contains(&path) {
                Err("Thread deletion is still pending".to_owned())
            } else if group.iter().any(|member| {
                date(member, "settledAt").is_some()
                    && date(member, "updatedAt")
                        .is_some_and(|updated| now - updated.max(activity) < Duration::minutes(1))
            }) {
                Err("Settled moments ago".to_owned())
            } else {
                rules.eligibility(now, activity, settled)
            };
            Some(Plan {
                candidate: Candidate {
                    thread_id: id(thread)?.to_owned(),
                    project_id: project_id.to_owned(),
                    title: thread["title"].as_str()?.to_owned(),
                    project_path: project_path.clone(),
                    path,
                    branch: branch.to_owned(),
                    keep: rules.keep,
                },
                reason,
                activity,
            })
        })
        .collect())
}

fn id(thread: &Value) -> Option<&str> {
    thread["id"].as_str()
}

fn physical_path(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn date(thread: &Value, key: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(thread[key].as_str()?, &Rfc3339).ok()
}

fn days_since(elapsed: Duration) -> i64 {
    elapsed.whole_seconds().div_euclid(86_400)
}

fn activity(thread: &Value) -> Option<OffsetDateTime> {
    [
        "createdAt",
        "latestUserMessageAt",
        "latestRunRequestedAt",
        "latestRunStartedAt",
        "latestRunCompletedAt",
    ]
    .into_iter()
    .filter_map(|key| date(thread, key))
    .max()
}

fn idle(thread: &Value, now: OffsetDateTime, settings: &Value) -> bool {
    if !thread["activeRunId"].is_null()
        || !matches!(
            thread["status"].as_str(),
            Some("idle" | "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back")
        )
        || !thread["pendingRuntimeRequest"].is_null()
        || thread["pendingBackgroundTasks"]
            .as_array()
            .is_some_and(|tasks| !tasks.is_empty())
    {
        return false;
    }
    if thread["status"] == "failed" && thread["lastErrorClass"] == "usage_limit" {
        return thread["archivedAt"].is_string()
            || thread["settledOverride"] == "settled"
            || (settings["autoResumeLimitedThreads"] != true
                && settings["snoozeLimitedThreads"] != true);
    }
    !date(thread, "latestUserMessageAt").is_some_and(|message| {
        thread["status"] != "failed"
            && (now - message).abs() <= Duration::minutes(2)
            && (thread["latestRunId"].is_null()
                || [
                    "latestRunRequestedAt",
                    "latestRunStartedAt",
                    "latestRunCompletedAt",
                ]
                .into_iter()
                .all(|key| date(thread, key).is_none_or(|run| run < message)))
    })
}

fn owner<'a>(group: &[&'a Value], by_id: &HashMap<&str, &'a Value>) -> Option<&'a Value> {
    let group_ids: HashSet<_> = group.iter().filter_map(|thread| id(thread)).collect();
    let parents = |thread: &Value| {
        thread["lineage"]["parentThreadId"]
            .as_str()
            .filter(|parent| group_ids.contains(parent))
            .and_then(|parent| by_id.get(parent))
            .copied()
    };
    let mut tops = group
        .iter()
        .copied()
        .filter(|thread| parents(thread).is_none());
    let top = tops.next()?;
    if tops.next().is_some() {
        return None;
    }
    for mut member in group.iter().copied() {
        let mut seen = HashSet::new();
        while id(member) != id(top) {
            let lineage = &member["lineage"];
            if !seen.insert(id(member)?)
                || (lineage["relationshipToParent"] != "subagent" && lineage["sideChat"] != true)
                || lineage["rootThreadId"] != top["lineage"]["rootThreadId"]
            {
                return None;
            }
            member = parents(member)?;
        }
    }
    Some(top)
}

fn settlement<'a>(
    mut thread: &'a Value,
    by_id: &HashMap<&str, &'a Value>,
    now: OffsetDateTime,
    settings: &Value,
) -> Option<OffsetDateTime> {
    let mut seen = HashSet::new();
    loop {
        if let Some(settled) = date(thread, "settledAt") {
            return Some(settled);
        }
        if !seen.insert(id(thread)?)
            || thread["lineage"]["relationshipToParent"] != "subagent"
            || thread["settledOverride"] == "active"
            || !thread["pinnedAt"].is_null()
        {
            return None;
        }
        let parent = *by_id.get(thread["lineage"]["parentThreadId"].as_str()?)?;
        if parent["projectId"] != thread["projectId"]
            || parent["lineage"]["rootThreadId"] != thread["lineage"]["rootThreadId"]
            || !idle(parent, now, settings)
        {
            return None;
        }
        thread = parent;
    }
}
