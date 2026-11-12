use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const MAX_FILE_SIZE: usize = 1024 * 1024;
const MAX_SYMLINKS: usize = 40;
const MAX_TASK_DIRECTORY_ENTRIES: usize = 1024;

#[derive(Debug)]
pub(crate) struct CliError(String);

impl CliError {
    fn new(message: &'static str) -> Self {
        Self(message.to_owned())
    }

    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) struct WorkspaceRoot {
    descriptor: OwnedFd,
    path: PathBuf,
}

pub(crate) fn capture_workspace_root() -> Result<WorkspaceRoot, String> {
    let descriptor = open_current_directory()
        .map_err(|error| format!("unable to open current directory: {error}"))?;
    let path = fs::canonicalize(".")
        .map_err(|error| format!("unable to resolve current directory: {error}"))?;
    verify_root_descriptor(descriptor.as_raw_fd(), &path)
        .map_err(|error| format!("unable to verify current directory: {}", error.message()))?;
    Ok(WorkspaceRoot { descriptor, path })
}

pub(crate) fn read_text_file(path: &OsStr) -> Result<String, CliError> {
    read_open_text_file(open_cwd_relative(path, false)?)
}

pub(crate) fn read_text_file_at(root: &WorkspaceRoot, path: &OsStr) -> Result<String, CliError> {
    read_open_text_file(open_workspace_relative(root, path, false)?)
}

fn read_open_text_file(mut file: File) -> Result<String, CliError> {
    let metadata = file
        .metadata()
        .map_err(|_| CliError::new("unable to read file"))?;
    if !metadata.is_file() {
        return Err(CliError::new("not a regular file"));
    }
    if metadata.len() > MAX_FILE_SIZE as u64 {
        return Err(CliError::new("file exceeds 1 MiB"));
    }

    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_FILE_SIZE as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CliError::new("unable to read file"))?;
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("file exceeds 1 MiB"));
    }
    String::from_utf8(contents).map_err(|_| CliError::new("file is not valid UTF-8"))
}

pub(crate) fn list_directory(path: &OsStr) -> Result<Vec<String>, CliError> {
    list_directory_entries(path).map(|names| {
        names
            .into_iter()
            .map(|name| quote_json(&name.sort_key))
            .collect()
    })
}

pub(crate) fn list_task_directory_names_at(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<Vec<OsString>, CliError> {
    list_open_directory_with_limit(
        open_workspace_relative(root, path, true)?,
        Some(MAX_TASK_DIRECTORY_ENTRIES),
    )
    .map(|names| names.into_iter().map(|name| name.name).collect())
}

fn list_directory_entries(path: &OsStr) -> Result<Vec<ListedName>, CliError> {
    list_open_directory(open_cwd_relative(path, true)?)
}

fn list_open_directory(directory: File) -> Result<Vec<ListedName>, CliError> {
    list_open_directory_with_limit(directory, None)
}

fn list_open_directory_with_limit(
    directory: File,
    maximum_entries: Option<usize>,
) -> Result<Vec<ListedName>, CliError> {
    let duplicate = duplicate_fd(directory.as_raw_fd())
        .map_err(|_| CliError::new("unable to list directory"))?;
    let raw = duplicate.into_raw_fd();
    // SAFETY: `raw` is a valid, independently owned directory descriptor; `fdopendir` takes ownership on success.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        // SAFETY: `fdopendir` did not take ownership when it returned null.
        unsafe { libc::close(raw) };
        return Err(CliError::new("unable to list directory"));
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();

    loop {
        // SAFETY: `__errno_location` returns this thread's errno slot on Linux.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `stream` owns a live DIR pointer until its Drop implementation closes it.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: Reading the thread-local errno slot is valid after a failed `readdir` call.
            if unsafe { *libc::__errno_location() } != 0 {
                return Err(CliError::new("unable to list directory"));
            }
            break;
        }
        // SAFETY: `d_name` is NUL-terminated by `readdir`, and is copied before the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            if maximum_entries.is_some_and(|maximum| names.len() == maximum) {
                return Err(CliError::new("directory exceeds 1,024 entries"));
            }
            names.push(ListedName::new(name.to_vec()));
        }
    }

    names.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(names)
}

