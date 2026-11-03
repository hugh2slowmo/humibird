//! Session storage: one SQLite database per profile.
//!
//! The transcript is append-only: messages get a monotonic `seq` per
//! session, and triggers reject `UPDATE` and `DELETE`. Resuming a session is
//! replaying its messages in `seq` order.
//!
//! Mutations are handed to the dedicated SQLite thread `sqlez` owns and waited
//! for, so call sites stay synchronous; reads use a per-thread connection and do
//! not queue at all.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use futures::executor::block_on;
use sqlez::domain::Domain;
use sqlez::thread_safe_connection::ThreadSafeConnection;

/// Migrations run in order and are never rewritten once shipped; `sqlez` refuses
/// to start when a stored step no longer matches.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    workspace  TEXT NOT NULL,
    title      TEXT,
    model      TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE messages (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    seq        INTEGER NOT NULL,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (session_id, seq)
);

CREATE INDEX messages_by_session ON messages (session_id, seq);

CREATE TRIGGER messages_are_append_only_on_update
BEFORE UPDATE ON messages
BEGIN
    SELECT RAISE(ABORT, 'messages are append-only');
END;

CREATE TRIGGER messages_are_append_only_on_delete
BEFORE DELETE ON messages
BEGIN
    SELECT RAISE(ABORT, 'messages are append-only');
END;

CREATE TABLE tool_calls (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    message_id TEXT NOT NULL REFERENCES messages(id),
    name       TEXT NOT NULL,
    args       TEXT NOT NULL,
    result     TEXT,
    status     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE usage (
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    message_id    TEXT NOT NULL REFERENCES messages(id),
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    cost_micros   INTEGER
);
"#,
    // Compaction is a recorded boundary, not an edit: the messages stay exactly
    // as they were, and a row says "everything up to this seq is represented by
    // this summary".
    r#"
CREATE TABLE compactions (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    upto_seq   INTEGER NOT NULL,
    summary    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX compactions_by_session ON compactions (session_id, upto_seq);
"#,
    // A subagent writes into a session of its own, so what it did survives a
    // restart. That session belongs to the one that delegated it, and to the call
    // that did the delegating: it is never a thread of its own.
    r#"
ALTER TABLE sessions ADD COLUMN parent_session_id TEXT REFERENCES sessions(id);
ALTER TABLE sessions ADD COLUMN parent_tool_call_id TEXT;

CREATE INDEX sessions_by_parent ON sessions (parent_session_id, parent_tool_call_id);
"#,
    // Removing a thread is not editing its transcript: the whole
    // thread leaves, and nothing of it stays behind. A row here says "these
    // sessions are on their way out", so the delete trigger lets their messages
    // go; it is written and read inside one transaction, and the cascade clears
    // it with the session it names, so no other statement can ever see it.
    r#"
CREATE TABLE deleting_sessions (
    id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE
);

DROP TRIGGER messages_are_append_only_on_delete;

CREATE TRIGGER messages_are_append_only_on_delete
BEFORE DELETE ON messages
WHEN NOT EXISTS (SELECT 1 FROM deleting_sessions WHERE id = OLD.session_id)
BEGIN
    SELECT RAISE(ABORT, 'messages are append-only');
END;
"#,
    // A thread records which provider it was talking to, because the profile's
    // provider can change: a model id means different things at different
    // providers, so opening an old thread has to be able to notice the difference.
    r#"
ALTER TABLE sessions ADD COLUMN provider TEXT;
"#,
    // What the person attached to a message, as JSON: a message bubble shows the
    // text they typed, but the model saw the files and images too, so a reopened
    // thread has to send them again. Empty (`[]`) for a turn with no attachments
    // and for every row written before this column existed.
    r#"
ALTER TABLE messages ADD COLUMN attachments TEXT;
"#,
];

/// A thread and every session it owns: the subagent sessions of the tasks it
/// delegated. Removing a thread removes all of them.
const THREAD_TREE: &str = "WITH RECURSIVE thread(id) AS (
     SELECT id FROM sessions WHERE id = ?
     UNION ALL
     SELECT session.id FROM sessions session JOIN thread ON session.parent_session_id = thread.id
 )";

struct Schema;

impl Domain for Schema {
    const NAME: &str = "humibird_sessions";
    const MIGRATIONS: &[&str] = MIGRATIONS;
}

#[derive(Clone)]
pub struct Store {
    connection: ThreadSafeConnection,
}

impl Store {
    /// Opens the database at `path`, creating the file and its directory when
    /// missing, and applying any pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let uri = path.to_string_lossy();
        let connection = block_on(
            ThreadSafeConnection::builder::<Schema>(&uri, true)
                .with_db_initialization_query("PRAGMA journal_mode = WAL;")
                .with_connection_initialize_query("PRAGMA foreign_keys = ON;")
                .build(),
        )
        .with_context(|| format!("failed to open {}", path.display()))?;

