use super::{
    fstat, is_directory, is_regular_file, is_symlink, list_open_directory_with_limit,
    list_open_directory_with_limit_and_error, open_at, open_workspace_relative,
    open_workspace_relative_with_resolved_path, same_inode, stat_at, CliError, WorkspaceRoot,
    MAX_FILE_SIZE, MAX_TASK_DIRECTORY_ENTRIES,
};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub(super) const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
pub(super) const MAX_SEARCH_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_SEARCH_LOCATIONS: usize = 10_000;
pub(super) const MAX_TREE_DIRECTORY_DEPTH: usize = 16;
const MAX_TREE_DIRECTORY_ENTRIES: usize = 4_096;
const TREE_DIRECTORY_ENTRY_LIMIT_ERROR: &str = "search tree exceeds 4,096 directory entry limit";
const TREE_DIRECTORY_DEPTH_LIMIT_ERROR: &str = "search tree exceeds 16 directory depth limit";

#[derive(Clone, Default)]
struct IgnoreRules {
    layers: Vec<Arc<Gitignore>>,
}

impl IgnoreRules {
    fn push(&mut self, layer: Option<Gitignore>) {
        if let Some(layer) = layer {
            self.layers.push(Arc::new(layer));
        }
    }

    fn ignores(&self, root: &WorkspaceRoot, relative_path: &Path, is_directory: bool) -> bool {
        let path = root.path().join(normalize_relative_path(relative_path));
        let mut ignored = false;
        for layer in &self.layers {
            let matched = layer.matched_path_or_any_parents(&path, is_directory);
            if matched.is_ignore() {
                ignored = true;
            } else if matched.is_whitelist() {
                ignored = false;
            }
        }
        ignored
    }
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    normalized
}

fn is_git_metadata_path(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(".git"))
}