pub(crate) fn create_text_file(path: &OsStr, text: &str) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    let root =
        open_current_directory().map_err(|_| CliError::new("unable to open current directory"))?;
    create_text_file_at_with_writer(root.as_raw_fd(), path, contents, write_to_fd)
}

pub(crate) fn create_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    text: &str,
) -> Result<(), CliError> {
    let contents = validate_create_text_file(path, text)?;
    create_text_file_at_with_writer(root.descriptor.as_raw_fd(), path, contents, write_to_fd)
}

pub(crate) fn replace_text_file_at(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
) -> Result<usize, CliError> {
    replace_text_file_at_with_writer(root, path, match_text, replacement_text, write_to_fd)
}

fn replace_text_file_at_with_writer<F>(
    root: &WorkspaceRoot,
    path: &OsStr,
    match_text: &str,
    replacement_text: &str,
    mut writer: F,
) -> Result<usize, CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid file path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid file path"));
    }
    if match_text.is_empty() {
        return Err(CliError::new("match text must not be empty"));
    }
    if match_text.len() > MAX_FILE_SIZE {
        return Err(CliError::new("match text exceeds 1 MiB"));
    }
    if replacement_text.len() > MAX_FILE_SIZE {
        return Err(CliError::new("replacement text exceeds 1 MiB"));
    }

    let (parent, name) = open_workspace_parent(root, path)?;
    let parent_fd = parent.as_raw_fd();
    let original_status = stat_at(parent_fd, &name).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENOENT) {
            CliError::new("file not found")
        } else {
            CliError::new("invalid path")
        }
    })?;
    if is_symlink(&original_status) {
        return Err(CliError::new("symlink paths are not allowed"));
    }
    if !is_regular_file(&original_status) {
        return Err(CliError::new("not a regular file"));
    }

    let original_file = open_at(
        parent_fd,
        &name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to open file"))?;
    let opened_status =
        fstat(original_file.as_raw_fd()).map_err(|_| CliError::new("unable to open file"))?;
    if !same_inode(&original_status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("file changed while replacing it"));
    }

    let contents = read_open_text_file(File::from(original_file))?;
    let range = unique_match_range(&contents, match_text)?;
    let output_length = contents
        .len()
        .checked_sub(match_text.len())
        .and_then(|length| length.checked_add(replacement_text.len()))
        .filter(|length| *length <= MAX_FILE_SIZE)
        .ok_or_else(|| CliError::new("replacement would exceed 1 MiB"))?;
    let mut replacement = String::with_capacity(output_length);
    replacement.push_str(&contents[..range.start]);
    replacement.push_str(replacement_text);
    replacement.push_str(&contents[range.end..]);

    replace_file_contents_atomically(
        parent_fd,
        &name,
        &original_status,
        replacement.as_bytes(),
        &mut writer,
    )?;
    Ok(output_length)
}

fn open_workspace_parent(
    root: &WorkspaceRoot,
    path: &OsStr,
) -> Result<(OwnedFd, OsString), CliError> {
    let mut components = split_components(path.as_bytes());
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    if matches!(name.as_bytes(), b"" | b"." | b"..") {
        return Err(CliError::new("invalid file path"));
    }

    let mut directories = vec![duplicate_fd(root.descriptor.as_raw_fd())
        .map_err(|_| CliError::new("unable to open current directory"))?];
    for component in components {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("parent directory not found")
            } else {
                CliError::new("invalid parent directory")
            }
        })?;
        if is_symlink(&status) {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        if !is_directory(&status) {
            return Err(CliError::new("parent is not a directory"));
        }

        let opened = open_at(
            parent_fd,
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("invalid parent directory"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while replacing file"));
        }
        directories.push(opened);
    }

    let parent = directories
        .pop()
        .expect("invariant: the root descriptor remains on the stack");
    Ok((parent, name))
}

