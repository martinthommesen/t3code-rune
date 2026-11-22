use super::git::Git;
use super::{CheckpointError, add_index_entries, commit_tree, required_oid};

const SCAN_LIMIT: usize = 512;
const TRAILER: &str = "T3-Head-Moved-At: ";

pub(super) struct HeadHistory {
    moves: Vec<Move>,
    pinned_head: Option<String>,
}

struct Move {
    to: String,
    at: u64,
    switched: bool,
}

impl HeadHistory {
    pub(super) async fn read(git: Git<'_>, pinned_head: Option<&str>) -> Self {
        let records = if pinned_head.is_some() {
            let limit = SCAN_LIMIT.to_string();
            git.run(
                [
                    "log",
                    "--walk-reflogs",
                    "-n",
                    &limit,
                    "--date=unix",
                    "--format=%H %gd %gs",
                    "HEAD",
                    "--",
                ],
                None,
            )
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| output.stdout)
        } else {
            None
        };
        let moves = records
            .as_deref()
            .into_iter()
            .flat_map(|records| {
                records.split(|byte| *byte == b'\n').filter_map(|record| {
                    let record = String::from_utf8_lossy(record);
                    let mut fields = record.splitn(3, ' ');
                    let to = fields.next()?;
                    let at = fields
                        .next()?
                        .strip_prefix("HEAD@{")?
                        .strip_suffix('}')?
                        .parse()
                        .ok()?;
                    let reason = fields.next()?;
                    Some(Move {
                        to: to.to_owned(),
                        at,
                        switched: reason.starts_with("checkout: moving from "),
                    })
                })
            })
            .collect();
        Self {
            moves,
            pinned_head: pinned_head.map(str::to_owned),
        }
    }

    fn mark(&self) -> Option<(u64, usize)> {
        let last_move = self.moves.first()?;
        if self.pinned_head.as_deref()? != last_move.to {
            return None;
        }
        let count = self
            .moves
            .iter()
            .filter(|entry| entry.at == last_move.at)
            .count();
        Some((last_move.at, count))
    }

    pub(super) fn checkpoint_message(&self, checkpoint_ref: &str) -> String {
        let mut message = format!("t3 checkpoint ref={checkpoint_ref}");
        if let Some((at, count)) = self.mark() {
            message.push_str(&format!("\n\n{TRAILER}{at} {count}"));
        }
        message
    }

    pub(super) async fn turn_start(
        &self,
        git: Git<'_>,
        scratch: Git<'_>,
        previous_ref: &str,
        checkpoint_ref: &str,
    ) -> Result<Option<String>, CheckpointError> {
        if self.mark().is_none() {
            return Ok(None);
        }
        let previous = format!("{previous_ref}^{{commit}}");
        let previous = git
            .command([
                "log",
                "--no-walk",
                "--format=%H %T %ct %P%n%B",
                &previous,
                "--",
            ])
            .await?;
        let previous = String::from_utf8_lossy(&previous);
        let mut lines = previous.lines();
        let Some(info) = lines.next() else {
            return Ok(None);
        };
        let mut fields = info.split(' ');
        let (Some(commit), Some(tree), Some(captured_at), Some(previous_head)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Ok(None);
        };
        let Some((at, count)) = lines.find_map(|line| {
            let (at, count) = line.strip_prefix(TRAILER)?.split_once(' ')?;
            Some((at.parse::<u64>().ok()?, count.parse::<usize>().ok()?))
        }) else {
            return Ok(None);
        };
        let Some(oldest_marked) = self.moves.iter().rposition(|entry| entry.at == at) else {
            return Ok(None);
        };
        if oldest_marked == SCAN_LIMIT - 1 || count == 0 {
            return Ok(None);
        }
        let Some(since) = oldest_marked.checked_sub(count - 1) else {
            return Ok(None);
        };
        if self.moves[since].to != previous_head {
            return Ok(None);
        }
        let switches: Vec<_> = self.moves[..since]
            .iter()
            .zip(&self.moves[1..])
            .filter(|(entry, prior)| entry.switched && entry.to != prior.to)
            .collect();
        let Some(&(landed, _)) = switches.first() else {
            return Ok(None);
        };

        scratch.command(["read-tree", tree]).await?;
        for (entry, prior) in switches.into_iter().rev() {
            let changes = git
                .command([
                    "diff-tree",
                    "-r",
                    "--no-commit-id",
                    "--no-renames",
                    "-z",
                    &prior.to,
                    &entry.to,
                    "--",
                ])
                .await?;
            add_index_entries(scratch, &changes, true).await?;
        }
        let start_tree = required_oid(scratch.command(["write-tree"]).await?)?;
        if start_tree == tree {
            return Ok(None);
        }
        let committer = format!("{captured_at} +0000");
        let message = format!("t3 checkpoint turn start ref={checkpoint_ref}");
        let start = commit_tree(
            scratch.date(&committer),
            &start_tree,
            Some(&landed.to),
            Some(commit),
            &message,
        )
        .await?;
        Ok(Some(start))
    }
}
