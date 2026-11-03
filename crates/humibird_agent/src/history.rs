//! What a conversation remembers, and how it becomes a request.
//!
//! The transcript is the model's view: the system prompt, the summary that stands
//! in for the older turns, and the turns themselves. It lives here rather than in
//! the window so that what the model sees and what the person sees can only drift
//! in one direction — the window's turns are built from the same rows.

use std::collections::HashSet;

use futures::StreamExt as _;
use humibird_providers::{Attachment, Message, Provider, Request};
use humibird_store::{
    Compaction, Role as StoredRole, Session, Store, ToolCallRecord, ToolCallStatus,
};

use crate::{Conversation, Entry, TurnOutcome, TurnRun};

/// Asks the model for a title the way Zed does (`summarize_thread_prompt.txt`).
pub const THREAD_TITLE_PROMPT: &str = "Generate a concise 3-7 word title for this conversation, omitting punctuation.\n\
Go straight to the title, without any preamble and prefix like `Here's a concise suggestion:...` or `Title:`.\n\
If the conversation is about a specific subject, include it in the title.\n\
Be descriptive. DO NOT speak in the first person.";

/// What the model is asked for when the older turns no longer fit.
pub const COMPACTION_PROMPT: &str = "Summarize this conversation for your own future reference. Keep what \
the user wants, decisions that were made, files and commands that matter, and what is \
still open. Write it as notes to yourself, in at most 300 words, with no preamble.";

/// Turns at the end of a thread that a compaction never summarizes: the model
/// needs the recent exchange in full to continue the conversation.
const KEEP_RECENT_TURNS: usize = 2;

/// How many turns are worth summarizing at all.
const COMPACT_AFTER_TURNS: usize = 4;

/// A thread compacts itself once the provider reports this much of the model's
/// context in use. Below it, the extra request is not worth the reset prefix.
/// The share of a model's window at which a thread summarizes itself. Public so
/// the window can colour its context meter at the same point.
pub const AUTO_COMPACT_AT: f64 = 0.75;

/// How the summary reaches the model: as its own user turn, right after the
/// system prompt, so the turns that follow keep their own roles.
pub fn summary_message(summary: &str) -> String {
    format!(
        "Earlier in this conversation, before the messages that follow, the user and \
         you said the following. It is a summary you wrote, not a new request:\n\n{summary}"
    )
}

impl Conversation {
    /// Points the conversation at a saved row and rebuilds what the model sees: the
    /// transcript, the summary that stands in for its older turns, and the size the
    /// provider last reported.
    pub fn open(&mut self, session: &Session) {
        self.session = Some(session.id.clone());
        self.transcript = self.read_transcript(&session.id);
        self.compaction = self.read_compaction(&session.id);
        self.context_tokens = self.measure_context();
        self.too_long = false;
    }

    /// Starts writing into a session the window has just created.
    pub fn attach_session(&mut self, id: impl Into<String>) {
        self.session = Some(id.into());
    }

    /// The summary that stands in for the older turns, if there is one.
    pub fn compaction(&self) -> Option<&Compaction> {
        self.compaction.as_ref()
    }

    /// Records a summary and forgets what the last request measured: the next reply
    /// re-measures it against the smaller history.
    pub fn set_compaction(&mut self, compaction: Option<Compaction>) {
        self.context_tokens = 0;
        self.too_long = false;
        self.compaction = compaction;
    }

    /// What the provider last reported as the size of the conversation, in tokens.
    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    /// The window the current model has. `None` means there is nothing to measure
    /// the conversation against, not that the thread is unlimited: a model nobody
    /// describes has no reported window, so it compacts only when a request is
    /// refused.
    pub fn context_window(&self) -> Option<u32> {
        self.context_limit()
    }

    /// Tells the conversation what window to measure against. `None` falls back to
    /// what the provider catalogues.
    pub fn set_context_window(&mut self, window: Option<u32>) {
        self.explicit_window = window;
    }

