use crate::Workspace;
use crate::lock_file::virtual_packages::spec_version;
use fancy_display::FancyDisplay;
use itertools::Itertools;
use miette::{Diagnostic, LabeledSpan};
use pixi_manifest::{EnvironmentName, PixiPlatformName, PlatformMatchDiagnosis, TaskName};
use rattler_conda_types::{GenericVirtualPackage, MatchSpec, Platform, StringMatcher, Version};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use thiserror::Error;

/// An error that occurs when data is requested for a platform that is not supported.
#[derive(Debug, Clone)]
pub struct UnsupportedPlatformError {
    /// Platforms supported by the environment
    pub environments_platforms: Vec<PixiPlatformName>,

    /// The environment that the platform is not supported for.
    pub environment: EnvironmentName,

    /// The platform that was requested
    pub platform: Platform,

    /// What this machine fails to provide: either virtual packages declared by
    /// workspace platforms matching the host subdir, or the requirements the
    /// locked packages impose when no declared platform matched at all. Empty
    /// when the platform mismatch isn't caused by virtual packages -- for
    /// example, when the user explicitly asked for a platform the environment
    /// doesn't declare at all.
    pub unsatisfied_requirements: Vec<UnmetRequirement>,

    /// Why each platform the environment declares does not run on this
    /// machine (unrunnable subdir or missing virtual packages). Empty when no
    /// breakdown is available (e.g. an explicit `--platform` the environment
    /// doesn't declare).
    pub platform_diagnostics: Vec<PlatformMatchDiagnosis>,
}

/// Something this machine fails to provide.
///
/// A *capability* is what a workspace platform declares it needs -- concrete by
/// construction, since a manifest can only name a version and a build string. A
/// *requirement* comes from a locked package's `depends` and can carry a version
/// range and a build-string pattern, so it stays a [`MatchSpec`]: flattening it
/// into a concrete package would drop the constraint it expresses.
#[derive(Debug, Clone)]
pub enum UnmetRequirement {
    /// A virtual package a workspace platform declares.
    Capability(GenericVirtualPackage),
    /// A virtual-package spec a locked package depends on. Boxed: a `MatchSpec`
    /// is several times the size of a `GenericVirtualPackage`.
    Requirement(Box<MatchSpec>),
}

impl From<GenericVirtualPackage> for UnmetRequirement {
    fn from(package: GenericVirtualPackage) -> Self {
        Self::Capability(package)
    }
}

impl From<MatchSpec> for UnmetRequirement {
    fn from(spec: MatchSpec) -> Self {
        Self::Requirement(Box::new(spec))
    }
}

impl UnmetRequirement {
    /// The `CONDA_OVERRIDE_*` hint that would mock this away, if any.
    fn override_hint(&self) -> Option<String> {
        match self {
            Self::Capability(package) => conda_override_hint(
                package.name.as_normalized(),
                Some(&package.version),
                Some(package.build_string.as_str()),
            ),
            Self::Requirement(spec) => {
                let name = spec.name.as_exact()?;
                let build = match spec.build.as_ref() {
                    Some(StringMatcher::Exact(build)) => Some(build.as_str()),
                    // A pattern names no single value to suggest.
                    Some(_) | None => None,
                };
                conda_override_hint(
                    name.as_normalized(),
                    spec.version.as_ref().and_then(spec_version),
                    build,
                )
            }
        }
    }
}

impl Display for UnmetRequirement {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            // A match spec already renders as the requirement it is.
            Self::Requirement(spec) => write!(f, "{spec}"),
            Self::Capability(package) => {
                // `__archspec` carries a microarchitecture name in its build
                // string; its version is a meaningless constant.
                if let Some(microarchitecture) = archspec_microarchitecture_of(package) {
                    return write!(f, "{} {microarchitecture}", package.name.as_normalized());
                }
                // Version 0 encodes a version-less requirement (a bare
                // `__cuda` dependency): present at any version.
                if package.version == Version::major(0) {
                    write!(f, "{} (any version)", package.name.as_normalized())
                } else {
                    // Conda's canonical spec spelling, so capabilities and
                    // match-spec requirements read alike in one message.
                    write!(f, "{} >={}", package.name.as_normalized(), package.version)
                }
            }
        }
    }
}

impl Error for UnsupportedPlatformError {}

