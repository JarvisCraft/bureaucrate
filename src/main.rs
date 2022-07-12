use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use anyhow::{anyhow, Result};
use chrono::{Date, NaiveDate, Utc};
use clap::{ArgGroup, Parser};
use git2::{DiffOptions, Repository, Sort};
use guppy::graph::{DependencyDirection, PackageMetadata};
use jrsonnet_evaluator::{typed::Typed, FileImportResolver, State};
use semver::Version;
use std::fmt::Write as _;
use toml_edit::TableLike;
use tracing::{info, warn};

mod bump;
use bump::Bump;

use crate::generator::Commit;

mod generator;

const COMMENT_START: &str = "<!-- bureaucrate goes here -->\n";

#[derive(Parser)]
#[clap(group = ArgGroup::new("since_rev").required(true))]
struct Opts {
    /// Last release revision
    #[clap(group = "since_rev")]
    rev: Option<String>,
    /// Walk from beginning of revision history,
    /// you can't have rev pointing to parent of first commit
    #[clap(long, group = "since_rev")]
    root: bool,

    /// Custom commit processor written in jsonnet
    #[clap(long)]
    generator: PathBuf,

    /// Default mode is dry-run, add --executed to actually
    /// append changes to codebase
    #[clap(long)]
    execute: bool,
}
impl Opts {
    fn since_rev(&self) -> Option<String> {
        if let Some(rev) = &self.rev {
            Some(rev.clone())
        } else {
            assert!(self.root);
            None
        }
    }
}

