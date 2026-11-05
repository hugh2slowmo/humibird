use anyhow::{Result, ensure};
use sqlez::statement::Statement;

use crate::{Role, Store};

/// A store is already scoped to one profile. Workspace filtering is optional;
/// active and subagent sessions are excluded for both search and passage reads.
#[derive(Clone, Copy)]
pub struct SearchScope<'a> {
    pub workspace: Option<&'a str>,
    pub exclude_session: Option<&'a str>,
}

#[derive(Debug)]
pub struct SearchHit {
    pub session_id: String,
    pub title: Option<String>,
    pub workspace: String,
    pub seq: i64,
    pub role: Role,
    pub excerpt: String,
    pub created_at: i64,
}

#[derive(Debug)]
pub struct SessionPassage {
    pub session_id: String,
    pub title: Option<String>,
    pub workspace: String,
    pub messages: Vec<PassageMessage>,
}

#[derive(Debug)]
pub struct PassageMessage {
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub created_at: i64,
    pub truncated: bool,
}

impl Store {
    /// Read a seven-sequence window around an existing user/assistant message.
    /// Tool output and attachments are excluded. Each message is capped at
    /// 1,500 characters, independently of the size of the saved transcript.
    pub fn session_passage(
        &self,
        session_id: &str,
        seq: i64,
        scope: SearchScope<'_>,
    ) -> Result<SessionPassage> {
        ensure!(seq > 0, "message sequence must be positive");
        let metadata = self.connection.select_row_bound::<(&str, Option<&str>, Option<&str>, i64), (Option<String>, String)>(
            "SELECT title, workspace FROM sessions s
             WHERE s.id = ?1 AND s.parent_session_id IS NULL
             AND (?2 IS NULL OR s.workspace = ?2) AND (?3 IS NULL OR s.id != ?3)
             AND EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id AND m.seq = ?4 AND m.role IN ('user', 'assistant'))"
        )?((session_id, scope.workspace, scope.exclude_session, seq))?;
        let (title, workspace) = metadata
            .ok_or_else(|| anyhow::anyhow!("passage is not available in this search scope"))?;
        let rows = self
            .connection
            .select_bound::<(&str, i64, i64), (i64, String, String, i64, bool)>(
                "SELECT seq, role, substr(content, 1, 1500), created_at, length(content) > 1500
             FROM messages WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3
             AND role IN ('user', 'assistant') ORDER BY seq LIMIT 7",
            )?((session_id, seq.saturating_sub(3), seq.saturating_add(3)))?;
        let messages = rows
            .into_iter()
            .map(|(seq, role, content, created_at, truncated)| {
                Ok(PassageMessage {
                    seq,
                    role: Role::from_column(&role)?,
                    content,
                    created_at,
                    truncated,
                })
            })
            .collect::<Result<_>>()?;
        Ok(SessionPassage {
            session_id: session_id.to_owned(),
            title,
            workspace,
            messages,
        })
    }

    /// Search message bodies using literal whitespace-separated terms (AND).
    /// Short terms cannot use trigram MATCH and are checked as substrings.
    pub fn search_messages(
        &self,
        query: &str,
        scope: SearchScope<'_>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        ensure!(
            (1..=20).contains(&limit),
            "search limit must be between 1 and 20"
        );
        ensure!(
            query.chars().count() <= 256,
            "search query exceeds 256 characters"
        );
        let terms: Vec<_> = query.split_whitespace().collect();
        ensure!(!terms.is_empty(), "search needs a nonempty query");
        ensure!(terms.len() <= 16, "search accepts at most 16 terms");
        let (long, short): (Vec<&str>, Vec<&str>) = terms
            .into_iter()
            .partition(|term| term.chars().count() >= 3);
        let indexed = !long.is_empty();
        let excerpt = if indexed {
            "snippet(message_search, 1, '', '', '…', 48)"
        } else {
            "substr(m.content, max(1, instr(lower(m.content), lower(?4)) - 80), 400)"
        };
        let source = if indexed {
            "message_search JOIN messages m ON m.id = message_search.message_id"
        } else {
            "messages m"
        };
        let order = if indexed { "bm25(message_search)," } else { "" };
        let mut sql = format!(
            "SELECT s.id, s.title, s.workspace, m.seq, m.role, {excerpt}, m.created_at
             FROM {source} JOIN sessions s ON s.id = m.session_id
             WHERE s.parent_session_id IS NULL AND m.role IN ('user', 'assistant')
             AND (?1 IS NULL OR s.workspace = ?1) AND (?2 IS NULL OR s.id != ?2)"
        );
        let mut values = Vec::new();
        if indexed {
            sql.push_str(" AND message_search MATCH ?4");
            values.push(
                long.iter()
                    .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" AND "),
            );
        }
        for term in short {
            sql.push_str(&format!(
                " AND instr(lower(m.content), lower(?{})) > 0",
                values.len() + 4
            ));
            values.push(term.to_owned());
        }
        sql.push_str(&format!(
            " ORDER BY {order} m.created_at DESC, s.id, m.seq LIMIT ?3"
        ));
        let mut statement = Statement::prepare(&self.connection, &sql)?;
        statement.bind(&(scope.workspace, scope.exclude_session, limit as i64), 1)?;
        for (index, value) in values.iter().enumerate() {
            statement.bind(value, index as i32 + 4)?;
        }
        type Row = (String, Option<String>, String, i64, String, String, i64);
        statement
            .rows::<Row>()?
            .into_iter()
            .map(
                |(session_id, title, workspace, seq, role, excerpt, created_at)| {
                    Ok(SearchHit {
                        session_id,
                        title,
                        workspace,
                        seq,
                        role: Role::from_column(&role)?,
                        excerpt: excerpt.chars().take(500).collect(),
                        created_at,
                    })
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passages_are_anchored_bounded_and_obey_search_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.db")).unwrap();
        let session = store.create_session("/one", None, None).unwrap();
        let scope = SearchScope {
            workspace: Some("/one"),
            exclude_session: None,
        };
        for _ in 0..12 {
            store
                .append_message(&session.id, Role::User, &"中".repeat(2000))
                .unwrap();
        }
        store
            .append_message(&session.id, Role::Tool, "private tool output")
            .unwrap();
        store
            .append_message(&session.id, Role::Assistant, "last answer")
            .unwrap();
        let passage = store.session_passage(&session.id, 6, scope).unwrap();
        assert_eq!(
            passage
                .messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![3, 4, 5, 6, 7, 8, 9]
        );
        assert!(
            passage
                .messages
                .iter()
                .all(|message| message.content.chars().count() == 1500 && message.truncated)
        );
        let tail = store.session_passage(&session.id, 14, scope).unwrap();
        assert_eq!(
            tail.messages
                .iter()
                .map(|message| message.seq)
                .collect::<Vec<_>>(),
            vec![11, 12, 14]
        );
        assert!(!tail.messages.last().unwrap().truncated);
        assert_eq!(
            store
                .session_passage(&session.id, 1, scope)
                .unwrap()
                .messages
                .len(),
            4
        );
        for seq in [0, -1, 13, 100, i64::MAX] {
            assert!(store.session_passage(&session.id, seq, scope).is_err());
        }
        assert!(store.session_passage("missing", 1, scope).is_err());
        assert!(
            store
                .session_passage(
                    &session.id,
                    1,
                    SearchScope {
                        workspace: Some("/elsewhere"),
                        ..scope
                    }
                )
                .is_err()
        );
        assert!(
            store
                .session_passage(
                    &session.id,
                    1,
                    SearchScope {
                        exclude_session: Some(&session.id),
                        ..scope
                    }
                )
                .is_err()
        );
        let child = store
            .create_subagent_session(&session.id, "call", "/one", None, None)
            .unwrap();
        store
            .append_message(&child.id, Role::User, "child")
            .unwrap();
        assert!(store.session_passage(&child.id, 1, scope).is_err());
        let other_profile = Store::open(&dir.path().join("other.db")).unwrap();
        assert!(
            other_profile
                .session_passage(&session.id, 1, scope)
                .is_err()
        );
        store.delete_thread(&session.id).unwrap();
        assert!(store.session_passage(&session.id, 1, scope).is_err());
    }

    #[test]
    fn search_ranks_and_scopes_history_without_tool_or_subagent_noise() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.db")).unwrap();
        let first = store.create_session("/one", None, None).unwrap();
        let other = store.create_session("/two", None, None).unwrap();
        let active = store.create_session("/one", None, None).unwrap();
        let child = store
            .create_subagent_session(&first.id, "call", "/one", None, None)
            .unwrap();
        store.rename_session(&first.id, "Database notes").unwrap();
        store
            .append_message(&first.id, Role::User, "database migration")
            .unwrap();
        store.append_message(&first.id, Role::Assistant, "database migration with numerous unrelated additional words explaining other details").unwrap();
        for (session, role) in [
            (&other.id, Role::User),
            (&active.id, Role::User),
            (&child.id, Role::User),
            (&first.id, Role::Tool),
        ] {
            store
                .append_message(session, role, "database migration")
                .unwrap();
        }
        let scope = SearchScope {
            workspace: Some("/one"),
            exclude_session: Some(&active.id),
        };
        let hits = store
            .search_messages("MIGRATION database", scope, 20)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].seq, 1);
        assert_eq!(hits[0].title.as_deref(), Some("Database notes"));
        assert!(hits.iter().all(|hit| hit.session_id == first.id));
        assert_eq!(
            store.search_messages("database", scope, 1).unwrap().len(),
            1
        );
        assert_eq!(
            store
                .search_messages(
                    "database",
                    SearchScope {
                        workspace: None,
                        ..scope
                    },
                    20
                )
                .unwrap()
                .len(),
            3
        );
        store.delete_thread(&first.id).unwrap();
        assert!(
            store
                .search_messages("database", scope, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_handles_chinese_short_terms_and_literal_operators() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("history.db")).unwrap();
        let session = store.create_session("/one", None, None).unwrap();
        store
            .append_message(
                &session.id,
                Role::User,
                "中文会话搜索功能 GPUI x 100% foo_bar \"quoted\" OR",
            )
            .unwrap();
        let scope = SearchScope {
            workspace: None,
            exclude_session: None,
        };
        for query in [
            "会话搜",
            "搜索",
            "搜",
            "搜索 GPUI",
            "x GPUI",
            "100%",
            "foo_bar",
            "\"quoted\"",
            "OR",
            "x",
        ] {
            assert_eq!(
                store.search_messages(query, scope, 8).unwrap().len(),
                1,
                "{query}"
            );
        }
        for query in ["NOT", "x missing", "%_", "nothing"] {
            assert!(
                store.search_messages(query, scope, 8).unwrap().is_empty(),
                "{query}"
            );
        }
        assert!(store.search_messages(" ", scope, 8).is_err());
        assert!(store.search_messages(&"a".repeat(257), scope, 8).is_err());
        assert!(store.search_messages("x", scope, 0).is_err());
        assert!(store.search_messages("x", scope, 21).is_err());
        assert!(store.search_messages(&"a ".repeat(17), scope, 8).is_err());
    }
}
