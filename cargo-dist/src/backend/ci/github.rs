//! CI script generation
//!
//! In the future this may get split up into submodules.

// FIXME(#283): migrate this to minijinja (steal logic from oranda to load a whole dir)

use axoasset::LocalAsset;
use cargo_dist_schema::{GithubMatrix, GithubMatrixEntry};
use serde::Serialize;
use tracing::warn;

use crate::{
    backend::templates::TEMPLATE_CI_GITHUB,
    config::CiStyle,
    errors::{DistError, DistResult},
    DistGraph, SortedMap, SortedSet, TargetTriple,
};

const GITHUB_CI_DIR: &str = ".github/workflows/";
const GITHUB_CI_FILE: &str = "release.yml";

/// Info about running cargo-dist in Github CI
#[derive(Debug, Serialize)]
pub struct GithubCiInfo {
    /// Version of rust toolchain to install (deprecated)
    pub rust_version: Option<String>,
    /// expression to use for installing cargo-dist via shell script
    pub install_dist_sh: String,
    /// expression to use for installing cargo-dist via powershell script
    pub install_dist_ps1: String,
    /// Whether to fail-fast
    pub fail_fast: bool,
    /// Matrix for upload-local-artifacts
    pub artifacts_matrix: cargo_dist_schema::GithubMatrix,
    /// What kind of job to run on pull request
    pub pr_run_mode: cargo_dist_schema::PrRunMode,
    /// global task
    pub global_task: Option<GithubMatrixEntry>,
    /// homebrew tap
    pub tap: Option<String>,
    /// publish jobs
    pub publish_jobs: Vec<String>,
    /// whether to create the release or assume an existing one
    pub create_release: bool,
    /// whether to ignore on-disk changes to the configuration
    pub allow_dirty: bool,
}

impl GithubCiInfo {
    /// Compute the Github CI stuff
    pub fn new(dist: &DistGraph) -> GithubCiInfo {
        // Legacy deprecated support
        let rust_version = dist.desired_rust_toolchain.clone();

        // If they don't specify a cargo-dist version, use this one
        let self_dist_version = super::SELF_DIST_VERSION.parse().unwrap();
        let dist_version = dist
            .desired_cargo_dist_version
            .as_ref()
            .unwrap_or(&self_dist_version);
        let fail_fast = dist.fail_fast;
        let create_release = dist.create_release;

        // Figure out what builds we need to do
        let mut needs_global_build = false;
        let mut local_targets = SortedSet::new();
        for release in &dist.releases {
            if !release.global_artifacts.is_empty() {
                needs_global_build = true;
            }
            local_targets.extend(release.targets.iter());
        }

        // Get the platform-specific installation methods
        let install_dist_sh = super::install_dist_sh_for_version(dist_version);
        let install_dist_ps1 = super::install_dist_ps1_for_version(dist_version);

        // Build up the task matrix for building Artifacts
        let mut tasks = vec![];

        // If we have Global Artifacts, we need one task for that. If we've done a Good Job
        // then these artifacts should be possible to build on *any* platform. Linux is usually
        // fast/cheap, so that's a reasonable choice.s
        let global_task = if needs_global_build {
            Some(GithubMatrixEntry {
                runner: Some(GITHUB_LINUX_RUNNER.into()),
                dist_args: Some("--artifacts=global".into()),
                install_dist: Some(install_dist_sh.clone()),
            })
        } else {
            None
        };

        let pr_run_mode = dist.pr_run_mode.clone();
        let allow_dirty = dist.allow_dirty.contains(&CiStyle::Github);

        let tap = dist.tap.clone();
        let publish_jobs = dist.publish_jobs.iter().map(|j| j.to_string()).collect();

        // Figure out what Local Artifact tasks we need
        let local_runs = if dist.merge_tasks {
            distribute_targets_to_runners_merged(local_targets)
        } else {
            distribute_targets_to_runners_split(local_targets)
        };
        for (runner, targets) in local_runs {
            use std::fmt::Write;
            let install_dist =
                install_dist_for_github_runner(runner, &install_dist_sh, &install_dist_ps1);
            let mut dist_args = String::from("--artifacts=local");
            for target in targets {
                write!(dist_args, " --target={target}").unwrap();
            }
            tasks.push(GithubMatrixEntry {
                runner: Some(runner.to_owned()),
                dist_args: Some(dist_args),
                install_dist: Some(install_dist.to_owned()),
            });
        }

        GithubCiInfo {
            rust_version,
            install_dist_sh,
            install_dist_ps1,
            fail_fast,
            tap,
            publish_jobs,
            artifacts_matrix: GithubMatrix { include: tasks },
            pr_run_mode,
            global_task,
            create_release,
            allow_dirty,
        }
    }

    fn github_ci_path(&self, dist: &DistGraph) -> camino::Utf8PathBuf {
        let ci_dir = dist.workspace_dir.join(GITHUB_CI_DIR);
        ci_dir.join(GITHUB_CI_FILE)
    }

