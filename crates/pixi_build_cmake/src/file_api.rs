//! Reads what a configured CMake project declares from its [file API].
//!
//! The build script leaves a query in the build directory, so every configure
//! run writes a reply describing the project. That reply complements what
//! [`crate::inputs`] learns from ninja, which knows the build graph but not
//! the project:
//!
//! * ninja only knows the headers of translation units it has actually
//!   compiled in this build directory. A target that never built, either
//!   because the build was partial or because it is `EXCLUDE_FROM_ALL`,
//!   contributes nothing.
//! * ninja never sees the files `install(FILES)` and friends copy out of the
//!   source tree, because installing runs a CMake script rather than a build
//!   edge.
//!
//! The file API covers both, and does so right after configuring, before
//! anything is built. What it cannot report is a header that no target lists
//! and that is only reached through an `#include`; that one only ninja knows.
//! The two are unioned for that reason.
//!
//! [file API]: https://cmake.org/cmake/help/latest/manual/cmake-file-api.7.html

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::inputs::cmake_build_dir;

/// Where CMake writes the replies to the queries it finds in the build tree.
const REPLY_DIR: &str = ".cmake/api/v1/reply";

/// Name of the query directory the build script writes and the backend reads
/// the reply of. The file API allows one directory per client.
pub const CLIENT: &str = "client-pixi-build-cmake";

