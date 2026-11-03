//! The turn loop.
//!
//! One turn asks the model, publishes what it says, runs the calls it makes —
//! asking the person first when a tool needs consent — and asks again until it
//! answers. The loop owns that policy and nothing else: it reports what happens
//! through [`AgentEvent`], reads the person through [`TurnHost`], and hands
//! blocking work to [`Background`]. It has no window and no GPUI, so a subagent is
//! a second turn and a test is a plain future.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::oneshot;
use futures::future::{Either, LocalBoxFuture};
use humibird_providers::ToolCall;
use humibird_providers::{Message, Provider, Request, StreamEvent, Usage, context_exceeded};
use humibird_store::Store;
use humibird_tools::{DELEGATION_CANCELLED, Delegation, Question, Tool};

use crate::{Answer, Decision, StepState, ToolStep};

/// How many tool rounds one turn may take before it is stopped.
pub const MAX_TOOL_ROUNDS: usize = 8;

/// How long a round may go without a single event before it is treated as a
/// stalled connection. Reasoning deltas count, so a thinking model keeps this
/// alive.
pub const PROVIDER_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// One thing the turn did, in the order it happened. The window applies these to
/// its own projection, which is the only turn-shaped state it holds.
#[derive(Debug)]
pub enum AgentEvent {
    /// The answer, as it streams.
    Text(String),
    /// A stretch of reasoning, as it streams.
    Thinking(String),
    /// A call the model made, at `index` among this turn's steps.
    Step { index: usize, step: ToolStep },
    /// The call moved on: running, or waiting for the person.
    StepState { index: usize, state: StepState },
    /// What the call returned, or why it failed.
    StepDone {
        index: usize,
        failed: bool,
        result: String,
    },
    /// The call needs the person's consent before it runs. Dropping `respond` — a
    /// stop, a closed window — reads as a denial. `key` is what an "always" is
    /// remembered under: the tool name, or the narrower key a tool reports.
    Approval {
        index: usize,
        name: String,
        key: String,
        summary: String,
        respond: oneshot::Sender<Decision>,
    },
    /// The call asks the person a question. Dropping `respond` reads as
    /// a decline.
    Question {
        index: usize,
        question: Question,
        respond: oneshot::Sender<Answer>,
    },
    /// The call hands a task to a subagent. The window runs the second
    /// conversation and answers with its final text; dropping `respond` reads as a
    /// cancellation. `call` is the model's id for the call, so the window can keep
    /// the subagent's own record and find it again.
    Delegate {
        index: usize,
        call: String,
        delegation: Delegation,
        respond: oneshot::Sender<String>,
    },
}

/// What the turn needs from the window while it runs: somewhere to put events,
/// and the one question only the window can answer.
pub trait TurnHost {
    fn event(&mut self, event: AgentEvent);

    /// Whether the person marked their next message as steering, which ends the
    /// turn at the next boundary instead of letting it run on. The queue is view
    /// state, so the turn asks rather than decides.
    fn steer_requested(&mut self) -> bool;
}

/// Where a turn puts the work that must not block it: a tool call that can take
/// minutes, and the clock its idle timeout measures. The window hands in GPUI's
/// background executor; a test may run everything inline.
pub trait Background {
    /// Runs `work` away from the turn's thread.
    fn run<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
        &self,
        work: F,
    ) -> LocalBoxFuture<'static, R>;

    /// A future that is ready after `duration`.
    fn sleep(&self, duration: Duration) -> LocalBoxFuture<'static, ()>;
}

/// What a turn did, once it is over. The loop reports; the window decides what to
/// show and what to store.
#[derive(Debug, PartialEq)]
pub enum TurnOutcome {
    /// The model finished. The reply is complete and belongs in the transcript.
    Done {
        reply: String,
        usage: Option<Usage>,
        calls: Vec<RecordedCall>,
    },
    /// The turn ended early, and `message` is what the person should read.
    ///
    /// `context_exceeded` says the provider refused the request as too long. That
    /// is the one failure the thread can do something about: it has to be
    /// summarized before the next request, and it is also the only sign a thread
    /// gets on a model whose limit the catalogue does not know.
    Failed {
        message: String,
        context_exceeded: bool,
    },
}

impl TurnOutcome {
    /// A failure that is only what it says.
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
            context_exceeded: false,
        }
    }
}

/// A call that ran, with what came back, kept for the transcript the window
/// writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub failed: bool,
    pub result: String,
}

