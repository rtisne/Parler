#[path = "../build_support/openblas.rs"]
mod openblas;

use openblas::{stage_openblas_runtime, StageError};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("parler-openblas-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn create_openblas_layout(root: &Path, dll_name: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::write(root.join("bin").join(dll_name), b"runtime").unwrap();
    fs::write(root.join("lib/libopenblas.lib"), b"import-library").unwrap();
    fs::write(root.join("lib/blas.lib"), b"import-library").unwrap();
}

#[test]
fn non_windows_targets_do_not_require_or_stage_openblas() {
    let manifest = TempDir::new();

    let staged = stage_openblas_runtime("linux", "x86_64", manifest.path(), None).unwrap();

    assert!(staged.is_empty());
    assert!(!manifest.path().join("openblas-libs").exists());
}

#[test]
fn windows_requires_openblas_path() {
    let manifest = TempDir::new();

    let error = stage_openblas_runtime("windows", "x86_64", manifest.path(), None).unwrap_err();

    assert_eq!(error, StageError::MissingOpenBlasPath);
}

#[test]
fn windows_rejects_an_incomplete_linker_layout() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    fs::create_dir_all(openblas.path().join("bin")).unwrap();
    fs::write(openblas.path().join("bin/libopenblas.dll"), b"runtime").unwrap();

    let error = stage_openblas_runtime("windows", "x86_64", manifest.path(), Some(openblas.path()))
        .unwrap_err();

    assert_eq!(
        error,
        StageError::MissingImportLibrary(openblas.path().join("lib/libopenblas.lib"))
    );
}

#[test]
fn windows_requires_the_generic_blas_alias_used_by_cmake() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    fs::create_dir_all(openblas.path().join("bin")).unwrap();
    fs::create_dir_all(openblas.path().join("lib")).unwrap();
    fs::write(openblas.path().join("bin/libopenblas.dll"), b"runtime").unwrap();
    fs::write(
        openblas.path().join("lib/libopenblas.lib"),
        b"import-library",
    )
    .unwrap();

    let error = stage_openblas_runtime("windows", "x86_64", manifest.path(), Some(openblas.path()))
        .unwrap_err();

    assert_eq!(
        error,
        StageError::MissingImportLibrary(openblas.path().join("lib/blas.lib"))
    );
}

#[test]
fn x64_rejects_an_unrelated_dll_without_destroying_existing_staging() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    fs::create_dir_all(openblas.path().join("bin")).unwrap();
    fs::create_dir_all(openblas.path().join("lib")).unwrap();
    fs::write(openblas.path().join("bin/unrelated.dll"), b"unrelated").unwrap();
    fs::write(openblas.path().join("lib/libopenblas.lib"), b"import").unwrap();
    fs::write(openblas.path().join("lib/blas.lib"), b"import").unwrap();
    fs::create_dir_all(manifest.path().join("openblas-libs")).unwrap();
    fs::write(
        manifest.path().join("openblas-libs/libopenblas.dll"),
        b"known-good",
    )
    .unwrap();

    let error = stage_openblas_runtime("windows", "x86_64", manifest.path(), Some(openblas.path()))
        .unwrap_err();

    assert_eq!(
        error,
        StageError::MissingLoaderDll {
            directory: openblas.path().join("bin"),
            expected: "libopenblas.dll",
        }
    );
    assert_eq!(
        fs::read(manifest.path().join("openblas-libs/libopenblas.dll")).unwrap(),
        b"known-good"
    );
}

#[test]
fn arm64_rejects_a_layout_without_an_openblas_loader() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    fs::create_dir_all(openblas.path().join("bin")).unwrap();
    fs::create_dir_all(openblas.path().join("lib")).unwrap();
    fs::write(openblas.path().join("bin/unrelated.dll"), b"unrelated").unwrap();
    fs::write(openblas.path().join("lib/libopenblas.lib"), b"import").unwrap();
    fs::write(openblas.path().join("lib/blas.lib"), b"import").unwrap();

    let error =
        stage_openblas_runtime("windows", "aarch64", manifest.path(), Some(openblas.path()))
            .unwrap_err();

    assert_eq!(
        error,
        StageError::MissingLoaderDll {
            directory: openblas.path().join("bin"),
            expected: "openblas.dll or libopenblas.dll",
        }
    );
    assert!(!manifest.path().join("openblas-libs").exists());
}

#[test]
fn x64_stages_runtime_and_removes_stale_dlls() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    create_openblas_layout(openblas.path(), "libopenblas.dll");
    fs::create_dir_all(manifest.path().join("openblas-libs")).unwrap();
    fs::write(manifest.path().join("openblas-libs/stale.dll"), b"stale").unwrap();

    let staged =
        stage_openblas_runtime("windows", "x86_64", manifest.path(), Some(openblas.path()))
            .unwrap();

    assert_eq!(staged, vec!["libopenblas.dll"]);
    assert_eq!(
        fs::read(manifest.path().join("openblas-libs/libopenblas.dll")).unwrap(),
        b"runtime"
    );
    assert!(!manifest.path().join("openblas-libs/stale.dll").exists());
}

#[test]
fn arm64_stages_both_names_required_by_the_loader_and_linker() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    create_openblas_layout(openblas.path(), "libopenblas.dll");

    let staged =
        stage_openblas_runtime("windows", "aarch64", manifest.path(), Some(openblas.path()))
            .unwrap();

    assert_eq!(staged, vec!["libopenblas.dll", "openblas.dll"]);
    assert_eq!(
        fs::read(manifest.path().join("openblas-libs/openblas.dll")).unwrap(),
        b"runtime"
    );
}

#[test]
fn windows_rejects_a_layout_without_runtime_dlls() {
    let manifest = TempDir::new();
    let openblas = TempDir::new();
    fs::create_dir_all(openblas.path().join("lib")).unwrap();
    fs::write(
        openblas.path().join("lib/libopenblas.lib"),
        b"import-library",
    )
    .unwrap();
    fs::write(openblas.path().join("lib/blas.lib"), b"import-library").unwrap();

    let error = stage_openblas_runtime("windows", "x86_64", manifest.path(), Some(openblas.path()))
        .unwrap_err();

    assert_eq!(
        error,
        StageError::MissingRuntimeDll(openblas.path().join("bin"))
    );
}
