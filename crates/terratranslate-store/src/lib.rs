//! SQLite metadata and content-addressed media persistence.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use terratranslate_core::{
    BranchRef, CommitId, ContextSnapshot, GameId, GameIdentity, MergePlan, ModelMetadata,
    TranslationCommit, plan_context_merge,
};

pub const DEFAULT_GAME_ID: &str = "default";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("commit {0} was not found")]
    CommitNotFound(CommitId),
    #[error("branch {0:?} was not found")]
    BranchNotFound(String),
    #[error("game {0} was not found")]
    GameNotFound(GameId),
    #[error("game ID must not be blank or contain whitespace")]
    InvalidGameId,
    #[error("branch name must not be blank or contain whitespace")]
    InvalidBranchName,
    #[error("commit ID does not match its content")]
    InvalidCommitId,
    #[error("commit references missing parent {0}")]
    MissingParent(CommitId),
    #[error("branches have no common ancestor")]
    NoMergeBase,
    #[error("invalid content digest")]
    InvalidDigest,
}

pub struct SessionStore {
    connection: Connection,
    blob_root: PathBuf,
}

impl SessionStore {
    pub fn open(
        database: impl AsRef<Path>,
        blob_root: impl Into<PathBuf>,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open(database)?;
        let store = Self {
            connection,
            blob_root: blob_root.into(),
        };
        store.initialize()?;
        Ok(store)
    }

    pub fn in_memory(blob_root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        let store = Self {
            connection,
            blob_root: blob_root.into(),
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), StoreError> {
        self.connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS commits (
                 id TEXT PRIMARY KEY,
                 created_at_ms INTEGER NOT NULL,
                 body_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS commit_parents (
                 child_id TEXT NOT NULL REFERENCES commits(id) ON DELETE CASCADE,
                 parent_id TEXT NOT NULL REFERENCES commits(id),
                 position INTEGER NOT NULL,
                 PRIMARY KEY(child_id, position)
             );
             CREATE INDEX IF NOT EXISTS commit_parents_parent ON commit_parents(parent_id);
             CREATE TABLE IF NOT EXISTS branches (
                 name TEXT PRIMARY KEY,
                 head_id TEXT NOT NULL REFERENCES commits(id),
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS games (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 executable_path TEXT NOT NULL,
                 image_id TEXT,
                 platform TEXT NOT NULL,
                 runtime TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS game_branches (
                 game_id TEXT NOT NULL REFERENCES games(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 head_id TEXT NOT NULL REFERENCES commits(id),
                 updated_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(game_id, name)
             );
             CREATE INDEX IF NOT EXISTS game_branches_head ON game_branches(head_id);
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value_json TEXT NOT NULL
             );",
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO games(
                 id, name, executable_path, image_id, platform, runtime, updated_at_ms
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0)",
            params![DEFAULT_GAME_ID, "Default session", "", "unknown", "unknown"],
        )?;
        // Databases created before game namespaces had one unscoped branch table. Copy
        // those refs into the default game exactly once; the compatibility methods below
        // use game_branches from then on.
        self.connection.execute(
            "INSERT OR IGNORE INTO game_branches(game_id, name, head_id, updated_at_ms)
             SELECT ?1, name, head_id, updated_at_ms FROM branches",
            [DEFAULT_GAME_ID],
        )?;
        Ok(())
    }

    /// Register a game identity and create its independent `main` history if needed.
    pub fn ensure_game(
        &mut self,
        identity: &GameIdentity,
        updated_at_ms: i64,
    ) -> Result<(), StoreError> {
        validate_game_id(&identity.id)?;
        self.connection.execute(
            "INSERT INTO games(
                 id, name, executable_path, image_id, platform, runtime, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 name = excluded.name,
                 executable_path = excluded.executable_path,
                 image_id = excluded.image_id,
                 platform = excluded.platform,
                 runtime = excluded.runtime,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                identity.id.0,
                identity.name,
                identity.executable_path,
                identity.image_id,
                identity.platform,
                identity.runtime,
                updated_at_ms
            ],
        )?;
        let has_main: bool = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM game_branches WHERE game_id = ?1 AND name = 'main'
             )",
            [&identity.id.0],
            |row| row.get(0),
        )?;
        if !has_main {
            let root = TranslationCommit::create(
                vec![],
                updated_at_ms,
                vec![],
                String::new(),
                String::new(),
                ContextSnapshot::default(),
                vec![],
                vec![],
                ModelMetadata::default(),
                format!("Initialize {} history ({})", identity.name, identity.id),
            )
            .map_err(|_| StoreError::InvalidCommitId)?;
            self.put_commit(&root)?;
            self.create_game_branch(&identity.id, "main", &root.id, updated_at_ms)?;
        }
        Ok(())
    }