impl Display for UnsupportedPlatformError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let lead = match &self.environment {
            EnvironmentName::Default => {
                format!("The workspace does not support '{}'", self.platform)
            }
            EnvironmentName::Named(name) => {
                format!(
                    "the environment '{name}' does not support '{}'",
                    self.platform
                )
            }
        };
        let breakdown: Vec<&PlatformMatchDiagnosis> = self
            .platform_diagnostics
            .iter()
            .filter(|d| !d.matches_host())
            .collect();

        // Nothing to elaborate: keep the terse legacy message.
        if breakdown.is_empty() && self.unsatisfied_requirements.is_empty() {
            return match &self.environment {
                EnvironmentName::Default => write!(
                    f,
                    "{lead}.\nAdd it with 'pixi workspace platform add {}'.",
                    self.platform,
                ),
                EnvironmentName::Named(_) => write!(f, "{lead}"),
            };
        }

        write!(f, "{lead} on this machine.")?;

        if breakdown.is_empty() {
            // No per-platform breakdown available; fall back to the aggregate line.
            write!(
                f,
                "\nNo declared platform's virtual packages are satisfied here."
            )?;
        } else {
            write!(f, "\n\nDeclared platforms and why none runs here:")?;
            for diagnosis in breakdown {
                write!(f, "\n  - {}", format_platform_diagnosis(diagnosis))?;
            }
        }

        if !self.unsatisfied_requirements.is_empty() {
            write!(
                f,
                "\n\nUnsatisfied requirements: {}",
                format_requirements(&self.unsatisfied_requirements),
            )?;
        }

        // Adding the host platform to the workspace is the primary remedy for
        // the default environment; named environments get their platform list
        // from features, where the workspace-level hint may not apply.
        match &self.environment {
            EnvironmentName::Default => write!(
                f,
                "\n\nAdd it with 'pixi workspace platform add {}'.",
                self.platform,
            )?,
            EnvironmentName::Named(_) => {}
        }
        Ok(())
    }
}

/// One line in the "declared platforms and why none runs here" list: the
/// platform's name (with its subdir when they differ) followed by the reason
/// it can't run -- an unrunnable subdir or the virtual packages this machine
/// doesn't provide.
fn format_platform_diagnosis(diagnosis: &PlatformMatchDiagnosis) -> String {
    let label = if diagnosis.name.as_str() == diagnosis.subdir.as_str() {
        diagnosis.name.as_str().to_string()
    } else {
        format!("{} (subdir {})", diagnosis.name.as_str(), diagnosis.subdir)
    };
    let reason = if !diagnosis.subdir_matches_host {
        format!(
            "requires subdir '{}', which this machine can't run",
            diagnosis.subdir
        )
    } else {
        format!(
            "missing virtual packages: {}",
            format_requirements(
                &diagnosis
                    .unsatisfied_virtual_packages
                    .iter()
                    .cloned()
                    .map(UnmetRequirement::from)
                    .collect::<Vec<_>>(),
            ),
        )
    };
    format!("{label}: {reason}")
}

impl Diagnostic for UnsupportedPlatformError {
    fn code(&self) -> Option<Box<dyn Display + '_>> {
        Some(Box::new("unsupported-platform".to_string()))
    }

    fn help(&self) -> Option<Box<dyn Display + '_>> {
        let overrides: Vec<String> = self
            .unsatisfied_requirements
            .iter()
            .filter_map(UnmetRequirement::override_hint)
            .collect();

        let base = if overrides.is_empty() {
            format!(
                "supported platforms are {}",
                self.environments_platforms.iter().format(", ")
            )
        } else {
            format!(
                "Mock the missing virtual packages via the environment, e.g.:\n  {}",
                overrides.join("\n  ")
            )
        };
        // `--platform` pins a target and skips host virtual-package
        // validation, so it's the escape hatch when the machine can't run any
        // declared platform.
        Some(Box::new(format!(
            "{base}\nOr install for a target this machine cannot run with \
             `pixi install --platform <platform>`."
        )))
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        None
    }
}

fn format_requirements(reqs: &[UnmetRequirement]) -> String {
    reqs.iter().map(ToString::to_string).join(", ")
}

/// The microarchitecture a `__archspec` entry names, or `None` for any other
/// virtual package (and for an explicitly unknown microarchitecture).
fn archspec_microarchitecture_of(package: &GenericVirtualPackage) -> Option<&str> {
    if package.name.as_normalized() != "__archspec" {
        return None;
    }
    pixi_manifest::platform::archspec_microarchitecture(&package.build_string)
}

