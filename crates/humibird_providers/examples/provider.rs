//! Streams one prompt through a provider and prints the deltas.
//!
//! Manual check for the adapter (the automated tests use a fake transport):
//!
//! ```sh
//! DEEPSEEK_API_KEY=... cargo run -p humibird_providers --example provider
//! PROVIDER=ollama MODEL=qwen3 cargo run -p humibird_providers --example provider
//! PROVIDER=custom BASE_URL=http://localhost:1234/v1 MODEL=local \
//!     cargo run -p humibird_providers --example provider
//! ```

use std::sync::Arc;

use futures::StreamExt as _;
use humibird_providers::{Message, OpenAiCompatible, Provider, StreamEvent, preset};
use reqwest_client::ReqwestClient;

fn main() -> anyhow::Result<()> {
    let id = std::env::var("PROVIDER").unwrap_or_else(|_| "deepseek".to_owned());
    let preset = preset(&id).ok_or_else(|| anyhow::anyhow!("unknown provider {id}"))?;

    let api_key = preset
        .key_env
        .and_then(|variable| std::env::var(variable).ok());
    if preset.key_env.is_some() && api_key.is_none() {
        anyhow::bail!(
            "set {} to run this example",
            preset.key_env.unwrap_or_default()
        );
    }

    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| preset.base_url.to_owned());
    anyhow::ensure!(!base_url.is_empty(), "set BASE_URL for {id}");
    let model = std::env::var("MODEL")
        .ok()
        .or_else(|| preset.default_model().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("set MODEL: {id} names no models of its own"))?;

    let provider = OpenAiCompatible::new(preset, Arc::new(ReqwestClient::new()), api_key)
        .with_base_url(base_url)
        .with_model(model);
    let request = provider.request(vec![Message::user(
        "In one sentence, what is a profile in an agent application?",
    )]);

    futures::executor::block_on(async {
        let mut events = provider.stream(request);
        while let Some(event) = events.next().await {
            match event? {
                StreamEvent::Text(text) => print!("{text}"),
                StreamEvent::Reasoning(text) => eprint!("{text}"),
                StreamEvent::ToolCall(call) => {
                    eprintln!("\n[tool] {} {}", call.name, call.arguments)
                }
                StreamEvent::Usage(usage) => eprintln!("\n[usage] {usage:?}"),
                StreamEvent::Finished { reason } => eprintln!("\n[finished] {reason:?}"),
            }
        }
        Ok::<_, anyhow::Error>(())
    })
}