    pub fn game(&self, id: &GameId) -> Result<GameIdentity, StoreError> {
        self.connection
            .query_row(
                "SELECT name, executable_path, image_id, platform, runtime
                 FROM games WHERE id = ?1",
                [&id.0],
                |row| {
                    Ok(GameIdentity {
                        id: id.clone(),
                        name: row.get(0)?,
                        executable_path: row.get(1)?,
                        image_id: row.get(2)?,
                        platform: row.get(3)?,
                        runtime: row.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::GameNotFound(id.clone()))
    }

    pub fn list_games(&self) -> Result<Vec<GameIdentity>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, executable_path, image_id, platform, runtime
             FROM games ORDER BY name, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(GameIdentity {
                id: GameId(row.get(0)?),
                name: row.get(1)?,
                executable_path: row.get(2)?,
                image_id: row.get(3)?,
                platform: row.get(4)?,
                runtime: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn create_game_branch(
        &mut self,
        game_id: &GameId,
        name: &str,
        head: &CommitId,
        updated_at_ms: i64,
    ) -> Result<BranchRef, StoreError> {
        validate_game_id(game_id)?;
        validate_branch_name(name)?;
        self.game(game_id)?;
        self.get_commit(head)?;
        self.connection.execute(
            "INSERT INTO game_branches(game_id, name, head_id, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(game_id, name) DO UPDATE SET
                 head_id = excluded.head_id,
                 updated_at_ms = excluded.updated_at_ms",
            params![game_id.0, name, head.0, updated_at_ms],
        )?;
        Ok(BranchRef {
            name: name.to_owned(),
            head: head.clone(),
            updated_at_ms,
        })
    }

    pub fn game_branch(&self, game_id: &GameId, name: &str) -> Result<BranchRef, StoreError> {
        validate_game_id(game_id)?;
        self.game(game_id)?;
        self.connection
            .query_row(
                "SELECT head_id, updated_at_ms FROM game_branches
                 WHERE game_id = ?1 AND name = ?2",
                params![game_id.0, name],
                |row| {
                    Ok(BranchRef {
                        name: name.to_owned(),
                        head: CommitId(row.get(0)?),
                        updated_at_ms: row.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::BranchNotFound(name.to_owned()))
    }

    pub fn list_game_branches(&self, game_id: &GameId) -> Result<Vec<BranchRef>, StoreError> {
        validate_game_id(game_id)?;
        self.game(game_id)?;
        let mut statement = self.connection.prepare(
            "SELECT name, head_id, updated_at_ms FROM game_branches
             WHERE game_id = ?1 ORDER BY name",
        )?;
        let rows = statement.query_map([&game_id.0], |row| {
            Ok(BranchRef {
                name: row.get(0)?,
                head: CommitId(row.get(1)?),
                updated_at_ms: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn game_branch_history(
        &self,
        game_id: &GameId,
        name: &str,
    ) -> Result<Vec<TranslationCommit>, StoreError> {
        let head = self.game_branch(game_id, name)?.head;
        self.history_from_head(head)
    }

    pub fn put_commit(&mut self, commit: &TranslationCommit) -> Result<(), StoreError> {
        if !commit
            .verify_id()
            .map_err(|_| StoreError::InvalidCommitId)?
        {
            return Err(StoreError::InvalidCommitId);
        }
        let transaction = self.connection.transaction()?;
        for parent in &commit.parents {
            if !commit_exists(&transaction, parent)? {
                return Err(StoreError::MissingParent(parent.clone()));
            }
        }
        let json = serde_json::to_string(commit)?;
        transaction.execute(
            "INSERT OR IGNORE INTO commits(id, created_at_ms, body_json) VALUES (?1, ?2, ?3)",
            params![commit.id.0, commit.created_at_ms, json],
        )?;
        for (position, parent) in commit.parents.iter().enumerate() {
            transaction.execute(
                "INSERT OR IGNORE INTO commit_parents(child_id, parent_id, position) VALUES (?1, ?2, ?3)",
                params![commit.id.0, parent.0, position as i64],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn get_commit(&self, id: &CommitId) -> Result<TranslationCommit, StoreError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT body_json FROM commits WHERE id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .optional()?;
        let json = json.ok_or_else(|| StoreError::CommitNotFound(id.clone()))?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn create_branch(
        &mut self,
        name: &str,
        head: &CommitId,
        updated_at_ms: i64,
    ) -> Result<BranchRef, StoreError> {
        self.create_game_branch(&default_game_id(), name, head, updated_at_ms)
    }

    pub fn branch(&self, name: &str) -> Result<BranchRef, StoreError> {
        self.game_branch(&default_game_id(), name)
    }

    pub fn list_branches(&self) -> Result<Vec<BranchRef>, StoreError> {
        self.list_game_branches(&default_game_id())
    }

    /// Return every commit reachable from a branch head in oldest-first order.
    ///
    /// Merge parents are included as well, so the returned history contains the
    /// complete context graph represented by the branch rather than only its
    /// first-parent presentation.
    pub fn branch_history(&self, name: &str) -> Result<Vec<TranslationCommit>, StoreError> {
        self.game_branch_history(&default_game_id(), name)
    }

    fn history_from_head(&self, head: CommitId) -> Result<Vec<TranslationCommit>, StoreError> {
        let mut seen = HashSet::new();
        let mut history = Vec::new();
        let mut stack = vec![(head, false)];

        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                history.push(self.get_commit(&id)?);
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }

            let commit = self.get_commit(&id)?;
            stack.push((id, true));
            for parent in commit.parents.into_iter().rev() {
                stack.push((parent, false));
            }
        }

        Ok(history)
    }

    pub fn advance_branch(
        &mut self,
        name: &str,
        expected_head: &CommitId,
        new_head: &CommitId,
        updated_at_ms: i64,
    ) -> Result<bool, StoreError> {
        self.get_commit(new_head)?;
        let changed = self.connection.execute(
            "UPDATE game_branches SET head_id = ?1, updated_at_ms = ?2
             WHERE game_id = ?3 AND name = ?4 AND head_id = ?5",
            params![
                new_head.0,
                updated_at_ms,
                DEFAULT_GAME_ID,
                name,
                expected_head.0
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn advance_game_branch(
        &mut self,
        game_id: &GameId,
        name: &str,
        expected_head: &CommitId,
        new_head: &CommitId,
        updated_at_ms: i64,
    ) -> Result<bool, StoreError> {
        self.game_branch(game_id, name)?;
        self.get_commit(new_head)?;
        let changed = self.connection.execute(
            "UPDATE game_branches SET head_id = ?1, updated_at_ms = ?2
             WHERE game_id = ?3 AND name = ?4 AND head_id = ?5",
            params![new_head.0, updated_at_ms, game_id.0, name, expected_head.0],
        )?;
        Ok(changed == 1)
    }

    pub fn merge_base(&self, left: &CommitId, right: &CommitId) -> Result<CommitId, StoreError> {
        let left_ancestors = self.ancestor_distances(left)?;
        let right_ancestors = self.ancestor_distances(right)?;
        left_ancestors
            .iter()
            .filter_map(|(id, left_distance)| {
                right_ancestors
                    .get(id)
                    .map(|right_distance| (id, left_distance + right_distance))
            })
            .min_by_key(|(_, distance)| *distance)
            .map(|(id, _)| id.clone())
            .ok_or(StoreError::NoMergeBase)
    }

    pub fn plan_merge(
        &self,
        left: &CommitId,
        right: &CommitId,
    ) -> Result<(CommitId, MergePlan), StoreError> {
        let base = self.merge_base(left, right)?;
        let base_context = self.get_commit(&base)?.context;
        let left_context = self.get_commit(left)?.context;
        let right_context = self.get_commit(right)?.context;
        Ok((
            base,
            plan_context_merge(&base_context, &left_context, &right_context),
        ))
    }

    pub fn create_merge_commit(
        &mut self,
        left: CommitId,
        right: CommitId,
        context: ContextSnapshot,
        created_at_ms: i64,
        message: String,
    ) -> Result<TranslationCommit, StoreError> {
        self.create_game_merge_commit(
            &default_game_id(),
            left,
            right,
            context,
            created_at_ms,
            message,
        )
    }

    pub fn create_game_merge_commit(
        &mut self,
        game_id: &GameId,
        left: CommitId,
        right: CommitId,
        context: ContextSnapshot,
        created_at_ms: i64,
        message: String,
    ) -> Result<TranslationCommit, StoreError> {
        self.game(game_id)?;
        self.get_commit(&left)?;
        self.get_commit(&right)?;
        let commit = TranslationCommit::create(
            vec![left, right],
            created_at_ms,
            vec![],
            String::new(),
            String::new(),
            context,
            vec![],
            vec![],
            ModelMetadata::default(),
            message,
        )
        .map_err(|_| StoreError::InvalidCommitId)?;
        self.put_commit(&commit)?;
        Ok(commit)
    }

    fn ancestor_distances(&self, start: &CommitId) -> Result<HashMap<CommitId, usize>, StoreError> {
        self.get_commit(start)?;
        let mut distances = HashMap::new();
        let mut queue = VecDeque::from([(start.clone(), 0usize)]);
        while let Some((id, distance)) = queue.pop_front() {
            if distances.get(&id).is_some_and(|old| *old <= distance) {
                continue;
            }
            distances.insert(id.clone(), distance);
            for parent in self.get_commit(&id)?.parents {
                queue.push_back((parent, distance + 1));
            }
        }
        Ok(distances)
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<String, StoreError> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        let path = self.blob_path(&digest)?;
        if !path.exists() {
            let parent = path.parent().expect("blob path always has a parent");
            fs::create_dir_all(parent)?;
            let temporary = parent.join(format!(".{digest}.tmp"));
            fs::write(&temporary, bytes)?;
            fs::rename(temporary, path)?;
        }
        Ok(digest)
    }

    pub fn get_blob(&self, digest: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.blob_path(digest)?;
        Ok(fs::read(path)?)
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, StoreError> {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidDigest);
        }
        Ok(self.blob_root.join(&digest[..2]).join(&digest[2..]))
    }
}

fn commit_exists(transaction: &Transaction<'_>, id: &CommitId) -> Result<bool, rusqlite::Error> {
    transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM commits WHERE id = ?1)",
        [&id.0],
        |row| row.get(0),
    )
}

fn validate_branch_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        Err(StoreError::InvalidBranchName)
    } else {
        Ok(())
    }
}

fn validate_game_id(id: &GameId) -> Result<(), StoreError> {
    if id.0.is_empty() || id.0.chars().any(char::is_whitespace) {
        Err(StoreError::InvalidGameId)
    } else {
        Ok(())
    }
}

fn default_game_id() -> GameId {
    GameId(DEFAULT_GAME_ID.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_blob_root() -> PathBuf {
        std::env::temp_dir().join(format!("terratranslate-test-{}", uuid::Uuid::new_v4()))
    }

    fn commit(parent: Option<CommitId>, at: i64, summary: &str) -> TranslationCommit {
        TranslationCommit::create(
            parent.into_iter().collect(),
            at,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot {
                summary: summary.into(),
                ..Default::default()
            },
            vec![],
            vec![],
            ModelMetadata::default(),
            summary.into(),
        )
        .unwrap()
    }

    #[test]
    fn persists_dag_and_plans_merge() {
        let mut store = SessionStore::in_memory(temp_blob_root()).unwrap();
        let root = commit(None, 1, "root");
        store.put_commit(&root).unwrap();
        let left = commit(Some(root.id.clone()), 2, "left");
        let right = commit(Some(root.id.clone()), 3, "right");
        store.put_commit(&left).unwrap();
        store.put_commit(&right).unwrap();

        assert_eq!(store.merge_base(&left.id, &right.id).unwrap(), root.id);
        let (_, plan) = store.plan_merge(&left.id, &right.id).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
    }

    #[test]
    fn branch_history_is_oldest_first_and_includes_merge_parents() {
        let mut store = SessionStore::in_memory(temp_blob_root()).unwrap();
        let root = commit(None, 1, "root");
        let left = commit(Some(root.id.clone()), 2, "left");
        let right = commit(Some(root.id.clone()), 3, "right");
        let merge = TranslationCommit::create(
            vec![left.id.clone(), right.id.clone()],
            4,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot {
                summary: "merge".into(),
                ..Default::default()
            },
            vec![],
            vec![],
            ModelMetadata::default(),
            "merge".into(),
        )
        .unwrap();
        for value in [&root, &left, &right, &merge] {
            store.put_commit(value).unwrap();
        }
        store.create_branch("main", &merge.id, 4).unwrap();

        let history = store.branch_history("main").unwrap();
        assert_eq!(
            history
                .iter()
                .map(|value| value.id.clone())
                .collect::<Vec<_>>(),
            vec![root.id, left.id, right.id, merge.id]
        );
    }

    #[test]
    fn branch_advance_is_compare_and_swap() {
        let mut store = SessionStore::in_memory(temp_blob_root()).unwrap();
        let root = commit(None, 1, "root");
        store.put_commit(&root).unwrap();
        store.create_branch("main", &root.id, 1).unwrap();
        let next = commit(Some(root.id.clone()), 2, "next");
        store.put_commit(&next).unwrap();
        assert!(store.advance_branch("main", &root.id, &next.id, 2).unwrap());
        assert!(!store.advance_branch("main", &root.id, &next.id, 3).unwrap());
    }

    #[test]
    fn blob_round_trip() {
        let root = temp_blob_root();
        let store = SessionStore::in_memory(&root).unwrap();
        let digest = store.put_blob(b"frame data").unwrap();
        assert_eq!(store.get_blob(&digest).unwrap(), b"frame data");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn timestamp_source_is_available_for_clients() {
        assert!(SystemTime::now().duration_since(UNIX_EPOCH).is_ok());
    }

    #[test]
    fn game_histories_keep_same_named_branches_independent() {
        let mut store = SessionStore::in_memory(temp_blob_root()).unwrap();
        let first_game = GameIdentity::from_stable_key(
            "first.exe",
            "First game",
            "C:/first.exe",
            None,
            "windows",
            "wine",
        );
        let second_game = GameIdentity::from_stable_key(
            "second.exe",
            "Second game",
            "C:/second.exe",
            None,
            "windows",
            "wine",
        );
        store.ensure_game(&first_game, 1).unwrap();
        store.ensure_game(&second_game, 1).unwrap();

        let first_root = store.game_branch(&first_game.id, "main").unwrap();
        let second_root = store.game_branch(&second_game.id, "main").unwrap();
        assert_ne!(first_root.head, second_root.head);

        let first_turn = commit(Some(first_root.head.clone()), 2, "first turn");
        let second_turn = commit(Some(second_root.head.clone()), 2, "second turn");
        store.put_commit(&first_turn).unwrap();
        store.put_commit(&second_turn).unwrap();
        assert!(
            store
                .advance_game_branch(&first_game.id, "main", &first_root.head, &first_turn.id, 2,)
                .unwrap()
        );
        assert!(
            store
                .advance_game_branch(
                    &second_game.id,
                    "main",
                    &second_root.head,
                    &second_turn.id,
                    2,
                )
                .unwrap()
        );

        let first_history = store.game_branch_history(&first_game.id, "main").unwrap();
        let second_history = store.game_branch_history(&second_game.id, "main").unwrap();
        assert!(first_history.iter().any(|value| value.id == first_turn.id));
        assert!(!first_history.iter().any(|value| value.id == second_turn.id));
        assert!(
            second_history
                .iter()
                .any(|value| value.id == second_turn.id)
        );
        assert!(!second_history.iter().any(|value| value.id == first_turn.id));
    }
}