    /// Generate the requested configuration and returns it as a string.
    pub fn generate_github_ci(&self, dist: &DistGraph) -> DistResult<String> {
        let rendered = dist
            .templates
            .render_file_to_clean_string(TEMPLATE_CI_GITHUB, self)?;

        Ok(rendered)
    }

    /// Write release.yml to disk
    pub fn write_to_disk(&self, dist: &DistGraph) -> Result<(), miette::Report> {
        let ci_file = self.github_ci_path(dist);
        let rendered = self.generate_github_ci(dist)?;

        LocalAsset::write_new_all(&rendered, &ci_file)?;
        eprintln!("generated Github CI to {}", ci_file);

        Ok(())
    }

    /// Check whether the new configuration differs from the config on disk
    /// writhout actually writing the result.
    pub fn check_github_ci(&self, dist: &DistGraph) -> DistResult<()> {
        let ci_file = self.github_ci_path(dist);

        let rendered = self.generate_github_ci(dist)?;
        // FIXME: should we catch all errors, or only LocalAssetNotFound?
        let existing = LocalAsset::load_string(&ci_file).unwrap_or("".to_owned());
        if rendered != existing && !self.allow_dirty {
            Err(DistError::CheckFileMismatch {
                file: ci_file.to_string(),
            })
        } else {
            Ok(())
        }
    }
}

/// Given a set of targets we want to build local artifacts for, map them to Github Runners
/// while preferring to merge builds that can happen on the same machine.
///
/// This optimizes for machine-hours, at the cost of latency and fault-isolation.
///
/// Typically this will result in both x64 macos and arm64 macos getting shoved onto
/// the same runner, making the entire release process get bottlenecked on the twice-as-long
/// macos builds. It also makes it impossible to have one macos build fail and the other
/// succeed (uploading itself to the draft release).
///
/// In priniciple it does remove some duplicated setup work, so this is ostensibly "cheaper".
fn distribute_targets_to_runners_merged(
    targets: SortedSet<&TargetTriple>,
) -> std::vec::IntoIter<(GithubRunner, Vec<&TargetTriple>)> {
    let mut groups = SortedMap::<GithubRunner, Vec<&TargetTriple>>::new();
    for target in targets {
        let runner = github_runner_for_target(target);
        let runner = runner.unwrap_or_else(|| {
            let default = GITHUB_LINUX_RUNNER;
            warn!("not sure which github runner should be used for {target}, assuming {default}");
            default
        });
        groups.entry(runner).or_default().push(target);
    }
    // This extra into_iter+collect is needed to make this have the same
    // return type as distribute_targets_to_runners_split
    groups.into_iter().collect::<Vec<_>>().into_iter()
}

/// Given a set of targets we want to build local artifacts for, map them to Github Runners
/// while preferring each target gets its own runner for latency and fault-isolation.
fn distribute_targets_to_runners_split(
    targets: SortedSet<&TargetTriple>,
) -> std::vec::IntoIter<(GithubRunner, Vec<&TargetTriple>)> {
    let mut groups = vec![];
    for target in targets {
        let runner = github_runner_for_target(target);
        let runner = runner.unwrap_or_else(|| {
            let default = GITHUB_LINUX_RUNNER;
            warn!("not sure which github runner should be used for {target}, assuming {default}");
            default
        });
        groups.push((runner, vec![target]));
    }
    groups.into_iter()
}

/// A string representing a Github Runner
type GithubRunner = &'static str;
/// The Github Runner to use for Linux
const GITHUB_LINUX_RUNNER: &str = "ubuntu-20.04";
/// The Github Runner to use for macos
const GITHUB_MACOS_RUNNER: &str = "macos-11";
/// The Github Runner to use for windows
const GITHUB_WINDOWS_RUNNER: &str = "windows-2019";

/// Get the appropriate Github Runner for building a target
fn github_runner_for_target(target: &TargetTriple) -> Option<GithubRunner> {
    // We want to default to older runners to minimize the places
    // where random system dependencies can creep in and be very
    // recent. This helps with portability!
    if target.contains("linux") {
        Some(GITHUB_LINUX_RUNNER)
    } else if target.contains("apple") {
        Some(GITHUB_MACOS_RUNNER)
    } else if target.contains("windows") {
        Some(GITHUB_WINDOWS_RUNNER)
    } else {
        None
    }
}

/// Select the cargo-dist installer approach for a given Github Runner
fn install_dist_for_github_runner<'a>(
    runner: GithubRunner,
    install_sh: &'a str,
    install_ps1: &'a str,
) -> &'a str {
    if runner == GITHUB_LINUX_RUNNER || runner == GITHUB_MACOS_RUNNER {
        install_sh
    } else if runner == GITHUB_WINDOWS_RUNNER {
        install_ps1
    } else {
        unreachable!("internal error: unknown github runner!?")
    }
}
