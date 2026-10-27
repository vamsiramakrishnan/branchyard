//! Reading the user configuration file before changing it.

use std::io::ErrorKind;
use std::path::Path;

use crate::commands::Failure;

/// The user configuration file's text: empty when there is no file yet, an
/// error naming the file when there is one that cannot be read (a
/// permission error, bytes that are not UTF-8, a directory). An unreadable
/// file is never taken for an empty one: the write that follows replaces the
/// whole file, so that would erase the person's configuration.
pub(crate) fn read(file: &Path) -> Result<String, Failure> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(crate::errors::Report::new(
            "config",
            format!(
                "could not read {}: {}; it was left as it is",
                file.display(),
                crate::errors::plain_io(&error)
            ),
        )
        .next("fix the file or its permissions, then run this again")
        .next("set BRANCHYARD_USER_CONFIG to another path")
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::read;

    #[test]
    fn a_missing_file_is_an_empty_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("config.toml")).unwrap(), "");
    }

    #[test]
    fn a_file_is_read_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "[defaults]\nharness = \"codex\"\n").unwrap();
        assert_eq!(read(&file).unwrap(), "[defaults]\nharness = \"codex\"\n");
    }

    /// Neither of these is "no configuration": taken for one, the file
    /// would be replaced by a new one holding only what was just added.
    #[test]
    fn a_file_that_cannot_be_read_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let not_utf8 = dir.path().join("config.toml");
        std::fs::write(&not_utf8, b"# caf\xe9 \xff\n").unwrap();
        let directory = dir.path().join("a-directory");
        std::fs::create_dir(&directory).unwrap();
        for file in [&not_utf8, &directory] {
            let message = read(file).unwrap_err().to_string();
            assert!(
                message.contains(&format!("could not read {}", file.display())),
                "{message}"
            );
            assert!(message.contains("left as it is"), "{message}");
        }
        assert_eq!(std::fs::read(&not_utf8).unwrap(), b"# caf\xe9 \xff\n");
    }
}
