use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ContextSnapshot, Entity};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextField {
    Summary,
    Glossary,
    Entity,
    Style,
    Scratchpad,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConflict {
    pub field: ContextField,
    pub key: Option<String>,
    pub base: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePlan {
    /// All conflict-free values are already applied here. Conflict locations retain the base.
    pub auto_merged: ContextSnapshot,
    pub conflicts: Vec<ContextConflict>,
}

fn merge_scalar(
    field: ContextField,
    base: &str,
    left: &str,
    right: &str,
    conflicts: &mut Vec<ContextConflict>,
) -> String {
    if left == right {
        left.to_owned()
    } else if left == base {
        right.to_owned()
    } else if right == base {
        left.to_owned()
    } else {
        conflicts.push(ContextConflict {
            field,
            key: None,
            base: Some(base.to_owned()),
            left: Some(left.to_owned()),
            right: Some(right.to_owned()),
        });
        base.to_owned()
    }
}

fn json_value<T: Serialize>(value: Option<&T>) -> Option<String> {
    value.map(|value| serde_json::to_string(value).expect("domain value must serialize"))
}

fn merge_map<T>(
    field: ContextField,
    base: &BTreeMap<String, T>,
    left: &BTreeMap<String, T>,
    right: &BTreeMap<String, T>,
    conflicts: &mut Vec<ContextConflict>,
) -> BTreeMap<String, T>
where
    T: Clone + Eq + Serialize,
{
    let keys: BTreeSet<_> = base.keys().chain(left.keys()).chain(right.keys()).collect();
    let mut merged = BTreeMap::new();

    for key in keys {
        let b = base.get(key);
        let l = left.get(key);
        let r = right.get(key);
        let selected = if l == r {
            l
        } else if l == b {
            r
        } else if r == b {
            l
        } else {
            conflicts.push(ContextConflict {
                field: field.clone(),
                key: Some(key.clone()),
                base: json_value(b),
                left: json_value(l),
                right: json_value(r),
            });
            b
        };
        if let Some(value) = selected {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

pub fn plan_context_merge(
    base: &ContextSnapshot,
    left: &ContextSnapshot,
    right: &ContextSnapshot,
) -> MergePlan {
    let mut conflicts = Vec::new();
    let summary = merge_scalar(
        ContextField::Summary,
        &base.summary,
        &left.summary,
        &right.summary,
        &mut conflicts,
    );
    let glossary = merge_map::<String>(
        ContextField::Glossary,
        &base.glossary,
        &left.glossary,
        &right.glossary,
        &mut conflicts,
    );
    let entities = merge_map::<Entity>(
        ContextField::Entity,
        &base.entities,
        &left.entities,
        &right.entities,
        &mut conflicts,
    );
    let style = merge_scalar(
        ContextField::Style,
        &base.style,
        &left.style,
        &right.style,
        &mut conflicts,
    );
    let scratchpad = merge_scalar(
        ContextField::Scratchpad,
        &base.scratchpad,
        &left.scratchpad,
        &right.scratchpad,
        &mut conflicts,
    );

    MergePlan {
        auto_merged: ContextSnapshot {
            summary,
            glossary,
            entities,
            style,
            scratchpad,
        },
        conflicts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(summary: &str, glossary: &[(&str, &str)]) -> ContextSnapshot {
        ContextSnapshot {
            summary: summary.into(),
            glossary: glossary
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn three_way_merge_accepts_one_sided_changes() {
        let base = context("start", &[("先輩", "Senpai")]);
        let left = context("left summary", &[("先輩", "Senpai")]);
        let right = context("start", &[("先輩", "Upperclassman")]);
        let plan = plan_context_merge(&base, &left, &right);
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.auto_merged.summary, "left summary");
        assert_eq!(plan.auto_merged.glossary["先輩"], "Upperclassman");
    }

    #[test]
    fn three_way_merge_reports_two_sided_conflict() {
        let base = context("start", &[]);
        let left = context("left", &[]);
        let right = context("right", &[]);
        let plan = plan_context_merge(&base, &left, &right);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].field, ContextField::Summary);
        assert_eq!(plan.auto_merged.summary, "start");
    }
}
