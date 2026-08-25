//! History matching and session-local replay cursors.

use std::collections::HashMap;

use terratranslate_core::{CommitId, ContextSnapshot, GameId, TranslationCommit, TurnSignature};
use terratranslate_store::{ReplayPath, SessionStore, StoreError};

/// A ranked position identified by one or more observed turns.
///
/// `commit_id` is the last completed commit. `next_commit_id` is the historical
/// turn represented by the first observed signature. The cursor is therefore
/// created at `commit_id`, not at the matching turn itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeCandidate {
    pub game_id: Option<GameId>,
    pub branch: String,
    pub commit_id: CommitId,
    pub next_commit_id: CommitId,
    pub next_index: usize,
    pub matched_turns: usize,
    pub occurrence_count: usize,
}

impl ResumeCandidate {
    pub fn is_unambiguous(&self) -> bool {
        self.occurrence_count == 1
    }
}

/// Session-local replay state.  It never becomes a durable ref; only a later
/// divergence creates a branch in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayCursor {
    pub game_id: Option<GameId>,
    pub branch: String,
    pub path: Vec<TranslationCommit>,
    pub position: usize,
}

impl ReplayCursor {
    pub fn from_path(path: &ReplayPath, selected_commit: &CommitId) -> Option<Self> {
        Self::new(path.clone(), selected_commit)
    }

    pub fn new(path: ReplayPath, selected_commit: &CommitId) -> Option<Self> {
        let position = path
            .commits
            .iter()
            .position(|commit| &commit.id == selected_commit)?;
        Some(Self {
            game_id: None,
            branch: path.branch.name,
            path: path.commits,
            position,
        })
    }

    pub fn for_candidate(paths: &[ReplayPath], candidate: &ResumeCandidate) -> Option<Self> {
        let path = paths
            .iter()
            .find(|path| path.branch.name == candidate.branch)?
            .clone();
        let mut cursor = Self::new(path, &candidate.commit_id)?;
        cursor.game_id = candidate.game_id.clone();
        Some(cursor)
    }

    pub fn current_commit(&self) -> &TranslationCommit {
        &self.path[self.position]
    }

    pub fn current_commit_id(&self) -> CommitId {
        self.current_commit().id.clone()
    }

    /// The context at the selected/last matched position. Translation engines
    /// use the durable branch created on divergence to load this exact snapshot.
    pub fn context_snapshot(&self) -> ContextSnapshot {
        self.current_commit().context.clone()
    }

    pub fn last_matched_commit(&self) -> &TranslationCommit {
        self.current_commit()
    }

    pub fn at_head(&self) -> bool {
        self.position + 1 >= self.path.len()
    }

    pub fn at_translatable_head(&self) -> bool {
        self.path
            .get(self.position + 1..)
            .is_none_or(|remaining| remaining.iter().all(is_structural_commit))
    }

    /// Compare one grouped turn with the next historical translatable commit.
    /// Structural commits are advanced silently before comparison.
    pub fn step(
        &mut self,
        store: &SessionStore,
        observed: &TurnSignature,
    ) -> Result<ReplayStep, StoreError> {
        let mut index = self.position + 1;
        while index < self.path.len() && is_structural_commit(&self.path[index]) {
            self.position = index;
            index += 1;
        }
        if index >= self.path.len() {
            return Ok(ReplayStep::AtHead);
        }
        let commit = &self.path[index];
        let Some(expected) = store.commit_replay_signature(commit)? else {
            return Ok(ReplayStep::Unavailable);
        };
        if &expected == observed {
            self.position = index;
            Ok(ReplayStep::Matched(Box::new(commit.clone())))
        } else {
            Ok(ReplayStep::Diverged {
                at: self.current_commit_id(),
            })
        }
    }

