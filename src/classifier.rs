#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Spam,
    NotSpam,
}

/// Classify a message as spam or not using the Claude CLI subprocess.
///
/// Uses `claude --print --model haiku` — no Anthropic API key required in config.
/// Times out after 10 seconds to prevent blocking the Telegram dispatcher.
pub async fn classify(text: &str) -> Result<Classification, String> {
    let prompt = format!(
        r#"You are a spam classifier for a Telegram group. Respond with exactly one word: SPAM or NOT_SPAM.

Spam includes:
- Crypto/forex/investment scams
- Unsolicited promotions or ads
- Phishing attempts
- Invite links to other groups/channels
- "Get rich quick" schemes
- Adult content promotion

NOT_SPAM includes:
- Normal conversation
- Questions and answers
- Opinions and discussions
- Sharing relevant content

Message: "{text}"

Respond with exactly one word: SPAM or NOT_SPAM"#
    );

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("claude")
            .args(["--print", "--model", "haiku", "-p", &prompt])
            .output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("Failed to run claude classifier: {e}")),
        Err(_) => return Err("Classifier timed out after 10s".to_string()),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Claude classifier failed: {stderr}"));
    }

    let result = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_uppercase()
        .to_string();

    if result.contains("SPAM") && !result.contains("NOT") {
        Ok(Classification::Spam)
    } else {
        Ok(Classification::NotSpam)
    }
}
