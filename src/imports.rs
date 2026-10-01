use crate::format::{decode_token, encode_token};
use crate::json;
use crate::private_fs;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

const REGISTRY_FILE: &str = "managed-imports.tsv";
const IMPORTS_DIRECTORY: &str = "managed-imports";
const FILE_MANIFEST: &str = "files.tsv";
const REGISTRY_HEADER: &str = "#ebira-managed-imports\tv=1";
const FILES_HEADER: &str = "#ebira-managed-import-files\tv=1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedImport {
    pub import_id: String,
    pub provenance: String,
    pub label: String,
    pub source_computer: String,
    pub original_path: String,
    pub imported_at_ms: u64,
    pub managed_relative: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct ObservedImport {
    pub import: ManagedImport,
    pub managed_source: String,
    pub file_manifest: String,
    pub disposition: String,
}

#[derive(Clone, Debug, Default)]
pub struct ImportInventory {
    pub entries: Vec<ObservedImport>,
    pub unavailable_imports: u64,
}

impl ImportInventory {
    pub fn source_inputs(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.managed_source.clone())
            .collect()
    }

    /// The registered imports as a JSON array, for `ebira status`.
    pub fn entries_json(&self) -> String {
        observed_entries_json(&self.entries)
    }
}

#[derive(Clone, Debug)]
pub struct ImportReport {
    pub import: ManagedImport,
    pub store: String,
    pub registry: String,
    pub managed_source: String,
    pub file_manifest: String,
    pub corpus: String,
    pub projection_state: &'static str,
}

impl ImportReport {
    pub fn json(&self) -> String {
        let sync = json::Object::new()
            .name("action", "sync")
            .raw(
                "request",
                &json::Object::new().name("corpus", &self.corpus).finish(),
            )
            .finish();
        json::Object::new()
            .name("disposition", "imported")
            .name("mode", "managed_jsonl_import")
            .name("import_id", &self.import.import_id)
            .text("provenance", &self.import.provenance)
            .text("label", &self.import.label)
            .name("source_computer", &self.import.source_computer)
            .name("original_path", &self.import.original_path)
            .number("imported_at_ms", self.import.imported_at_ms)
            .name("store", &self.store)
            .name("registry", &self.registry)
            .name("managed_source", &self.managed_source)
            .name("file_manifest", &self.file_manifest)
            .number("files_copied", self.import.files)
            .number("bytes_copied", self.import.bytes)
            .name("copy_disposition", "complete")
            .name("projection_state", self.projection_state)
            .raw("next_actions", &json::array([sync]))
            .finish()
    }
}

#[derive(Clone, Debug)]
struct ImportFile {
    source: PathBuf,
    relative: PathBuf,
    bytes: u64,
    modified_ms: u64,
}

/// Managed imports live inside the corpus directory, beside the files generated from them, so
/// one directory holds everything a corpus needs and the corpus lock covers both.
pub fn store_root(corpus: &Path) -> PathBuf {
    corpus.to_path_buf()
}

pub fn inventory(corpus: &Path) -> io::Result<ImportInventory> {
    let store = store_root(corpus);
    let registry = store.join(REGISTRY_FILE);
    let imports = load_registry(&registry)?;
    let mut entries = Vec::with_capacity(imports.len());
    let mut unavailable_imports = 0u64;
    for import in imports {
        let managed_source_path = store.join(&import.managed_relative);
        let file_manifest_path = managed_source_path
            .parent()
            .unwrap_or(&managed_source_path)
            .join(FILE_MANIFEST);
        let disposition = match fs::metadata(&managed_source_path) {
            Ok(metadata) if metadata.is_dir() => "present",
            Ok(_) => "not_directory",
            Err(error) if error.kind() == io::ErrorKind::NotFound => "missing",
            Err(_) => "unreadable",
        };
        if disposition != "present" {
            unavailable_imports = unavailable_imports.saturating_add(1);
        }
        entries.push(ObservedImport {
            import,
            managed_source: managed_source_path.to_string_lossy().into_owned(),
            file_manifest: file_manifest_path.to_string_lossy().into_owned(),
            disposition: disposition.to_string(),
        });
    }
    Ok(ImportInventory {
        entries,
        unavailable_imports,
    })
}

