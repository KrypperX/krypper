use crate::task_lifecycle::MAX_TASK_SESSION_CHECKPOINT_BYTES;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

pub(super) fn read_task_checkpoint(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(MAX_TASK_SESSION_CHECKPOINT_BYTES + 1);
    file.take((MAX_TASK_SESSION_CHECKPOINT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TASK_SESSION_CHECKPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("checkpoint exceeds {MAX_TASK_SESSION_CHECKPOINT_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

pub(super) fn validate_checkpoint_output_path(path: &Path) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "checkpoint output must name a new file",
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "checkpoint output already exists",
            ));
        }
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
        Err(cause) => return Err(cause),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !fs::metadata(parent)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "checkpoint output parent is not a directory",
        ));
    }
    Ok(())
}

pub(super) fn write_task_checkpoint(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_TASK_SESSION_CHECKPOINT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("checkpoint exceeds {MAX_TASK_SESSION_CHECKPOINT_BYTES} bytes"),
        ));
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::{read_task_checkpoint, validate_checkpoint_output_path, write_task_checkpoint};
    use crate::task_lifecycle::MAX_TASK_SESSION_CHECKPOINT_BYTES;
    use crate::test_support::TempDir;
    use std::fs;
    use std::io;
    use std::path::Path;

    struct FixtureDirectory(TempDir);

    impl FixtureDirectory {
        fn new() -> Self {
            Self(TempDir::with_prefix("krypper-checkpoint-files-"))
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    #[test]
    fn reads_checkpoint_up_to_the_size_limit() {
        let fixture = FixtureDirectory::new();
        let path = fixture.path().join("checkpoint.json");
        let expected = vec![b'x'; MAX_TASK_SESSION_CHECKPOINT_BYTES];
        fs::write(&path, &expected).expect("write checkpoint fixture");

        assert_eq!(
            read_task_checkpoint(&path).expect("read checkpoint"),
            expected
        );
    }

    #[test]
    fn rejects_checkpoint_larger_than_the_size_limit() {
        let fixture = FixtureDirectory::new();
        let path = fixture.path().join("checkpoint.json");
        fs::write(&path, vec![b'x'; MAX_TASK_SESSION_CHECKPOINT_BYTES + 1])
            .expect("write oversized checkpoint fixture");

        let error = read_task_checkpoint(&path).expect_err("oversized checkpoint must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            format!("checkpoint exceeds {MAX_TASK_SESSION_CHECKPOINT_BYTES} bytes")
        );
    }

    #[test]
    fn validates_output_path_name_parent_and_existing_file() {
        let fixture = FixtureDirectory::new();
        let no_name = validate_checkpoint_output_path(Path::new("/"))
            .expect_err("root path must not be an output file");
        assert_eq!(no_name.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            no_name.to_string(),
            "checkpoint output must name a new file"
        );

        let valid = fixture.path().join("new checkpoint.json");
        validate_checkpoint_output_path(&valid).expect("new file in existing directory");

        let parent_file = fixture.path().join("parent-file");
        fs::write(&parent_file, b"file").expect("write parent fixture");
        let invalid_parent_path = parent_file.join("checkpoint.json");
        let expected_parent_error = fs::symlink_metadata(&invalid_parent_path)
            .expect_err("a regular file cannot contain a checkpoint file");
        let invalid_parent = validate_checkpoint_output_path(&invalid_parent_path)
            .expect_err("output parent must be a directory");
        assert_eq!(invalid_parent.kind(), expected_parent_error.kind());
        assert_eq!(
            invalid_parent.to_string(),
            expected_parent_error.to_string()
        );

        fs::write(&valid, b"existing").expect("create existing checkpoint fixture");
        let existing = validate_checkpoint_output_path(&valid)
            .expect_err("existing output path must be rejected");
        assert_eq!(existing.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(existing.to_string(), "checkpoint output already exists");
    }

    #[test]
    fn writes_checkpoint_with_create_new_semantics() {
        let fixture = FixtureDirectory::new();
        let path = fixture.path().join("checkpoint.json");
        write_task_checkpoint(&path, b"first checkpoint").expect("write new checkpoint");

        let collision = write_task_checkpoint(&path, b"replacement")
            .expect_err("checkpoint writes must not overwrite");
        assert_eq!(collision.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("read existing checkpoint"),
            b"first checkpoint"
        );
    }

    #[test]
    fn rejects_oversized_checkpoint_before_creating_output() {
        let fixture = FixtureDirectory::new();
        let path = fixture.path().join("checkpoint.json");
        let error =
            write_task_checkpoint(&path, &vec![b'x'; MAX_TASK_SESSION_CHECKPOINT_BYTES + 1])
                .expect_err("oversized checkpoint must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            format!("checkpoint exceeds {MAX_TASK_SESSION_CHECKPOINT_BYTES} bytes")
        );
        assert!(!path.exists());
    }
}