/// The query that makes CMake describe the project on every configure run.
pub const QUERY: &str =
    r#"{"requests":[{"kind":"codemodel","version":2},{"kind":"cmakeFiles","version":1}]}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    objects: Vec<Object>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Object {
    kind: String,
    json_file: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CMakeFiles {
    paths: Paths,
    inputs: Vec<Input>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paths {
    source: PathBuf,
    build: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    path: String,
    /// The file ships with CMake itself.
    #[serde(default)]
    is_cmake: bool,
    /// The file lives outside of the source directory.
    #[serde(default)]
    is_external: bool,
    /// The file is written by the build itself.
    #[serde(default)]
    is_generated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Codemodel {
    configurations: Vec<Configuration>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Configuration {
    #[serde(default)]
    targets: Vec<Reference>,
    #[serde(default)]
    directories: Vec<Reference>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reference {
    json_file: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    #[serde(default)]
    sources: Vec<Source>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    path: String,
    #[serde(default)]
    is_generated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Directory {
    #[serde(default)]
    installers: Vec<Installer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installer {
    #[serde(rename = "type")]
    installer_type: String,
    #[serde(default)]
    paths: Vec<InstallPath>,
}

/// An installed path, either a plain path or a rename of a source path.
#[derive(Deserialize)]
#[serde(untagged)]
enum InstallPath {
    Plain(String),
    Renamed { from: String },
}

impl InstallPath {
    /// The path the installer reads from.
    fn source(&self) -> &str {
        match self {
            InstallPath::Plain(path) => path,
            InstallPath::Renamed { from } => from,
        }
    }
}

/// Returns the source-relative files that the configured project declares,
/// or an error if there is no readable reply. Callers should treat an error
/// as "the file API contributed nothing".
pub fn declared_inputs(workdir: &Path) -> io::Result<BTreeSet<String>> {
    let reply_directory = cmake_build_dir(workdir).join(REPLY_DIR);

    let index = read_index(&reply_directory)?;
    let mut paths = BTreeSet::new();
    let mut source_and_build = None;

    for object in &index.objects {
        match object.kind.as_str() {
            "cmakeFiles" => {
                let cmake_files: CMakeFiles = read_object(&reply_directory, &object.json_file)?;
                paths.extend(
                    cmake_files
                        .inputs
                        .iter()
                        .filter(|input| {
                            !input.is_cmake && !input.is_external && !input.is_generated
                        })
                        .map(|input| input.path.clone()),
                );
                source_and_build = Some(cmake_files.paths);
            }
            "codemodel" => {
                let codemodel: Codemodel = read_object(&reply_directory, &object.json_file)?;
                paths.extend(codemodel_paths(&reply_directory, &codemodel)?);
            }
            _ => {}
        }
    }

    // Without the `cmakeFiles` object the source directory is unknown, and
    // there is no way to tell which paths belong to the source tree.
    let Some(paths_object) = source_and_build else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the file API reply has no cmakeFiles object",
        ));
    };

    // The build directory is commonly a subdirectory of the source directory,
    // where everything it holds is a build artifact rather than an input.
    let build_prefix = paths_object
        .build
        .strip_prefix(&paths_object.source)
        .ok()
        .map(|prefix| normalize(&prefix.to_string_lossy()));

    Ok(paths
        .iter()
        .map(|path| normalize(path))
        .filter(|path| is_source_input(path, build_prefix.as_deref()))
        .collect())
}

/// Collects the source paths of every target and installer in the codemodel.
fn codemodel_paths(reply_directory: &Path, codemodel: &Codemodel) -> io::Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();

    for configuration in &codemodel.configurations {
        for reference in &configuration.targets {
            let target: Target = read_object(reply_directory, &reference.json_file)?;
            paths.extend(
                target
                    .sources
                    .iter()
                    .filter(|source| !source.is_generated)
                    .map(|source| source.path.clone()),
            );
        }

        for reference in &configuration.directories {
            let directory: Directory = read_object(reply_directory, &reference.json_file)?;
            paths.extend(directory.installers.iter().flat_map(installer_paths));
        }
    }

    Ok(paths)
}

/// Returns the source paths an installer reads from.
///
/// Only installers that copy files out of the source tree contribute inputs.
/// Installers of build artifacts, exports and scripts name paths in the build
/// directory or nothing at all.
fn installer_paths(installer: &Installer) -> Vec<String> {
    match installer.installer_type.as_str() {
        "file" | "fileSet" => installer
            .paths
            .iter()
            .map(|path| path.source().to_string())
            .collect(),
        // A directory is copied recursively, so everything below it is input.
        "directory" => installer
            .paths
            .iter()
            .map(|path| format!("{}/**", path.source().trim_end_matches('/')))
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a path points into the source tree rather than at an absolute
/// location or a build artifact.
fn is_source_input(path: &str, build_prefix: Option<&str>) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with("../") {
        return false;
    }

    // A Windows path such as `C:/src` is absolute despite the leading letter.
    if path.chars().nth(1) == Some(':') {
        return false;
    }

    match build_prefix {
        Some(prefix) if !prefix.is_empty() => {
            path != prefix && !path.starts_with(&format!("{prefix}/"))
        }
        _ => true,
    }
}

/// Rewrites a path to the forward slashes that globs are matched with.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// Reads the reply index, which CMake names after the time it was written.
///
/// A configure run that failed writes an `error-*.json` instead, which leaves
/// no index to find.
fn read_index(reply_directory: &Path) -> io::Result<Index> {
    let mut index_files: Vec<PathBuf> = fs_err::read_dir(reply_directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("index-") && name.ends_with(".json"))
        })
        .collect();

    // Several indices can pile up in a reply directory; the last one by name
    // is the most recent, as the name holds a timestamp.
    index_files.sort();
    let index_file = index_files.last().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no file API index in {}", reply_directory.display()),
        )
    })?;

    let contents = fs_err::read_to_string(index_file)?;
    serde_json::from_str(&contents).map_err(io::Error::other)
}

