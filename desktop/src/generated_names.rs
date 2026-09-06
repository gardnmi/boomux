use std::collections::HashSet;

const ADJECTIVES: &[&str] = &[
    "agile", "amber", "bold", "brave", "bright", "brisk", "calm", "clever", "cool", "coral",
    "cosmic", "crisp", "daring", "eager", "fair", "fast", "gentle", "golden", "grand", "happy",
    "hazy", "keen", "kind", "lively", "lucid", "lucky", "mellow", "merry", "misty", "noble",
    "quiet", "rapid", "ready", "rosy", "royal", "sage", "sharp", "silver", "sleek", "solar",
    "steady", "swift", "tidy", "vivid", "warm", "wild", "wise", "witty",
];

const NOUNS: &[&str] = &[
    "badger", "bear", "beaver", "bison", "cedar", "comet", "crane", "dolphin", "eagle", "ember",
    "falcon", "fern", "fox", "gecko", "heron", "ibis", "jay", "koala", "lark", "lynx", "maple",
    "marmot", "otter", "owl", "panda", "pine", "puma", "raven", "reef", "robin", "sable", "seal",
    "shark", "sparrow", "spruce", "starling", "tiger", "trout", "turtle", "whale", "willow",
    "wolf", "wren", "yak",
];

pub fn random_excluding<'a>(unavailable: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let count = ADJECTIVES.len() * NOUNS.len();
    from_index(fastrand::usize(..count), unavailable)
}

fn from_index<'a>(start: usize, unavailable: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let unavailable = unavailable.into_iter().collect::<HashSet<_>>();
    let count = ADJECTIVES.len() * NOUNS.len();
    (0..count).find_map(|offset| {
        let index = (start + offset) % count;
        let name = format!(
            "{}-{}",
            ADJECTIVES[index / NOUNS.len()],
            NOUNS[index % NOUNS.len()]
        );
        (!unavailable.contains(name.as_str())).then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_use_the_boomux_style() {
        assert_eq!(from_index(0, []), Some("agile-badger".into()));
        assert_eq!(from_index(1, []), Some("agile-bear".into()));
    }

    #[test]
    fn generated_names_skip_collisions_and_wrap() {
        assert_eq!(
            from_index(0, ["agile-badger", "agile-bear"]),
            Some("agile-beaver".into())
        );
        let last = format!("{}-{}", ADJECTIVES.last().unwrap(), NOUNS.last().unwrap());
        assert_eq!(
            from_index(ADJECTIVES.len() * NOUNS.len() - 1, [last.as_str()]),
            Some("agile-badger".into())
        );
    }
}