fn unique_match_range(
    contents: &str,
    match_text: &str,
) -> Result<std::ops::Range<usize>, CliError> {
    let start = contents
        .find(match_text)
        .ok_or_else(|| CliError::new("match text was not found"))?;
    let next_character = contents[start..].chars().next().map_or(0, char::len_utf8);
    if contents[start + next_character..].contains(match_text) {
        return Err(CliError::new("match text is ambiguous"));
    }
    Ok(start..start + match_text.len())
}

fn replace_file_contents_atomically<F>(
    parent_fd: RawFd,
    name: &OsStr,
    original_status: &libc::stat,
    contents: &[u8],
    writer: &mut F,
) -> Result<(), CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let (temporary_name, temporary_file) = (0..3)
        .find_map(|_| {
            let name = OsString::from(format!(".krypper-replace-{}", uuid::Uuid::new_v4()));
            match open_at(
                parent_fd,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            ) {
                Ok(file) => Some(Ok((name, file))),
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => None,
                Err(_) => Some(Err(CliError::new("unable to stage file replacement"))),
            }
        })
        .transpose()?
        .ok_or_else(|| CliError::new("unable to stage file replacement"))?;

    let identity = fstat(temporary_file.as_raw_fd())
        .ok()
        .or_else(|| stat_at(parent_fd, &temporary_name).ok());
    let raw_fd = temporary_file.into_raw_fd();
    let mut failed = identity.is_none();
    if !failed && write_all_to_fd(raw_fd, contents, writer).is_err() {
        failed = true;
    }
    if !failed
        // SAFETY: `raw_fd` is the open temporary file descriptor transferred from `OwnedFd`.
        && unsafe { libc::fchmod(raw_fd, original_status.st_mode & 0o777) } != 0
    {
        failed = true;
    }
    if !failed
        // SAFETY: `raw_fd` remains open and refers to the complete staged file.
        && unsafe { libc::fsync(raw_fd) } != 0
    {
        failed = true;
    }
    // SAFETY: `raw_fd` was transferred from `OwnedFd` above and is closed exactly once here.
    if unsafe { libc::close(raw_fd) } != 0 {
        failed = true;
    }
    if failed {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("unable to stage file replacement"));
    }

    let current_status = stat_at(parent_fd, name).map_err(|_| {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        CliError::new("file changed while replacing it")
    })?;
    if !same_inode(original_status, &current_status) || !is_regular_file(&current_status) {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("file changed while replacing it"));
    }

    let temporary_name_c =
        c_string(&temporary_name).map_err(|_| CliError::new("unable to stage file replacement"))?;
    let name_c = c_string(name).map_err(|_| CliError::new("invalid file path"))?;
    // SAFETY: both names are NUL-terminated single path components and `parent_fd` is an open directory.
    if unsafe {
        libc::renameat(
            parent_fd,
            temporary_name_c.as_ptr(),
            parent_fd,
            name_c.as_ptr(),
        )
    } != 0
    {
        unlink_created_file(parent_fd, &temporary_name, identity.as_ref());
        return Err(CliError::new("unable to replace file"));
    }
    Ok(())
}

fn unlink_created_file(parent_fd: RawFd, name: &OsStr, identity: Option<&libc::stat>) {
    let Some(identity) = identity else {
        return;
    };
    let Ok(current) = stat_at(parent_fd, name) else {
        return;
    };
    let Ok(name) = c_string(name) else {
        return;
    };
    if same_inode(identity, &current) {
        // SAFETY: `parent_fd` is open, `name` is a single component, and the inode still matches the temporary file created above.
        unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
    }
}