    /// Puts the user's message in the transcript — and in the store, so a stopped
    /// turn still leaves the message the person can see. Separate from
    /// [`start_turn`](Self::start_turn) because a turn that would not fit is
    /// summarized in between, and the split counts the turn being sent.
    pub fn submit(&mut self, text: &str, attachments: &[Attachment]) {
        self.append_user(text, attachments);
    }

    /// Hands back what the turn runs on: the request as the transcript stands now.
    /// `None` when there is no provider to ask.
    pub fn start_turn(&mut self, approved: HashSet<String>) -> Option<TurnRun> {
        let provider = self.provider.clone()?;
        let mut request = provider.request(self.messages());
        request.model = self.model.clone();
        request.tools = self.definitions.clone();
        request.thinking = self.thinking;
        Some(TurnRun::new(
            provider,
            self.tools.clone(),
            approved,
            request,
            self.writes(),
        ))
    }

    /// Whether the next turn should be summarized first (Zed compacts on the way
    /// into a request, for the same reason). Two things ask for it:
    ///
    /// - the provider reported the conversation past three quarters of the
    ///   model's limit, which needs a catalogue entry to measure against;
    /// - the last turn was refused for being too long, which needs nothing — a
    ///   model the catalogue does not describe has no limit, so a refusal is the
    ///   only thing that will ever tell the thread to summarize itself.
    pub fn compaction_due(&self) -> bool {
        let past_the_threshold = self.context_limit().is_some_and(|limit| {
            self.context_tokens >= (f64::from(limit) * AUTO_COMPACT_AT) as u32
        });
        (self.too_long || past_the_threshold) && self.compaction_split().is_some()
    }

    /// The request that would summarize the older turns, when there is one. This is
    /// the only way a request gets shorter, so it is also what the window asks
    /// before a turn that would not fit.
    pub fn compaction_run(&mut self) -> Option<CompactionRun> {
        let split = self.compaction_split()?;
        self.compaction_request(split)
    }

    /// How many turns a summary would stand in for, if any are left uncovered. A
    /// turn is the user's message and the reply to it; the last few stay whole.
    fn compaction_split(&self) -> Option<usize> {
        let turns = self.turns();
        if turns < COMPACT_AFTER_TURNS {
            return None;
        }
        let split = turns.checked_sub(KEEP_RECENT_TURNS)?;
        let covered = self
            .compaction
            .as_ref()
            .map(|compaction| compaction.upto_seq);
        let newly_covered = self
            .transcript
            .iter()
            .filter(|entry| entry.turn < split && !is_covered(entry.turn_seq, covered))
            .filter(|entry| entry.message.role == humibird_providers::Role::User)
            .count();
        (newly_covered > 0).then_some(split)
    }

    /// How many turns the transcript holds.
    fn turns(&self) -> usize {
        self.transcript.last().map_or(0, |entry| entry.turn + 1)
    }

    /// How much context the model takes, as the provider that will answer describes
    /// it. The model id alone would not do: the same id can be a different model at
    /// another provider, with a different window.
    fn context_limit(&self) -> Option<u32> {
        self.explicit_window.or_else(|| {
            self.provider
                .as_ref()
                .and_then(|provider| provider.context_limit(&self.model))
        })
    }

    /// The store's row for the turn that was just opened. The window needs it to
    /// place the divider between the turns a summary stands in for and the live
    /// ones.
    pub fn last_turn_seq(&self) -> Option<i64> {
        self.transcript.last().and_then(|entry| entry.turn_seq)
    }

