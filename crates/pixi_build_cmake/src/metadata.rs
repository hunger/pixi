//! Provides package metadata that the `project()` call of a CMake project
//! declares, for the fields the pixi manifest leaves out.

use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    str::FromStr,
};

use pixi_build_backend::generated_recipe::MetadataProvider;
use rattler_conda_types::Version;

use crate::cmake_lists::{ProjectDeclaration, compilers_for_languages, parse_project};

/// Reads package metadata from the `project()` call in the top level
/// `CMakeLists.txt`.
///
/// Every value the pixi manifest declares takes precedence, so this only fills
/// in what the manifest leaves out.
pub struct CMakeMetadataProvider {
    cmake_lists: PathBuf,
    /// The parsed call, read on first use. The outer `Option` tracks whether
    /// the file was looked at, the inner one whether it declares a project.
    declaration: Option<Option<ProjectDeclaration>>,
    /// The `VERSION` the `project()` call declares, when it could not be used.
    /// Kept so a build left without any version can say why.
    unusable_version: Option<String>,
}

impl CMakeMetadataProvider {
    pub fn new(manifest_root: &Path) -> Self {
        Self {
            cmake_lists: manifest_root.join("CMakeLists.txt"),
            declaration: None,
            unusable_version: None,
        }
    }

    /// Returns the compilers for the languages the project enables.
    ///
    /// Falls back to the languages CMake enables by default when the file is
    /// missing, unreadable, or has no `project()` call to read them from.
    pub fn compilers(&mut self) -> Vec<String> {
        // Bind the languages before logging, so the borrow of `self` the
        // lookup takes ends before `self.cmake_lists` is read.
        let languages = self
            .declaration()
            .map(|declaration| declaration.languages.clone());

        let Some(languages) = languages else {
            tracing::debug!(
                "no project() languages found in {}, assuming the CMake default",
                self.cmake_lists.display()
            );
            // The languages CMake enables for a project() call that names none.
            return vec!["c".to_string(), "cxx".to_string()];
        };

        tracing::debug!(
            "{} enables the languages: {}",
            self.cmake_lists.display(),
            languages.join(", ")
        );

        compilers_for_languages(&languages)
    }

    /// Returns the parsed `project()` call, reading the file on first use.
    fn declaration(&mut self) -> Option<&ProjectDeclaration> {
        if self.declaration.is_none() {
            let contents = fs_err::read_to_string(&self.cmake_lists)
                .inspect_err(|err| {
                    tracing::debug!("could not read {}: {err}", self.cmake_lists.display());
                })
                .ok();

            self.declaration = Some(contents.as_deref().and_then(parse_project));
        }

        self.declaration.as_ref().and_then(Option::as_ref)
    }

    /// The `VERSION` of the `project()` call that could not become a package
    /// version, if there was one.
    pub fn unusable_version(&self) -> Option<&str> {
        self.unusable_version.as_deref()
    }

    /// The `CMakeLists.txt` the metadata was read from.
    pub fn cmake_lists(&self) -> &Path {
        &self.cmake_lists
    }

    /// Returns a declared value, unless it holds a reference this backend
    /// cannot resolve.
    ///
    /// CMake expands `${...}` and `@...@` while it configures the project, and
    /// reporting the reference itself would be worse than reporting nothing.
    fn resolved(&mut self, field: fn(&ProjectDeclaration) -> Option<&String>) -> Option<String> {
        let value = self.declaration().and_then(field)?;

        if value.contains("${") || contains_at_reference(value) {
            tracing::debug!("ignoring the unresolved project() value `{value}`");
            return None;
        }

        Some(value.clone())
    }
}

/// Says whether `value` holds an unexpanded `@VAR@` reference, which is a
/// variable name between two `@` signs.
fn contains_at_reference(value: &str) -> bool {
    let mut rest = value;

    while let Some(start) = rest.find('@') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('@') else {
            return false;
        };

        let name = &rest[..end];
        let is_variable_name = !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character));
        if is_variable_name {
            return true;
        }

        // The second `@` may still open a reference of its own.
        rest = &rest[end..];
    }

    false
}

impl MetadataProvider for CMakeMetadataProvider {
    type Error = Infallible;

    /// A version CMake accepts always parses here, so a version that does not
    /// is left to CMake to reject when it configures the project.
    fn version(&mut self) -> Result<Option<Version>, Self::Error> {
        let declared = self
            .declaration()
            .and_then(|declaration| declaration.version.clone());

        let Some(version) = self.resolved(|declaration| declaration.version.as_ref()) else {
            self.unusable_version = declared;
            return Ok(None);
        };

        let parsed = Version::from_str(&version)
            .inspect_err(|err| {
                tracing::debug!("ignoring the project() version `{version}`: {err}");
            })
            .ok();
        if parsed.is_none() {
            self.unusable_version = Some(version);
        }

        Ok(parsed)
    }