    pub fn advance(
        &mut self,
        store: &SessionStore,
        observed: &TurnSignature,
    ) -> Result<ReplayStep, StoreError> {
        self.step(store, observed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayStep {
    Matched(Box<TranslationCommit>),
    AtHead,
    Diverged {
        at: CommitId,
    },
    /// A missing or unreadable payload prevents safe automatic matching.
    Unavailable,
}

pub fn is_structural_commit(commit: &TranslationCommit) -> bool {
    commit.parents.len() > 1
        || commit.source_events.is_empty()
        || commit.source_text.trim().is_empty()
}

/// Rank positions matching all observed signatures.  Shared commit IDs are
/// deduplicated across branch paths; when the resulting position is otherwise
/// tied, `main` wins and then branch name provides deterministic ordering.
pub fn find_resume_candidates(
    store: &SessionStore,
    paths: &[ReplayPath],
    observed: &[TurnSignature],
    game_id: Option<&GameId>,
) -> Result<Vec<ResumeCandidate>, StoreError> {
    if observed.is_empty() {
        return Ok(Vec::new());
    }

    #[derive(Clone)]
    struct Raw {
        branch: String,
        start: usize,
        matched: usize,
        next: CommitId,
        selected: CommitId,
    }

    let mut raw = Vec::new();
    for path in paths {
        let mut translatable = Vec::new();
        for (index, commit) in path.commits.iter().enumerate() {
            if is_structural_commit(commit) {
                continue;
            }
            let Some(signature) = store.commit_replay_signature(commit)? else {
                continue;
            };
            translatable.push((index, commit, signature));
        }
        for start in 0..translatable.len() {
            if translatable[start].2 != observed[0] {
                continue;
            }
            let mut matched = 1;
            while matched < observed.len()
                && start + matched < translatable.len()
                && translatable[start + matched].2 == observed[matched]
            {
                matched += 1;
            }
            // A partial window cannot identify a position when the caller has
            // supplied more than one signature.
            if matched != observed.len() {
                continue;
            }
            let next_index = translatable[start].0;
            if next_index == 0 {
                // There is no durable "last completed" commit before a
                // translatable root; ordinary translation is safer than
                // pretending that root itself is the cursor position.
                continue;
            }
            let selected_index = next_index.saturating_sub(1);
            raw.push(Raw {
                branch: path.branch.name.clone(),
                start: next_index,
                matched,
                next: translatable[start].1.id.clone(),
                selected: path.commits[selected_index].id.clone(),
            });
        }
    }

    // A shared first-parent commit is one historical occurrence even when it is
    // reachable through several named refs. Keep the best path presentation.
    let mut by_commit = HashMap::<CommitId, Raw>::new();
    for candidate in raw {
        let replace = match by_commit.get(&candidate.next) {
            None => true,
            Some(existing) => {
                candidate.matched > existing.matched
                    || (candidate.matched == existing.matched
                        && candidate.branch == "main"
                        && existing.branch != "main")
                    || (candidate.matched == existing.matched && candidate.branch < existing.branch)
            }
        };
        if replace {
            by_commit.insert(candidate.next.clone(), candidate);
        }
    }
    let occurrence_count = by_commit.len();
    let mut ranked = by_commit
        .into_values()
        .map(|candidate| ResumeCandidate {
            game_id: game_id.cloned(),
            branch: candidate.branch,
            commit_id: candidate.selected,
            next_commit_id: candidate.next,
            next_index: candidate.start,
            matched_turns: candidate.matched,
            occurrence_count,
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .matched_turns
            .cmp(&left.matched_turns)
            .then_with(|| left.occurrence_count.cmp(&right.occurrence_count))
            .then_with(|| (left.branch != "main").cmp(&(right.branch != "main")))
            .then_with(|| left.branch.cmp(&right.branch))
            .then_with(|| left.next_commit_id.0.cmp(&right.next_commit_id.0))
    });
    Ok(ranked)
}

pub fn rank_resume_candidates(
    store: &SessionStore,
    paths: &[ReplayPath],
    observed: &[TurnSignature],
) -> Result<Vec<ResumeCandidate>, StoreError> {
    find_resume_candidates(store, paths, observed, None)
}

/// Return the best candidate's canonical path, useful to UI/manual selectors.
pub fn candidate_path<'a>(
    paths: &'a [ReplayPath],
    candidate: &ResumeCandidate,
) -> Option<&'a ReplayPath> {
    paths
        .iter()
        .find(|path| path.branch.name == candidate.branch)
}

/// Whether all candidate positions refer to one unique historical occurrence.
pub fn uniquely_identified(candidates: &[ResumeCandidate]) -> bool {
    candidates.len() == 1 && candidates[0].is_unambiguous()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use terratranslate_core::{
        ContextSnapshot, EventId, Modality, ModelMetadata, PayloadRef, SourceEvent, SourceKind,
    };

    fn blobs() -> PathBuf {
        std::env::temp_dir().join(format!("terratranslate-replay-{}", uuid::Uuid::new_v4()))
    }

    fn commit(
        store: &mut SessionStore,
        parent: &CommitId,
        at: i64,
        hook: &str,
        text: &str,
        translated: &str,
    ) -> TranslationCommit {
        let digest = store.put_blob(text.as_bytes()).unwrap();
        let event = SourceEvent {
            id: EventId::new(),
            captured_at_ms: at,
            modality: Modality::Text,
            source: SourceKind::Manual,
            target: "target".into(),
            payload: PayloadRef {
                digest,
                media_type: "text/plain".into(),
                byte_len: text.len() as u64,
            },
            metadata: [("stable_hook_key".into(), hook.into())]
                .into_iter()
                .collect(),
        };
        TranslationCommit::create(
            vec![parent.clone()],
            at,
            vec![event],
            text.into(),
            translated.into(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "turn".into(),
        )
        .unwrap()
    }

    #[test]
    fn finds_unique_position_and_cursor_replays_without_branch_changes() {
        let mut store = SessionStore::in_memory(blobs()).unwrap();
        let root = TranslationCommit::create(
            vec![],
            1,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "root".into(),
        )
        .unwrap();
        store.put_commit(&root).unwrap();
        let first = commit(&mut store, &root.id, 2, "dialogue", "Hello", "Bonjour");
        let second = commit(&mut store, &first.id, 3, "choice", "Yes", "Oui");
        store.put_commit(&first).unwrap();
        store.put_commit(&second).unwrap();
        store.create_branch("main", &second.id, 3).unwrap();
        let paths = store.default_replay_paths().unwrap();
        let signature = store.commit_replay_signature(&first).unwrap().unwrap();
        let candidates = find_resume_candidates(&store, &paths, &[signature], None).unwrap();
        assert!(uniquely_identified(&candidates));
        assert_eq!(candidates[0].commit_id, root.id);
        let mut cursor = ReplayCursor::for_candidate(&paths, &candidates[0]).unwrap();
        let step = cursor
            .step(
                &store,
                &store.commit_replay_signature(&first).unwrap().unwrap(),
            )
            .unwrap();
        assert!(matches!(step, ReplayStep::Matched(commit) if commit.id == first.id));
    }

    #[test]
    fn two_signatures_disambiguate_repeated_first_turn_and_main_wins_ties() {
        let mut store = SessionStore::in_memory(blobs()).unwrap();
        let root = TranslationCommit::create(
            vec![],
            1,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "root".into(),
        )
        .unwrap();
        store.put_commit(&root).unwrap();
        let first = commit(&mut store, &root.id, 2, "dialogue", "Hello", "A");
        let second = commit(&mut store, &first.id, 3, "choice", "Yes", "B");
        let repeated = commit(&mut store, &second.id, 4, "dialogue", "Hello", "C");
        let tail = commit(&mut store, &repeated.id, 5, "choice", "No", "D");
        for value in [&first, &second, &repeated, &tail] {
            store.put_commit(value).unwrap();
        }
        store.create_branch("main", &tail.id, 5).unwrap();
        let other_tail = commit(&mut store, &second.id, 6, "choice", "Maybe", "E");
        store.put_commit(&other_tail).unwrap();
        store.create_branch("other", &other_tail.id, 6).unwrap();
        let paths = store.default_replay_paths().unwrap();
        let observed = vec![
            store.commit_replay_signature(&first).unwrap().unwrap(),
            store.commit_replay_signature(&second).unwrap().unwrap(),
        ];
        let candidates = find_resume_candidates(&store, &paths, &observed, None).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].branch, "main");
    }

