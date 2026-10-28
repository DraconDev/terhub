fn main() {
    for line in [
        " one pushed bright. The plain-prompt version is ready to run the moment either quota resets.",
        "Next action for you: run /goal resume once Codex or MMX quota is back and I'll generate the plain-prompt set",
        "To see the quality improvements, the rate limit needs to clear (or you can raise the budget in Settings → Usage).",
        "The page on /pol/2 is being rate-limited by the provider right now: 228 spans were classified",
        "⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 2h5m35s.",
        "Error: rate limit reached",
        "Error: Retry failed after 15 attempts: 429 {\"type\":\"error\"}",
        "Error: 429 quota exceeded",
        "HTTP 429 slow down",
        "upstream quota exceeded for this account",
        "The Token Plan usage limit has been reached.",
        "1,429 spans were processed",
        "cost 429 tokens so far",
    ] {
        println!("{}  {}", if terhub::agent::is_quota_wall(line) { "WALL " } else { "prose" }, line.trim());
    }
}