fn ignore_rules_for_directory(
    root: &WorkspaceRoot,
    directory_path: &OsStr,
    selected_directory: &File,
    bytes_scanned: &mut usize,
) -> Result<Option<IgnoreRules>, CliError> {
    let normalized_directory = normalize_relative_path(Path::new(directory_path));
    if normalized_directory
        .components()
        .any(|part| part.as_os_str() == OsStr::new(".git"))
    {
        return Ok(None);
    }

    let has_parent_component = Path::new(directory_path)
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    let can_walk_normalized_path = if has_parent_component {
        match open_workspace_relative(root, normalized_directory.as_os_str(), true) {
            Ok(normalized) => {
                let normalized_status = fstat(normalized.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                let selected_status = fstat(selected_directory.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                same_inode(&normalized_status, &selected_status) && is_directory(&normalized_status)
            }
            Err(_) => false,
        }
    } else {
        true
    };

    let root_directory = File::from(
        root.process_descriptor()
            .map_err(|_| CliError::new("unable to read ignore file"))?,
    );
    let mut rules = IgnoreRules::default();
    rules.push(read_ignore_file(
        root,
        &root_directory,
        Path::new("."),
        bytes_scanned,
    )?);

    if can_walk_normalized_path {
        let mut prefix = PathBuf::new();
        let components = normalized_directory.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                continue;
            };
            prefix.push(name);
            if rules.ignores(root, &prefix, true) {
                return Ok(None);
            }

            let directory = open_workspace_relative(root, prefix.as_os_str(), true)?;
            if index + 1 == components.len() {
                let opened_status = fstat(directory.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                let selected_status = fstat(selected_directory.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                if !same_inode(&opened_status, &selected_status) || !is_directory(&opened_status) {
                    return Err(CliError::new("directory changed while searching"));
                }
            }
            rules.push(read_ignore_file(root, &directory, &prefix, bytes_scanned)?);
        }
    } else {
        if rules.ignores(root, &normalized_directory, true) {
            return Ok(None);
        }
        rules.push(read_ignore_file(
            root,
            selected_directory,
            &normalized_directory,
            bytes_scanned,
        )?);
    }
    Ok(Some(rules))
}

fn read_ignore_file(
    root: &WorkspaceRoot,
    directory: &File,
    relative_directory: &Path,
    bytes_scanned: &mut usize,
) -> Result<Option<Gitignore>, CliError> {
    let directory_fd = directory.as_raw_fd();
    let name = OsStr::new(".gitignore");
    let status = match stat_at(directory_fd, name) {
        Ok(status) => status,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(_) => return Err(CliError::new("unable to read ignore file")),
    };
    if is_symlink(&status) || !is_regular_file(&status) {
        return Err(CliError::new("unable to read ignore file"));
    }
    let size =
        u64::try_from(status.st_size).map_err(|_| CliError::new("unable to read ignore file"))?;
    if size > MAX_FILE_SIZE as u64 {
        return Err(CliError::new("ignore file exceeds 1 MiB"));
    }
    let remaining = MAX_SEARCH_BYTES - *bytes_scanned;
    if size > remaining as u64 {
        return Err(CliError::new("search exceeds 16 MiB scan limit"));
    }

    let file = open_at(
        directory_fd,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to read ignore file"))?;
    let opened_status =
        fstat(file.as_raw_fd()).map_err(|_| CliError::new("unable to read ignore file"))?;
    if !same_inode(&status, &opened_status) || !is_regular_file(&opened_status) {
        return Err(CliError::new("ignore file changed while searching"));
    }
    let opened_size = u64::try_from(opened_status.st_size)
        .map_err(|_| CliError::new("unable to read ignore file"))?;
    if opened_size > MAX_FILE_SIZE as u64 {
        return Err(CliError::new("ignore file exceeds 1 MiB"));
    }
    if opened_size > remaining as u64 {
        return Err(CliError::new("search exceeds 16 MiB scan limit"));
    }

    let read_limit = MAX_FILE_SIZE.min(remaining);
    let mut contents = Vec::with_capacity(opened_size as usize);
    File::from(file)
        .take(read_limit as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|_| CliError::new("unable to read ignore file"))?;
    if contents.len() > MAX_FILE_SIZE {
        return Err(CliError::new("ignore file exceeds 1 MiB"));
    }
    if contents.len() > remaining {
        return Err(CliError::new("search exceeds 16 MiB scan limit"));
    }
    let current_status = stat_at(directory_fd, name)
        .map_err(|_| CliError::new("ignore file changed while searching"))?;
    if !same_inode(&opened_status, &current_status) || !is_regular_file(&current_status) {
        return Err(CliError::new("ignore file changed while searching"));
    }
    *bytes_scanned += contents.len();

    let contents =
        String::from_utf8(contents).map_err(|_| CliError::new("ignore file is not valid UTF-8"))?;
    let ignore_path = root
        .path()
        .join(normalize_relative_path(relative_directory))
        .join(name);
    let mut builder = GitignoreBuilder::new(root.path());
    builder.allow_unclosed_class(false);
    // Parse only the bytes read above; never let the parser reopen a path or load global rules.
    for (index, line) in contents.lines().enumerate() {
        let line = if index == 0 {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        builder
            .add_line(Some(ignore_path.clone()), line)
            .map_err(|_| CliError::new("ignore rules are invalid"))?;
    }
    let matcher = builder
        .build()
        .map_err(|_| CliError::new("ignore rules are invalid"))?;
    Ok((!matcher.is_empty()).then_some(matcher))
}

pub(crate) fn search_text_files_at(
    root: &WorkspaceRoot,
    directory_path: &OsStr,
    query: &str,
) -> Result<Vec<(PathBuf, usize)>, CliError> {
    if query.is_empty() {
        return Err(CliError::new("search query must not be empty"));
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(CliError::new("search query exceeds 4 KiB"));
    }

    let mut relative_directory = PathBuf::new();
    for component in Path::new(directory_path).components() {
        match component {
            Component::CurDir => {}
            other => relative_directory.push(other.as_os_str()),
        }
    }

    let (directory, resolved_directory_path) =
        open_workspace_relative_with_resolved_path(root, directory_path, true)?;
    if is_git_metadata_path(&resolved_directory_path) {
        return Ok(Vec::new());
    }
    let mut bytes_scanned = 0_usize;
    let Some(ignore_rules) =
        ignore_rules_for_directory(root, directory_path, &directory, &mut bytes_scanned)?
    else {
        return Ok(Vec::new());
    };
    let directory_fd = directory.as_raw_fd();
    let entries_directory = open_at(
        directory_fd,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| CliError::new("unable to search directory"))?;
    let names = list_open_directory_with_limit(
        File::from(entries_directory),
        Some(MAX_TASK_DIRECTORY_ENTRIES),
    )?;
    let mut locations = Vec::new();

    for entry in names {
        let status = stat_at(directory_fd, &entry.name)
            .map_err(|_| CliError::new("unable to search directory"))?;
        if is_symlink(&status) || !is_regular_file(&status) {
            continue;
        }
        let relative_path = relative_directory.join(&entry.name);
        if ignore_rules.ignores(root, &relative_path, false) {
            continue;
        }
        let size = u64::try_from(status.st_size)
            .map_err(|_| CliError::new("unable to read search file"))?;
        if size > MAX_FILE_SIZE as u64 {
            return Err(CliError::new("file exceeds 1 MiB"));
        }

        let remaining = MAX_SEARCH_BYTES - bytes_scanned;
        if size > remaining as u64 {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }

        let file = open_at(
            directory_fd,
            &entry.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("unable to read search file"))?;
        let file = File::from(file);
        let metadata = file
            .metadata()
            .map_err(|_| CliError::new("unable to read search file"))?;
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_FILE_SIZE as u64 {
            return Err(CliError::new("file exceeds 1 MiB"));
        }
        if metadata.len() > remaining as u64 {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }

        let read_limit = MAX_FILE_SIZE.min(remaining);
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(read_limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CliError::new("unable to read search file"))?;
        if bytes.len() > MAX_FILE_SIZE {
            return Err(CliError::new("file exceeds 1 MiB"));
        }
        if bytes.len() > remaining {
            return Err(CliError::new("search exceeds 16 MiB scan limit"));
        }
        bytes_scanned += bytes.len();

        let contents =
            String::from_utf8(bytes).map_err(|_| CliError::new("file is not valid UTF-8"))?;
        append_search_locations(&contents, query, &relative_path, &mut locations)?;
    }

    Ok(locations)
}

pub(crate) fn search_tree_text_files_at(
    root: &WorkspaceRoot,
    directory_path: &OsStr,
    query: &str,
) -> Result<Vec<(PathBuf, usize)>, CliError> {
    if query.is_empty() {
        return Err(CliError::new("search query must not be empty"));
    }
    if query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(CliError::new("search query exceeds 4 KiB"));
    }

    let mut relative_directory = PathBuf::new();
    for component in Path::new(directory_path).components() {
        match component {
            Component::CurDir => {}
            other => relative_directory.push(other.as_os_str()),
        }
    }

    let (directory, resolved_directory_path) =
        open_workspace_relative_with_resolved_path(root, directory_path, true)?;
    if is_git_metadata_path(&resolved_directory_path) {
        return Ok(Vec::new());
    }
    let mut bytes_scanned = 0_usize;
    let Some(ignore_rules) =
        ignore_rules_for_directory(root, directory_path, &directory, &mut bytes_scanned)?
    else {
        return Ok(Vec::new());
    };
    let mut pending = VecDeque::new();
    pending.push_back((relative_directory, 0, directory, ignore_rules));
    let mut locations = Vec::new();
    let mut entries_considered = 0_usize;

    while let Some((relative_directory, depth, directory, ignore_rules)) = pending.pop_front() {
        let directory_fd = directory.as_raw_fd();
        let entries_directory = open_at(
            directory_fd,
            OsStr::new("."),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .map_err(|_| CliError::new("unable to search directory"))?;
        let remaining_entries = MAX_TREE_DIRECTORY_ENTRIES - entries_considered;
        let names = list_open_directory_with_limit_and_error(
            File::from(entries_directory),
            Some(remaining_entries),
            TREE_DIRECTORY_ENTRY_LIMIT_ERROR,
        )?;
        entries_considered += names.len();

        for entry in names {
            let status = stat_at(directory_fd, &entry.name)
                .map_err(|_| CliError::new("unable to search directory"))?;
            if is_symlink(&status) {
                continue;
            }
            let relative_path = relative_directory.join(&entry.name);
            if is_directory(&status) {
                if entry.name == OsStr::new(".git")
                    || ignore_rules.ignores(root, &relative_path, true)
                {
                    continue;
                }
                if depth == MAX_TREE_DIRECTORY_DEPTH {
                    return Err(CliError::new(TREE_DIRECTORY_DEPTH_LIMIT_ERROR));
                }
                let child = open_at(
                    directory_fd,
                    &entry.name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
                .map_err(|_| CliError::new("unable to search directory"))?;
                let opened_status = fstat(child.as_raw_fd())
                    .map_err(|_| CliError::new("unable to search directory"))?;
                if !same_inode(&status, &opened_status) || !is_directory(&opened_status) {
                    return Err(CliError::new("directory changed while searching"));
                }
                let child_directory = File::from(child);
                let mut child_rules = ignore_rules.clone();
                child_rules.push(read_ignore_file(
                    root,
                    &child_directory,
                    &relative_path,
                    &mut bytes_scanned,
                )?);
                pending.push_back((relative_path, depth + 1, child_directory, child_rules));
                continue;
            }
            if !is_regular_file(&status) {
                continue;
            }
            if ignore_rules.ignores(root, &relative_path, false) {
                continue;
            }

            let size = u64::try_from(status.st_size)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if size > MAX_FILE_SIZE as u64 {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            let remaining_bytes = MAX_SEARCH_BYTES - bytes_scanned;
            if size > remaining_bytes as u64 {
                return Err(CliError::new("search exceeds 16 MiB scan limit"));
            }

            let file = open_at(
                directory_fd,
                &entry.name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0,
            )
            .map_err(|_| CliError::new("unable to read search file"))?;
            let opened_status =
                fstat(file.as_raw_fd()).map_err(|_| CliError::new("unable to read search file"))?;
            if !same_inode(&status, &opened_status) {
                return Err(CliError::new("file changed while searching"));
            }
            if !is_regular_file(&opened_status) {
                continue;
            }
            let opened_size = u64::try_from(opened_status.st_size)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if opened_size > MAX_FILE_SIZE as u64 {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            if opened_size > remaining_bytes as u64 {
                return Err(CliError::new("search exceeds 16 MiB scan limit"));
            }

            let read_limit = MAX_FILE_SIZE.min(remaining_bytes);
            let mut bytes = Vec::with_capacity(opened_size as usize);
            File::from(file)
                .take(read_limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| CliError::new("unable to read search file"))?;
            if bytes.len() > MAX_FILE_SIZE {
                return Err(CliError::new("file exceeds 1 MiB"));
            }
            if bytes.len() > remaining_bytes {
                return Err(CliError::new("search exceeds 16 MiB scan limit"));
            }
            bytes_scanned += bytes.len();

            let contents =
                String::from_utf8(bytes).map_err(|_| CliError::new("file is not valid UTF-8"))?;
            append_search_locations(&contents, query, &relative_path, &mut locations)?;
        }
    }

    locations.sort_unstable();
    Ok(locations)
}

fn append_search_locations(
    contents: &str,
    query: &str,
    path: &Path,
    locations: &mut Vec<(PathBuf, usize)>,
) -> Result<(), CliError> {
    let query = query.as_bytes();
    let mut prefix = vec![0; query.len()];
    for index in 1..query.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && query[index] != query[matched] {
            matched = prefix[matched - 1];
        }
        if query[index] == query[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }

    let mut matched = 0;
    let mut line = 1;
    let mut recent_lines = VecDeque::with_capacity(query.len());
    let mut last_location_line = None;
    for &byte in contents.as_bytes() {
        recent_lines.push_back(line);
        if recent_lines.len() > query.len() {
            recent_lines.pop_front();
        }

        while matched > 0 && byte != query[matched] {
            matched = prefix[matched - 1];
        }
        if byte == query[matched] {
            matched += 1;
        }
        if matched == query.len() {
            let location_line = recent_lines
                .front()
                .copied()
                .ok_or_else(|| CliError::new("unable to search file"))?;
            if last_location_line != Some(location_line) {
                if locations.len() == MAX_SEARCH_LOCATIONS {
                    return Err(CliError::new("search exceeds 10,000 result locations"));
                }
                locations.push((path.to_path_buf(), location_line));
                last_location_line = Some(location_line);
            }
            matched = prefix[matched - 1];
        }

        if byte == b'\n' {
            line += 1;
        }
    }

    Ok(())
}