    #[test]
    fn cursor_skips_structural_commits_and_reports_divergence_at_last_match() {
        let mut store = SessionStore::in_memory(blobs()).unwrap();
        let root = TranslationCommit::create(
            vec![],
            1,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "root".into(),
        )
        .unwrap();
        store.put_commit(&root).unwrap();
        let first = commit(&mut store, &root.id, 2, "dialogue", "one", "uno");
        store.put_commit(&first).unwrap();
        let scratch = TranslationCommit::create(
            vec![first.id.clone()],
            3,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot {
                scratchpad: "note".into(),
                ..Default::default()
            },
            vec![],
            vec![],
            ModelMetadata::default(),
            "scratchpad".into(),
        )
        .unwrap();
        store.put_commit(&scratch).unwrap();
        let second = commit(&mut store, &scratch.id, 4, "dialogue", "two", "dos");
        store.put_commit(&second).unwrap();
        store.create_branch("main", &second.id, 4).unwrap();
        let path = store.default_replay_paths().unwrap().remove(0);
        let mut cursor = ReplayCursor::new(path, &root.id).unwrap();
        let first_signature = store.commit_replay_signature(&first).unwrap().unwrap();
        assert!(matches!(
            cursor.step(&store, &first_signature).unwrap(),
            ReplayStep::Matched(commit) if commit.id == first.id
        ));
        assert_eq!(cursor.context_snapshot(), first.context);
        let wrong = TurnSignature::from_pairs([("dialogue", "different")]);
        assert!(matches!(
            cursor.step(&store, &wrong).unwrap(),
            ReplayStep::Diverged { at } if at == scratch.id
        ));
        let second_signature = store.commit_replay_signature(&second).unwrap().unwrap();
        assert!(matches!(
            cursor.step(&store, &second_signature).unwrap(),
            ReplayStep::Matched(commit) if commit.id == second.id
        ));
        assert!(matches!(
            cursor.step(&store, &second_signature).unwrap(),
            ReplayStep::AtHead
        ));
    }
}
