use std::fs;
use std::io;

use super::super::create::create_text_file;
use super::super::create::create_text_file_at_with_writer;
use super::super::types::MAX_FILE_SIZE;
use super::temp_dir;
use crate::test_support::CurrentDirGuard;

#[test]
fn failed_write_removes_the_partial_file_it_created() {
    let directory = temp_dir("krypper-write-failure-");
    let path = directory.path();
    let root = super::super::types::DirHandle::from_file(fs::File::open(path).unwrap());
    let mut writes = 0;
    let result = create_text_file_at_with_writer(
        &root,
        std::ffi::OsStr::new("partial.txt"),
        b"complete text",
        |file, bytes| {
            writes += 1;
            if writes == 1 {
                super::write_to_file(file, &bytes[..3])
            } else {
                Err(io::Error::other("simulated disk failure"))
            }
        },
    );
    assert_eq!(result.unwrap_err().message(), "unable to write file");
    assert!(!path.join("partial.txt").exists());
}

#[test]
fn write_enforces_the_encoded_one_mib_limit_before_creation() {
    let directory = temp_dir("krypper-write-limit-");
    let path = directory.path();
    let _current_dir = CurrentDirGuard::enter(path);
    let exact = "é".repeat(MAX_FILE_SIZE / 2);
    let result = create_text_file(std::ffi::OsStr::new("exact.txt"), &exact);
    let too_large = "é".repeat(MAX_FILE_SIZE / 2 + 1);
    let rejected = create_text_file(std::ffi::OsStr::new("large.txt"), &too_large);
    assert!(result.is_ok());
    assert_eq!(rejected.unwrap_err().message(), "text exceeds 1 MiB");
    assert_eq!(
        fs::metadata(path.join("exact.txt")).unwrap().len(),
        MAX_FILE_SIZE as u64
    );
    assert!(!path.join("large.txt").exists());
}

#[test]
fn directory_names_match_python_json_escaping_and_sort_order() {
    let codepoints = vec![0xdcff];
    assert_eq!(super::quote_json(&codepoints), "\"\\udcff\"");
}
