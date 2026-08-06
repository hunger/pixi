//! Finds the paths a configure run took from outside the conda environment.
//!
//! CMake searches the machine it runs on, and a native build is not confined
//! to the prefixes conda created for it. A library or tool picked up from
//! `/usr` builds fine here and then goes missing, or arrives in a different
//! version, wherever the package is installed. The cache records every path
//! the configure run settled on, so it can be checked after the fact.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::inputs::cmake_build_dir;

/// Cache entries CMake fills with an install destination rather than
/// something it found. `/usr/include` as `CMAKE_INSTALL_OLDINCLUDEDIR` says
/// nothing about what the build used.
const DESTINATION_PREFIX: &str = "CMAKE_INSTALL_";

/// The macOS SDK is supposed to come from the machine.
const SYSTEM_SDK: &str = "CMAKE_OSX_SYSROOT";

/// Where CMake records the source directory it configured.
const SOURCE_DIRECTORY_KEY: &str = "CMAKE_HOME_DIRECTORY";

/// A path the build took from outside the conda environment.
#[derive(Debug, PartialEq, Eq)]
pub struct SystemPath {
    /// The cache entry that holds it, such as `ZLIB_LIBRARY_RELEASE`.
    pub variable: String,
    pub path: String,
}

/// Returns the paths the configure run took from outside the environment
/// conda built for it, or an error when there is no cache to read.
///
/// Everything below the work directory belongs to conda: the host prefix, the
/// build prefix and the sysroot are all created there. The project's own
/// source tree is not a dependency either, wherever it sits.
pub fn system_dependencies(workdir: &Path) -> io::Result<Vec<SystemPath>> {
    let cache = fs_err::read_to_string(cmake_build_dir(workdir).join("CMakeCache.txt"))?;
    Ok(system_paths_in(&cache, workdir))
}

fn system_paths_in(cache: &str, workdir: &Path) -> Vec<SystemPath> {
    let source_directory = entry(cache, SOURCE_DIRECTORY_KEY).map(PathBuf::from);

    cache
        .lines()
        .filter_map(parse_entry)
        .filter(|(variable, kind, value)| {
            // Only entries naming a file or directory can name a dependency.
            matches!(*kind, "FILEPATH" | "PATH")
                && !variable.starts_with(DESTINATION_PREFIX)
                && *variable != SYSTEM_SDK
                && is_outside(value, workdir, source_directory.as_deref())
        })
        .map(|(variable, _, value)| SystemPath {
            variable: variable.to_string(),
            path: value.to_string(),
        })
        .collect()
}

/// Splits a `NAME:TYPE=value` cache line.
fn parse_entry(line: &str) -> Option<(&str, &str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
        return None;
    }

    let (name, rest) = line.split_once(':')?;
    let (kind, value) = rest.split_once('=')?;
    Some((name, kind, value))
}

/// The value of one cache entry, whatever its type.
fn entry<'a>(cache: &'a str, key: &str) -> Option<&'a str> {
    cache
        .lines()
        .filter_map(parse_entry)
        .find(|(name, _, _)| *name == key)
        .map(|(_, _, value)| value)
}

/// Whether an absolute path belongs to neither conda nor the project.
fn is_outside(value: &str, workdir: &Path, source_directory: Option<&Path>) -> bool {
    // A relative value, an empty one or a `NOTFOUND` marker names no path.
    if !Path::new(value).is_absolute() || value.ends_with("NOTFOUND") {
        return false;
    }

    let path = Path::new(value);
    !path.starts_with(workdir) && source_directory.is_none_or(|source| !path.starts_with(source))
}

