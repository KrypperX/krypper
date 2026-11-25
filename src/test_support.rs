use std::path::{Path, PathBuf};

pub(crate) struct TempDir {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl TempDir {
    pub(crate) fn new() -> Self {
        Self::with_prefix("krypper-test-")
    }

    pub(crate) fn with_prefix(prefix: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("create temporary test directory");
        let path = directory.path().to_path_buf();
        Self {
            path,
            _directory: directory,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub(crate) fn write_file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write temporary test file");
        path
    }
}