/// A turn, ready to run: a snapshot of everything it needs, so the loop can await
/// without borrowing the window that owns the conversation.
pub struct TurnRun {
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Arc<dyn Tool>>>,
    /// The calls the person already allowed for this session, as of this turn. An
    /// "always" during the turn is remembered here, for the rest of it.
    approved: HashSet<String>,
    pub(crate) request: Request,
    /// Where the turn records what it did, when the conversation has a store. The
    /// turn writes; the window only reads.
    session: Option<SessionWrites>,
}

/// Where the record of a turn goes: the saved row it belongs to, and whether that
/// row still needs a name.
pub(crate) struct SessionWrites {
    pub(crate) store: Store,
    pub(crate) session: String,
    pub(crate) needs_title: bool,
}

impl TurnRun {
    pub(crate) fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<Vec<Arc<dyn Tool>>>,
        approved: HashSet<String>,
        request: Request,
        session: Option<SessionWrites>,
    ) -> Self {
        Self {
            provider,
            tools,
            approved,
            request,
            session,
        }
    }

    /// Runs the turn to its end, records it, and comes back with what happened.
    /// Every await point here is cancellation: dropping the future stops the turn,
    /// which is what the stop button does.
    pub async fn run<B: Background>(
        mut self,
        host: &mut dyn TurnHost,
        background: &B,
    ) -> TurnOutcome {
        let outcome = self.drive(host, background).await;
        self.record(&outcome).await;
        outcome
    }

    /// The rounds themselves: stream, publish, run the calls, ask again.
    async fn drive<B: Background>(
        &mut self,
        host: &mut dyn TurnHost,
        background: &B,
    ) -> TurnOutcome {
        // What the model sees this turn: the history the request carries, plus the
        // calls and results it asks for along the way. Tool plumbing stays here —
        // the stored transcript is the conversation the person can see.
        let mut conversation = self.request.messages.clone();
        let mut reply = String::new();
        let mut usage = None;
        let mut calls = Vec::new();
        let mut steps = 0;

        for round in 0..MAX_TOOL_ROUNDS {
            let mut round_request = self.request.clone();
            round_request.messages = conversation.clone();
            let mut asked = Vec::new();
            log::info!(
                "round {} of at most {MAX_TOOL_ROUNDS}: {} message(s)",
                round + 1,
                conversation.len()
            );
            let mut stream = self.provider.stream(round_request);

            let mut finished = false;
            loop {
                // A stalled connection must become an error, not a turn that waits
                // forever with nothing on screen.
                let event = match futures::future::select(
                    stream.next(),
                    background.sleep(PROVIDER_IDLE_TIMEOUT),
                )
                .await
                {
                    Either::Left((event, _)) => event,
                    Either::Right((_, _)) => {
                        log::warn!(
                            "no output for {PROVIDER_IDLE_TIMEOUT:?}; giving up on the round"
                        );
                        return TurnOutcome::failed(format!(
                            "the model stopped responding after {} seconds",
                            PROVIDER_IDLE_TIMEOUT.as_secs()
                        ));
                    }
                };
                let Some(event) = event else {
                    log::warn!("the reply stream ended without a finish reason");
                    break;
                };

                match event {
                    Ok(StreamEvent::Text(delta)) => {
                        reply.push_str(&delta);
                        host.event(AgentEvent::Text(delta));
                    }
                    Ok(StreamEvent::Reasoning(delta)) => {
                        host.event(AgentEvent::Thinking(delta));
                    }
                    Ok(StreamEvent::ToolCall(call)) => asked.push(call),
                    Ok(StreamEvent::Usage(reported)) => usage = Some(reported),
                    Ok(StreamEvent::Finished { .. }) => {
                        finished = true;
                        break;
                    }
                    Err(error) => {
                        // The cause is what says whether this is the one failure the
                        // thread can act on, so the text stays as it arrived.
                        let context_exceeded = context_exceeded(&error).is_some();
                        if context_exceeded {
                            log::warn!(
                                "the request did not fit the model's context window: {error:#}"
                            );
                        }
                        return TurnOutcome::Failed {
                            message: error.to_string(),
                            context_exceeded,
                        };
                    }
                }
            }

            if !finished {
                return TurnOutcome::failed("the reply ended before it finished; nothing was kept");
            }
            if asked.is_empty() {
                return TurnOutcome::Done {
                    reply,
                    usage,
                    calls,
                };
            }

            conversation.push(Message::tool_calls(asked.clone()));
            for call in asked {
                let (failed, result) = self.handle_call(&call, steps, host, background).await;
                steps += 1;
                conversation.push(Message::tool_result(call.id.clone(), result.clone()));
                calls.push(RecordedCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    failed,
                    result,
                });
            }

            // A steering message ends the turn here, so it can be sent as the next
            // one instead of waiting for the model to finish.
            if host.steer_requested() {
                log::info!("a queued message asked to steer; ending the turn here");
                return TurnOutcome::Done {
                    reply,
                    usage,
                    calls,
                };
            }
        }

        log::warn!("stopping after {MAX_TOOL_ROUNDS} tool rounds");
        TurnOutcome::failed("the model kept asking for tools; stopped to avoid a loop")
    }

    /// Names the thread when it still needs a name. Everything else the turn did is
    /// written by the conversation when the window tells it the outcome.
    async fn record(&self, outcome: &TurnOutcome) {
        let TurnOutcome::Done { reply, .. } = outcome else {
            return;
        };
        let Some(writes) = &self.session else {
            return;
        };
        if reply.is_empty() || !writes.needs_title {
            return;
        }
        self.name(writes, reply).await;
    }

    /// Asks for the thread's name once, and falls back to its first message when the
    /// model gives nothing usable: an unnamed thread is worse than a plain one.
    async fn name(&self, writes: &SessionWrites, reply: &str) {
        let mut messages = self.request.messages.clone();
        messages.push(Message::assistant(reply));
        messages.push(Message::user(crate::THREAD_TITLE_PROMPT));
        let mut request = self.provider.request(messages);
        // Zed only sets a temperature here and stops reading at the first newline. A
        // small `max_tokens` looks tempting, but a thinking model spends it on
        // reasoning and answers with no text at all.
        request.temperature = Some(0.2);
        log::debug!("asking {} for a thread title", writes.session);

        let title = match read_thread_title(self.provider.stream(request)).await {
            Some(title) => Some(title),
            None => {
                let fallback = self
                    .request
                    .messages
                    .iter()
                    .find(|message| message.role == humibird_providers::Role::User)
                    .and_then(|message| fallback_title(&message.content));
                if fallback.is_some() {
                    log::warn!("naming {} from its first message instead", writes.session);
                }
                fallback
            }
        };
        let Some(title) = title else {
            return;
        };
        match writes.store.rename_session(&writes.session, &title) {
            Ok(()) => log::info!("named session {}: {title}", writes.session),
            Err(error) => log::warn!("could not name the session: {error:#}"),
        }
    }

    /// Runs one call: publishes the step, asks the person when the tool needs it,
    /// and returns whether it failed plus what the model reads.
    async fn handle_call<B: Background>(
        &mut self,
        call: &ToolCall,
        index: usize,
        host: &mut dyn TurnHost,
        background: &B,
    ) -> (bool, String) {
        let summary = summarize_arguments(&call.arguments);
        let asks_user = self.asks_user(&call.name);
        let key = self.approval_key(call);
        let needs_approval = !asks_user && self.approval_needed(&call.name, &key);

        host.event(AgentEvent::Step {
            index,
            step: ToolStep {
                name: call.name.clone(),
                summary: summary.clone(),
                arguments: call.arguments.clone(),
                result: String::new(),
                state: if asks_user || needs_approval {
                    StepState::Awaiting
                } else {
                    StepState::Running
                },
            },
        });

        if asks_user {
            let (failed, result) = self.ask(call, index, host).await;
            host.event(AgentEvent::StepDone {
                index,
                failed,
                result: result.clone(),
            });
            return (failed, result);
        }

        if self.delegates(&call.name) {
            let (failed, result) = self.delegate(call, index, host).await;
            host.event(AgentEvent::StepDone {
                index,
                failed,
                result: result.clone(),
            });
            return (failed, result);
        }

        if needs_approval {
            let (respond, decided) = oneshot::channel();
            host.event(AgentEvent::Approval {
                index,
                name: call.name.clone(),
                key: key.clone(),
                summary,
                respond,
            });
            let decision = decided.await.unwrap_or(Decision::Deny);
            host.event(AgentEvent::StepState {
                index,
                state: StepState::Running,
            });
            match decision {
                Decision::Deny => {
                    log::info!("the person denied {}", call.name);
                    let denied = "error: the user denied this call".to_owned();
                    host.event(AgentEvent::StepDone {
                        index,
                        failed: true,
                        result: denied.clone(),
                    });
                    return (true, denied);
                }
                Decision::Always => {
                    self.approved.insert(key);
                }
                Decision::Once => {}
            }
        }

        let (failed, result) = self.run_tool(call, background).await;
        host.event(AgentEvent::StepDone {
            index,
            failed,
            result: result.clone(),
        });
        (failed, result)
    }

    /// Asks the person what the call should answer with. A question the card
    /// cannot show is answered with why, so the model can ask again in a shape the
    /// person can act on.
    async fn ask(&self, call: &ToolCall, index: usize, host: &mut dyn TurnHost) -> (bool, String) {
        let question = match Question::parse(&call.arguments) {
            Ok(question) => question,
            Err(error) => return (true, format!("error: {error}")),
        };
        let (respond, answered) = oneshot::channel();
        host.event(AgentEvent::Question {
            index,
            question,
            respond,
        });
        match answered.await {
            Ok(Answer::Selected(text)) => (false, humibird_tools::answer_text(&text)),
            Ok(Answer::Declined) | Err(_) => (true, humibird_tools::DECLINED_TEXT.to_owned()),
        }
    }

    /// Hands the task to a subagent. The window runs it — a second conversation with
    /// its own context — and this awaits its final text. A task that
    /// cannot be parsed is answered with why, the way a broken question is.
    async fn delegate(
        &self,
        call: &ToolCall,
        index: usize,
        host: &mut dyn TurnHost,
    ) -> (bool, String) {
        let delegation = match Delegation::parse(&call.arguments) {
            Ok(delegation) => delegation,
            Err(error) => return (true, format!("error: {error}")),
        };
        let (respond, answer) = oneshot::channel();
        host.event(AgentEvent::Delegate {
            index,
            call: call.id.clone(),
            delegation,
            respond,
        });
        match answer.await {
            Ok(text) => (false, text),
            Err(_) => (true, DELEGATION_CANCELLED.to_owned()),
        }
    }

    /// Runs the call away from the turn: `terminal` can take minutes, and even
    /// `grep` walks the whole tree.
    async fn run_tool<B: Background>(&self, call: &ToolCall, background: &B) -> (bool, String) {
        let tools = self.tools.clone();
        let call = call.clone();
        background.run(move || run(&tools, &call)).await
    }

    fn approval_needed(&self, name: &str, key: &str) -> bool {
        self.tool(name).is_some_and(|tool| tool.needs_approval()) && !self.approved.contains(key)
    }

    /// What an "always" for this call would cover. A tool that says nothing is
    /// remembered by its name; `fetch` says one key per host, so approving a page
    /// does not approve the internet.
    fn approval_key(&self, call: &ToolCall) -> String {
        self.tool(&call.name)
            .and_then(|tool| {
                let input = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
                tool.approval_key(&input)
            })
            .unwrap_or_else(|| call.name.clone())
    }

    fn asks_user(&self, name: &str) -> bool {
        self.tool(name).is_some_and(|tool| tool.asks_user())
    }

    fn delegates(&self, name: &str) -> bool {
        self.tool(name).is_some_and(|tool| tool.delegates())
    }

    fn tool(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.as_ref())
    }
}