    /// Writes what the turn did — to the store, so a reopened thread has it, and to
    /// the transcript, so the next request carries it. The transcript is
    /// append-only: a failed turn contributes its user message and nothing else,
    /// which is exactly what a reopened thread would send.
    pub fn finish_turn(&mut self, outcome: &TurnOutcome) {
        if let TurnOutcome::Failed {
            context_exceeded: true,
            ..
        } = outcome
        {
            self.too_long = true;
        }
        let TurnOutcome::Done {
            reply,
            usage,
            calls,
        } = outcome
        else {
            return;
        };
        // The request went through, so whatever was too long is not any more.
        self.too_long = false;
        if let Some(usage) = usage {
            self.context_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
        }
        // Only complete replies enter the transcript: a partial answer that never
        // reached `Finished` would be replayed as if it had.
        if reply.is_empty() {
            return;
        }
        // The reply answers the turn the last user message opened.
        let turn = self.transcript.last().map_or(0, |entry| entry.turn);
        let turn_seq = self.transcript.last().and_then(|entry| entry.turn_seq);
        self.transcript.push(Entry {
            message: Message::assistant(reply.clone()),
            turn,
            turn_seq,
        });

        let (Some(store), Some(session)) = (&self.store, &self.session) else {
            return;
        };
        let message = match store.append_message(session, StoredRole::Assistant, reply) {
            Ok(message) => message,
            Err(error) => {
                log::warn!("could not append the reply: {error:#}");
                return;
            }
        };
        if let Some(usage) = usage
            && let Err(error) = store.record_usage(
                session,
                &message.id,
                humibird_store::Usage {
                    input_tokens: i64::from(usage.prompt_tokens),
                    output_tokens: i64::from(usage.completion_tokens),
                    cached_tokens: i64::from(usage.cache_hit_tokens),
                },
            )
        {
            log::warn!("could not record token usage: {error:#}");
        }
        // The steps the person saw are attached to the reply so a reopened thread
        // can show them (invariant 6).
        for call in calls {
            let status = if call.failed {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Done
            };
            let record = ToolCallRecord {
                id: call.id.clone(),
                message_id: message.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: call.result.clone(),
                status,
            };
            if let Err(error) = store.append_tool_call(session, record) {
                log::warn!("could not record the {} call: {error:#}", call.name);
            }
        }
    }

    /// The messages one request carries: the system prompt, the summary if there is
    /// one, and every turn the summary does not stand in for.
    fn messages(&mut self) -> Vec<Message> {
        let mut messages = vec![Message::system(self.system_prompt())];
        if let Some(compaction) = &self.compaction {
            messages.push(Message::user(summary_message(&compaction.summary)));
        }
        let covered = self
            .compaction
            .as_ref()
            .map(|compaction| compaction.upto_seq);
        messages.extend(
            self.transcript
                .iter()
                .filter(|entry| !is_covered(entry.turn_seq, covered))
                .map(|entry| entry.message.clone()),
        );
        messages
    }

    /// The request that turns the first `split` turns into a summary, if there is
    /// anything to summarize and somewhere to record it.
    pub fn compaction_request(&mut self, split: usize) -> Option<CompactionRun> {
        let store = self.store.clone()?;
        let session = self.session.clone()?;
        let provider = self.provider.clone()?;
        // The boundary is the last summarized turn's first message: everything the
        // model saw up to and including it is what the summary stands in for.
        let upto_seq = self
            .transcript
            .iter()
            .filter(|entry| entry.turn < split)
            .filter_map(|entry| entry.turn_seq)
            .next_back()?;

        let covered = self
            .compaction
            .as_ref()
            .map(|compaction| compaction.upto_seq);
        let mut messages = vec![Message::system(self.system_prompt())];
        if let Some(compaction) = &self.compaction {
            messages.push(Message::user(summary_message(&compaction.summary)));
        }
        messages.extend(
            self.transcript
                .iter()
                .filter(|entry| entry.turn < split && !is_covered(entry.turn_seq, covered))
                .map(|entry| entry.message.clone()),
        );
        messages.push(Message::user(COMPACTION_PROMPT));

        let mut request = provider.request(messages);
        request.model = self.model.clone();
        request.temperature = Some(0.2);
        Some(CompactionRun {
            split,
            upto_seq,
            session,
            provider,
            store,
            request,
        })
    }