/// `CONDA_OVERRIDE_*` hint for a missing virtual package: the required
/// version when known, a realistic example otherwise. `None` for virtual
/// packages without a known override (e.g. `__unix`).
///
/// `build_string` carries `__archspec`'s microarchitecture, whose version is a
/// constant an override can't usefully name.
pub(crate) fn conda_override_hint(
    name: &str,
    version: Option<&Version>,
    build_string: Option<&str>,
) -> Option<String> {
    let env_var = match name {
        "__glibc" => "CONDA_OVERRIDE_GLIBC",
        "__cuda" => "CONDA_OVERRIDE_CUDA",
        "__osx" => "CONDA_OVERRIDE_OSX",
        "__linux" => "CONDA_OVERRIDE_LINUX",
        "__win" => "CONDA_OVERRIDE_WIN",
        "__archspec" => "CONDA_OVERRIDE_ARCHSPEC",
        _ => return None,
    };
    // `__archspec` is named by microarchitecture, not version.
    if name == "__archspec" {
        let example = build_string
            .and_then(pixi_manifest::platform::archspec_microarchitecture)
            .unwrap_or("skylake");
        return Some(format!("{env_var}={example}"));
    }
    // A version-0 requirement means "any version"; "=0" reads like nonsense,
    // so suggest a realistic value instead.
    let example = match version.filter(|v| **v != Version::major(0)) {
        Some(version) => version.to_string(),
        None => match name {
            "__glibc" => "2.17".to_string(),
            "__cuda" => "12.0".to_string(),
            "__osx" => "10.15".to_string(),
            _ => "0".to_string(),
        },
    };
    Some(format!("{env_var}={example}"))
}