#[derive(Debug)]
struct PackageStatus<'g> {
    changelog: String,
    bump: Bump,
    bump_reasons: Vec<String>,
    package: PackageMetadata<'g>,
}
impl PackageStatus<'_> {
    fn final_version(&self) -> Version {
        self.bump.apply(self.package.version())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let opts = Opts::parse();

    info!("opening repo");
    let repo = Repository::open(".")?;

    info!("searching for top-level packages");
    let cargo_metadata = guppy::MetadataCommand::new().exec()?;
    let metadata = cargo_metadata.build_graph()?;

    let mut statuses = HashMap::new();

    let workspace = metadata.resolve_workspace();
    let mut nested = HashSet::new();
    let mut nested_pairs = Vec::new();
    for outer in workspace.packages(DependencyDirection::Forward) {
        let path = outer
            .source()
            .workspace_path()
            .expect("this is workspace package");

        for inner in workspace
            .packages(DependencyDirection::Forward)
            .filter(|inner| inner != &outer)
        {
            let inner_dir = inner
                .source()
                .workspace_path()
                .expect("this is workspace package");

            if !inner_dir.starts_with(path) {
                continue;
            }
            warn!(
                "package {} is nested inside {}, changelog will be merged",
                inner.name(),
                outer.name()
            );
            nested.insert(inner.id());
            nested_pairs.push((outer.id(), inner.id()));
        }

        statuses.insert(
            outer.id(),
            PackageStatus {
                changelog: String::new(),
                bump: Bump::None,
                bump_reasons: vec![],
                package: outer.clone(),
            },
        );
    }
    let outers = workspace.filter(DependencyDirection::Forward, |c| !nested.contains(&c.id()));

    let hide = if let Some(since) = opts.since_rev() {
        Some(repo.revparse_single(&since)?.id())
    } else {
        None
    };

    for pkg in outers.packages(DependencyDirection::Forward) {
        let pkgdir = pkg
            .source()
            .workspace_path()
            .expect("this is workspace package");

        info!("checking for updates in {} ({pkgdir})", pkg.name());
        let mut walk = repo.revwalk()?;
        walk.reset()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        walk.push_head()?;
        if let Some(hide) = hide {
            walk.hide(hide)?;
        }

        let s = State::default();
        s.set_import_resolver(Box::new(FileImportResolver::default()));
        s.with_stdlib();

        let gen = s
            .import(opts.generator.canonicalize()?)
            .map_err(|e| anyhow!("{}", s.stringify_err(&e)))?;
        let gen = generator::Generator::from_untyped(gen, s.clone())
            .map_err(|e| anyhow!("{}", s.stringify_err(&e)))?;

        let mut commits = vec![];
        for rev in walk {
            let rev = rev?;
            let commit = repo.find_commit(rev)?;
            let commit_tree = commit.tree()?;

            let mut changed = false;
            for parent in commit.parents() {
                let tree = parent.tree()?;
                let mut opts = DiffOptions::new();
                let mut diff = repo.diff_tree_to_tree(
                    Some(&tree),
                    Some(&commit_tree),
                    Some(opts.old_prefix("").new_prefix("")),
                )?;
                diff.find_similar(None)?;
                // TODO: use pathspec matcher, instead of naive delta iteration
                for diff in diff.deltas() {
                    for file in [diff.old_file().path(), diff.new_file().path()]
                        .into_iter()
                        .flatten()
                    {
                        if file.starts_with(pkgdir.as_std_path()) {
                            changed = true;
                            break;
                        }
                    }
                }
            }
            if changed {
                let message = commit.message().ok_or_else(|| anyhow!("expected utf-8"))?;
                let author = commit.author_with_mailmap(&repo.mailmap()?)?;
                let id = commit.id();
                commits.push(Commit {
                    id: id.to_string(),
                    author_email: author.email().ok_or(anyhow!("utf-8 email"))?.to_owned(),
                    author_name: author.name().ok_or(anyhow!("utf-8 name"))?.to_owned(),
                    message: message.to_owned(),
                })
            }
        }

        let verdict = (gen.commit_handler)(s.clone(), commits)
            .map_err(|e| anyhow!("{}", s.stringify_err(&e)))?;

        let mut pkg_status = statuses.get_mut(pkg.id()).expect("there is all packages");
        pkg_status.changelog = verdict.changelog.clone();
        pkg_status.bump = Bump::from_raw(verdict.bump);
        if pkg_status.bump > Bump::None {
            pkg_status.bump_reasons.push(format!(
                "changelog generator decided to bump to {:?}",
                pkg_status.bump
            ));
        }
    }

    let mut bumped = true;
    while bumped {
        bumped = false;
        for (outer, inner) in &nested_pairs {
            for (a, b) in [(outer, inner), (inner, outer)] {
                if statuses[b].bump < statuses[a].bump {
                    let bump = statuses[a].bump;
                    let mut a = statuses.get_mut(inner).expect("there is all packages");
                    a.bump_reasons
                        .push(format!("nested packages should have equal bump"));
                    a.bump = bump;
                    bumped = true;
                }
            }
        }
        for id in workspace.package_ids(DependencyDirection::Forward) {
            if statuses[id].bump == Bump::None {
                continue;
            }
            for dependent in workspace.package_ids(DependencyDirection::Forward) {
                if !metadata.directly_depends_on(&dependent, &id)? {
                    continue;
                }
                let old_bump = statuses[dependent].bump;
                if old_bump >= Bump::Patch {
                    continue;
                }
                let mut dependent = statuses.get_mut(dependent).expect("there is all packages");
                dependent
                    .bump_reasons
                    .push(format!("dependency ({id}) had bump",));
                dependent.bump = Bump::Patch;
                bumped = true;
            }
        }
    }

    if !opts.execute {
        // TODO: move result message generation to generator
        let mut out = String::new();
        write!(
            out,
            "Hey, seems like you need to have changelog and version bumps for your PR?\n\nDon't worry, i've got you covered, if you have proper commit messages, then changelog generated by me should be okay for you\n\n"
        )?;

        write!(out, "# Changes\n\n")?;
        write!(
            out,
            "After your confirmation, I will append the following entries to changelogs of packages:\n\n"
        )?;
        for (_, package) in &statuses {
            if package.changelog.trim() == "" {
                continue;
            }
            write!(
                out,
                "## {} v{} ({:?} bump)\n\n",
                package.package.name(),
                package.final_version(),
                package.bump
            )?;
            for line in package.changelog.trim().lines() {
                if line.starts_with("#") {
                    write!(out, "#")?;
                }
                writeln!(out, "{}", line)?;
            }
        }
        write!(out, "\n\n")?;
        write!(out, "# Bumps\n\n")?;
        // TODO: We only have at most one bump reason per bump level, but there may be multiple
        write!(
            out,
            "I may not be able to describe reason for bump, but they should be required:\n\n"
        )?;
        for (_, package) in statuses {
            if package.bump == Bump::None {
                continue;
            }
            write!(
                out,
                "{} `{}` -> `{}`\n\n",
                package.package.name(),
                package.package.version(),
                package.bump.apply(package.package.version())
            )?;
            for reason in &package.bump_reasons {
                write!(out, "- {}\n\n", reason)?;
            }
        }
        println!("{out}");
        return Ok(());
    }

    for (_, package) in &statuses {
        if package.changelog.is_empty() {
            continue;
        }
        let mut changelog_path = package.package.manifest_path().to_path_buf();
        changelog_path.pop();
        changelog_path.push("CHANGELOG.md");

        // TODO: check if error is not ENOENT
        let old_changelog = fs::read_to_string(&changelog_path).unwrap_or_default();
        let mut new_changelog = String::new();

        let next_start = if let Some(offset) = old_changelog.find(COMMENT_START) {
            new_changelog.push_str(&old_changelog[..offset + COMMENT_START.len()]);

            offset + COMMENT_START.len()
        } else {
            new_changelog.push_str(COMMENT_START);
            0
        };
        let next = &old_changelog[next_start..];

        let date = Utc::now().date().format("%Y-%m-%d").to_string();
        write!(
            new_changelog,
            "## [v{}] {}\n\n",
            package.final_version(),
            date.to_string()
        )?;
        for line in package.changelog.trim().lines() {
            if line.starts_with("#") {
                write!(new_changelog, "#")?;
            }
            writeln!(new_changelog, "{}", line)?;
        }
        new_changelog.push('\n');
        new_changelog.push_str(next);

        fs::write(&changelog_path, new_changelog.trim())?;
    }
    for (_, package) in statuses {
        let manifest_path = package.package.manifest_path();
        let manifest = fs::read_to_string(&manifest_path)?;
        let mut manifest: toml_edit::Document = manifest.parse()?;
        let root_table = manifest.as_table_mut();

        let package_table = root_table
            .get_mut("package")
            .expect("cargo metadata is fine")
            .as_table_like_mut()
            .expect("metadata is fine");
        package_table.insert(
            "version",
            toml_edit::value(package.final_version().to_string()),
        );
        fs::write(&manifest_path, manifest.to_string())?;
    }

    Ok(())
}
