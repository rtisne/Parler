use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum StageError {
    MissingOpenBlasPath,
    MissingImportLibrary(PathBuf),
    MissingRuntimeDll(PathBuf),
    MissingLoaderDll {
        directory: PathBuf,
        expected: &'static str,
    },
    Io(String),
}

impl fmt::Display for StageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOpenBlasPath => write!(
                formatter,
                "OPENBLAS_PATH is required for Windows builds with Whisper OpenBLAS support"
            ),
            Self::MissingImportLibrary(path) => write!(
                formatter,
                "OpenBLAS import library is missing: {}",
                path.display()
            ),
            Self::MissingRuntimeDll(path) => write!(
                formatter,
                "OpenBLAS runtime directory contains no DLLs: {}",
                path.display()
            ),
            Self::MissingLoaderDll {
                directory,
                expected,
            } => write!(
                formatter,
                "OpenBLAS runtime directory {} does not contain {expected}",
                directory.display()
            ),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StageError {}

pub fn stage_openblas_runtime(
    target_os: &str,
    target_arch: &str,
    manifest_dir: &Path,
    openblas_path: Option<&Path>,
) -> Result<Vec<String>, StageError> {
    if target_os != "windows" {
        return Ok(Vec::new());
    }

    let openblas_path = openblas_path.ok_or(StageError::MissingOpenBlasPath)?;
    for import_library_name in ["libopenblas.lib", "blas.lib"] {
        let import_library = openblas_path.join("lib").join(import_library_name);
        if !import_library.is_file() {
            return Err(StageError::MissingImportLibrary(import_library));
        }
    }

    let runtime_dir = openblas_path.join("bin");
    let mut runtime_dlls = Vec::new();
    let entries = fs::read_dir(&runtime_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            StageError::MissingRuntimeDll(runtime_dir.clone())
        } else {
            io_error("read OpenBLAS runtime directory", &runtime_dir, error)
        }
    })?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            io_error("read OpenBLAS runtime directory entry", &runtime_dir, error)
        })?;
        let source = entry.path();
        let is_dll = source
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"));
        if source.is_file() && is_dll {
            runtime_dlls.push(source);
        }
    }
    runtime_dlls.sort();
    if runtime_dlls.is_empty() {
        return Err(StageError::MissingRuntimeDll(runtime_dir.clone()));
    }

    let (expected_loader, staged_names): (&str, &[&str]) = if target_arch == "aarch64" {
        (
            "openblas.dll or libopenblas.dll",
            &["libopenblas.dll", "openblas.dll"],
        )
    } else {
        ("libopenblas.dll", &["libopenblas.dll"])
    };
    let loader_source = runtime_dlls.iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                if target_arch == "aarch64" {
                    name.eq_ignore_ascii_case("openblas.dll")
                        || name.eq_ignore_ascii_case("libopenblas.dll")
                } else {
                    name.eq_ignore_ascii_case("libopenblas.dll")
                }
            })
    });
    let loader_source = loader_source.ok_or_else(|| StageError::MissingLoaderDll {
        directory: runtime_dir,
        expected: expected_loader,
    })?;

    let destination = manifest_dir.join("openblas-libs");
    if destination.exists() {
        fs::remove_dir_all(&destination)
            .map_err(|error| io_error("clean OpenBLAS staging directory", &destination, error))?;
    }
    fs::create_dir_all(&destination)
        .map_err(|error| io_error("create OpenBLAS staging directory", &destination, error))?;

    let mut staged = Vec::with_capacity(staged_names.len());
    for staged_name in staged_names {
        fs::copy(loader_source, destination.join(staged_name))
            .map_err(|error| io_error("stage OpenBLAS runtime DLL", loader_source, error))?;
        staged.push((*staged_name).to_string());
    }

    Ok(staged)
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> StageError {
    StageError::Io(format!("{action} at {}: {error}", path.display()))
}