fn write_all_to_fd<F>(fd: RawFd, contents: &[u8], writer: &mut F) -> io::Result<()>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let mut offset = 0;
    while offset < contents.len() {
        match writer(fd, &contents[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "zero-byte write")),
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => return Err(io::Error::other("writer reported an invalid byte count")),
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn validate_create_text_file<'a>(path: &OsStr, text: &'a str) -> Result<&'a [u8], CliError> {
    let path_bytes = path.as_bytes();
    if path_bytes.contains(&0) {
        return Err(CliError::new("invalid file path"));
    }
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    if path_bytes.is_empty() || path_bytes.ends_with(b"/") {
        return Err(CliError::new("invalid file path"));
    }

    let contents = text.as_bytes();
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("text exceeds 1 MiB"));
    }
    Ok(contents)
}

fn open_cwd_relative(path: &OsStr, directory: bool) -> Result<File, CliError> {
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }

    let root = open_current_directory().map_err(|_| CliError::new("unable to read file"))?;
    let root_path = fs::canonicalize(".").map_err(|_| CliError::new("invalid path"))?;
    open_root_relative(root, &root_path, path, directory)
}

fn open_workspace_relative(
    root: &WorkspaceRoot,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    if Path::new(path).is_absolute() {
        return Err(CliError::new("absolute paths are not allowed"));
    }
    let descriptor = duplicate_fd(root.descriptor.as_raw_fd())
        .map_err(|_| CliError::new("unable to read file"))?;
    open_root_relative(descriptor, &root.path, path, directory)
}

fn open_root_relative(
    root: OwnedFd,
    root_path: &Path,
    path: &OsStr,
    directory: bool,
) -> Result<File, CliError> {
    let mut directories = vec![root];
    let mut pending = split_components(path.as_bytes());
    let mut symlinks = 0;

    while let Some(component) = pending.pop_front() {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("file not found")
            } else {
                CliError::new("invalid path")
            }
        })?;

        if is_symlink(&status) {
            symlinks += 1;
            if symlinks > MAX_SYMLINKS {
                return Err(CliError::new("invalid path"));
            }
            let target =
                read_link_at(parent_fd, &component).map_err(|_| CliError::new("invalid path"))?;
            if target.first() == Some(&b'/') {
                let target_path = PathBuf::from(OsString::from_vec(target));
                let target_path = normalize_absolute(&target_path);
                if !target_path.starts_with(root_path) {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                verify_root_descriptor(directories[0].as_raw_fd(), root_path)?;
                let relative = target_path
                    .strip_prefix(root_path)
                    .map_err(|_| CliError::new("invalid path"))?;
                directories.truncate(1);
                prepend_components(&mut pending, relative.as_os_str().as_bytes());
            } else {
                prepend_components(&mut pending, &target);
            }
            continue;
        }

        let final_component = !pending
            .iter()
            .any(|part| !matches!(part.as_bytes(), b"" | b"."));
        if final_component {
            if directory && !is_directory(&status) {
                return Err(CliError::new("not a directory"));
            }
            if !directory && !is_regular_file(&status) {
                return Err(CliError::new("not a regular file"));
            }
        }

        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        if !final_component || directory {
            flags |= libc::O_DIRECTORY;
        }
        let opened = open_at(parent_fd, &component, flags, 0)
            .map_err(|_| CliError::new("unable to read file"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("unable to read file"))?;
        if !same_inode(&status, &opened_status)
            || (final_component && directory && !is_directory(&opened_status))
            || (final_component && !directory && !is_regular_file(&opened_status))
        {
            return Err(CliError::new("unable to read file"));
        }
        if final_component {
            return Ok(File::from(opened));
        }
        directories.push(opened);
    }

    let descriptor = duplicate_fd(directories[0].as_raw_fd())
        .map_err(|_| CliError::new("unable to read file"))?;
    Ok(File::from(descriptor))
}

