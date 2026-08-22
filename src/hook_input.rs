use std::io::{self, Read};

pub(crate) const MAX_HOOK_INPUT_BYTES: usize = 1024 * 1024;

pub(crate) fn read_bounded_hook_input(
    mut reader: impl Read,
    integration: &str,
) -> io::Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{integration} hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes"),
        ));
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_input_and_names_the_integration() {
        assert_eq!(
            read_bounded_hook_input(&b"input"[..], "Test").unwrap(),
            b"input"
        );
        let error =
            read_bounded_hook_input(&vec![b'x'; MAX_HOOK_INPUT_BYTES + 1][..], "Test").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().starts_with("Test hook input exceeds"));
    }
}