/// Runs one call and returns whether it failed plus what the model reads.
pub fn run(tools: &[Arc<dyn Tool>], call: &ToolCall) -> (bool, String) {
    let description = summarize_arguments(&call.arguments);
    log::info!("calling {} {description}", call.name);

    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        let message = format!("error: there is no tool named {:?}", call.name);
        log::warn!("{message}");
        return (true, message);
    };

    let input: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(input) => input,
        Err(error) => {
            let message = format!("error: the arguments are not valid JSON: {error}");
            log::warn!("{message}");
            return (true, message);
        }
    };

    match tool.run(&input) {
        Ok(output) => {
            log::info!("{} returned {} bytes", call.name, output.len());
            (false, output)
        }
        Err(error) => {
            log::warn!("{} failed: {error}", call.name);
            (true, format!("error: {error}"))
        }
    }
}

/// Reads a title out of the stream, stopping at the first line break so a
/// preamble cannot leak into the sidebar.
async fn read_thread_title(mut stream: humibird_providers::EventStream) -> Option<String> {
    let mut title = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::Text(text)) => {
                if let Some(index) = text.find(['\n', '\r']) {
                    title.push_str(&text[..index]);
                    break;
                }
                title.push_str(&text);
            }
            Ok(StreamEvent::Finished { .. }) => break,
            Ok(StreamEvent::Usage(_) | StreamEvent::Reasoning(_) | StreamEvent::ToolCall(_)) => {}
            Err(error) => {
                log::warn!("the title request failed: {error:#}");
                return None;
            }
        }
    }

    let title = title.trim();
    if title.is_empty() {
        log::warn!("the title request produced no text");
        return None;
    }
    Some(title.chars().take(80).collect())
}

