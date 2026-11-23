use std::fs;
use std::io;
use std::sync::atomic::Ordering;

use super::super::create::create_text_file;
use super::super::create::create_text_file_at_with_writer;
use super::super::types::MAX_FILE_SIZE;
use super::NEXT_TEMP;

#[test]
fn failed_write_removes_the_partial_file_it_created() {
    let path = std::env::temp_dir().join(format!(
        "krypper-write-failure-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let root = super::super::types::DirHandle::from_file(fs::File::open(&path).unwrap());
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
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn write_enforces_the_encoded_one_mib_limit_before_creation() {
    let _lock = crate::CURRENT_DIR_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_directory = std::env::current_dir().unwrap();
    let path = std::env::temp_dir().join(format!(
        "krypper-write-limit-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    std::env::set_current_dir(&path).unwrap();
    let exact = "é".repeat(MAX_FILE_SIZE / 2);
    let result = create_text_file(std::ffi::OsStr::new("exact.txt"), &exact);
    let too_large = "é".repeat(MAX_FILE_SIZE / 2 + 1);
    let rejected = create_text_file(std::ffi::OsStr::new("large.txt"), &too_large);
    std::env::set_current_dir(previous_directory).unwrap();
    assert!(result.is_ok());
    assert_eq!(rejected.unwrap_err().message(), "text exceeds 1 MiB");
    assert_eq!(
        fs::metadata(path.join("exact.txt")).unwrap().len(),
        MAX_FILE_SIZE as u64
    );
    assert!(!path.join("large.txt").exists());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn directory_names_match_python_json_escaping_and_sort_order() {
    let codepoints = vec![0xdcff];
    assert_eq!(super::quote_json(&codepoints), "\"\\udcff\"");
}
