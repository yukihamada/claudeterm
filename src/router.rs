/// Route a message to the appropriate effort level.
/// Model is always Sonnet 4.6 — only `--effort` varies based on
/// structural complexity of the message (not keyword matching).
///
/// Rules (in priority order):
///   high   — code blocks, file diffs, or long messages (>400 chars)
///   low    — short questions (<60 chars with ? or ？)
///   medium — everything else

pub fn route_message(text: &str) -> (&'static str, &'static str) {
    const MODEL: &str = "claude-sonnet-4-6";
    let len = text.len();

    // Code or large content → needs full effort
    if text.contains("```") || text.contains("diff\n") || len > 400 {
        return (MODEL, "high");
    }

    // Short questions → low effort is enough
    let trimmed = text.trim();
    if len < 60 && (trimmed.ends_with('?') || trimmed.ends_with('？')) {
        return (MODEL, "low");
    }

    (MODEL, "medium")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_question_is_low() {
        assert_eq!(route_message("hello?").1, "low");
        assert_eq!(route_message("これ何？").1, "low");
        assert_eq!(route_message("what is this?").1, "low");
    }

    #[test]
    fn code_block_is_high() {
        assert_eq!(route_message("fix this:\n```rust\nfn main(){}\n```").1, "high");
    }

    #[test]
    fn long_message_is_high() {
        assert_eq!(route_message(&"a".repeat(401)).1, "high");
    }

    #[test]
    fn default_is_medium() {
        assert_eq!(route_message("Fix the login bug in auth.rs").1, "medium");
        assert_eq!(route_message("Add dark mode support").1, "medium");
    }

    #[test]
    fn model_is_always_sonnet() {
        assert_eq!(route_message("hello?").0, "claude-sonnet-4-6");
        assert_eq!(route_message("review entire codebase").0, "claude-sonnet-4-6");
    }
}