pub fn import(
    corpus: &Path,
    source: &Path,
    provenance: &str,
    label: &str,
    source_computer: &str,
) -> io::Result<ImportReport> {
    if provenance.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--provenance must name where this copy came from",
        ));
    }
    if label.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--label must name this managed import",
        ));
    }
    let original = fs::canonicalize(source).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot import {}: {}", source.display(), error),
        )
    })?;
    let plans = collect_jsonl(&original)?;
    if plans.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} contains no JSONL files", original.display()),
        ));
    }

    let store = store_root(corpus);
    private_fs::create_dir_all(&store)?;
    let imports_root = store.join(IMPORTS_DIRECTORY);
    private_fs::create_dir_all(&imports_root)?;
    let imported_at_ms = now_ms();
    let import_id = unused_import_id(&imports_root, imported_at_ms);
    let partial_root = imports_root.join(format!("{}.partial", import_id));
    let final_root = imports_root.join(&import_id);
    let partial_jsonl = partial_root.join("jsonl");
    private_fs::create_dir_all(&partial_jsonl)?;

    let mut copied_files = Vec::with_capacity(plans.len());
    let mut bytes_copied = 0u64;
    for plan in plans {
        let before = fs::metadata(&plan.source)?;
        if !before.is_file() {
            return Err(import_changed(&plan.source, "is no longer a file"));
        }
        let before_modified_ms = modified_ms(&before);
        let destination = partial_jsonl.join(&plan.relative);
        if let Some(parent) = destination.parent() {
            private_fs::create_dir_all(parent)?;
        }
        let copied = private_fs::copy(&plan.source, &destination)?;
        let after = fs::metadata(&plan.source)?;
        let destination_size = fs::metadata(&destination)?.len();
        if copied != before.len()
            || destination_size != before.len()
            || after.len() != before.len()
            || modified_ms(&after) != before_modified_ms
        {
            return Err(import_changed(
                &plan.source,
                "changed while its bytes were being copied",
            ));
        }
        bytes_copied = bytes_copied.saturating_add(copied);
        copied_files.push(ImportFile {
            source: plan.source,
            relative: plan.relative,
            bytes: copied,
            modified_ms: before_modified_ms,
        });
    }
    write_file_manifest(&partial_root.join(FILE_MANIFEST), &copied_files)?;
    fs::rename(&partial_root, &final_root)?;

    let managed_relative = Path::new(IMPORTS_DIRECTORY)
        .join(&import_id)
        .join("jsonl")
        .to_string_lossy()
        .into_owned();
    let entry = ManagedImport {
        import_id: import_id.clone(),
        provenance: provenance.to_string(),
        label: label.to_string(),
        source_computer: source_computer.to_string(),
        original_path: original.to_string_lossy().into_owned(),
        imported_at_ms,
        managed_relative: managed_relative.clone(),
        files: copied_files.len() as u64,
        bytes: bytes_copied,
    };
    let registry = store.join(REGISTRY_FILE);
    let mut entries = load_registry(&registry)?;
    entries.push(entry.clone());
    write_registry(&registry, &entries)?;

    let managed_source = store.join(&managed_relative);
    let projection_state = if corpus.join("sources.tsv").is_file() {
        "not_synced"
    } else {
        "not_built"
    };
    Ok(ImportReport {
        import: entry,
        store: store.to_string_lossy().into_owned(),
        registry: registry.to_string_lossy().into_owned(),
        managed_source: managed_source.to_string_lossy().into_owned(),
        file_manifest: final_root
            .join(FILE_MANIFEST)
            .to_string_lossy()
            .into_owned(),
        corpus: corpus.to_string_lossy().into_owned(),
        projection_state,
    })
}

fn collect_jsonl(source: &Path) -> io::Result<Vec<ImportFile>> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is a symbolic link; import the resolved JSONL path",
                source.display()
            ),
        ));
    }
    let mut files = Vec::new();
    if metadata.is_file() {
        if !is_jsonl(source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a JSONL file", source.display()),
            ));
        }
        let relative = source.file_name().map(PathBuf::from).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source has no file name")
        })?;
        files.push(import_file(source.to_path_buf(), relative)?);
    } else if metadata.is_dir() {
        collect_directory(source, source, &mut files)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is neither a file nor a directory", source.display()),
        ));
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    let mut relative_paths = BTreeSet::new();
    for file in &files {
        if !relative_paths.insert(file.relative.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate JSONL import path: {}", file.relative.display()),
            ));
        }
    }
    Ok(files)
}

fn collect_directory(root: &Path, directory: &Path, files: &mut Vec<ImportFile>) -> io::Result<()> {
    let mut children = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is a symbolic link inside the import; import the resolved JSONL tree",
                    child.display()
                ),
            ));
        }
        if metadata.is_dir() {
            collect_directory(root, &child, files)?;
        } else if metadata.is_file() && is_jsonl(&child) {
            let relative = child.strip_prefix(root).map(PathBuf::from).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "import path escaped root")
            })?;
            files.push(import_file(child, relative)?);
        }
    }
    Ok(())
}

fn import_file(source: PathBuf, relative: PathBuf) -> io::Result<ImportFile> {
    let metadata = fs::metadata(&source)?;
    Ok(ImportFile {
        source,
        relative,
        bytes: metadata.len(),
        modified_ms: modified_ms(&metadata),
    })
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false)
}

fn unused_import_id(root: &Path, imported_at_ms: u64) -> String {
    let base = format!("import-{}-{}", imported_at_ms, std::process::id());
    for suffix in 0u64.. {
        let candidate = if suffix == 0 {
            base.clone()
        } else {
            format!("{}-{}", base, suffix)
        };
        if !root.join(&candidate).exists() && !root.join(format!("{}.partial", candidate)).exists()
        {
            return candidate;
        }
    }
    unreachable!("u64 import suffixes exhausted")
}