/// Errors that can occur while resolving workspace build variants.
#[derive(Debug, Diagnostic, Error)]
pub enum VariantsError {
    #[error("failed to read variant file '{path}'")]
    #[diagnostic(code(workspace::variants::read_file))]
    ReadVariantFile {
        /// Absolute path to the variant file that failed to read.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// An error that occurs when a task is requested which could not be found.
/// TODO: Make this error better.
///     - Include names that might have been meant instead
///     - If the tasks is only available for a certain platform, explain that.
#[derive(Debug, Clone, Diagnostic, Error)]
#[error("the task '{0}' could not be found", task_name.fancy_display())]
pub struct UnknownTask<'p> {
    /// The project that the platform is not supported for.
    pub project: &'p Workspace,

    /// The environment that the platform is not supported for.
    pub environment: EnvironmentName,

    /// The platform that was requested (if any)
    pub platform: Option<PixiPlatformName>,

    /// The name of the task
    pub task_name: TaskName,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rattler_conda_types::{PackageName, Version};
    use std::str::FromStr;

    fn vp(name: &str, version: &str) -> GenericVirtualPackage {
        GenericVirtualPackage {
            name: PackageName::from_str(name).unwrap(),
            version: Version::from_str(version).unwrap(),
            build_string: String::new(),
        }
    }

    fn err(unsatisfied: Vec<GenericVirtualPackage>) -> UnsupportedPlatformError {
        UnsupportedPlatformError {
            environments_platforms: vec![],
            environment: EnvironmentName::Default,
            platform: Platform::Linux64,
            unsatisfied_requirements: unsatisfied.into_iter().map(Into::into).collect(),
            platform_diagnostics: vec![],
        }
    }

    #[test]
    fn missing_cuda_reports_requirement_and_override_hint() {
        let e = err(vec![vp("__cuda", "11")]);
        let display = e.to_string();
        assert!(
            display.contains("Unsatisfied requirements: __cuda >=11"),
            "{display}"
        );
        let help = e.help().unwrap().to_string();
        assert!(help.contains("CONDA_OVERRIDE_CUDA=11"), "{help}");
        assert!(help.contains("pixi install --platform"), "{help}");
    }

    #[test]
    fn missing_multiple_vps_list_all_overrides() {
        let e = err(vec![vp("__cuda", "12.0"), vp("__glibc", "2.27")]);
        let help = e.help().unwrap().to_string();
        assert!(help.contains("CONDA_OVERRIDE_CUDA=12"), "{help}");
        assert!(help.contains("CONDA_OVERRIDE_GLIBC=2.27"), "{help}");
    }

    #[test]
    fn named_environment_renders_unsatisfied_requirements() {
        let mut e = err(vec![vp("__cuda", "11")]);
        e.environment = EnvironmentName::Named("gpu".into());
        let display = e.to_string();
        assert!(
            display.contains("the environment 'gpu' does not support 'linux-64' on this machine"),
            "{display}"
        );
        assert!(
            display.contains("Unsatisfied requirements: __cuda >=11"),
            "{display}"
        );
    }

    fn diagnosis(
        name: &str,
        subdir: Platform,
        subdir_matches_host: bool,
        unsatisfied: Vec<GenericVirtualPackage>,
    ) -> PlatformMatchDiagnosis {
        PlatformMatchDiagnosis {
            name: PixiPlatformName::try_from(name).unwrap(),
            subdir,
            subdir_matches_host,
            unsatisfied_virtual_packages: unsatisfied,
        }
    }

    #[test]
    fn breakdown_lists_missing_virtual_packages_per_platform() {
        let mut e = err(vec![vp("__cuda", "12.0")]);
        e.environment = EnvironmentName::Named("gpu".into());
        e.platform_diagnostics = vec![diagnosis(
            "gpu-linux",
            Platform::Linux64,
            true,
            vec![vp("__cuda", "12.0")],
        )];
        let display = e.to_string();
        assert!(
            display.contains("Declared platforms and why none runs here:"),
            "{display}"
        );
        assert!(
            display.contains("gpu-linux (subdir linux-64): missing virtual packages: __cuda >=12"),
            "{display}"
        );
    }

    #[test]
    fn breakdown_reports_unrunnable_subdir() {
        let mut e = err(vec![]);
        e.environment = EnvironmentName::Named("mac".into());
        e.platform_diagnostics = vec![diagnosis("osx-arm64", Platform::OsxArm64, false, vec![])];
        let display = e.to_string();
        assert!(
            display
                .contains("osx-arm64: requires subdir 'osx-arm64', which this machine can't run"),
            "{display}"
        );
    }

    #[test]
    fn breakdown_skips_platforms_that_match_host() {
        // A platform that runs here (subdir ok, no unmet VPs) is filtered out of
        // the "why none runs here" list.
        let mut e = err(vec![]);
        e.environment = EnvironmentName::Named("gpu".into());
        e.platform_diagnostics = vec![
            diagnosis("linux-64", Platform::Linux64, true, vec![]),
            diagnosis(
                "gpu-linux",
                Platform::Linux64,
                true,
                vec![vp("__cuda", "12.0")],
            ),
        ];
        let display = e.to_string();
        assert!(!display.contains("linux-64:"), "{display}");
        assert!(
            display.contains("gpu-linux (subdir linux-64):"),
            "{display}"
        );
    }

    #[test]
    fn breakdown_keeps_platform_add_hint_for_default_environment() {
        let mut e = err(vec![]);
        e.platform_diagnostics = vec![diagnosis("win-64", Platform::Win64, false, vec![])];
        let display = e.to_string();
        assert!(
            display.contains("Add it with 'pixi workspace platform add linux-64'."),
            "{display}"
        );

        e.environment = EnvironmentName::Named("gpu".into());
        let display = e.to_string();
        assert!(
            !display.contains("pixi workspace platform add"),
            "{display}"
        );
    }

    #[test]
    fn empty_unsatisfied_keeps_legacy_supported_platforms_help() {
        let mut e = err(vec![]);
        e.environments_platforms = vec![PixiPlatformName::try_from("linux-64").unwrap()];
        let display = e.to_string();
        assert!(
            display.contains("Add it with 'pixi workspace platform add linux-64'"),
            "{display}"
        );
        let help = e.help().unwrap().to_string();
        assert!(help.contains("supported platforms are linux-64"), "{help}");
        assert!(help.contains("pixi install --platform"), "{help}");
    }

    #[test]
    fn unknown_vp_name_skips_override_hint_but_still_lists_requirement() {
        // Version 0 means "must be present at any version" and renders as
        // such instead of a nonsensical ">= 0".
        let e = err(vec![vp("__unix", "0")]);
        let display = e.to_string();
        assert!(
            display.contains("Unsatisfied requirements: __unix (any version)"),
            "{display}"
        );
        let help = e.help().unwrap().to_string();
        assert!(!help.contains("CONDA_OVERRIDE"), "{help}");
    }

    #[test]
    fn versionless_requirement_suggests_realistic_override() {
        let e = err(vec![vp("__cuda", "0")]);
        let display = e.to_string();
        assert!(
            display.contains("Unsatisfied requirements: __cuda (any version)"),
            "{display}"
        );
        let help = e.help().unwrap().to_string();
        assert!(help.contains("CONDA_OVERRIDE_CUDA=12.0"), "{help}");
    }
}