/// Tells the user which dependencies came from the machine instead of the
/// environment, which is what makes a package work here and nowhere else.
pub fn report_system_dependencies(workdir: &Path) {
    let paths = match system_dependencies(workdir) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::debug!("no CMake cache to check for system dependencies: {err}");
            return;
        }
    };

    for found in &paths {
        tracing::warn!(
            "cmake took {} from outside the environment: {}",
            found.variable,
            found.path
        );
    }

    if !paths.is_empty() {
        tracing::warn!(
            "{} path(s) above come from the machine rather than the package's dependencies",
            paths.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cache in the shape rattler-build produces: the prefixes live under
    /// the work directory, the project sits elsewhere.
    fn cache() -> String {
        [
            "# This is the CMakeCache file.",
            "//A comment line",
            "CMAKE_HOME_DIRECTORY:INTERNAL=/src/demo",
            "CMAKE_INSTALL_PREFIX:PATH=/work/host_placehold",
            "CMAKE_INSTALL_OLDINCLUDEDIR:PATH=/usr/include",
            "CMAKE_AR:FILEPATH=/work/bld/bin/ar",
            "ZLIB_LIBRARY_RELEASE:FILEPATH=/work/host_placehold/lib/libz.so",
            "PKG_CONFIG_EXECUTABLE:FILEPATH=/usr/bin/pkg-config",
            "OpenSSL_DIR:PATH=/usr/lib/cmake/OpenSSL",
            "DEMO_DATA:PATH=/src/demo/data",
            "SOME_FLAG:BOOL=ON",
            "SOME_TEXT:STRING=/usr/looks/like/a/path",
            "MISSING_LIB:FILEPATH=MISSING_LIB-NOTFOUND",
            "RELATIVE_THING:PATH=lib",
        ]
        .join("\n")
    }

    #[test]
    fn test_finds_only_what_came_from_the_machine() {
        let found = system_paths_in(&cache(), Path::new("/work"));

        assert_eq!(
            found,
            vec![
                SystemPath {
                    variable: "PKG_CONFIG_EXECUTABLE".to_string(),
                    path: "/usr/bin/pkg-config".to_string(),
                },
                SystemPath {
                    variable: "OpenSSL_DIR".to_string(),
                    path: "/usr/lib/cmake/OpenSSL".to_string(),
                },
            ]
        );
    }

    /// Everything conda created lives under the work directory.
    #[test]
    fn test_the_prefixes_are_not_system_paths() {
        let found = system_paths_in(&cache(), Path::new("/work"));
        let variables: Vec<&str> = found.iter().map(|f| f.variable.as_str()).collect();

        assert!(!variables.contains(&"CMAKE_AR"));
        assert!(!variables.contains(&"ZLIB_LIBRARY_RELEASE"));
        assert!(!variables.contains(&"CMAKE_INSTALL_PREFIX"));
    }

    /// The project is not one of its own dependencies, wherever it sits.
    #[test]
    fn test_the_source_tree_is_not_a_system_path() {
        let found = system_paths_in(&cache(), Path::new("/work"));

        assert!(!found.iter().any(|f| f.variable == "DEMO_DATA"));
    }

    /// An install destination says nothing about what the build used.
    #[test]
    fn test_install_destinations_are_ignored() {
        let found = system_paths_in(&cache(), Path::new("/work"));

        assert!(
            !found
                .iter()
                .any(|f| f.variable.starts_with("CMAKE_INSTALL_"))
        );
    }

    /// Only entries that name a path count, and only when they found one.
    #[test]
    fn test_other_entry_kinds_are_ignored() {
        let found = system_paths_in(&cache(), Path::new("/work"));
        let variables: Vec<&str> = found.iter().map(|f| f.variable.as_str()).collect();

        assert!(!variables.contains(&"SOME_FLAG"));
        assert!(!variables.contains(&"SOME_TEXT"), "a STRING is not a path");
        assert!(!variables.contains(&"MISSING_LIB"), "nothing was found");
        assert!(!variables.contains(&"RELATIVE_THING"));
    }

    /// macOS builds take the SDK from the machine on purpose.
    #[test]
    fn test_the_macos_sdk_is_expected_to_be_a_system_path() {
        let cache = "CMAKE_OSX_SYSROOT:PATH=/Library/Developer/SDKs/MacOSX.sdk";

        assert!(system_paths_in(cache, Path::new("/work")).is_empty());
    }

    /// A cache without the source directory recorded must not lose findings.
    #[test]
    fn test_a_cache_without_a_source_directory() {
        let cache = "PKG_CONFIG_EXECUTABLE:FILEPATH=/usr/bin/pkg-config";

        assert_eq!(system_paths_in(cache, Path::new("/work")).len(), 1);
    }

    #[test]
    fn test_a_build_without_a_cache() {
        let workdir = tempfile::tempdir().unwrap();

        assert!(system_dependencies(workdir.path()).is_err());
        // Reporting has to stay quiet rather than fail the build.
        report_system_dependencies(workdir.path());
    }
}
