use std::borrow::Cow;

use serde::Deserialize;

use crate::connection::{ModelSelection, ProviderCatalog, ProviderModel};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct ModelSwitch {
    from: ModelSelection,
    to: ModelSelection,
}

impl ModelSwitch {
    pub(super) fn is_codex(&self) -> bool {
        self.from.instance_id() == "codex" && self.to.instance_id() == "codex"
    }

    pub(crate) fn label(&self, catalog: Option<&ProviderCatalog>) -> Option<String> {
        let provider = catalog.and_then(|catalog| catalog.get("codex"));
        let from = provider.and_then(|provider| provider.model(self.from.model()));
        let to = provider.and_then(|provider| provider.model(self.to.model()));
        let model_changed = self.from.model() != self.to.model();
        let effort = resolved_effort(&self.from, from)
            .zip(resolved_effort(&self.to, to))
            .filter(|(previous, next)| previous.0 != next.0);
        if let Some((previous, next)) = effort {
            return Some(if model_changed {
                format!(
                    "{} {} → {} {}",
                    model_name(&self.from, from),
                    previous.1,
                    model_name(&self.to, to),
                    next.1
                )
            } else {
                format!("{} → {}", previous.1, next.1)
            });
        }
        model_changed.then(|| {
            format!(
                "{} → {}",
                model_name(&self.from, from),
                model_name(&self.to, to)
            )
        })
    }
}

fn resolved_effort<'a>(
    selection: &ModelSelection,
    model: Option<&'a ProviderModel>,
) -> Option<(&'a str, &'a str)> {
    if let Some(effort) = selection.reasoning_effort() {
        Some((effort.as_str(), effort.label()))
    } else {
        model?.default_reasoning()
    }
}

fn model_name<'a>(selection: &'a ModelSelection, model: Option<&'a ProviderModel>) -> Cow<'a, str> {
    model.map_or_else(
        || Cow::Borrowed(selection.model()),
        ProviderModel::codex_short_name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_the_new_model_catalog_default_when_no_effort_was_selected() {
        let switch: ModelSwitch = serde_json::from_value(json!({
            "from":{"instanceId":"codex","model":"gpt-5.4","options":[{"id":"reasoningEffort","value":"high"}]},
            "to":{"instanceId":"codex","model":"gpt-5.6-sol"},
        })).unwrap();
        let catalog: ProviderCatalog = serde_json::from_value(json!({
            "codex":{"instanceId":"codex","driver":"codex","models":[
                {"slug":"gpt-5.4","name":"GPT-5.4"},
                {"slug":"gpt-5.6-sol","name":"GPT-5.6-Sol","capabilities":{"optionDescriptors":[
                    {"id":"reasoningEffort","type":"select","currentValue":"low","options":[{"id":"low","label":"Low"}]}
                ]}},
            ]},
        })).unwrap();
        assert_eq!(
            switch.label(Some(&catalog)).as_deref(),
            Some("GPT-5.4 High → 5.6 Sol Low")
        );
    }
}
