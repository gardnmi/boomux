use std::collections::BTreeSet;

use uuid::Uuid;

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

pub(crate) fn random() -> String {
    from_seed(random_seed(), std::iter::empty()).expect("the generated name catalog is nonempty")
}

pub(crate) fn random_excluding<'a>(
    unavailable: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    from_seed(random_seed(), unavailable)
}

fn random_seed() -> u64 {
    let bytes = Uuid::new_v4();
    u64::from_le_bytes(
        bytes.as_bytes()[..8]
            .try_into()
            .expect("UUID prefix is 8 bytes"),
    )
}

fn from_seed<'a>(seed: u64, unavailable: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let unavailable = unavailable.into_iter().collect::<BTreeSet<_>>();
    let count = ADJECTIVES.len() * NOUNS.len();
    let start = seed as usize % count;
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
    fn generated_names_are_stable_for_injected_entropy() {
        assert_eq!(from_seed(0, []), Some("agile-badger".into()));
        assert_eq!(from_seed(1, []), Some("agile-bear".into()));
        assert_eq!(
            from_seed((NOUNS.len() + 1) as u64, []),
            Some("amber-bear".into())
        );
    }

    #[test]
    fn generated_names_skip_collisions_and_wrap() {
        assert_eq!(
            from_seed(0, ["agile-badger", "agile-bear"]),
            Some("agile-beaver".into())
        );
        let last = format!("{}-{}", ADJECTIVES.last().unwrap(), NOUNS.last().unwrap());
        assert_eq!(
            from_seed((ADJECTIVES.len() * NOUNS.len() - 1) as u64, [last.as_str()]),
            Some("agile-badger".into())
        );
    }

    #[test]
    fn exhausted_catalog_returns_none() {
        let names = ADJECTIVES
            .iter()
            .flat_map(|adjective| NOUNS.iter().map(move |noun| format!("{adjective}-{noun}")))
            .collect::<Vec<_>>();
        assert_eq!(from_seed(0, names.iter().map(String::as_str)), None);
    }
}
