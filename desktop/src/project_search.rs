//! Bounded, in-memory filtering for the local project picker.
use std::path::Path;

pub fn matches(name: &str, path: &Path, normalized_query: &str) -> bool {
    normalized_query.is_empty()
        || name.to_lowercase().contains(normalized_query)
        || path
            .to_string_lossy()
            .to_lowercase()
            .contains(normalized_query)
}

pub fn append(query: &mut String, text: &str) {
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        if query.len() + ch.len_utf8() > 256 {
            break;
        }
        query.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_names_and_parent_paths_without_case_sensitivity() {
        let path = Path::new("/home/user/Projects/Tools/Boomux");
        for query in ["", "boom", "projects/tools", "tools/boom"] {
            assert!(matches("Boomux", path, query));
        }
        assert!(!matches("Boomux", path, "other-project"));
    }

    #[test]
    fn pasted_text_is_bounded_without_splitting_unicode_or_inserting_controls() {
        let mut query = String::new();
        append(&mut query, "boom\nux\r\t");
        assert_eq!(query, "boomux");
        query = "a".repeat(255);
        append(&mut query, "é");
        assert_eq!(query.len(), 255);
        append(&mut query, "bextra");
        assert_eq!(query.len(), 256);
    }
}
