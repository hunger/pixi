use crate::workspace::errors::conda_override_hint;
use fancy_display::FancyDisplay;
use itertools::Itertools;
use miette::Diagnostic;
use pixi_manifest::platform::archspec_requirement_satisfied;
use pixi_manifest::{EnvironmentName, PixiPlatform, PixiPlatformName};
use pypi_modifiers::pypi_tags::{PyPITagError, get_tags_from_machine, is_python_record};
use rattler_conda_types::ParseMatchSpecError;
use rattler_conda_types::ParseStrictness::Lenient;
use rattler_conda_types::{
    GenericVirtualPackage, MatchSpec, Matches, Platform, StringMatcher, Version, VersionSpec,
};
use rattler_lock::{CondaPackageData, ConversionError, LockFile, PypiPackageData};
use rattler_virtual_packages::{
    DetectVirtualPackageError, VirtualPackage, VirtualPackageOverrides,
};
use std::collections::HashMap;
use std::str::FromStr;
use thiserror::Error;
use uv_distribution_filename::WheelFilename;

/// Define accepted virtual packages as a constant set
/// These packages will be checked against the system virtual packages.
const ACCEPTED_VIRTUAL_PACKAGES: &[&str] = &[
    "__glibc",
    "__musl",
    "__eglibc",
    "__cuda",
    "__osx",
    "__win",
    "__linux",
    "__archspec",
];

#[derive(Debug, Error, Diagnostic)]
#[error("{msg}")]
pub struct VirtualPackageNotFoundError {
    msg: String,
    #[help]
    help: Option<String>,
}

impl VirtualPackageNotFoundError {
    pub fn new(
        required_package: &MatchSpec,
        system_virtual_packages: &Vec<&GenericVirtualPackage>,
    ) -> Self {
        let required_version = required_package.version.as_ref().and_then(spec_version);
        // `__archspec` is named by microarchitecture; a pattern build matcher
        // names no single value to suggest, so the hint falls back to an example.
        let required_build = match required_package.build.as_ref() {
            Some(StringMatcher::Exact(build)) => Some(build.as_str()),
            Some(_) | None => None,
        };
        let help = required_package
            .name
            .as_exact()
            .and_then(|name| {
                conda_override_hint(name.as_normalized(), required_version, required_build)
            })
            .map(|hint| {
                format!(
                    " You can mock the virtual package by overriding the environment variable, e.g.: '`{hint}`'"
                )
            });

        let msg = format!(
            "Virtual package '{}' does not match any of the available virtual packages on your machine: [{}]",
            required_package,
            system_virtual_packages
                .iter()
                .map(|vpkg| vpkg.to_string())
                .join(", "),
        );
        VirtualPackageNotFoundError { msg, help }
    }
}