fn create_text_file_at_with_writer<F>(
    root_fd: RawFd,
    path: &OsStr,
    contents: &[u8],
    mut writer: F,
) -> Result<(), CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let mut components = split_components(path.as_bytes());
    let Some(name) = components.pop_back() else {
        return Err(CliError::new("invalid file path"));
    };
    if matches!(name.as_bytes(), b"" | b"." | b"..") {
        return Err(CliError::new("invalid file path"));
    }

    let mut directories =
        vec![duplicate_fd(root_fd).map_err(|_| CliError::new("unable to open current directory"))?];
    for component in components {
        match component.as_bytes() {
            b"" | b"." => continue,
            b".." => {
                if directories.len() == 1 {
                    return Err(CliError::new("path must stay inside the current directory"));
                }
                directories.pop();
                continue;
            }
            _ => {}
        }

        let parent_fd = directories
            .last()
            .expect("invariant: the root descriptor remains on the stack")
            .as_raw_fd();
        let status = stat_at(parent_fd, &component).map_err(|error| {
            if error.raw_os_error() == Some(libc::ENOENT) {
                CliError::new("parent directory not found")
            } else {
                CliError::new("invalid parent directory")
            }
        })?;
        if is_symlink(&status) {
            return Err(CliError::new("symlink paths are not allowed"));
        }
        if !is_directory(&status) {
            return Err(CliError::new("parent is not a directory"));
        }

        let opened = open_at(
            parent_fd,
            &component,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("invalid parent directory"))?;
        let opened_status =
            fstat(opened.as_raw_fd()).map_err(|_| CliError::new("invalid parent directory"))?;
        if !same_inode(&status, &opened_status) {
            return Err(CliError::new("path changed while creating file"));
        }
        directories.push(opened);
    }

    let parent_fd = directories
        .last()
        .expect("invariant: the root descriptor remains on the stack")
        .as_raw_fd();
    let file = open_at(
        parent_fd,
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o666,
    )
    .map_err(|error| match error.raw_os_error() {
        Some(libc::EEXIST) => CliError::new("file already exists"),
        Some(libc::ENOENT) => CliError::new("parent directory not found"),
        _ => CliError::new("unable to create file"),
    })?;
    finish_new_file(file, parent_fd, &name, contents, &mut writer)
}

fn finish_new_file<F>(
    file: OwnedFd,
    parent_fd: RawFd,
    name: &OsStr,
    contents: &[u8],
    writer: &mut F,
) -> Result<(), CliError>
where
    F: FnMut(RawFd, &[u8]) -> io::Result<usize>,
{
    let raw_fd = file.into_raw_fd();
    let (identity, mut failure) = match fstat(raw_fd) {
        Ok(identity) => (Some(identity), false),
        Err(_) => (None, true),
    };
    let mut offset = 0;

    while !failure && offset < contents.len() {
        match writer(raw_fd, &contents[offset..]) {
            Ok(0) => failure = true,
            Ok(written) if written <= contents.len() - offset => offset += written,
            Ok(_) => failure = true,
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => failure = true,
        }
    }

    // SAFETY: `raw_fd` is the descriptor transferred from `OwnedFd` above and is closed exactly once here.
    if unsafe { libc::close(raw_fd) } != 0 {
        failure = true;
    }
    if !failure {
        return Ok(());
    }

    if let (Some(created), Ok(cleanup_name)) = (identity, c_string(name)) {
        if let Ok(current) = stat_at(parent_fd, name) {
            if same_inode(&created, &current) {
                // SAFETY: `parent_fd` is open and `cleanup_name` names one component; this removes only the inode created above.
                if unsafe { libc::unlinkat(parent_fd, cleanup_name.as_ptr(), 0) } != 0 {
                    return Err(CliError::new("unable to write file"));
                }
            }
        }
    }
    Err(CliError::new("unable to write file"))
}

fn write_to_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: `bytes` points to a readable buffer for `bytes.len()` bytes and `fd` is open for writing.
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