    /// Adds the user's message to the transcript and the store. The store's row
    /// number is what a later compaction records as its boundary.
    fn append_user(&mut self, text: &str, attachments: &[Attachment]) {
        let seq = match (&self.store, &self.session) {
            (Some(store), Some(session)) => {
                let attachments = Attachment::list_to_json(attachments);
                match store.append_message_with(session, StoredRole::User, text, &attachments) {
                    Ok(message) => Some(message.seq),
                    Err(error) => {
                        log::warn!("could not append the user message: {error:#}");
                        None
                    }
                }
            }
            _ => None,
        };
        // Every user message opens a new turn, whether or not the one before it was
        // answered.
        let turn = self.transcript.last().map_or(0, |entry| entry.turn + 1);
        let mut message = Message::user(text);
        message.attachments = attachments.to_vec();
        self.transcript.push(Entry {
            message,
            turn,
            turn_seq: seq,
        });
    }

    /// Where this turn's record goes, when there is a store to write to.
    fn writes(&self) -> Option<crate::turn::SessionWrites> {
        let store = self.store.clone()?;
        let session = self.session.clone()?;
        Some(crate::turn::SessionWrites {
            needs_title: self.names_thread && self.session_has_no_title(&store, &session),
            store,
            session,
        })
    }

    /// Whether the thread still needs a name. Read from the store rather than kept
    /// as a flag, so a title request that failed is tried again on the next turn.
    fn session_has_no_title(&self, store: &Store, session: &str) -> bool {
        match store.sessions() {
            Ok(sessions) => sessions
                .into_iter()
                .find(|row| row.id == session)
                .is_some_and(|row| row.title.is_none()),
            Err(error) => {
                log::warn!("could not read the session list: {error:#}");
                false
            }
        }
    }

    fn read_transcript(&self, session: &str) -> Vec<Entry> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let rows = match store.messages(session) {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("could not read the transcript of {session}: {error:#}");
                return Vec::new();
            }
        };

        let mut transcript: Vec<Entry> = Vec::with_capacity(rows.len());
        for row in rows {
            // A turn opens with the user's message; everything after it answers it.
            let turn = match transcript.last() {
                None => 0,
                Some(entry) => entry.turn + usize::from(row.role == StoredRole::User),
            };
            let turn_seq = match row.role {
                StoredRole::User => Some(row.seq),
                _ => transcript.last().and_then(|entry| entry.turn_seq),
            };
            transcript.push(Entry {
                message: match row.role {
                    StoredRole::User => {
                        let mut message = Message::user(row.content);
                        message.attachments = Attachment::list_from_json(&row.attachments);
                        message
                    }
                    StoredRole::Assistant => Message::assistant(row.content),
                    StoredRole::Tool => Message::tool(row.content),
                },
                turn,
                turn_seq,
            });
        }
        transcript
    }

    fn read_compaction(&self, session: &str) -> Option<Compaction> {
        let store = self.store.as_ref()?;
        store
            .latest_compaction(session)
            .inspect_err(|error| log::warn!("could not read the compaction: {error:#}"))
            .ok()
            .flatten()
    }

    /// The size of the conversation as the provider last reported it, from the
    /// store: a reopened thread does not start over from zero.
    fn measure_context(&self) -> u32 {
        let (Some(store), Some(session)) = (&self.store, &self.session) else {
            return 0;
        };
        store
            .latest_usage(session)
            .inspect_err(|error| log::warn!("could not read the last usage: {error:#}"))
            .ok()
            .flatten()
            .map(|usage| {
                u32::try_from(usage.input_tokens + usage.output_tokens).unwrap_or(u32::MAX)
            })
            .unwrap_or(0)
    }
}

/// Whether a turn has been replaced in the request by the summary.
fn is_covered(turn_seq: Option<i64>, covered: Option<i64>) -> bool {
    match (turn_seq, covered) {
        (Some(seq), Some(upto)) => seq <= upto,
        _ => false,
    }
}