/// Reads one of the objects the index refers to.
fn read_object<T: serde::de::DeserializeOwned>(
    reply_directory: &Path,
    json_file: &str,
) -> io::Result<T> {
    let contents = fs_err::read_to_string(reply_directory.join(json_file))?;
    serde_json::from_str(&contents).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a reply describing a project with a library, an installer of
    /// each kind, and a target whose source is never compiled.
    fn write_reply(workdir: &Path, source: &str, build: &str) {
        let reply = cmake_build_dir(workdir).join(REPLY_DIR);
        fs_err::create_dir_all(&reply).unwrap();

        let write = |name: &str, contents: String| {
            fs_err::write(reply.join(name), contents).unwrap();
        };

        write(
            "index-2026-01-01T00-00-00-0000.json",
            r#"{"objects":[
                {"kind":"codemodel","version":{"major":2},"jsonFile":"codemodel.json"},
                {"kind":"cmakeFiles","version":{"major":1},"jsonFile":"cmakeFiles.json"}
            ]}"#
            .to_string(),
        );
        write(
            "cmakeFiles.json",
            format!(
                r#"{{"paths":{{"source":"{source}","build":"{build}"}},"inputs":[
                    {{"path":"CMakeLists.txt"}},
                    {{"path":"cmake/helpers.cmake"}},
                    {{"path":"gen.hpp.in"}},
                    {{"path":"build/CMakeFiles/CMakeSystem.cmake","isGenerated":true}},
                    {{"path":"/usr/share/cmake/Modules/CMakeSystem.cmake","isCMake":true,"isExternal":true}}
                ]}}"#
            ),
        );
        write(
            "codemodel.json",
            r#"{"configurations":[{
                "targets":[{"jsonFile":"target-a.json"},{"jsonFile":"target-tool.json"}],
                "directories":[{"jsonFile":"directory.json"}]
            }]}"#
                .to_string(),
        );
        write(
            "target-a.json",
            r#"{"sources":[
                {"path":"src/a.cpp"},
                {"path":"include/a.hpp"},
                {"path":"build/gen.hpp","isGenerated":true}
            ]}"#
            .to_string(),
        );
        // EXCLUDE_FROM_ALL: never compiled, so ninja never reports it.
        write(
            "target-tool.json",
            r#"{"sources":[{"path":"src/tool.cpp"}]}"#.to_string(),
        );
        write(
            "directory.json",
            r#"{"installers":[
                {"type":"target","destination":"lib","paths":["liba.a"]},
                {"type":"file","destination":"share","paths":["data/app.conf"]},
                {"type":"directory","destination":"include","paths":[{"from":"include","to":"."}]},
                {"type":"export","destination":"lib/cmake","paths":["CMakeFiles/Export/x.cmake"]},
                {"type":"code"}
            ]}"#
            .to_string(),
        );
    }

    #[test]
    fn test_reports_what_ninja_cannot_know() {
        let workdir = tempfile::tempdir().unwrap();
        write_reply(workdir.path(), "/src/demo", "/src/demo/build");

        let paths = declared_inputs(workdir.path()).unwrap();

        insta::assert_debug_snapshot!(paths);
    }

    /// Artifacts, generated files and anything outside the source tree must
    /// not end up in the input set.
    #[test]
    fn test_skips_paths_that_are_not_source_inputs() {
        let workdir = tempfile::tempdir().unwrap();
        write_reply(workdir.path(), "/src/demo", "/src/demo/build");

        let paths = declared_inputs(workdir.path()).unwrap();

        assert!(!paths.iter().any(|path| path.starts_with("build/")));
        assert!(!paths.iter().any(|path| path.starts_with('/')));
        assert!(!paths.contains("liba.a"));
        assert!(!paths.contains("CMakeFiles/Export/x.cmake"));
    }

    /// A build directory outside the source tree has no prefix to filter on.
    #[test]
    fn test_build_directory_outside_the_source_tree() {
        let workdir = tempfile::tempdir().unwrap();
        write_reply(workdir.path(), "/src/demo", "/tmp/build");

        let paths = declared_inputs(workdir.path()).unwrap();

        assert!(paths.contains("CMakeLists.txt"));
        assert!(paths.contains("src/a.cpp"));
    }

    #[test]
    fn test_without_a_reply() {
        let workdir = tempfile::tempdir().unwrap();

        assert!(declared_inputs(workdir.path()).is_err());
    }

    #[test]
    fn test_with_a_malformed_reply() {
        let workdir = tempfile::tempdir().unwrap();
        let reply = cmake_build_dir(workdir.path()).join(REPLY_DIR);
        fs_err::create_dir_all(&reply).unwrap();
        fs_err::write(reply.join("index-2026-01-01T00-00-00-0000.json"), "{").unwrap();

        assert!(declared_inputs(workdir.path()).is_err());
    }

    #[test]
    fn test_source_input_filter() {
        assert!(is_source_input("src/a.cpp", Some("build")));
        assert!(!is_source_input("build/gen.hpp", Some("build")));
        assert!(!is_source_input("C:/src/demo/a.cpp", None));
        assert!(!is_source_input("../outside/a.cpp", None));
        assert!(!is_source_input("", None));
        // A directory that merely starts with the build prefix is an input.
        assert!(is_source_input("buildsystem/a.cpp", Some("build")));
    }
}
