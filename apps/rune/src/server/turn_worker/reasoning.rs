use std::time::Instant;

use t3_rune::providers::codex::ReasoningStream;

use super::DELTA_FLUSH_INTERVAL;
use crate::server::turn_commands::reasoning::ReasoningPart;

#[derive(Default)]
pub(super) struct ReasoningOutput {
    pending: Vec<PartOutput>,
    next_due: Option<Instant>,
}

pub(super) struct PartOutput {
    pub(super) part: ReasoningPart,
    pub(super) text: String,
    dirty: bool,
}

impl ReasoningOutput {
    pub(super) fn append(&mut self, part: ReasoningPart, delta: &str) -> Option<&ReasoningPart> {
        if delta.is_empty() {
            return None;
        }
        self.next_due
            .get_or_insert_with(|| Instant::now() + DELTA_FLUSH_INTERVAL);
        if let Some(existing) = self
            .pending
            .iter_mut()
            .find(|existing| existing.part == part)
        {
            existing.text.push_str(delta);
            existing.dirty = true;
            return None;
        }
        self.pending.push(PartOutput {
            part,
            text: delta.to_owned(),
            dirty: true,
        });
        self.pending.last().map(|output| &output.part)
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.next_due
    }

    pub(super) fn flush_due(&mut self) -> impl Iterator<Item = &PartOutput> {
        let due = self
            .next_due
            .is_some_and(|deadline| Instant::now() >= deadline);
        if due {
            self.next_due = None;
        }
        self.pending.iter_mut().filter_map(move |output| {
            (due && std::mem::take(&mut output.dirty)).then_some(&*output)
        })
    }

    pub(super) fn complete(
        &mut self,
        native_item_id: &str,
        summary: Vec<String>,
        content: Vec<String>,
    ) -> impl Iterator<Item = PartOutput> + '_ {
        for (stream, parts) in [
            (ReasoningStream::Summary, summary),
            (ReasoningStream::Content, content),
        ] {
            for (index, text) in parts
                .into_iter()
                .enumerate()
                .filter(|(_, text)| !text.is_empty())
            {
                let part = ReasoningPart {
                    native_item_id: native_item_id.to_owned(),
                    stream,
                    index,
                };
                if let Some(existing) = self
                    .pending
                    .iter_mut()
                    .find(|existing| existing.part == part)
                {
                    existing.text = text;
                } else {
                    self.pending.push(PartOutput {
                        part,
                        text,
                        dirty: false,
                    });
                }
            }
        }
        let native_item_id = native_item_id.to_owned();
        self.pending.extract_if(.., move |output| {
            output.part.native_item_id == native_item_id
        })
    }

    pub(super) fn finish(&mut self) -> impl Iterator<Item = PartOutput> + '_ {
        self.pending.drain(..)
    }
}