/// One compaction ready to run: what to ask the model and where the boundary goes.
pub struct CompactionRun {
    split: usize,
    upto_seq: i64,
    session: String,
    provider: std::sync::Arc<dyn Provider>,
    store: Store,
    request: Request,
}

impl CompactionRun {
    /// Asks the model for the summary and records it. Nothing is written when the
    /// model answers with no text or the store refuses the row.
    pub async fn run(self) -> Option<Compaction> {
        let summary = read_all_text(self.provider.stream(self.request)).await?;
        if let Err(error) = self
            .store
            .record_compaction(&self.session, self.upto_seq, &summary)
        {
            log::warn!("could not record the compaction: {error:#}");
            return None;
        }
        log::info!(
            "summarized {} turn(s) of {} up to seq {}",
            self.split,
            self.session,
            self.upto_seq
        );
        self.store
            .latest_compaction(&self.session)
            .inspect_err(|error| log::warn!("could not re-read the compaction: {error:#}"))
            .ok()
            .flatten()
    }
}

/// Reads a whole stream as text. Used for requests whose answer is one block of
/// prose rather than a reply the person is watching.
async fn read_all_text(mut stream: humibird_providers::EventStream) -> Option<String> {
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(humibird_providers::StreamEvent::Text(delta)) => text.push_str(&delta),
            Ok(humibird_providers::StreamEvent::Finished { .. }) => break,
            Ok(_) => {
                // A reasoning delta or a call is not text for the summary.
            }
            Err(error) => {
                log::warn!("the stream failed: {error:#}");
                return None;
            }
        }
    }
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use humibird_profile::Profile;
    use humibird_providers::{DEFAULT_MODEL, EventStream, Request, Role, StopReason, StreamEvent};
    use tempfile::TempDir;

    use super::*;
    use crate::{RecordedCall, TurnOutcome};
    use humibird_providers::Usage;

    /// A provider that answers with nothing: these tests are about what a request
    /// carries, not about the reply.
    struct Silent;

    impl Provider for Silent {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new(DEFAULT_MODEL, messages)
        }

        /// This provider describes the model as the largest window the app knows,
        /// which is what the compaction tests measure against.
        fn context_limit(&self, _model: &str) -> Option<u32> {
            Some(1_000_000)
        }

        fn stream(&self, _request: Request) -> EventStream {
            futures::stream::iter(vec![Ok(StreamEvent::Finished {
                reason: StopReason::Stop,
            })])
            .boxed()
        }
    }

    /// A store and a profile inside the test's own directory.
    fn store(directory: &TempDir) -> (Profile, Store) {
        let profile = Profile::at(directory.path().join("profile"));
        let store = Store::open(&profile.database_path()).expect("store");
        (profile, store)
    }

    fn empty(profile: &Profile, store: &Store) -> Conversation {
        Conversation::new(
            Some(profile.clone()),
            "/workspace",
            DEFAULT_MODEL,
            Some(Arc::new(Silent)),
            Some(store.clone()),
        )
    }

    /// A conversation over a fresh database and the session the window would have
    /// created for it.
    fn conversation(directory: &TempDir) -> (Conversation, Session) {
        let (profile, store) = store(directory);
        let session = store
            .create_session("/workspace", Some("deepseek"), Some(DEFAULT_MODEL))
            .expect("session");
        let mut conversation = empty(&profile, &store);
        conversation.attach_session(session.id.clone());
        (conversation, session)
    }

    /// Sends one message and starts the turn over the transcript as it stands.
    fn turn(conversation: &mut Conversation, text: &str) -> TurnRun {
        conversation.submit(text, &[]);
        conversation.start_turn(HashSet::new()).expect("a turn")
    }

    fn said(reply: &str) -> TurnOutcome {
        TurnOutcome::Done {
            reply: reply.to_owned(),
            usage: None,
            calls: Vec::new(),
        }
    }

    fn roles(request: &Request) -> Vec<(Role, &str)> {
        request
            .messages
            .iter()
            .map(|message| (message.role, message.content.as_str()))
            .collect()
    }

    #[test]
    fn the_request_carries_the_turns_before_it() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, _session) = conversation(&directory);

        let first = turn(&mut conversation, "look");
        assert_eq!(
            roles(&first.request)[1..],
            [(Role::User, "look")],
            "the message being sent is part of the request"
        );
        conversation.finish_turn(&said("found it"));

        let second = turn(&mut conversation, "and now");
        assert_eq!(
            roles(&second.request)[1..],
            [
                (Role::User, "look"),
                (Role::Assistant, "found it"),
                (Role::User, "and now"),
            ]
        );
    }

    #[test]
    fn a_stopped_turn_keeps_its_message_and_nothing_else() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, _session) = conversation(&directory);

        turn(&mut conversation, "look");

        let next = turn(&mut conversation, "again");
        assert_eq!(
            roles(&next.request)[1..],
            [(Role::User, "look"), (Role::User, "again")],
            "the turn that never answered is one message, live and after a restart"
        );
    }

    #[test]
    fn a_reopened_conversation_sends_what_the_live_one_would() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, session) = conversation(&directory);
        turn(&mut conversation, "look");
        conversation.finish_turn(&said("found it"));

        let (profile, store) = store(&directory);
        let mut reopened = empty(&profile, &store);
        reopened.open(&session);

        let live = turn(&mut conversation, "and now");
        let restored = turn(&mut reopened, "and now");
        assert_eq!(roles(&live.request), roles(&restored.request));
    }

    #[test]
    fn a_summary_stands_in_for_the_turns_it_covers() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, session) = conversation(&directory);
        for reply in ["one", "two", "three", "four"] {
            turn(&mut conversation, "go");
            conversation.finish_turn(&said(reply));
        }

        let run = conversation
            .compaction_request(2)
            .expect("something to summarize");
        assert_eq!(
            run.request.messages.last().expect("the prompt").content,
            COMPACTION_PROMPT
        );
        assert_eq!(
            roles(&run.request)[1..5],
            [
                (Role::User, "go"),
                (Role::Assistant, "one"),
                (Role::User, "go"),
                (Role::Assistant, "two"),
            ],
            "only the turns before the boundary are summarized"
        );

        // What the window does with the run's result.
        let (_, store) = store(&directory);
        store
            .record_compaction(&session.id, run.upto_seq, "we went four times")
            .expect("record the compaction");
        conversation.set_compaction(store.latest_compaction(&session.id).expect("read"));
        assert_eq!(conversation.context_tokens(), 0, "re-measured next turn");

        let next = turn(&mut conversation, "five");
        assert_eq!(
            roles(&next.request)[1..],
            [
                (Role::User, summary_message("we went four times").as_str()),
                (Role::User, "go"),
                (Role::Assistant, "three"),
                (Role::User, "go"),
                (Role::Assistant, "four"),
                (Role::User, "five"),
            ],
            "the summary replaces the turns it covers"
        );
    }

    /// Four turns is what a summary is worth; a thread that was already summarized
    /// asks again only once new turns appear.
    #[test]
    fn a_summary_is_offered_once_the_older_turns_are_new() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, session) = conversation(&directory);

        for reply in ["one", "two", "three"] {
            turn(&mut conversation, "go");
            conversation.finish_turn(&said(reply));
        }
        assert!(
            conversation.compaction_run().is_none(),
            "three turns is not worth it"
        );

        turn(&mut conversation, "four");
        conversation.finish_turn(&said("four"));
        assert!(conversation.compaction_run().is_some());

        let run = conversation.compaction_run().expect("a compaction");
        let (_, store) = store(&directory);
        store
            .record_compaction(&session.id, run.upto_seq, "the first two")
            .expect("record the compaction");
        conversation.set_compaction(store.latest_compaction(&session.id).expect("read"));
        assert!(
            conversation.compaction_run().is_none(),
            "nothing was said since the summary"
        );
    }

    #[test]
    fn a_thread_compacts_itself_only_once_the_context_is_close_to_the_limit() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, _session) = conversation(&directory);
        for reply in ["one", "two", "three", "four"] {
            turn(&mut conversation, "go");
            conversation.finish_turn(&TurnOutcome::Done {
                reply: reply.to_owned(),
                usage: Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cache_hit_tokens: 0,
                }),
                calls: Vec::new(),
            });
        }
        assert_eq!(conversation.context_tokens(), 15);
        assert!(!conversation.compaction_due(), "15 tokens is nothing");

        conversation.finish_turn(&TurnOutcome::Done {
            reply: "five".to_owned(),
            usage: Some(Usage {
                prompt_tokens: 900_000,
                completion_tokens: 1_000,
                total_tokens: 901_000,
                cache_hit_tokens: 0,
            }),
            calls: Vec::new(),
        });
        assert!(
            conversation.compaction_due(),
            "past three quarters of the window"
        );
    }

    #[test]
    fn a_refusal_makes_the_next_turn_summarize_itself() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, _session) = conversation(&directory);
        // A model the catalogue does not describe has no limit to measure against,
        // so nothing but a refusal can tell this thread it is too big.
        conversation.set_model("a-model-the-catalogue-does-not-know");
        for reply in ["one", "two", "three", "four"] {
            turn(&mut conversation, "go");
            conversation.finish_turn(&said(reply));
        }
        assert!(
            !conversation.compaction_due(),
            "with no usage and no limit there is nothing to act on"
        );

        // An ordinary failure is not the context saying anything.
        conversation.finish_turn(&TurnOutcome::failed("the connection dropped"));
        assert!(!conversation.compaction_due());

        conversation.finish_turn(&TurnOutcome::Failed {
            message: "the conversation is larger than the model's context window".to_owned(),
            context_exceeded: true,
        });
        assert!(
            conversation.compaction_due(),
            "the next request has to be summarized first"
        );
        assert!(
            conversation.compaction_run().is_some(),
            "and there is a summary to make"
        );

        // Once it fit, the thread stops asking: the reply proves it.
        conversation.finish_turn(&said("five"));
        assert!(!conversation.compaction_due());
    }

    #[test]
    fn a_refusal_with_nothing_to_summarize_does_not_stall_the_thread() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, _session) = conversation(&directory);
        turn(&mut conversation, "go");
        conversation.finish_turn(&TurnOutcome::Failed {
            message: "the conversation is larger than the model's context window".to_owned(),
            context_exceeded: true,
        });

        assert!(
            !conversation.compaction_due(),
            "three turns or fewer is nothing a summary can stand in for"
        );
    }

    #[test]
    fn a_turn_without_a_provider_never_starts() {
        let directory = tempfile::tempdir().expect("temp dir");
        let profile = Profile::at(directory.path().join("profile"));
        let mut conversation =
            Conversation::new(Some(profile), "/workspace", "test-model", None, None);

        conversation.submit("look", &[]);
        assert!(conversation.start_turn(HashSet::new()).is_none());
        assert_eq!(conversation.last_turn_seq(), None);
    }

    #[test]
    fn a_call_is_recorded_with_the_reply() {
        let directory = tempfile::tempdir().expect("temp dir");
        let (mut conversation, session) = conversation(&directory);
        turn(&mut conversation, "look");
        conversation.finish_turn(&TurnOutcome::Done {
            reply: "wrote it".to_owned(),
            usage: None,
            calls: vec![RecordedCall {
                id: "call_1".to_owned(),
                name: "write_file".to_owned(),
                arguments: "{\"path\":\"a.md\"}".to_owned(),
                failed: false,
                result: "ok".to_owned(),
            }],
        });

        let (_, store) = store(&directory);
        let calls = store.tool_calls(&session.id).expect("the calls");
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "write_file");
    }
}