        Ok(Self { connection })
    }

    /// The threads: sessions a person can open, most recently updated first. A
    /// subagent's session is not one of them — its parent owns it.
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let rows = self.connection.select::<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<String>,
        )>(
            "SELECT id, workspace, title, model, provider, created_at, updated_at,
                    parent_session_id, parent_tool_call_id
             FROM sessions
             WHERE parent_session_id IS NULL
             ORDER BY updated_at DESC, rowid DESC",
        )?()?;

        Ok(rows
            .into_iter()
            .map(
                |(id, workspace, title, model, provider, created_at, updated_at, parent, call)| {
                    Session {
                        id,
                        workspace,
                        title,
                        model,
                        provider,
                        created_at,
                        updated_at,
                        parent,
                        parent_tool_call: call,
                    }
                },
            )
            .collect())
    }

    /// The transcript of one session, oldest first.
    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let rows = self
            .connection
            .select_bound::<&str, (String, String, i64, String, String, String, i64)>(
                "SELECT id, session_id, seq, role, content, COALESCE(attachments, '[]'), created_at
             FROM messages
             WHERE session_id = ?
             ORDER BY seq",
            )?(session_id)?;

        rows.into_iter()
            .map(
                |(id, session_id, seq, role, content, attachments, created_at)| {
                    Ok(Message {
                        id,
                        session_id,
                        seq,
                        role: Role::from_column(&role)?,
                        content,
                        attachments,
                        created_at,
                    })
                },
            )
            .collect()
    }

    pub fn create_session(
        &self,
        workspace: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<Session> {
        let now = now_millis();
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: workspace.to_owned(),
            title: None,
            model: model.map(str::to_owned),
            provider: provider.map(str::to_owned),
            created_at: now,
            updated_at: now,
            parent: None,
            parent_tool_call: None,
        };

        let pending = session.clone();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                i64,
                i64,
            )>(
                "INSERT INTO sessions (id, workspace, title, model, provider, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?((
                pending.id,
                pending.workspace,
                pending.title,
                pending.model,
                pending.provider,
                pending.created_at,
                pending.updated_at,
            ))?;
            Ok(())
        }))?;

        Ok(session)
    }

    /// Creates the session a delegated task writes to, owned by the session and the
    /// tool call that delegated it.
    pub fn create_subagent_session(
        &self,
        parent_session: &str,
        parent_tool_call: &str,
        workspace: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<Session> {
        let now = now_millis();
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: workspace.to_owned(),
            title: None,
            model: model.map(str::to_owned),
            provider: provider.map(str::to_owned),
            created_at: now,
            updated_at: now,
            parent: Some(parent_session.to_owned()),
            parent_tool_call: Some(parent_tool_call.to_owned()),
        };

        let pending = session.clone();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                i64,
                i64,
                Option<String>,
                Option<String>,
            )>(
                "INSERT INTO sessions
                     (id, workspace, title, model, provider, created_at, updated_at,
                      parent_session_id, parent_tool_call_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?((
                pending.id,
                pending.workspace,
                pending.title,
                pending.model,
                pending.provider,
                pending.created_at,
                pending.updated_at,
                pending.parent,
                pending.parent_tool_call,
            ))?;
            Ok(())
        }))?;

        Ok(session)
    }

    /// The sessions the tasks of `parent_session` write to, oldest first.
    pub fn subagent_sessions(&self, parent_session: &str) -> Result<Vec<Session>> {
        let rows = self.connection.select_bound::<&str, (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<String>,
        )>(
            "SELECT id, workspace, title, model, provider, created_at, updated_at,
                    parent_session_id, parent_tool_call_id
             FROM sessions
             WHERE parent_session_id = ?
             ORDER BY created_at, rowid",
        )?(parent_session)?;

        Ok(rows
            .into_iter()
            .map(
                |(id, workspace, title, model, provider, created_at, updated_at, parent, call)| {
                    Session {
                        id,
                        workspace,
                        title,
                        model,
                        provider,
                        created_at,
                        updated_at,
                        parent,
                        parent_tool_call: call,
                    }
                },
            )
            .collect())
    }

    /// Removes a thread: its transcript, the steps it ran, the usage it cost, its
    /// compaction boundaries, and the sessions of the subagents it delegated to.
    ///
    /// The transcript is append-only, so this is not an edit of it: the
    /// thread leaves whole, and the trigger is told so for exactly these sessions
    /// inside this transaction. Deleting a thread that is already gone succeeds —
    /// the caller's intent is that it is not there. A subagent's session is not a
    /// thread: it goes when the thread that owns it goes.
    pub fn delete_thread(&self, session_id: &str) -> Result<()> {
        let thread_id = session_id.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.with_savepoint("delete_thread", || {
                let found = connection.select_row_bound::<&str, (String, Option<String>)>(
                    "SELECT id, parent_session_id FROM sessions WHERE id = ?",
                )?(thread_id.as_str())?;

                let Some((found_id, parent)) = found else {
                    return Ok(());
                };
                anyhow::ensure!(
                    parent.is_none(),
                    "a subagent session is not a thread; deleting its thread removes it"
                );

                // A subagent cannot delegate again, so this is two
                // levels today; writing it as a walk keeps that out of the deletion
                // path if it ever changes.
                let statement = format!(
                    "{THREAD_TREE} INSERT INTO deleting_sessions (id) SELECT id FROM thread"
                );
                connection.exec_bound::<&str>(&statement)?(found_id.as_str())?;

                // Children before parents: the foreign keys are checked at once.
                for statement in [
                    "DELETE FROM usage WHERE session_id IN (SELECT id FROM thread)",
                    "DELETE FROM tool_calls WHERE session_id IN (SELECT id FROM thread)",
                    "DELETE FROM compactions WHERE session_id IN (SELECT id FROM thread)",
                    "DELETE FROM messages WHERE session_id IN (SELECT id FROM thread)",
                    "DELETE FROM sessions WHERE id IN (SELECT id FROM thread)",
                ] {
                    let statement = format!("{THREAD_TREE} {statement}");
                    connection.exec_bound::<&str>(&statement)?(found_id.as_str())?;
                }

                Ok(())
            })
        }))
    }

    /// Appends a message, assigning the next `seq` for the session inside the
    /// same savepoint that bumps the session's `updated_at`.
    pub fn append_message(&self, session_id: &str, role: Role, content: &str) -> Result<Message> {
        self.append_message_with(session_id, role, content, "[]")
    }

    /// Appends a message that carries attachments, as JSON in the shape
    /// `humibird_providers` writes. The bytes are opaque here, deliberately: the
    /// store never needs to know what an image is.
    pub fn append_message_with(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
        attachments: &str,
    ) -> Result<Message> {
        let message = Message {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_owned(),
            seq: 0,
            role,
            content: content.to_owned(),
            attachments: attachments.to_owned(),
            created_at: now_millis(),
        };

        let pending = message.clone();
        let seq = block_on(self.connection.write(move |connection| {
            connection.with_savepoint("append_message", || {
                let mut next_seq = connection.select_row_bound::<&str, i64>(
                    "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?",
                )?;
                let seq = next_seq(pending.session_id.as_str())?
                    .context("a MAX() aggregate always returns a row")?;

                connection.exec_bound::<(String, String, i64, &str, String, String, i64)>(
                    "INSERT INTO messages (id, session_id, seq, role, content, attachments, created_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                )?((
                    pending.id.clone(),
                    pending.session_id.clone(),
                    seq,
                    pending.role.as_str(),
                    pending.content.clone(),
                    pending.attachments.clone(),
                    pending.created_at,
                ))?;

                connection.exec_bound::<(&str, i64)>(
                    "UPDATE sessions SET updated_at = ? WHERE id = ?",
                )?((pending.session_id.as_str(), pending.created_at))?;

                Ok(seq)
            })
        }))?;

        Ok(Message { seq, ..message })
    }

    /// Records one tool call of a turn, attached to the assistant message of the
    /// round that asked for it. The message is the transcript; this is the step,
    /// so a reopened session can show what ran and whether it worked.
    pub fn append_tool_call(&self, session_id: &str, call: ToolCallRecord) -> Result<()> {
        let session_id = session_id.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str, &str, &str, &str, &str, &str, i64)>(
                "INSERT INTO tool_calls
                     (id, session_id, message_id, name, args, result, status, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )?((
                call.id.as_str(),
                session_id.as_str(),
                call.message_id.as_str(),
                call.name.as_str(),
                call.arguments.as_str(),
                call.result.as_str(),
                call.status.as_str(),
                now_millis(),
            ))?;
            Ok(())
        }))
    }

    /// The tool calls of a session, in the order they ran.
    pub fn tool_calls(&self, session_id: &str) -> Result<Vec<ToolCallRecord>> {
        let rows = self
            .connection
            .select_bound::<&str, (String, String, String, String, Option<String>, String)>(
                "SELECT id, message_id, name, args, result, status
                 FROM tool_calls
                 WHERE session_id = ?
                 ORDER BY rowid",
            )?(session_id)?;

        rows.into_iter()
            .map(|(id, message_id, name, arguments, result, status)| {
                Ok(ToolCallRecord {
                    id,
                    message_id,
                    name,
                    arguments,
                    result: result.unwrap_or_default(),
                    status: ToolCallStatus::from_column(&status)?,
                })
            })
            .collect()
    }

    /// Records that everything up to `upto_seq` is represented by `summary` from
    /// now on. The messages are untouched: the transcript keeps what the user can
    /// still scroll to.
    pub fn record_compaction(&self, session_id: &str, upto_seq: i64, summary: &str) -> Result<()> {
        let session_id = session_id.to_owned();
        let summary = summary.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(String, &str, i64, String, i64)>(
                "INSERT INTO compactions (id, session_id, upto_seq, summary, created_at)
                     VALUES (?, ?, ?, ?, ?)",
            )?((
                uuid::Uuid::new_v4().to_string(),
                session_id.as_str(),
                upto_seq,
                summary,
                now_millis(),
            ))?;
            Ok(())
        }))
    }

    /// The compaction that covers the most of the session, if any.
    pub fn latest_compaction(&self, session_id: &str) -> Result<Option<Compaction>> {
        let row = self
            .connection
            .select_row_bound::<&str, (String, i64, String, i64)>(
                "SELECT session_id, upto_seq, summary, created_at
                 FROM compactions
                 WHERE session_id = ?
                 ORDER BY upto_seq DESC, rowid DESC
                 LIMIT 1",
            )?(session_id)?;

        Ok(
            row.map(|(session_id, upto_seq, summary, created_at)| Compaction {
                session_id,
                upto_seq,
                summary,
                created_at,
            }),
        )
    }

    /// Records the token usage of one reply. `cost_micros` stays null until the
    /// store knows model pricing.
    pub fn record_usage(&self, session_id: &str, message_id: &str, usage: Usage) -> Result<()> {
        let session_id = session_id.to_owned();
        let message_id = message_id.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(String, String, i64, i64, i64)>(
                "INSERT INTO usage (session_id, message_id, input_tokens, output_tokens, cached_tokens)
                     VALUES (?, ?, ?, ?, ?)",
            )?((session_id, message_id, usage.input_tokens, usage.output_tokens, usage.cached_tokens))?;
            Ok(())
        }))
    }

    /// Records which model a session is talking to, so reopening it does not
    /// silently continue on a different one. Like a rename, this is not activity:
    /// `updated_at` stays put and the sidebar order does not move.
    pub fn set_session_model(&self, session_id: &str, model: &str) -> Result<()> {
        let session_id = session_id.to_owned();
        let model = model.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str)>("UPDATE sessions SET model = ? WHERE id = ?")?(
                (model.as_str(), session_id.as_str()),
            )?;
            Ok(())
        }))
    }

    /// Records which provider a session is talking to. The model id alone does not
    /// say: the same id can be a different model at a different provider, so
    /// reopening a thread has to know both.
    pub fn set_session_provider(&self, session_id: &str, provider: &str) -> Result<()> {
        let session_id = session_id.to_owned();
        let provider = provider.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection
                .exec_bound::<(&str, &str)>("UPDATE sessions SET provider = ? WHERE id = ?")?(
                (
                provider.as_str(),
                session_id.as_str(),
            )
            )?;
            Ok(())
        }))
    }

    /// Names a session. Leaving `updated_at` alone keeps a rename from
    /// reordering the sidebar.
    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        let session_id = session_id.to_owned();
        let title = title.to_owned();
        block_on(self.connection.write(move |connection| -> Result<()> {
            connection.exec_bound::<(&str, &str)>("UPDATE sessions SET title = ? WHERE id = ?")?(
                (title.as_str(), session_id.as_str()),
            )?;
            Ok(())
        }))
    }

    /// The usage of the newest reply, which is how big the conversation the
    /// provider last saw was. Auto-compaction measures itself against this.
    pub fn latest_usage(&self, session_id: &str) -> Result<Option<Usage>> {
        let row = self.connection.select_row_bound::<&str, (i64, i64, i64)>(
            "SELECT input_tokens, output_tokens, cached_tokens
                 FROM usage
                 WHERE session_id = ?
                 ORDER BY rowid DESC
                 LIMIT 1",
        )?(session_id)?;

        Ok(
            row.map(|(input_tokens, output_tokens, cached_tokens)| Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            }),
        )
    }

    /// Summed usage of a session.
    pub fn session_usage(&self, session_id: &str) -> Result<Usage> {
        let totals = self.connection.select_row_bound::<&str, (i64, i64, i64)>(
            "SELECT COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM usage
             WHERE session_id = ?",
        )?(session_id)?
        .context("an aggregate without GROUP BY always returns a row")?;

        Ok(Usage {
            input_tokens: totals.0,
            output_tokens: totals.1,
            cached_tokens: totals.2,
        })
    }

    #[cfg(test)]
    fn execute(&self, sql: &'static str) -> Result<()> {
        block_on(
            self.connection
                .write(move |connection| connection.exec(sql)?()),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    pub title: Option<String>,
    pub model: Option<String>,
    /// The provider the thread was talking to. `None` for a session written before
    /// the column existed.
    pub provider: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The session that delegated this one, when this is a subagent's.
    pub parent: Option<String>,
    /// The tool call that delegated it, so its card can be found again.
    pub parent_tool_call: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    pub role: Role,
    pub content: String,
    /// The attachments of this turn as JSON, in the shape `humibird_providers`
    /// wrote them. The store keeps the bytes; the runtime decides what they mean.
    pub attachments: String,
    pub created_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    fn from_column(value: &str) -> Result<Self> {
        Ok(match value {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            other => anyhow::bail!("unknown message role {other:?}"),
        })
    }
}

/// One tool call as it happened. `status` mirrors what the step showed: the call
/// ran and worked, or it ran and failed — a denied call is a failed one, because
/// the model is told the same thing either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// The provider's call id, so a stored result can be paired with its call.
    pub id: String,
    /// The assistant message of the round that asked for the call.
    pub message_id: String,
    pub name: String,
    /// The arguments as JSON text, exactly as the model produced them.
    pub arguments: String,
    /// What the model was told, verbatim.
    pub result: String,
    pub status: ToolCallStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallStatus {
    Done,
    Failed,
}

impl ToolCallStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn from_column(value: &str) -> Result<Self> {
        Ok(match value {
            "done" => Self::Done,
            "failed" => Self::Failed,
            other => anyhow::bail!("unknown tool call status {other:?}"),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
}

/// One recorded compaction. `upto_seq` is the last message it replaces in the
/// request, not in the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    pub session_id: String,
    pub upto_seq: i64,
    pub summary: String,
    pub created_at: i64,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn open_store(directory: &tempfile::TempDir) -> (Store, PathBuf) {
        let path = directory.path().join("sessions/sessions.sqlite3");
        let store = Store::open(&path).expect("open the store");
        (store, path)
    }

    #[test]
    fn a_message_keeps_the_attachments_it_was_written_with() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _path) = open_store(&directory);
        let session = store
            .create_session("/workspace", Some("deepseek"), Some("deepseek-flash"))
            .expect("the session");
        let attachments = r#"[{"kind":"text","name":"notes.md","text":"the build is flaky"}]"#;

        store
            .append_message_with(&session.id, Role::User, "look", attachments)
            .expect("append");

        let stored = store.messages(&session.id).expect("read back");
        assert_eq!(stored[0].content, "look", "the bubble keeps what was typed");
        assert_eq!(
            stored[0].attachments, attachments,
            "and the model keeps what came with it"
        );

        // A message with none is written as an empty list, not as NULL.
        store
            .append_message(&session.id, Role::Assistant, "found it")
            .expect("append");
        let stored = store.messages(&session.id).expect("read back");
        assert_eq!(stored[1].attachments, "[]");
    }

    #[test]
    fn a_subagent_session_belongs_to_the_call_that_delegated_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _path) = open_store(&directory);
        let parent = store
            .create_session("/workspace", Some("deepseek"), Some("deepseek-flash"))
            .expect("the parent session");

        let subagent = store
            .create_subagent_session(
                &parent.id,
                "call_1",
                "/workspace",
                Some("deepseek"),
                Some("deepseek-flash"),
            )
            .expect("the subagent session");

        assert_eq!(
            store.sessions().expect("threads"),
            vec![parent.clone()],
            "a subagent's session is not a thread"
        );
        let found = store.subagent_sessions(&parent.id).expect("subagents");
        assert_eq!(found, vec![subagent.clone()], "the parent owns it");
        assert_eq!(found[0].parent.as_deref(), Some(parent.id.as_str()));
        assert_eq!(found[0].parent_tool_call.as_deref(), Some("call_1"));
        assert_eq!(
            store.messages(&subagent.id).expect("messages"),
            Vec::new(),
            "its transcript starts empty and is written the same way"
        );
    }

    #[test]
    fn creates_the_database_under_the_profile_directory() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);

        assert!(path.is_file());
        assert_eq!(store.sessions().expect("sessions"), Vec::new());
    }

    #[test]
    fn reopening_keeps_sessions_and_their_messages() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);

        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), Some("deepseek-flash"))
            .expect("create session");
        store
            .append_message(&session.id, Role::User, "hello")
            .expect("append");
        drop(store);

        let reopened = Store::open(&path).expect("reopen");
        let sessions = reopened.sessions().expect("sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session.id);
        assert_eq!(sessions[0].workspace, "/tmp/workspace");
        assert_eq!(sessions[0].model.as_deref(), Some("deepseek-flash"));

        let messages = reopened.messages(&session.id).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "hello");
    }

    #[test]
    fn messages_get_the_next_sequence_number() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");

        let first = store
            .append_message(&session.id, Role::User, "one")
            .expect("first");
        let second = store
            .append_message(&session.id, Role::Assistant, "two")
            .expect("second");
        assert_eq!((first.seq, second.seq), (1, 2));

        // The sequence continues across a reopen, so resuming never reuses one.
        let reopened = Store::open(&path).expect("reopen");
        let third = reopened
            .append_message(&session.id, Role::User, "three")
            .expect("third");
        assert_eq!(third.seq, 3);

        let seqs: Vec<i64> = reopened
            .messages(&session.id)
            .expect("messages")
            .into_iter()
            .map(|message| message.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn stored_messages_cannot_be_updated_or_deleted() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        store
            .append_message(&session.id, Role::User, "original")
            .expect("append");

        for statement in [
            "UPDATE messages SET content = 'rewritten'",
            "DELETE FROM messages",
        ] {
            let error = store.execute(statement).expect_err(statement);
            assert!(
                format!("{error:#}").contains("append-only"),
                "{statement}: {error:#}"
            );
        }

        let messages = store.messages(&session.id).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "original");
    }

    #[test]
    fn appending_to_an_unknown_session_fails() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);

        let error = store
            .append_message("missing", Role::User, "hi")
            .expect_err("the session does not exist");

        assert!(
            format!("{error:#}").to_lowercase().contains("foreign key"),
            "{error:#}"
        );
    }

    #[test]
    fn records_token_usage_per_message() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let message = store
            .append_message(&session.id, Role::Assistant, "hi")
            .expect("message");

        assert_eq!(
            store.session_usage(&session.id).expect("usage"),
            Usage::default()
        );

        store
            .record_usage(
                &session.id,
                &message.id,
                Usage {
                    input_tokens: 120,
                    output_tokens: 30,
                    cached_tokens: 64,
                },
            )
            .expect("record usage");

        assert_eq!(
            store.session_usage(&session.id).expect("usage"),
            Usage {
                input_tokens: 120,
                output_tokens: 30,
                cached_tokens: 64,
            }
        );
    }

    #[test]
    fn reads_the_newest_usage_for_a_session() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let first = store
            .append_message(&session.id, Role::Assistant, "one")
            .expect("message");
        let second = store
            .append_message(&session.id, Role::Assistant, "two")
            .expect("message");

        assert_eq!(store.latest_usage(&session.id).expect("none"), None);

        for (message, input, output) in [(&first, 100, 10), (&second, 900, 90)] {
            store
                .record_usage(
                    &session.id,
                    &message.id,
                    Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cached_tokens: 0,
                    },
                )
                .expect("record");
        }

        let latest = store.latest_usage(&session.id).expect("latest");
        let latest = latest.expect("a row");
        assert_eq!(latest.input_tokens, 900);
        assert_eq!(latest.output_tokens, 90);
    }

    #[test]
    fn a_thread_records_the_provider_it_talks_to() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("ollama"), Some("qwen3"))
            .expect("session");

        assert_eq!(session.provider.as_deref(), Some("ollama"));

        store
            .set_session_provider(&session.id, "deepseek")
            .expect("record the new provider");

        // It outlives the process, so a restart reads back what the thread used.
        let reopened = Store::open(&path).expect("reopen");
        let sessions = reopened.sessions().expect("sessions");
        assert_eq!(sessions[0].provider.as_deref(), Some("deepseek"));
        assert_eq!(sessions[0].model.as_deref(), Some("qwen3"));
    }

    #[test]
    fn a_session_from_before_the_column_has_no_provider() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", None, Some("deepseek-flash"))
            .expect("session");

        assert_eq!(session.provider, None);
        assert_eq!(store.sessions().expect("sessions")[0].provider, None);
    }

    #[test]
    fn renames_a_session_without_touching_its_place_in_the_list() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let other = store
            .create_session("/tmp/other", Some("deepseek"), None)
            .expect("other");

        store
            .rename_session(&session.id, "Fix the flaky test")
            .expect("rename");

        let sessions = store.sessions().expect("sessions");
        assert_eq!(sessions[0].id, other.id, "a rename is not activity");
        assert_eq!(sessions[1].title.as_deref(), Some("Fix the flaky test"));

        // The name is durable, not just a sidebar decoration.
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened.sessions().expect("sessions")[1].title.as_deref(),
            Some("Fix the flaky test")
        );
    }

    #[test]
    fn records_the_model_a_session_switched_to() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let other = store
            .create_session("/tmp/other", Some("deepseek"), None)
            .expect("other");

        store
            .set_session_model(&session.id, "deepseek-v4-pro")
            .expect("set the model");

        let sessions = store.sessions().expect("sessions");
        assert_eq!(sessions[0].id, other.id, "a model switch is not activity");
        assert_eq!(sessions[1].model.as_deref(), Some("deepseek-v4-pro"));

        drop(store);
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened.sessions().expect("sessions")[1].model.as_deref(),
            Some("deepseek-v4-pro"),
            "the choice survives a restart"
        );
    }

    #[test]
    fn records_tool_calls_in_the_order_they_ran() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        store
            .append_message(&session.id, Role::User, "fix it")
            .expect("user message");
        let reply = store
            .append_message(&session.id, Role::Assistant, "done")
            .expect("assistant message");

        for (id, name, status) in [
            ("call_1", "read_file", ToolCallStatus::Done),
            ("call_2", "terminal", ToolCallStatus::Failed),
        ] {
            store
                .append_tool_call(
                    &session.id,
                    ToolCallRecord {
                        id: id.to_owned(),
                        message_id: reply.id.clone(),
                        name: name.to_owned(),
                        arguments: format!("{{\"path\":\"{name}\"}}"),
                        result: format!("{name} says no"),
                        status,
                    },
                )
                .expect("append the call");
        }

        let reopened = Store::open(&path).expect("reopen");
        let calls = reopened.tool_calls(&session.id).expect("tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, "{\"path\":\"read_file\"}");
        assert_eq!(calls[0].result, "read_file says no");
        assert_eq!(calls[0].status, ToolCallStatus::Done);
        assert_eq!(calls[0].message_id, reply.id);
        assert_eq!(calls[1].name, "terminal");
        assert_eq!(calls[1].status, ToolCallStatus::Failed);
    }

    #[test]
    fn a_tool_call_belongs_to_a_stored_message_and_its_own_session() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let other = store
            .create_session("/tmp/other", Some("deepseek"), None)
            .expect("other");
        let reply = store
            .append_message(&session.id, Role::Assistant, "done")
            .expect("message");

        let error = store
            .append_tool_call(
                &session.id,
                ToolCallRecord {
                    id: "call_1".to_owned(),
                    message_id: "missing".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                    result: String::new(),
                    status: ToolCallStatus::Done,
                },
            )
            .expect_err("the message does not exist");
        assert!(
            format!("{error:#}").to_lowercase().contains("foreign key"),
            "{error:#}"
        );

        store
            .append_tool_call(
                &session.id,
                ToolCallRecord {
                    id: "call_1".to_owned(),
                    message_id: reply.id.clone(),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                    result: "ok".to_owned(),
                    status: ToolCallStatus::Done,
                },
            )
            .expect("append");

        assert_eq!(store.tool_calls(&other.id).expect("calls"), Vec::new());
        assert_eq!(store.tool_calls(&session.id).expect("calls").len(), 1);
    }

    #[test]
    fn records_a_compaction_and_reads_the_newest_one() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let session = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("session");
        let other = store
            .create_session("/tmp/other", Some("deepseek"), None)
            .expect("other");

        assert_eq!(
            store.latest_compaction(&session.id).expect("none yet"),
            None
        );

        store
            .record_compaction(&session.id, 4, "we fixed the flaky test")
            .expect("record");
        store
            .record_compaction(&session.id, 10, "we shipped it")
            .expect("record");

        let latest = store.latest_compaction(&session.id).expect("latest");
        let latest = latest.expect("a compaction");
        assert_eq!(latest.upto_seq, 10);
        assert_eq!(latest.summary, "we shipped it");
        assert_eq!(
            store.latest_compaction(&other.id).expect("other"),
            None,
            "a compaction belongs to its session"
        );

        // The messages are still there: compaction is a boundary, not an edit.
        drop(store);
        let reopened = Store::open(&path).expect("reopen");
        assert!(
            reopened
                .latest_compaction(&session.id)
                .expect("latest")
                .is_some()
        );
    }

    #[test]
    fn a_compaction_belongs_to_a_stored_session() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);

        let error = store
            .record_compaction("missing", 1, "summary")
            .expect_err("the session does not exist");

        assert!(
            format!("{error:#}").to_lowercase().contains("foreign key"),
            "{error:#}"
        );
    }

    #[test]
    fn deleting_a_thread_takes_its_transcript_its_steps_and_its_subagents() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, path) = open_store(&directory);
        let thread = store
            .create_session("/tmp/workspace", Some("deepseek"), Some("deepseek-flash"))
            .expect("thread");
        store
            .append_message(&thread.id, Role::User, "try it out")
            .expect("user message");
        let reply = store
            .append_message(&thread.id, Role::Assistant, "delegating")
            .expect("assistant message");
        store
            .append_tool_call(
                &thread.id,
                ToolCallRecord {
                    id: "call_1".to_owned(),
                    message_id: reply.id.clone(),
                    name: "delegate".to_owned(),
                    arguments: "{}".to_owned(),
                    result: "done".to_owned(),
                    status: ToolCallStatus::Done,
                },
            )
            .expect("tool call");
        store
            .record_usage(
                &thread.id,
                &reply.id,
                Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cached_tokens: 0,
                },
            )
            .expect("usage");
        store
            .record_compaction(&thread.id, 1, "we tried it out")
            .expect("compaction");

        let subagent = store
            .create_subagent_session(
                &thread.id,
                "call_1",
                "/tmp/workspace",
                Some("deepseek"),
                None,
            )
            .expect("subagent session");
        store
            .append_message(&subagent.id, Role::User, "look around")
            .expect("subagent message");

        store.delete_thread(&thread.id).expect("delete the thread");

        assert_eq!(store.sessions().expect("threads"), Vec::new());
        for session_id in [&thread.id, &subagent.id] {
            assert!(
                store.messages(session_id).expect("messages").is_empty(),
                "{session_id} keeps no transcript"
            );
            assert!(store.tool_calls(session_id).expect("calls").is_empty());
            assert_eq!(
                store.session_usage(session_id).expect("usage"),
                Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_tokens: 0,
                }
            );
            assert_eq!(
                store.latest_compaction(session_id).expect("compaction"),
                None
            );
        }
        assert_eq!(
            store.subagent_sessions(&thread.id).expect("subagents"),
            Vec::new()
        );

        // Nothing survives a restart, and the append-only trigger is still in
        // place: the exception lasted exactly as long as the transaction.
        drop(store);
        let reopened = Store::open(&path).expect("reopen");
        assert!(reopened.sessions().expect("threads").is_empty());
        let fresh = reopened
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("a thread made after the deletion");
        reopened
            .append_message(&fresh.id, Role::User, "still append-only")
            .expect("append");
        let error = reopened
            .execute("DELETE FROM messages")
            .expect_err("messages are still append-only");
        assert!(format!("{error:#}").contains("append-only"), "{error:#}");
    }

    #[test]
    fn deleting_a_thread_leaves_other_threads_alone() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let doomed = store
            .create_session("/tmp/doomed", Some("deepseek"), None)
            .expect("doomed");
        let kept = store
            .create_session("/tmp/kept", Some("deepseek"), None)
            .expect("kept");
        store
            .append_message(&doomed.id, Role::User, "throw this away")
            .expect("doomed message");
        store
            .append_message(&kept.id, Role::User, "keep this")
            .expect("kept message");

        store.delete_thread(&doomed.id).expect("delete");

        let threads = store.sessions().expect("threads");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, kept.id);
        assert_eq!(
            store
                .messages(&kept.id)
                .expect("messages")
                .into_iter()
                .map(|message| message.content)
                .collect::<Vec<_>>(),
            vec!["keep this".to_owned()]
        );
    }

    #[test]
    fn a_subagent_session_is_deleted_only_with_the_thread_that_owns_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);
        let thread = store
            .create_session("/tmp/workspace", Some("deepseek"), None)
            .expect("thread");
        let subagent = store
            .create_subagent_session(
                &thread.id,
                "call_1",
                "/tmp/workspace",
                Some("deepseek"),
                None,
            )
            .expect("subagent session");

        let error = store
            .delete_thread(&subagent.id)
            .expect_err("a subagent's session is not a thread");
        assert!(format!("{error:#}").contains("not a thread"), "{error:#}");
        assert_eq!(
            store.subagent_sessions(&thread.id).expect("subagents"),
            vec![subagent],
            "it is untouched"
        );
    }

    #[test]
    fn deleting_a_thread_that_is_already_gone_succeeds() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);

        store
            .delete_thread("missing")
            .expect("the caller wanted it gone and it is");
    }

    #[test]
    fn sessions_are_listed_most_recent_first() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (store, _) = open_store(&directory);

        let older = store
            .create_session("/tmp/older", Some("deepseek"), None)
            .expect("older");
        let newer = store
            .create_session("/tmp/newer", Some("deepseek"), None)
            .expect("newer");

        let ids: Vec<String> = store
            .sessions()
            .expect("sessions")
            .into_iter()
            .map(|session| session.id)
            .collect();
        assert_eq!(ids, vec![newer.id, older.id]);
    }
}
