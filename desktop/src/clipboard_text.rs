//! Conservative prose reflow for application-written terminal display lines.
//! VT soft wraps are already removed by Ghostty before this runs. Real newlines
//! carry no semantic information, so ambiguous lines retain their formatting.

pub(crate) fn readable_copy(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous = "";
    let mut continuation_indent = None;
    let mut fence = None;
    let mut command_output = false;
    for (index, line) in text.split('\n').enumerate() {
        let content = line.trim();
        let indent = line.len() - line.trim_start_matches(' ').len();
        let marker = if content.starts_with("```") {
            Some('`')
        } else if content.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if content.starts_with("• ") {
            command_output = content.starts_with("• Ran ") || content.starts_with("• Running ");
        }
        let protected = command_output || fence.is_some() || marker.is_some();
        let bullet = bullet_prefix(content);
        let join = !protected
            && bullet.is_none()
            && prose(content)
            && continuation_indent == Some(indent)
            && !previous.ends_with(['.', '!', '?', ':']);
        if index > 0 {
            output.push(if join { ' ' } else { '\n' });
        }
        output.push_str(if join { content } else { line });
        if !join {
            let body = bullet.map_or(content, |prefix| &content[prefix..]);
            continuation_indent = (!protected
                && prose(body)
                && indent <= 2
                && body.chars().count() >= 28
                && body.split_whitespace().count() >= 5)
                .then_some(indent + if bullet.is_some() { 2 } else { 0 });
        }
        previous = content;
        if let Some(marker) = marker {
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
        }
    }
    output
}

fn bullet_prefix(text: &str) -> Option<usize> {
    if text.starts_with("- ") || text.starts_with("* ") {
        return Some(2);
    }
    // The display marker is one terminal column, despite occupying three bytes.
    if text.starts_with("• ") {
        return Some("• ".len());
    }
    None
}

fn prose(text: &str) -> bool {
    text.chars().next().is_some_and(char::is_alphabetic)
        && !text.contains(['{', '}', '[', ']', '=', ';', '|', '\\', '\t', '`'])
        && ![
            "git ", "cat ", "echo ", "printf ", "sudo ", "cargo ", "npm ", "bun ", "python ",
            "python3 ",
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_the_reported_agent_bullets_and_paragraph() {
        let narrow = "• Implemented and running—refresh Farmr.\n\n  - Router picker is removed from the composer.\n  - Automatic currently chooses Luna based on our\n    completed comparison.\n\n  - Manual override: Menu → Settings → Routing.\n  - Selection stays within your provider and adds no\n    network calls.\n\n  - Anthropic support and benchmarking remain pending.\n\n  Backend and browser checks passed. Your agent model and\n  conversations are preserved.";
        let wide = narrow
            .replace("our\n    completed", "our completed")
            .replace("no\n    network", "no network")
            .replace("and\n  conversations", "and conversations");
        assert_eq!(readable_copy(narrow), wide);
        assert_eq!(readable_copy(&wide), wide);
    }

    #[test]
    fn preserves_code_commands_blank_lines_and_list_boundaries() {
        for text in [
            "• Ran cat >> docs/verification.md <<'EOF'\n  │\n  │ ## Automatic router selection and Settings\n  │ … +71 lines\n  └ (no output)",
            "```text\nThis line has enough words to look like prose\nbut this code block must not change\n```",
            "    This indented block has plenty of words\n    and remains an indented code block",
            "let command = something with several arguments\nand_this_is_code();",
            "git commit -m 'a long description of this change'\ngit push origin main",
            "A sentence with several words ends here.\nAnother separate sentence starts here.\n\n",
            "  - A long bullet with more than five words\n  - Another long bullet with more than five words",
            "Heading:\nnext item\n",
            "",
            "\n\n",
        ] {
            assert_eq!(readable_copy(text), text);
        }
    }

    #[test]
    fn joins_multiple_continuations_without_flattening_paragraphs() {
        assert_eq!(
            readable_copy(
                "  The browser checks pass including saving an override\n  and restoring Automatic while retaining drafts\n  for mobile.\n\n  Your agent model will stay unchanged."
            ),
            "  The browser checks pass including saving an override and restoring Automatic while retaining drafts for mobile.\n\n  Your agent model will stay unchanged."
        );
    }
}
