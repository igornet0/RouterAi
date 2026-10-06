//! Minimal DeepSeek chat example.

use secrecy::SecretString;
use universal_ai::{AiClient, AiResult, DeepSeek};

#[tokio::main]
async fn main() -> AiResult<()> {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY");
    let client = AiClient::builder()
        .provider(DeepSeek::new(SecretString::from(key))?)
        .with_example_prices()
        .build()?;

    let response = client
        .chat()
        .model("deepseek-chat")
        .message("Hello")
        .send()
        .await?;

    println!("{}", response.text());
    Ok(())
}