fn open_current_directory() -> io::Result<OwnedFd> {
    let path = CString::new(".").expect("invariant: a dot path contains no NUL byte");
    // SAFETY: `path` is NUL-terminated and remains valid for this call.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned_fd_or_error(descriptor)
}

fn open_at(
    parent_fd: RawFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    // SAFETY: `parent_fd` is an open directory descriptor and `name` is a NUL-terminated component.
    let descriptor = unsafe { libc::openat(parent_fd, name.as_ptr(), flags, mode) };
    owned_fd_or_error(descriptor)
}

fn owned_fd_or_error(descriptor: RawFd) -> io::Result<OwnedFd> {
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative descriptor returned by `open`/`openat` is newly owned by this function.
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn duplicate_fd(descriptor: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `descriptor` is live for the duration of this call.
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
    owned_fd_or_error(duplicate)
}

fn stat_at(parent_fd: RawFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_string(name)?;
    // SAFETY: the output points to initialized writable storage, and the parent/name arguments remain valid.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `parent_fd` is open, `name` is NUL-terminated, and `status` is writable.
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn fstat(descriptor: RawFd) -> io::Result<libc::stat> {
    // SAFETY: the output points to initialized writable storage and `descriptor` is a live file descriptor.
    let mut status = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: `descriptor` is live and `status` is writable for the syscall.
    if unsafe { libc::fstat(descriptor, &mut status) } == 0 {
        Ok(status)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_link_at(parent_fd: RawFd, name: &OsStr) -> io::Result<Vec<u8>> {
    let name = c_string(name)?;
    let mut capacity = 256;
    loop {
        let mut target = vec![0_u8; capacity];
        // SAFETY: `target` is writable for `capacity` bytes, and the path arguments are valid during the call.
        let length = unsafe {
            libc::readlinkat(
                parent_fd,
                name.as_ptr(),
                target.as_mut_ptr().cast(),
                target.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < target.len() {
            target.truncate(length);
            return Ok(target);
        }
        capacity = capacity
            .checked_mul(2)
            .filter(|length| *length <= 1024 * 1024)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "symlink target is too long")
            })?;
    }
}

fn split_components(path: &[u8]) -> VecDeque<OsString> {
    path.split(|byte| *byte == b'/')
        .map(|part| OsString::from_vec(part.to_vec()))
        .collect()
}

fn prepend_components(pending: &mut VecDeque<OsString>, path: &[u8]) {
    for component in split_components(path).into_iter().rev() {
        pending.push_front(component);
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn verify_root_descriptor(descriptor: RawFd, root_path: &Path) -> Result<(), CliError> {
    let opened = fstat(descriptor).map_err(|_| CliError::new("invalid path"))?;
    let named = fs::metadata(root_path).map_err(|_| CliError::new("invalid path"))?;
    if opened.st_dev != named.dev() || opened.st_ino != named.ino() {
        return Err(CliError::new("invalid path"));
    }
    Ok(())
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn same_inode(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

fn file_type(status: &libc::stat) -> libc::mode_t {
    status.st_mode & libc::S_IFMT
}

fn is_symlink(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFLNK
}

fn is_directory(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFDIR
}

fn is_regular_file(status: &libc::stat) -> bool {
    file_type(status) == libc::S_IFREG
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this object uniquely owns the live DIR pointer returned by `fdopendir`.
        unsafe { libc::closedir(self.0) };
    }
}

struct ListedName {
    sort_key: Vec<u32>,
    name: OsString,
}

impl ListedName {
    fn new(bytes: Vec<u8>) -> Self {
        let sort_key = python_codepoints(&bytes);
        let name = OsString::from_vec(bytes);
        Self { sort_key, name }
    }
}

fn python_codepoints(bytes: &[u8]) -> Vec<u32> {
    let mut codepoints = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(valid) => {
                codepoints.extend(valid.chars().map(u32::from));
                break;
            }
            Err(error) => {
                let valid_length = error.valid_up_to();
                if valid_length > 0 {
                    let valid = std::str::from_utf8(&bytes[offset..offset + valid_length])
                        .expect("invariant: `valid_up_to` identifies valid UTF-8");
                    codepoints.extend(valid.chars().map(u32::from));
                    offset += valid_length;
                }
                let invalid_length = error.error_len().unwrap_or(bytes.len() - offset);
                codepoints.extend(
                    bytes[offset..offset + invalid_length]
                        .iter()
                        .map(|byte| 0xdc00 + u32::from(*byte)),
                );
                offset += invalid_length;
            }
        }
    }
    codepoints
}

fn quote_json(codepoints: &[u32]) -> String {
    let mut quoted = String::from("\"");
    for codepoint in codepoints {
        match *codepoint {
            0x08 => quoted.push_str("\\b"),
            0x09 => quoted.push_str("\\t"),
            0x0a => quoted.push_str("\\n"),
            0x0c => quoted.push_str("\\f"),
            0x0d => quoted.push_str("\\r"),
            0x00..=0x1f => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x20..=0x7e if *codepoint != u32::from(b'"') && *codepoint != u32::from(b'\\') => {
                quoted.push(
                    char::from_u32(*codepoint).expect("invariant: ASCII is a Unicode scalar"),
                );
            }
            0x22 => quoted.push_str("\\\""),
            0x5c => quoted.push_str("\\\\"),
            0x7f..=0xffff => quoted.push_str(&format!("\\u{codepoint:04x}")),
            0x10000..=0x10ffff => {
                let scalar = codepoint - 0x10000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                quoted.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
            _ => quoted.push('\u{fffd}'),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::{
        create_text_file, create_text_file_at_with_writer, replace_text_file_at,
        replace_text_file_at_with_writer, WorkspaceRoot, MAX_FILE_SIZE,
    };
    use std::fs;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn failed_write_removes_the_partial_file_it_created() {
        let path = std::env::temp_dir().join(format!(
            "krypper-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let root = fs::File::open(&path).unwrap();
        let mut writes = 0;
        let result = create_text_file_at_with_writer(
            root.as_raw_fd(),
            std::ffi::OsStr::new("partial.txt"),
            b"complete text",
            |fd, bytes| {
                writes += 1;
                if writes == 1 {
                    // SAFETY: `fd` is the open file created by the function under test; `bytes` has at least three bytes.
                    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), 3) };
                    if count < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(count as usize)
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
    fn replacement_changes_only_one_exact_match_in_an_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "before exact match after").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        let result = replace_text_file_at(
            &root,
            std::ffi::OsStr::new("note.txt"),
            "exact match",
            "new text",
        );

        assert_eq!(result.unwrap(), "before new text after".len());
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            "before new text after"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_accepts_a_result_at_the_one_mib_limit() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-limit-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "old").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };
        let replacement = "x".repeat(MAX_FILE_SIZE);

        let result =
            replace_text_file_at(&root, std::ffi::OsStr::new("note.txt"), "old", &replacement);

        assert_eq!(result.unwrap(), MAX_FILE_SIZE);
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            replacement
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_rejects_empty_missing_and_ambiguous_matches_without_changes() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-matches-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "aaaa").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };

        for (match_text, expected_error) in [
            ("", "match text must not be empty"),
            ("missing", "match text was not found"),
            ("aa", "match text is ambiguous"),
        ] {
            let error = replace_text_file_at(
                &root,
                std::ffi::OsStr::new("note.txt"),
                match_text,
                "replacement",
            )
            .unwrap_err();
            assert_eq!(error.message(), expected_error);
            assert_eq!(fs::read_to_string(path.join("note.txt")).unwrap(), "aaaa");
        }

        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn failed_replacement_keeps_the_original_and_removes_its_partial_stage() {
        let path = std::env::temp_dir().join(format!(
            "krypper-replace-write-failure-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("note.txt"), "before exact match after").unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&path).unwrap().into(),
            path: fs::canonicalize(&path).unwrap(),
        };
        let mut writes = 0;

        let result = replace_text_file_at_with_writer(
            &root,
            std::ffi::OsStr::new("note.txt"),
            "exact match",
            "a much longer replacement",
            |fd, bytes| {
                writes += 1;
                if writes == 1 {
                    // SAFETY: `fd` is the staged file descriptor and `bytes` contains at least three bytes.
                    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), 3) };
                    if count < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(count as usize)
                } else {
                    Err(io::Error::other("simulated disk failure"))
                }
            },
        );

        assert_eq!(
            result.unwrap_err().message(),
            "unable to stage file replacement"
        );
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            "before exact match after"
        );
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_rejects_unsafe_paths_non_regular_files_and_oversize_content() {
        use std::os::unix::fs::symlink;

        let path = std::env::temp_dir().join(format!(
            "krypper-replace-safety-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let outside = path.join("outside");
        let workspace = path.join("workspace");
        fs::create_dir(&path).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(&outside, "outside text").unwrap();
        fs::write(workspace.join("note.txt"), "regular text").unwrap();
        fs::write(workspace.join("large.txt"), vec![b'x'; MAX_FILE_SIZE + 1]).unwrap();
        fs::write(workspace.join("not-a-directory"), "file").unwrap();
        fs::create_dir(workspace.join("directory")).unwrap();
        symlink("note.txt", workspace.join("linked.txt")).unwrap();
        symlink(".", workspace.join("linked-parent")).unwrap();
        let root = WorkspaceRoot {
            descriptor: fs::File::open(&workspace).unwrap().into(),
            path: fs::canonicalize(&workspace).unwrap(),
        };
        let too_long = "x".repeat(MAX_FILE_SIZE + 1);
        let fits_but_overflows = "x".repeat(MAX_FILE_SIZE);
        let cases = [
            (
                std::ffi::OsStr::new("../outside"),
                "text",
                "replacement",
                "path must stay inside the current directory",
            ),
            (
                std::ffi::OsStr::new("linked.txt"),
                "text",
                "replacement",
                "symlink paths are not allowed",
            ),
            (
                std::ffi::OsStr::new("linked-parent/note.txt"),
                "text",
                "replacement",
                "symlink paths are not allowed",
            ),
            (
                std::ffi::OsStr::new("missing-parent/new.txt"),
                "text",
                "replacement",
                "parent directory not found",
            ),
            (
                std::ffi::OsStr::new("not-a-directory/new.txt"),
                "text",
                "replacement",
                "parent is not a directory",
            ),
            (
                std::ffi::OsStr::new("directory"),
                "text",
                "replacement",
                "not a regular file",
            ),
            (
                std::ffi::OsStr::new("large.txt"),
                "x",
                "replacement",
                "file exceeds 1 MiB",
            ),
            (
                std::ffi::OsStr::new("note.txt"),
                "text",
                fits_but_overflows.as_str(),
                "replacement would exceed 1 MiB",
            ),
            (
                std::ffi::OsStr::new("note.txt"),
                "text",
                too_long.as_str(),
                "replacement text exceeds 1 MiB",
            ),
        ];

        for (file, match_text, replacement_text, expected_error) in cases {
            let error =
                replace_text_file_at(&root, file, match_text, replacement_text).unwrap_err();
            assert_eq!(error.message(), expected_error);
        }
        assert_eq!(
            fs::read_to_string(workspace.join("note.txt")).unwrap(),
            "regular text"
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside text");
        assert_eq!(fs::read_dir(&workspace).unwrap().count(), 6);
        assert!(!workspace.join("missing-parent").exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn write_enforces_the_encoded_one_mib_limit_before_creation() {
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
        let name = super::ListedName::new(vec![b'\xff']);
        assert_eq!(super::quote_json(&name.sort_key), "\"\\udcff\"");
        assert_eq!(name.sort_key, vec![0xdcff]);
    }
}