/// A title for when the model gives us nothing usable: the first line of the
/// first message, cut on a word boundary.
fn fallback_title(first_message: &str) -> Option<String> {
    let line = first_message
        .lines()
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return None;
    }

    let mut title = String::new();
    for word in line.split(' ') {
        let extra = usize::from(!title.is_empty());
        if !title.is_empty() && title.chars().count() + extra + word.chars().count() > 48 {
            break;
        }
        if extra == 1 {
            title.push(' ');
        }
        title.push_str(word);
    }
    Some(title)
}

/// A short label for a call, taken from its arguments. Also what the window shows
/// next to a call, so a reopened session reads the same as a live one.
pub fn summarize_arguments(arguments: &str) -> String {
    // Prefer the argument that names what the call is about over whichever key
    // happens to come first in the JSON.
    const PREFERRED: [&str; 6] = ["path", "pattern", "command", "query", "question", "text"];

    let arguments = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|arguments| arguments.as_object().cloned());
    let Some(arguments) = arguments else {
        return String::new();
    };

    PREFERRED
        .iter()
        .find_map(|key| arguments.get(*key).and_then(|value| value.as_str()))
        .or_else(|| arguments.values().find_map(|value| value.as_str()))
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use futures::executor::block_on;
    use humibird_providers::{EventStream, StopReason, ToolCall};
    use humibird_tools::{AskUser, ToolResult};

    use super::*;

    /// A provider that answers from a script, one reply per request, and keeps
    /// every request so a test can read what the model was shown.
    struct Scripted {
        replies: Mutex<VecDeque<Vec<StreamEvent>>>,
        requests: Arc<Mutex<Vec<Request>>>,
    }

    impl Scripted {
        fn new(replies: Vec<Vec<StreamEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<Request>>>) {
            let requests = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    replies: Mutex::new(replies.into()),
                    requests: requests.clone(),
                }),
                requests,
            )
        }
    }

    impl Provider for Scripted {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("test-model", messages)
        }

        fn stream(&self, request: Request) -> EventStream {
            self.requests.lock().expect("requests lock").push(request);
            let events = self
                .replies
                .lock()
                .expect("replies lock")
                .pop_front()
                .unwrap_or_default();
            futures::stream::iter(events.into_iter().map(Ok)).boxed()
        }
    }

    /// Runs the work inline: a test turn has no threads, and its clock never
    /// reaches the idle timeout.
    struct Inline;

    impl Background for Inline {
        fn run<R: Send + 'static, F: FnOnce() -> R + Send + 'static>(
            &self,
            work: F,
        ) -> LocalBoxFuture<'static, R> {
            Box::pin(async move { work() })
        }

        fn sleep(&self, _duration: Duration) -> LocalBoxFuture<'static, ()> {
            Box::pin(std::future::pending())
        }
    }

    /// A tool that records that it ran, and can be told to ask first.
    struct Counted {
        runs: Arc<Mutex<Vec<String>>>,
        needs_approval: bool,
        /// Whether an "always" covers one `path` rather than the whole tool, the
        /// way `fetch` covers one host.
        per_path: bool,
    }

    impl Counted {
        fn counted(needs_approval: bool) -> (Arc<dyn Tool>, Arc<Mutex<Vec<String>>>) {
            let runs = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    runs: runs.clone(),
                    needs_approval,
                    per_path: false,
                }),
                runs,
            )
        }

        fn per_path() -> (Arc<dyn Tool>, Arc<Mutex<Vec<String>>>) {
            let runs = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    runs: runs.clone(),
                    needs_approval: true,
                    per_path: true,
                }),
                runs,
            )
        }
    }

    impl Tool for Counted {
        fn name(&self) -> &str {
            "counted"
        }

        fn description(&self) -> &str {
            "Records that it ran."
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn needs_approval(&self) -> bool {
            self.needs_approval
        }

        fn approval_key(&self, input: &serde_json::Value) -> Option<String> {
            self.per_path
                .then(|| input["path"].as_str().unwrap_or_default().to_owned())
        }

        fn run(&self, input: &serde_json::Value) -> ToolResult {
            let path = input["path"].as_str().unwrap_or_default().to_owned();
            self.runs.lock().expect("runs lock").push(path);
            Ok("counted".to_owned())
        }
    }

    /// A window that is not a window: it keeps what it was told and answers every
    /// prompt the way the test asked it to.
    struct TestHost {
        events: Vec<AgentEvent>,
        approvals: usize,
        decision: Decision,
        answer: Option<String>,
        /// What a delegated task reports, when the test lets it answer at all.
        delegation: Option<String>,
        steer: bool,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                approvals: 0,
                decision: Decision::Once,
                answer: None,
                delegation: None,
                steer: false,
            }
        }

        fn texts(&self) -> String {
            self.events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::Text(delta) => Some(delta.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    impl TurnHost for TestHost {
        fn event(&mut self, event: AgentEvent) {
            match event {
                AgentEvent::Approval { respond, .. } => {
                    self.approvals += 1;
                    respond
                        .send(self.decision)
                        .expect("the turn is waiting for the decision")
                }
                AgentEvent::Question { respond, .. } => {
                    let answer = match self.answer.take() {
                        Some(text) => Answer::Selected(text),
                        None => Answer::Declined,
                    };
                    respond
                        .send(answer)
                        .expect("the turn is waiting for the answer")
                }
                AgentEvent::Delegate { respond, .. } => {
                    if let Some(text) = self.delegation.take() {
                        respond
                            .send(text)
                            .expect("the turn is waiting for the task");
                    }
                    // A dropped sender is a cancelled subagent, which is what the
                    // cancel test wants.
                }
                other => self.events.push(other),
            }
        }

        fn steer_requested(&mut self) -> bool {
            self.steer
        }
    }

    fn text(delta: &str) -> StreamEvent {
        StreamEvent::Text(delta.to_owned())
    }

    fn finished() -> StreamEvent {
        StreamEvent::Finished {
            reason: StopReason::Stop,
        }
    }

    fn wants_tools() -> StreamEvent {
        StreamEvent::Finished {
            reason: StopReason::ToolCalls,
        }
    }

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> StreamEvent {
        StreamEvent::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        })
    }

    /// A turn over a scripted provider. The request is the model's history as the
    /// window would have handed it over.
    fn turn(
        replies: Vec<Vec<StreamEvent>>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> (TurnRun, Arc<Mutex<Vec<Request>>>) {
        let (provider, requests) = Scripted::new(replies);
        let request = provider.request(vec![Message::user("look")]);
        (
            TurnRun::new(provider, Arc::new(tools), HashSet::new(), request, None),
            requests,
        )
    }

    #[test]
    fn an_answer_streams_out_and_comes_back_in_the_outcome() {
        let (run, requests) = turn(
            vec![vec![text("Hello"), text(" there"), finished()]],
            Vec::new(),
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(
            outcome,
            TurnOutcome::Done {
                reply: "Hello there".to_owned(),
                usage: None,
                calls: Vec::new(),
            }
        );
        assert_eq!(host.texts(), "Hello there");
        assert_eq!(requests.lock().expect("requests lock").len(), 1);
    }

    #[test]
    fn a_tool_round_runs_the_call_and_asks_the_model_again() {
        let (tool, runs) = Counted::counted(false);
        let (run, requests) = turn(
            vec![
                vec![
                    call("call_1", "counted", serde_json::json!({ "path": "a.txt" })),
                    wants_tools(),
                ],
                vec![text("done"), finished()],
            ],
            vec![tool],
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(runs.lock().expect("runs lock").as_slice(), ["a.txt"]);
        let TurnOutcome::Done { reply, calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert_eq!(reply, "done");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].result, "counted");
        assert!(!calls[0].failed);
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 2, "the model was asked again");
        let last = requests[1].messages.last().expect("the result");
        assert_eq!(last.content, "counted", "the result went back to the model");
        assert!(!host.events.iter().any(|event| matches!(
            event,
            AgentEvent::Approval { .. } | AgentEvent::Question { .. }
        )));
    }

    #[test]
    fn a_denied_call_never_runs_and_tells_the_model_why() {
        let (tool, runs) = Counted::counted(true);
        let (run, _requests) = turn(
            vec![
                vec![
                    call("call_1", "counted", serde_json::json!({ "path": "a.txt" })),
                    wants_tools(),
                ],
                vec![text("fine"), finished()],
            ],
            vec![tool],
        );
        let mut host = TestHost::new();
        host.decision = Decision::Deny;

        let outcome = block_on(run.run(&mut host, &Inline));

        assert!(runs.lock().expect("runs lock").is_empty(), "it never ran");
        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert!(calls[0].failed);
        assert_eq!(calls[0].result, "error: the user denied this call");
        assert_eq!(host.approvals, 1);
    }

    #[test]
    fn an_always_is_remembered_for_the_rest_of_the_turn() {
        let (tool, runs) = Counted::counted(true);
        let (run, _requests) = turn(
            vec![
                vec![
                    call("call_1", "counted", serde_json::json!({ "path": "a.txt" })),
                    call("call_2", "counted", serde_json::json!({ "path": "b.txt" })),
                    wants_tools(),
                ],
                vec![finished()],
            ],
            vec![tool],
        );
        let mut host = TestHost::new();
        host.decision = Decision::Always;

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(
            runs.lock().expect("runs lock").as_slice(),
            ["a.txt", "b.txt"],
            "the second call needed no consent"
        );
        assert_eq!(host.approvals, 1);
        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert!(calls.iter().all(|call| !call.failed), "{calls:?}");
    }

    #[test]
    fn an_always_covers_the_key_the_tool_reports_not_the_whole_tool() {
        let (tool, runs) = Counted::per_path();
        let (run, _requests) = turn(
            vec![
                vec![
                    call("call_1", "counted", serde_json::json!({ "path": "a.txt" })),
                    call("call_2", "counted", serde_json::json!({ "path": "a.txt" })),
                    call("call_3", "counted", serde_json::json!({ "path": "b.txt" })),
                    wants_tools(),
                ],
                vec![finished()],
            ],
            vec![tool],
        );
        let mut host = TestHost::new();
        host.decision = Decision::Always;

        block_on(run.run(&mut host, &Inline));

        assert_eq!(
            runs.lock().expect("runs lock").as_slice(),
            ["a.txt", "a.txt", "b.txt"],
            "every call ran"
        );
        assert_eq!(
            host.approvals, 2,
            "a.txt is covered by its own always; b.txt is a different key"
        );
    }

    #[test]
    fn a_question_comes_back_as_the_answer() {
        let (run, _requests) = turn(
            vec![
                vec![
                    call(
                        "call_1",
                        "ask_user",
                        serde_json::json!({
                            "question": "Which one?",
                            "options": ["this", "that"],
                        }),
                    ),
                    wants_tools(),
                ],
                vec![text("ok"), finished()],
            ],
            vec![Arc::new(AskUser)],
        );
        let mut host = TestHost::new();
        host.answer = Some("this".to_owned());

        let outcome = block_on(run.run(&mut host, &Inline));

        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert!(!calls[0].failed);
        assert_eq!(calls[0].result, humibird_tools::answer_text("this"));
    }

    #[test]
    fn a_declined_question_tells_the_model_it_was_declined() {
        let (run, _requests) = turn(
            vec![
                vec![
                    call(
                        "call_1",
                        "ask_user",
                        serde_json::json!({
                            "question": "Which one?",
                            "options": ["this", "that"],
                        }),
                    ),
                    wants_tools(),
                ],
                vec![finished()],
            ],
            vec![Arc::new(AskUser)],
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert!(calls[0].failed);
        assert_eq!(calls[0].result, humibird_tools::DECLINED_TEXT);
    }

    #[test]
    fn a_delegation_comes_back_as_the_tool_result() {
        let (run, requests) = turn(
            vec![
                vec![
                    call(
                        "call_1",
                        "delegate",
                        serde_json::json!({
                            "description": "count the files",
                            "prompt": "Count the files in the workspace.",
                        }),
                    ),
                    wants_tools(),
                ],
                vec![text("three files"), finished()],
            ],
            vec![Arc::new(humibird_tools::Delegate)],
        );
        let mut host = TestHost::new();
        host.delegation = Some("there are three files".to_owned());

        let outcome = block_on(run.run(&mut host, &Inline));

        let TurnOutcome::Done { reply, calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert_eq!(reply, "three files");
        assert!(!calls[0].failed);
        assert_eq!(calls[0].result, "there are three files");
        let requests = requests.lock().expect("requests lock");
        assert_eq!(
            requests[1].messages.last().expect("the result").content,
            "there are three files",
            "the report is what the parent's model reads"
        );
    }

    #[test]
    fn a_delegation_nobody_answers_is_cancelled() {
        let (run, _requests) = turn(
            vec![
                vec![
                    call(
                        "call_1",
                        "delegate",
                        serde_json::json!({
                            "description": "count the files",
                            "prompt": "Count the files.",
                        }),
                    ),
                    wants_tools(),
                ],
                vec![finished()],
            ],
            vec![Arc::new(humibird_tools::Delegate)],
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert!(calls[0].failed, "a cancelled subagent is not an answer");
        assert_eq!(calls[0].result, humibird_tools::DELEGATION_CANCELLED);
    }

    #[test]
    fn fallback_titles_use_the_first_line_and_a_word_boundary() {
        assert_eq!(
            fallback_title("  fix the  flaky test \nand the rest"),
            Some("fix the flaky test".to_owned())
        );
        assert_eq!(fallback_title("   \n\n"), None);

        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa".repeat(2);
        let title = fallback_title(&long).expect("a title");
        assert!(title.chars().count() <= 48, "{title}");
        assert!(long.starts_with(&title), "{title}");
        assert!(!title.ends_with(' '), "{title}");
    }

    /// A provider whose stream breaks instead of answering.
    struct Broken;

    impl Provider for Broken {
        fn request(&self, messages: Vec<Message>) -> Request {
            Request::new("test-model", messages)
        }

        fn stream(&self, _request: Request) -> EventStream {
            futures::stream::iter(vec![Err(anyhow::anyhow!("the connection dropped"))]).boxed()
        }
    }

    #[test]
    fn a_stream_that_errors_fails_the_turn() {
        let provider: Arc<dyn Provider> = Arc::new(Broken);
        let request = provider.request(vec![Message::user("look")]);
        let run = TurnRun::new(
            provider,
            Arc::new(Vec::new()),
            HashSet::new(),
            request,
            None,
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(outcome, TurnOutcome::failed("the connection dropped"));
    }

    #[test]
    fn a_refusal_about_the_context_window_is_marked_as_one() {
        struct TooLong;

        impl Provider for TooLong {
            fn request(&self, messages: Vec<Message>) -> Request {
                Request::new("test-model", messages)
            }

            fn stream(&self, _request: Request) -> EventStream {
                let error = anyhow::Error::from(humibird_providers::ContextExceeded)
                    .context("the provider refused the request");
                futures::stream::iter(vec![Err(error)]).boxed()
            }
        }

        let provider: Arc<dyn Provider> = Arc::new(TooLong);
        let request = provider.request(vec![Message::user("look")]);
        let run = TurnRun::new(
            provider,
            Arc::new(Vec::new()),
            HashSet::new(),
            request,
            None,
        );
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                message: "the provider refused the request".to_owned(),
                context_exceeded: true,
            },
            "the outcome has to say this one is actionable"
        );
    }

    #[test]
    fn a_stream_that_ends_without_finishing_fails_the_turn() {
        let (run, _requests) = turn(vec![vec![text("half")]], Vec::new());
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(
            outcome,
            TurnOutcome::failed("the reply ended before it finished; nothing was kept")
        );
    }

    #[test]
    fn the_loop_stops_asking_after_the_round_limit() {
        let (tool, runs) = Counted::counted(false);
        let replies = (0..MAX_TOOL_ROUNDS)
            .map(|round| {
                vec![
                    call(
                        &format!("call_{round}"),
                        "counted",
                        serde_json::json!({ "path": "a.txt" }),
                    ),
                    wants_tools(),
                ]
            })
            .collect();
        let (run, _requests) = turn(replies, vec![tool]);
        let mut host = TestHost::new();

        let outcome = block_on(run.run(&mut host, &Inline));

        assert_eq!(
            outcome,
            TurnOutcome::failed("the model kept asking for tools; stopped to avoid a loop")
        );
        assert_eq!(runs.lock().expect("runs lock").len(), MAX_TOOL_ROUNDS);
    }

    #[test]
    fn a_steering_message_ends_the_turn_at_the_boundary() {
        let (tool, runs) = Counted::counted(false);
        let (run, requests) = turn(
            vec![
                vec![
                    call("call_1", "counted", serde_json::json!({ "path": "a.txt" })),
                    wants_tools(),
                ],
                vec![text("never asked for"), finished()],
            ],
            vec![tool],
        );
        let mut host = TestHost::new();
        host.steer = true;

        let outcome = block_on(run.run(&mut host, &Inline));

        let TurnOutcome::Done { calls, .. } = outcome else {
            panic!("the turn finished: {outcome:?}");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(runs.lock().expect("runs lock").len(), 1);
        assert_eq!(
            requests.lock().expect("requests lock").len(),
            1,
            "the turn ended instead of asking again"
        );
    }
}