    fn description(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(self.resolved(|declaration| declaration.description.as_ref()))
    }

    fn homepage(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(self.resolved(|declaration| declaration.homepage_url.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a provider for a manifest root that holds `cmake_lists`.
    fn provider(cmake_lists: &str) -> (tempfile::TempDir, CMakeMetadataProvider) {
        let manifest_root = tempfile::tempdir().expect("Failed to create temp dir");
        fs_err::write(manifest_root.path().join("CMakeLists.txt"), cmake_lists)
            .expect("Failed to write CMakeLists.txt");

        let provider = CMakeMetadataProvider::new(manifest_root.path());
        (manifest_root, provider)
    }

    #[test]
    fn test_reports_the_declared_metadata() {
        let (_root, mut provider) = provider(
            r#"project(demo VERSION 1.2.3 DESCRIPTION "a demo" HOMEPAGE_URL "https://example.com")"#,
        );

        assert_eq!(
            provider.version().unwrap(),
            Some(Version::from_str("1.2.3").unwrap())
        );
        assert_eq!(provider.description().unwrap().as_deref(), Some("a demo"));
        assert_eq!(
            provider.homepage().unwrap().as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn test_reports_nothing_without_the_keywords() {
        let (_root, mut provider) = provider("project(demo LANGUAGES CXX)");

        assert_eq!(provider.version().unwrap(), None);
        assert_eq!(provider.description().unwrap(), None);
        assert_eq!(provider.homepage().unwrap(), None);
    }

    #[test]
    fn test_reports_nothing_without_a_cmake_lists() {
        let manifest_root = tempfile::tempdir().expect("Failed to create temp dir");
        let mut provider = CMakeMetadataProvider::new(manifest_root.path());

        assert_eq!(provider.version().unwrap(), None);
        assert_eq!(
            provider.compilers(),
            vec!["c".to_string(), "cxx".to_string()]
        );
    }

    /// CMake expands these while configuring, so the backend cannot report
    /// anything useful for them.
    #[test]
    fn test_skips_unresolved_values() {
        let (_root, mut provider) = provider(
            r#"project(demo VERSION ${DEMO_VERSION} DESCRIPTION "@DEMO_DESCRIPTION@" HOMEPAGE_URL "https://example.com")"#,
        );

        assert_eq!(provider.version().unwrap(), None);
        assert_eq!(provider.description().unwrap(), None);
        assert_eq!(
            provider.homepage().unwrap().as_deref(),
            Some("https://example.com"),
            "a resolved value next to an unresolved one is still reported"
        );
    }

    /// An `@` on its own does not make a value an unexpanded `@VAR@`
    /// reference.
    #[test]
    fn test_a_lone_at_sign_is_not_a_reference() {
        let (_root, mut provider) = provider(
            r#"project(demo DESCRIPTION "maintained by @octocat" HOMEPAGE_URL "https://example.com/~user@2")"#,
        );

        assert_eq!(
            provider.description().unwrap().as_deref(),
            Some("maintained by @octocat")
        );
        assert_eq!(
            provider.homepage().unwrap().as_deref(),
            Some("https://example.com/~user@2")
        );
    }

    /// CMake rejects such a version itself, and reporting nothing leaves that
    /// error to it rather than failing earlier with less context.
    #[test]
    fn test_ignores_a_version_that_cannot_be_parsed() {
        let (_root, mut provider) = provider("project(demo VERSION 1..2)");

        assert_eq!(provider.version().unwrap(), None);
    }

    #[test]
    fn test_compilers_come_from_the_languages() {
        let (_root, mut provider) = provider("project(demo LANGUAGES C CXX Fortran)");

        assert_eq!(
            provider.compilers(),
            vec!["c".to_string(), "cxx".to_string(), "fortran".to_string()]
        );
    }

    #[test]
    fn test_the_file_is_read_once() {
        let (root, mut provider) = provider("project(demo VERSION 1.2.3 LANGUAGES Fortran)");

        assert_eq!(
            provider.version().unwrap(),
            Some(Version::from_str("1.2.3").unwrap())
        );

        // Once parsed, later calls must not go back to the file.
        fs_err::remove_file(root.path().join("CMakeLists.txt")).expect("Failed to remove the file");

        assert_eq!(provider.description().unwrap(), None);
        assert_eq!(provider.compilers(), vec!["fortran".to_string()]);
    }
}