#[derive(Debug, Error, Diagnostic)]
#[error("Failed to validate that machine meets the requirements of the environment")]
pub enum MachineValidationError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    VirtualPackageNotFound(#[from] VirtualPackageNotFoundError),

    #[error("Couldn't get the virtual packages from the system")]
    VirtualPackageDetectionError(#[from] DetectVirtualPackageError),

    #[error(transparent)]
    RepodataConversionError(#[from] ConversionError),

    #[error("Couldn't parse dependencies")]
    DependencyParsingError(#[from] ParseMatchSpecError),

    #[error("Can't find environment: {0}")]
    EnvironmentNotFound(String),

    #[error(transparent)]
    #[diagnostic(transparent)]
    PyPITagError(#[from] PyPITagError),

    #[error("Wheel: {0} doesn't match this systems virtual capabilities for tags: {1}")]
    WheelTagsMismatch(String, String),

    #[error("No Python record found in the lock file for platform: {0}.")]
    #[diagnostic(
        help = "Please make sure that 'python' is added in conda dependencies. Otherwise , please report this issue to the developers."
    )]
    NoPythonRecordFound(PixiPlatformName),
}

/// Get the required virtual packages from dependency strings.
pub(crate) fn get_required_virtual_packages_from_depends(
    depends: &[&str],
) -> Result<Vec<MatchSpec>, MachineValidationError> {
    depends
        .iter()
        .filter(|dep| dep.starts_with("__"))
        .map(|dep| MatchSpec::from_str(dep, Lenient))
        .dedup()
        .collect::<Result<Vec<MatchSpec>, _>>()
        .map_err(MachineValidationError::DependencyParsingError)
}

/// The single version a virtual-package match spec carries. Virtual-package
/// dependencies are always pinned to one exact version (`__cuda 12` /
/// `__cuda >=12` both mean version `12`), so the operator is irrelevant -- we
/// just read the version. Specs with no version (a bare `__cuda`) yield `None`.
pub(crate) fn spec_version(spec: &VersionSpec) -> Option<&Version> {
    match spec {
        VersionSpec::Range(_, version) | VersionSpec::Exact(_, version) => Some(version),
        _ => None,
    }
}

/// Compute the minimal requirements for each subdir the environment was
/// resolved for: exactly the virtual-package specs that some resolved dependency
/// requires.
///
/// The result is keyed by subdir; `declared_platforms` that share a subdir are
/// unioned. Only `depends` is considered, mirroring
/// [`validate_system_meets_environment_requirements`]. A subdir whose lock-file
/// entry has no conda packages is omitted (the caller falls back to the declared
/// platform); a subdir with packages but no virtual-package requirements yields
/// an empty requirement list.
pub(crate) fn compute_minimal_required_platforms(
    lock_file: &LockFile,
    environment_name: &EnvironmentName,
    declared_platforms: &[&PixiPlatform],
) -> HashMap<Platform, Vec<MatchSpec>> {
    let Some(environment) = lock_file.environment(environment_name.as_str()) else {
        return HashMap::new();
    };

    // subdir -> all `depends` strings of its resolved conda packages, unioned
    // across the declared platforms that share a subdir.
    let mut depends_by_subdir: HashMap<Platform, Vec<String>> = HashMap::new();

    for platform in declared_platforms {
        let lock_platform = super::resolve_lock_platform_for(environment.lock_file(), platform);
        let Some(conda_packages) = lock_platform.and_then(|p| environment.conda_packages(p)) else {
            continue;
        };
        let entry = depends_by_subdir.entry(platform.subdir()).or_default();
        entry.extend(
            conda_packages
                .flat_map(|data| data.depends())
                .map(ToString::to_string),
        );
    }

    depends_by_subdir
        .into_iter()
        .map(|(subdir, depends)| {
            let depends: Vec<&str> = depends.iter().map(String::as_str).collect_vec();
            (subdir, required_virtual_package_specs(&depends))
        })
        .collect()
}

/// The virtual-package requirements some dependency in `depends` places on the
/// machine: the specs themselves, deduplicated and sorted for a stable marker
/// file.
///
/// The specs are kept verbatim rather than folded into one entry per name. A
/// `depends` entry can carry a version range and a build-string pattern --
/// conda-forge's microarch metapackages emit
/// `__archspec[version='1.*', build='^(x86_64_v3|skylake|...)$']` -- so
/// collapsing them into a concrete `GenericVirtualPackage`, or into the highest
/// version seen, throws the constraint away. Requiring all of them is both
/// lossless and exactly what the resolver asked for.
///
/// This is the per-subdir core of `compute_minimal_required_platforms`, shared
/// with `pixi global`, which derives the same minimum from an installed
/// environment's records rather than a lock file.
pub fn required_virtual_package_specs(depends: &[&str]) -> Vec<MatchSpec> {
    let Ok(specs) = get_required_virtual_packages_from_depends(depends) else {
        return Vec::new();
    };

    let mut requirements: Vec<MatchSpec> = Vec::new();
    for spec in specs {
        // A spec with a non-exact name matches nothing we can check.
        if spec.name.as_exact().is_none() {
            continue;
        }
        let rendered = spec.to_string();
        if !requirements.iter().any(|seen| seen.to_string() == rendered) {
            requirements.push(spec);
        }
    }
    requirements.sort_by_key(ToString::to_string);
    requirements
}

/// Get the wheel filenames from the lock file pypi package data
fn get_wheels_from_pypi_package_data(pypi_packages: Vec<PypiPackageData>) -> Vec<WheelFilename> {
    pypi_packages
        .into_iter()
        .map(|package| package.location().clone())
        .flat_map(|location| {
            if let Some(file_name) = location.file_name() {
                WheelFilename::from_str(file_name).ok()
            } else {
                tracing::debug!("No file name found for location: {:?}", location);
                None
            }
        })
        .collect_vec()
}

/// Validate that current machine has all the required virtual packages for the given environment
pub(crate) fn validate_system_meets_environment_requirements(
    lock_file: &LockFile,
    platform: &PixiPlatform,
    environment_name: &EnvironmentName,
    virtual_package_overrides: Option<VirtualPackageOverrides>,
) -> Result<bool, MachineValidationError> {
    // Early out if there are no packages in the lock file
    if lock_file.is_empty() {
        tracing::debug!("No packages in the lock file, skipping virtual package validation");
        return Ok(true);
    }

    // Get the environment from the lock file
    let environment = lock_file.environment(environment_name.as_str()).ok_or(
        MachineValidationError::EnvironmentNotFound(environment_name.as_str().to_string()),
    )?;

    // Retrieve all conda packages for the specified platform (both binary and source).
    let lock_platform = super::resolve_lock_platform_for(environment.lock_file(), platform);
    let Some(conda_packages) = lock_platform.and_then(|p| environment.conda_packages(p)) else {
        // Early out if there are no packages, as we don't need to check for virtual packages
        return Ok(true);
    };

    // Collect conda packages (both binary and source) into a vector of CondaPackageData
    let conda_packages: Vec<&CondaPackageData> = conda_packages.collect_vec();

    if conda_packages.is_empty() {
        // Early out if there are no conda records, as we don't need to check for virtual packages
        return Ok(true);
    }

    // Get depends from all packages (binary and source, including partial)
    let all_depends: Vec<&str> = conda_packages
        .iter()
        .flat_map(|data| data.depends())
        .map(|s| s.as_str())
        .collect_vec();

    // Get the virtual packages required by the conda records
    let required_virtual_packages = get_required_virtual_packages_from_depends(&all_depends)?;

    // Find the python package record (needed for wheel tag validation below).
    // This works for binary and full source packages; partial source records
    // don't have a PackageRecord and are skipped.
    let python_record = conda_packages
        .iter()
        .filter_map(|data| data.record())
        .find(|record| is_python_record(record));

    tracing::debug!(
        "Required virtual packages of environment '{}': {}",
        environment_name.fancy_display(),
        required_virtual_packages
            .iter()
            .map(|spec| spec.to_string())
            .join(", "),
    );

    // Default to the environment variable overrides, but allow for an override for testing
    let virtual_package_overrides =
        virtual_package_overrides.unwrap_or(VirtualPackageOverrides::from_env());

    // Get the virtual packages available on the system
    let system_virtual_packages = VirtualPackage::detect(&virtual_package_overrides)?;
    let generic_system_virtual_packages = system_virtual_packages
        .iter()
        .cloned()
        .map(GenericVirtualPackage::from)
        .map(|vpkg| (vpkg.name.clone(), vpkg))
        .collect::<HashMap<_, _>>();

    tracing::debug!(
        "Generic system virtual packages for env: '{}' : [{}]",
        environment_name.fancy_display(),
        generic_system_virtual_packages
            .iter()
            .map(|(name, vpkg)| format!("{}: {}", name.as_normalized(), vpkg))
            .join(", ")
    );

    // Check if all the required virtual conda packages match the system virtual packages
    for required in required_virtual_packages {
        // Check if the package name is in our accepted list
        let is_accepted = required
            .name
            .as_exact()
            .map(|n| ACCEPTED_VIRTUAL_PACKAGES.contains(&n.as_normalized()))
            .unwrap_or(false);

        // Skip if not in accepted packages
        if !is_accepted {
            tracing::debug!(
                "Skipping virtual package: {} as it's not in the accepted packages",
                required
            );
            continue;
        }

        let name = if let Some(name_exact) = required.name.as_exact() {
            name_exact
        } else {
            continue;
        };

        if let Some(local_vpkg) = generic_system_virtual_packages.get(name) {
            // `__archspec` compares microarchitectures, not strings: its
            // version is a meaningless constant, and an exactly-named build
            // is a baseline any descendant host can run.
            let satisfied = if name.as_normalized() == "__archspec" {
                archspec_requirement_satisfied(required.build.as_ref(), &local_vpkg.build_string)
            } else {
                required.matches(local_vpkg)
            };
            if !satisfied {
                return Err(VirtualPackageNotFoundError::new(
                    &required,
                    &generic_system_virtual_packages.values().collect(),
                )
                .into());
            }
            tracing::debug!("Required virtual package: {} matches the system", required);
        } else {
            return Err(VirtualPackageNotFoundError::new(
                &required,
                &generic_system_virtual_packages.values().collect(),
            )
            .into());
        }
    }

    // Check if the wheel tags match the system virtual packages if there are any
    if lock_platform.is_some_and(|p| environment.has_pypi_packages(p))
        && let Some(pypi_packages) = lock_platform.and_then(|p| environment.pypi_packages(p))
    {
        let python_record = python_record
            .ok_or_else(|| MachineValidationError::NoPythonRecordFound(platform.name().clone()))?;

        // Check if all the wheel tags match the system virtual packages
        let pypi_packages = pypi_packages.cloned().collect_vec();

        let wheels = get_wheels_from_pypi_package_data(pypi_packages);

        let uv_system_tags =
            get_tags_from_machine(&system_virtual_packages, platform, python_record)?;

        // Check if all the wheel tags match the system virtual packages
        for wheel in wheels {
            if !wheel.is_compatible(&uv_system_tags) {
                return Err(MachineValidationError::WheelTagsMismatch(
                    wheel.to_string(),
                    uv_system_tags.to_string(),
                ));
            }
            tracing::debug!("Wheel: {} matches the system", wheel);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod test {
    use super::*;
    use insta::assert_snapshot;
    use pixi_test_utils::format_diagnostic;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{PackageName, PackageRecord, ParseStrictness, Platform};
    use rattler_lock::{CondaBinaryData, PlatformData, PlatformName, UrlOrPath};
    use rattler_virtual_packages::Override;
    use std::path::Path;
    use url::Url;

    #[test]
    fn test_get_minimal_virtual_packages() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path =
            root_dir.join("../../tests/data/lock_files/cuda_virtual_dependency.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let platform = Platform::Linux64;
        let env = lock_file.default_environment().unwrap();
        let lock_platform = lock_file.platform(&platform.to_string()).unwrap();
        let conda_packages = env
            .conda_packages(lock_platform)
            .unwrap()
            .collect::<Vec<_>>();

        let all_depends: Vec<&str> = conda_packages
            .iter()
            .flat_map(|data| data.depends())
            .map(|s| s.as_str())
            .collect();

        let virtual_matchspecs = get_required_virtual_packages_from_depends(&all_depends).unwrap();

        assert!(
            virtual_matchspecs
                .iter()
                .contains(&MatchSpec::from_str("__cuda >=12", Lenient).unwrap())
        );
    }

    #[test]
    fn test_spec_version() {
        assert_eq!(
            spec_version(&VersionSpec::from_str(">=12", Lenient).unwrap()),
            Some(&Version::from_str("12").unwrap()),
        );
        assert_eq!(
            spec_version(&VersionSpec::from_str("==12.0", Lenient).unwrap()),
            Some(&Version::from_str("12.0").unwrap()),
        );
        assert_eq!(spec_version(&VersionSpec::Any), None);
    }

    #[test]
    fn test_compute_minimal_required_platforms() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path =
            root_dir.join("../../tests/data/lock_files/cuda_virtual_dependency.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let declared = PixiPlatform::from_subdir(Platform::Linux64);

        let minimal = compute_minimal_required_platforms(
            &lock_file,
            &EnvironmentName::default(),
            &[&declared],
        );

        let requirements = minimal
            .get(&Platform::Linux64)
            .expect("linux-64 minimal requirements");

        // Every `__cuda` spec the resolved packages ask for is kept verbatim
        // rather than folded into one entry at the highest version.
        let cuda: Vec<String> = requirements
            .iter()
            .filter(|spec| {
                spec.name
                    .as_exact()
                    .is_some_and(|n| n.as_normalized() == "__cuda")
            })
            .map(ToString::to_string)
            .collect();
        assert_eq!(cuda, vec!["__cuda >=12".to_string()]);

        // Only depended-on virtual packages are present; subdir defaults are
        // not padded in (`__archspec` is a linux-64 default but never appears
        // in `depends`).
        assert!(!requirements.iter().any(|spec| {
            spec.name
                .as_exact()
                .is_some_and(|n| n.as_normalized() == "__archspec")
        }));
    }

    /// A `depends` entry's build matcher survives into the requirement set.
    ///
    /// conda-forge's microarch metapackages constrain `__archspec` with a
    /// CEP-29 regex naming every compatible microarchitecture. Folding these
    /// into a `GenericVirtualPackage` -- which can only hold one literal build
    /// string -- dropped the constraint entirely, and taking the "highest
    /// version" collapsed distinct requirements into one.
    #[test]
    fn required_specs_keep_build_matchers_and_every_distinct_spec() {
        let depends = [
            "__archspec[version='1.*', build='^(x86_64_v3|haswell|skylake)$']",
            "__archspec 1 haswell",
            "__cuda >=11",
            "__cuda >=12",
            // A duplicate of the first spec must not appear twice.
            "__archspec[version='1.*', build='^(x86_64_v3|haswell|skylake)$']",
            // A non-virtual dependency is not a requirement on the machine.
            "python >=3.12",
        ];
        let requirements = required_virtual_package_specs(&depends);
        let rendered: Vec<String> = requirements.iter().map(ToString::to_string).collect();

        // Both `__cuda` bounds are kept: the machine must satisfy every spec,
        // and nothing here justifies picking one.
        assert!(
            rendered.contains(&"__cuda >=11".to_string()),
            "{rendered:?}"
        );
        assert!(
            rendered.contains(&"__cuda >=12".to_string()),
            "{rendered:?}"
        );
        // The regex reached the requirement set as a regex.
        let regex_spec = requirements
            .iter()
            .find(|spec| matches!(spec.build, Some(StringMatcher::Regex(_))))
            .expect("the regex build matcher must survive");
        assert!(regex_spec.build.as_ref().is_some_and(|build| {
            build.matches("skylake") && build.matches("haswell") && !build.matches("nehalem")
        }));
        // Deduplicated, and non-virtual dependencies are excluded.
        assert_eq!(rendered.len(), 4, "{rendered:?}");
        assert!(!rendered.iter().any(|spec| spec.contains("python")));
    }

    /// A version-less virtual-package dependency (bare `__cuda`) still
    /// requires the package to be present. It used to be dropped from the
    /// minimal platform, making machines without the package look compatible
    /// while `validate_system_meets_environment_requirements` rejected them.
    #[test]
    fn test_compute_minimal_required_platforms_versionless_spec() {
        let lock_source = r#"version: 7
platforms:
- name: linux-64
environments:
  default:
    channels:
    - url: https://conda.anaconda.org/conda-forge/
    packages:
      linux-64:
      - conda: https://conda.anaconda.org/conda-forge/linux-64/foo-1.0-h0.conda
packages:
- conda: https://conda.anaconda.org/conda-forge/linux-64/foo-1.0-h0.conda
  depends:
  - __cuda
"#;
        let lock_file = LockFile::from_str_with_base_directory(lock_source, None).unwrap();
        let declared = PixiPlatform::from_subdir(Platform::Linux64);

        let minimal = compute_minimal_required_platforms(
            &lock_file,
            &EnvironmentName::default(),
            &[&declared],
        );

        let requirements = minimal
            .get(&Platform::Linux64)
            .expect("linux-64 minimal requirements");
        let cuda = requirements
            .iter()
            .find(|spec| {
                spec.name
                    .as_exact()
                    .is_some_and(|n| n.as_normalized() == "__cuda")
            })
            .expect("bare __cuda must survive into the minimal requirements");
        // A bare spec keeps its "any version" meaning instead of being pinned
        // to a synthetic version 0.
        assert_eq!(cuda.version, None);
        assert_eq!(cuda.to_string(), "__cuda");
    }

    #[test]
    fn test_validate_virtual_packages() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path =
            root_dir.join("../../tests/data/lock_files/cuda_virtual_dependency.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::Linux64);

        // Override the virtual package to a version that is not available on the system
        let mut overrides = VirtualPackageOverrides::default();
        overrides.cuda = Some(Override::String("12.0".to_string()));

        let result = validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        );
        assert!(result.is_ok(), "{result:?}");

        // Override the virtual package to a version that is not available on the system
        let mut overrides = VirtualPackageOverrides::default();
        overrides.cuda = Some(Override::String("11.0".to_string()));

        let result = validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        );
        assert!(result.is_err());
    }

    /// Build a single-package linux-64 lock file whose lone conda package
    /// carries `depends`, used to drive the required-virtual-package check.
    fn lock_requiring(depends: &str) -> LockFile {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked("needs-libc"),
            Version::from_str("1.0").unwrap(),
            "0".to_string(),
        );
        record.subdir = "linux-64".to_string();
        record.depends = vec![depends.to_string()];
        let package = CondaPackageData::Binary(Box::new(CondaBinaryData {
            package_record: record,
            location: UrlOrPath::Url(
                Url::parse("https://example.com/needs-libc-1.0-0.conda").unwrap(),
            ),
            file_name: DistArchiveIdentifier::try_from_filename("needs-libc-1.0-0.conda").unwrap(),
            channel: None,
        }));
        let mut builder = LockFile::builder()
            .with_platforms(vec![PlatformData {
                name: PlatformName::try_from("linux-64").unwrap(),
                subdir: Platform::Linux64,
                virtual_packages: vec![],
            }])
            .unwrap();
        builder.set_channels("default", Vec::<rattler_lock::Channel>::new());
        builder.set_options("default", rattler_lock::SolveOptions::default());
        builder
            .add_conda_package("default", "linux-64", package)
            .unwrap();
        builder.finish()
    }

    /// `__musl`/`__eglibc` are verified at run-time like `__glibc`, not silently
    /// skipped. Forcing the libc slot to glibc means the host never reports
    /// `__musl`, so a musl-requiring environment must fail verification
    /// regardless of the test machine.
    #[test]
    fn musl_requirement_is_verified_not_skipped() {
        let lock_file = lock_requiring("__musl >=1.2");
        let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::Linux64);
        let mut overrides = VirtualPackageOverrides::default();
        overrides.libc = Some(Override::String("2.28".to_string()));

        let result = validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        );
        assert!(
            matches!(
                result,
                Err(MachineValidationError::VirtualPackageNotFound(_))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn test_validate_wheel_tags() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path = root_dir.join("../../tests/data/lock_files/pypi-numpy.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::current());

        // To high version for the wheel, which is fine as we assume backwards compatibility
        let mut overrides = VirtualPackageOverrides::default();
        overrides.osx = Some(Override::String("15.1".to_string()));
        overrides.libc = Some(Override::String("2.9999".to_string()));

        let result = validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        );
        assert!(result.is_ok(), "{result:?}");

        // To low version for the wheel
        let mut overrides = VirtualPackageOverrides::default();
        overrides.osx = Some(Override::String("13.0".to_string()));
        overrides.libc = Some(Override::String("2.10".to_string()));

        let result = validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        );
        if Platform::current().is_unix() {
            assert!(
                matches!(result, Err(MachineValidationError::WheelTagsMismatch(_, _))),
                "{result:?}"
            );
        } else {
            // It's hard to make the wheels fail on windows
            assert!(result.is_ok(), "{result:?}");
        }
    }

    #[test]
    fn test_virtual_package_not_found_error() {
        // Create a test MatchSpec for glibc
        let spec = MatchSpec::from_str("__glibc >= 2.28", ParseStrictness::Strict).unwrap();

        // Define some available virtual packages
        let libc = GenericVirtualPackage {
            name: "__glibc".parse().unwrap(),
            version: "2.17".parse().unwrap(),
            build_string: "".to_string(),
        };
        let cuda = GenericVirtualPackage {
            name: "__cuda".parse().unwrap(),
            version: "11.8".parse().unwrap(),
            build_string: "".to_string(),
        };
        let osx = GenericVirtualPackage {
            name: "__osx".parse().unwrap(),
            version: "10.14".parse().unwrap(),
            build_string: "".to_string(),
        };
        let system_virtual_packages = vec![&libc, &cuda, &osx];

        let error1 = VirtualPackageNotFoundError::new(&spec, &system_virtual_packages);

        // Create a test MatchSpec for unix which doesn't have an override
        let spec = MatchSpec::from_str("__unix >= 1.2.3", ParseStrictness::Strict).unwrap();
        let error2 = VirtualPackageNotFoundError::new(&spec, &system_virtual_packages);

        assert_snapshot!(format!(
            "With override:\n{}\nWithout override:\n{}",
            format_diagnostic(&error1),
            format_diagnostic(&error2)
        ));
    }
    #[test]
    fn test_virtual_package_not_found_error_with_overrides() {
        // Check all overrides
        let overrides = vec![
            ("__glibc >= 2.17", "`CONDA_OVERRIDE_GLIBC=2.17`"),
            ("__cuda >= 12.0", "`CONDA_OVERRIDE_CUDA=12.0`"),
            ("__osx >= 10.15", "`CONDA_OVERRIDE_OSX=10.15`"),
        ];

        let system_virtual_packages = vec![];

        for (spec, msg) in overrides {
            let error = VirtualPackageNotFoundError::new(
                &MatchSpec::from_str(spec, ParseStrictness::Strict).unwrap(),
                &system_virtual_packages,
            );
            assert!(error.help.unwrap().contains(msg));
        }
    }

    /// `__archspec` requirements are validated against the host, through the
    /// microarchitecture graph. The fixture's package requires
    /// `__archspec 1 x86_64` -- an exactly-named build, which is the shape
    /// conda-forge's legacy per-name microarch builds still use -- so any
    /// descendant host satisfies it while a cross-family one does not.
    ///
    /// This used to be skipped entirely, letting a lock install on a machine
    /// that cannot run it.
    #[test]
    fn test_archspec_matched_through_the_dag() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path = root_dir.join("../../tests/data/lock_files/archspec.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::Linux64);

        let overrides = |archspec: &str| {
            let mut overrides = VirtualPackageOverrides::default();
            overrides.libc = Some(Override::String("2.17".to_string()));
            overrides.archspec = Some(Override::String(archspec.to_string()));
            overrides
        };
        let validate = |archspec: &str| {
            validate_system_meets_environment_requirements(
                &lock_file,
                &platform,
                &EnvironmentName::default(),
                Some(overrides(archspec)),
            )
        };

        // The exact baseline, and anything descending from it, can run.
        for archspec in ["x86_64", "nehalem", "skylake", "zen4"] {
            validate(archspec)
                .unwrap_or_else(|error| panic!("{archspec} should satisfy __archspec: {error:?}"));
        }

        // A cross-family host cannot run x86_64 code, nor can a host whose
        // microarchitecture is unknown.
        for archspec in ["m1", "0"] {
            let error =
                validate(archspec).expect_err(&format!("{archspec} should not satisfy __archspec"));
            assert!(
                matches!(error, MachineValidationError::VirtualPackageNotFound(_)),
                "{error:?}"
            );
        }
    }

    #[test]
    fn test_ignored_virtual_packages() {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock_file_path =
            root_dir.join("../../tests/data/lock_files/ignored_virtual_packages.lock");
        let lock_file = LockFile::from_path(&lock_file_path).unwrap();
        let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::Linux64);

        let mut overrides = VirtualPackageOverrides::default();
        overrides.libc = Some(Override::String("2.17".to_string()));
        overrides.cuda = Some(Override::String("11.0".to_string()));

        // validate that the ignored virtual packages are skipped
        validate_system_meets_environment_requirements(
            &lock_file,
            &platform,
            &EnvironmentName::default(),
            Some(overrides),
        )
        .unwrap();
    }
}