fn load_registry(path: &Path) -> io::Result<Vec<ManagedImport>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(invalid_registry(path, 1, "registry path is not a file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "{}: cannot inspect managed import registry: {}",
                    path.display(),
                    error
                ),
            ))
        }
    }
    let file = File::open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "{}: cannot read managed import registry: {}",
                path.display(),
                error
            ),
        )
    })?;
    let mut lines = BufReader::new(file).lines();
    let header = lines
        .next()
        .transpose()?
        .ok_or_else(|| invalid_registry(path, 1, "registry is empty"))?;
    if header != REGISTRY_HEADER {
        return Err(invalid_registry(
            path,
            1,
            "registry header is not the format this build writes",
        ));
    }
    let mut imports = Vec::new();
    let mut ids = BTreeSet::new();
    for (offset, line) in lines.enumerate() {
        let line_number = offset + 2;
        let line = line?;
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 9 {
            return Err(invalid_registry(
                path,
                line_number,
                "registry row does not have 9 columns",
            ));
        }
        let import_id = registry_token(path, line_number, parts[0])?;
        if import_id.is_empty() || !ids.insert(import_id.clone()) {
            return Err(invalid_registry(
                path,
                line_number,
                "import_id is empty or duplicated",
            ));
        }
        let managed_relative = registry_token(path, line_number, parts[6])?;
        if !is_safe_relative(Path::new(&managed_relative)) {
            return Err(invalid_registry(
                path,
                line_number,
                "managed path is not a relative path owned by this store",
            ));
        }
        imports.push(ManagedImport {
            import_id,
            provenance: registry_token(path, line_number, parts[1])?,
            label: registry_token(path, line_number, parts[2])?,
            source_computer: registry_token(path, line_number, parts[3])?,
            original_path: registry_token(path, line_number, parts[4])?,
            imported_at_ms: registry_u64(path, line_number, parts[5], "imported_at_ms")?,
            managed_relative,
            files: registry_u64(path, line_number, parts[7], "files")?,
            bytes: registry_u64(path, line_number, parts[8], "bytes")?,
        });
    }
    imports.sort_by(|left, right| {
        left.imported_at_ms
            .cmp(&right.imported_at_ms)
            .then_with(|| left.import_id.cmp(&right.import_id))
    });
    Ok(imports)
}

fn write_registry(path: &Path, imports: &[ManagedImport]) -> io::Result<()> {
    let partial = path.with_file_name(format!("{}.partial", REGISTRY_FILE));
    let mut writer = BufWriter::new(private_fs::create(&partial)?);
    writeln!(writer, "{}", REGISTRY_HEADER)?;
    let mut imports = imports.to_vec();
    imports.sort_by(|left, right| {
        left.imported_at_ms
            .cmp(&right.imported_at_ms)
            .then_with(|| left.import_id.cmp(&right.import_id))
    });
    for import in imports {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            encode_token(&import.import_id),
            encode_token(&import.provenance),
            encode_token(&import.label),
            encode_token(&import.source_computer),
            encode_token(&import.original_path),
            import.imported_at_ms,
            encode_token(&import.managed_relative),
            import.files,
            import.bytes,
        )?;
    }
    writer.flush()?;
    drop(writer);
    crate::corpus::replace_file(&partial, path)?;
    Ok(())
}

fn write_file_manifest(path: &Path, files: &[ImportFile]) -> io::Result<()> {
    let mut writer = BufWriter::new(private_fs::create(path)?);
    writeln!(writer, "{}", FILES_HEADER)?;
    for file in files {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}",
            encode_token(&file.source.to_string_lossy()),
            encode_token(&file.relative.to_string_lossy()),
            file.bytes,
            file.modified_ms,
        )?;
    }
    writer.flush()?;
    Ok(())
}

fn observed_entries_json(entries: &[ObservedImport]) -> String {
    json::array(entries.iter().map(|entry| {
        json::Object::new()
            .name("import_id", &entry.import.import_id)
            .text("provenance", &entry.import.provenance)
            .text("label", &entry.import.label)
            .name("source_computer", &entry.import.source_computer)
            .name("original_path", &entry.import.original_path)
            .number("imported_at_ms", entry.import.imported_at_ms)
            .name("managed_source", &entry.managed_source)
            .name("file_manifest", &entry.file_manifest)
            .number("files", entry.import.files)
            .number("bytes", entry.import.bytes)
            .name("disposition", &entry.disposition)
            .finish()
    }))
}

fn is_safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn registry_token(path: &Path, line: usize, value: &str) -> io::Result<String> {
    decode_token(value).map_err(|_| invalid_registry(path, line, "invalid escaped token"))
}

fn registry_u64(path: &Path, line: usize, value: &str, field: &str) -> io::Result<u64> {
    value
        .parse()
        .map_err(|_| invalid_registry(path, line, &format!("invalid {}", field)))
}

fn invalid_registry(path: &Path, line: usize, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}:{}: {}", path.display(), line, message),
    )
}

fn import_changed(path: &Path, disposition: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {}", path.display(), disposition),
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn modified_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
