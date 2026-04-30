use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml::Value;
use vmctl_util::command_runner::{self, CommandOptions, LogPrefix};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ModuleKind {
    Resource,
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleKey {
    pub kind: ModuleKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModuleOrigin {
    Local {
        collection_root: PathBuf,
        module_dir: PathBuf,
    },
    Git {
        repo_url: String,
        ref_: String,
        commit: String,
        checkout_root: PathBuf,
        module_dir: PathBuf,
    },
    Inline {
        config_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleLocation {
    pub key: ModuleKey,
    pub origin: ModuleOrigin,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedModule {
    pub kind: ModuleKind,
    pub name: String,
    pub module_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub origin: ModuleOrigin,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleRegistry {
    pub resources: BTreeMap<String, ModuleLocation>,
    pub services: BTreeMap<String, ModuleLocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleLayer {
    Remote,
    Local,
    Inline,
}

impl ModuleLayer {
    fn precedence(self) -> u8 {
        match self {
            ModuleLayer::Remote => 1,
            ModuleLayer::Local => 2,
            ModuleLayer::Inline => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    LocalPath {
        path: PathBuf,
    },
    Git {
        repo_url: String,
        ref_: String,
        subdir: Option<String>,
    },
    Inline,
}

pub trait SourceResolver {
    fn parse(&self, source: &str) -> Result<SourceSpec>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSourceResolver;

impl SourceResolver for DefaultSourceResolver {
    fn parse(&self, source: &str) -> Result<SourceSpec> {
        if source == "inline" {
            return Ok(SourceSpec::Inline);
        }

        if let Some(git) = source.strip_prefix("git::") {
            return parse_git_source(git);
        }

        if looks_like_git_source(source) {
            return Ok(SourceSpec::Git {
                repo_url: source.to_string(),
                ref_: "main".to_string(),
                subdir: None,
            });
        }

        if let Some(local) = source.strip_prefix("local://") {
            return Ok(SourceSpec::LocalPath {
                path: normalize_local_source_path(local)?,
            });
        }

        if source.trim().is_empty() {
            bail!("local source requires a path");
        }

        Ok(SourceSpec::LocalPath {
            path: normalize_local_source_path(source)?,
        })
    }
}

fn looks_like_git_source(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("ssh://") || source.starts_with("git@")
}

fn normalize_local_source_path(source: &str) -> Result<PathBuf> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("local source requires a path");
    }
    if let Some(stripped) = trimmed.strip_prefix("./") {
        if stripped.trim().is_empty() {
            bail!("local source requires a path");
        }
    }
    Ok(PathBuf::from(trimmed))
}

fn parse_git_source(value: &str) -> Result<SourceSpec> {
    let (path_part, query_part) = value
        .split_once('?')
        .ok_or_else(|| anyhow!("git source must include `?ref=`"))?;

    let ref_ = query_part
        .split('&')
        .find_map(|kv| kv.strip_prefix("ref="))
        .filter(|ref_| !ref_.trim().is_empty())
        .ok_or_else(|| anyhow!("git source requires non-empty `ref` query parameter"))?
        .to_string();

    let (repo_url, subdir) = split_git_path(path_part)?;
    Ok(SourceSpec::Git {
        repo_url,
        ref_,
        subdir,
    })
}

fn split_git_path(path_part: &str) -> Result<(String, Option<String>)> {
    let scheme_idx = path_part
        .find("://")
        .ok_or_else(|| anyhow!("git source must include scheme like https:// or ssh://"))?;
    let search_start = scheme_idx + 3;
    let split_idx = path_part[search_start..]
        .find("//")
        .map(|idx| idx + search_start);

    if let Some(split_idx) = split_idx {
        let repo_url = path_part[..split_idx].to_string();
        let subdir = normalize_subdir(&path_part[split_idx + 2..])?;
        Ok((repo_url, subdir))
    } else {
        Ok((path_part.to_string(), None))
    }
}

fn normalize_subdir(subdir: &str) -> Result<Option<String>> {
    let trimmed = subdir.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        bail!("git source subdir must be relative");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("git source subdir may not include `..`"),
            _ => bail!("git source subdir contains invalid path segment"),
        }
    }

    Ok(Some(trimmed.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoRef {
    pub repo_url: String,
    pub ref_: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepo {
    pub repo: RepoRef,
    pub commit: String,
    pub checkout_root: PathBuf,
}

pub trait RepoManager {
    fn ensure_repo(&self, repo: &RepoRef, offline: bool) -> Result<ResolvedRepo>;
    fn list_repos(&self) -> Result<Vec<ResolvedRepo>>;
}

#[derive(Debug, Clone)]
pub struct GitRepoManager {
    cache_root: PathBuf,
}

impl GitRepoManager {
    pub fn new(cache_root: PathBuf) -> Self {
        Self { cache_root }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    fn repo_ref_paths(&self, repo: &RepoRef) -> RepoRefPaths {
        let repo_hash = short_sha256(&repo.repo_url);
        let ref_segment = sanitize_ref(&repo.ref_);
        let root = self
            .cache_root
            .join(repo_hash)
            .join("refs")
            .join(ref_segment);
        RepoRefPaths {
            root: root.clone(),
            checkout: root.join("worktree"),
            resolved_commit: root.join("RESOLVED_COMMIT"),
        }
    }
}

impl RepoManager for GitRepoManager {
    fn ensure_repo(&self, repo: &RepoRef, offline: bool) -> Result<ResolvedRepo> {
        let paths = self.repo_ref_paths(repo);
        if paths.checkout.exists() {
            if !offline {
                update_checkout(&paths.checkout, repo)?;
            }
            let commit = read_or_resolve_commit(&paths.checkout, &paths.resolved_commit)?;
            return Ok(ResolvedRepo {
                repo: repo.clone(),
                commit,
                checkout_root: paths.checkout,
            });
        }

        if offline {
            bail!(
                "git source {}@{} is not cached and offline mode is enabled",
                repo.repo_url,
                repo.ref_
            );
        }

        fs::create_dir_all(&paths.root)
            .with_context(|| format!("failed to create {}", paths.root.display()))?;
        run_git(
            CommandOptions::new(
                "git",
                [
                    "clone",
                    &repo.repo_url,
                    paths.checkout.to_string_lossy().as_ref(),
                ],
            )
            .timeout(Duration::from_secs(300))
            .prefix(LogPrefix::Vmctl),
        )?;
        update_checkout(&paths.checkout, repo)?;

        let commit = resolve_head_commit(&paths.checkout)?;
        fs::write(&paths.resolved_commit, format!("{commit}\n")).with_context(|| {
            format!(
                "failed to write resolved commit file {}",
                paths.resolved_commit.display()
            )
        })?;

        Ok(ResolvedRepo {
            repo: repo.clone(),
            commit,
            checkout_root: paths.checkout,
        })
    }

    fn list_repos(&self) -> Result<Vec<ResolvedRepo>> {
        if !self.cache_root.exists() {
            return Ok(Vec::new());
        }

        let mut repos = Vec::new();
        for repo_entry in fs::read_dir(&self.cache_root)
            .with_context(|| format!("failed to read {}", self.cache_root.display()))?
        {
            let repo_entry = repo_entry?;
            if !repo_entry.file_type()?.is_dir() {
                continue;
            }
            let refs_dir = repo_entry.path().join("refs");
            if !refs_dir.exists() {
                continue;
            }
            for ref_entry in fs::read_dir(&refs_dir)
                .with_context(|| format!("failed to read {}", refs_dir.display()))?
            {
                let ref_entry = ref_entry?;
                if !ref_entry.file_type()?.is_dir() {
                    continue;
                }
                let checkout = ref_entry.path().join("worktree");
                if !checkout.exists() {
                    continue;
                }
                let repo_url = git_remote_origin_url(&checkout).unwrap_or_default();
                let ref_ = unsanitize_ref(ref_entry.file_name().to_string_lossy().as_ref());
                let commit = resolve_head_commit(&checkout)?;
                repos.push(ResolvedRepo {
                    repo: RepoRef { repo_url, ref_ },
                    commit,
                    checkout_root: checkout,
                });
            }
        }

        repos.sort_by(|a, b| {
            a.repo
                .repo_url
                .cmp(&b.repo.repo_url)
                .then_with(|| a.repo.ref_.cmp(&b.repo.ref_))
        });
        Ok(repos)
    }
}

struct RepoRefPaths {
    root: PathBuf,
    checkout: PathBuf,
    resolved_commit: PathBuf,
}

fn update_checkout(checkout: &Path, repo: &RepoRef) -> Result<()> {
    run_git(
        CommandOptions::new("git", ["fetch", "--tags", "--force", "origin", &repo.ref_])
            .cwd(checkout)
            .timeout(Duration::from_secs(180))
            .prefix(LogPrefix::Vmctl),
    )?;
    run_git(
        CommandOptions::new("git", ["checkout", "--detach", "FETCH_HEAD"])
            .cwd(checkout)
            .timeout(Duration::from_secs(120))
            .prefix(LogPrefix::Vmctl),
    )?;
    Ok(())
}

fn run_git(options: CommandOptions) -> Result<()> {
    command_runner::run(options)
        .map(|_| ())
        .map_err(|error| anyhow!(error))
}

fn read_or_resolve_commit(checkout: &Path, commit_path: &Path) -> Result<String> {
    if commit_path.exists() {
        let value = fs::read_to_string(commit_path)
            .with_context(|| format!("failed to read {}", commit_path.display()))?;
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let commit = resolve_head_commit(checkout)?;
    fs::write(commit_path, format!("{commit}\n"))
        .with_context(|| format!("failed to write {}", commit_path.display()))?;
    Ok(commit)
}

fn resolve_head_commit(checkout: &Path) -> Result<String> {
    let output = command_runner::run(
        CommandOptions::new("git", ["rev-parse", "HEAD"])
            .cwd(checkout)
            .timeout(Duration::from_secs(30))
            .prefix(LogPrefix::Vmctl)
            .stream(false),
    )
    .map_err(|error| anyhow!(error))?;
    let commit = output.stdout.trim();
    if commit.is_empty() {
        bail!("git rev-parse HEAD returned empty output");
    }
    Ok(commit.to_string())
}

fn git_remote_origin_url(checkout: &Path) -> Result<String> {
    let output = command_runner::run(
        CommandOptions::new("git", ["remote", "get-url", "origin"])
            .cwd(checkout)
            .timeout(Duration::from_secs(30))
            .prefix(LogPrefix::Vmctl)
            .stream(false),
    )
    .map_err(|error| anyhow!(error))?;
    Ok(output.stdout.trim().to_string())
}

fn short_sha256(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn sanitize_ref(reference: &str) -> String {
    reference
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn unsanitize_ref(reference: &str) -> String {
    reference.to_string()
}

pub trait ModuleIndexer {
    fn index_collection(
        &self,
        collection_root: &Path,
        origin: &ModuleOrigin,
    ) -> Result<Vec<IndexedModule>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FsModuleIndexer;

impl ModuleIndexer for FsModuleIndexer {
    fn index_collection(
        &self,
        collection_root: &Path,
        origin: &ModuleOrigin,
    ) -> Result<Vec<IndexedModule>> {
        if !collection_root.exists() {
            return Ok(Vec::new());
        }

        let mut modules = Vec::new();
        walk_collection(collection_root, collection_root, origin, &mut modules)?;
        modules.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.manifest_path.cmp(&right.manifest_path))
        });
        Ok(modules)
    }
}

fn walk_collection(
    collection_root: &Path,
    current: &Path,
    origin: &ModuleOrigin,
    modules: &mut Vec<IndexedModule>,
) -> Result<()> {
    if !current.is_dir() {
        return Ok(());
    }

    let resource_manifest = current.join("resource.toml");
    if resource_manifest.exists() {
        modules.push(index_manifest(
            ModuleKind::Resource,
            collection_root,
            current,
            &resource_manifest,
            origin,
        )?);
    }

    let service_manifest = current.join("service.toml");
    if service_manifest.exists() {
        modules.push(index_manifest(
            ModuleKind::Service,
            collection_root,
            current,
            &service_manifest,
            origin,
        )?);
    }

    for entry in
        fs::read_dir(current).with_context(|| format!("failed to read {}", current.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        walk_collection(collection_root, &entry.path(), origin, modules)?;
    }

    Ok(())
}

fn index_manifest(
    kind: ModuleKind,
    collection_root: &Path,
    module_dir: &Path,
    manifest_path: &Path,
    origin: &ModuleOrigin,
) -> Result<IndexedModule> {
    let raw = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let value = raw
        .parse::<Value>()
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("manifest `{}` is missing `name`", manifest_path.display()))?;

    let expected = module_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name != expected {
        bail!(
            "module `{}` declares name `{name}`; directory name must match",
            manifest_path.display()
        );
    }

    let version = if kind == ModuleKind::Service {
        value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };

    Ok(IndexedModule {
        kind,
        name,
        module_dir: module_dir.to_path_buf(),
        manifest_path: manifest_path.to_path_buf(),
        origin: enrich_origin(origin, collection_root, module_dir),
        version,
    })
}

fn enrich_origin(origin: &ModuleOrigin, collection_root: &Path, module_dir: &Path) -> ModuleOrigin {
    match origin {
        ModuleOrigin::Local { .. } => ModuleOrigin::Local {
            collection_root: collection_root.to_path_buf(),
            module_dir: module_dir.to_path_buf(),
        },
        ModuleOrigin::Git {
            repo_url,
            ref_,
            commit,
            checkout_root,
            ..
        } => ModuleOrigin::Git {
            repo_url: repo_url.clone(),
            ref_: ref_.clone(),
            commit: commit.clone(),
            checkout_root: checkout_root.clone(),
            module_dir: module_dir.to_path_buf(),
        },
        ModuleOrigin::Inline { config_path } => ModuleOrigin::Inline {
            config_path: config_path.clone(),
        },
    }
}

#[derive(Debug, Clone)]
struct LocatedModule {
    location: ModuleLocation,
    layer: ModuleLayer,
}

#[derive(Debug, Default, Clone)]
pub struct ModuleRegistryBuilder {
    resources: BTreeMap<String, LocatedModule>,
    services: BTreeMap<String, LocatedModule>,
}

impl ModuleRegistryBuilder {
    pub fn add_indexed(&mut self, modules: Vec<IndexedModule>, layer: ModuleLayer) -> Result<()> {
        for indexed in modules {
            let key = ModuleKey {
                kind: indexed.kind,
                name: indexed.name,
            };
            let location = ModuleLocation {
                key: key.clone(),
                origin: indexed.origin,
                manifest_path: indexed.manifest_path,
            };
            self.insert(location, layer)?;
        }
        Ok(())
    }

    fn insert(&mut self, location: ModuleLocation, layer: ModuleLayer) -> Result<()> {
        let target = match location.key.kind {
            ModuleKind::Resource => &mut self.resources,
            ModuleKind::Service => &mut self.services,
        };

        if let Some(existing) = target.get(&location.key.name) {
            let existing_precedence = existing.layer.precedence();
            let incoming_precedence = layer.precedence();
            if existing_precedence == incoming_precedence {
                bail!(
                    "duplicate module `{}` at precedence layer {}: {} and {}",
                    location.key.name,
                    layer,
                    existing.location.origin,
                    location.origin
                );
            }
            if incoming_precedence > existing_precedence {
                target.insert(location.key.name.clone(), LocatedModule { location, layer });
            }
            return Ok(());
        }

        target.insert(location.key.name.clone(), LocatedModule { location, layer });
        Ok(())
    }

    pub fn build(self) -> ModuleRegistry {
        ModuleRegistry {
            resources: self
                .resources
                .into_iter()
                .map(|(name, entry)| (name, entry.location))
                .collect(),
            services: self
                .services
                .into_iter()
                .map(|(name, entry)| (name, entry.location))
                .collect(),
        }
    }
}

impl fmt::Display for ModuleLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            ModuleLayer::Remote => "remote",
            ModuleLayer::Local => "local",
            ModuleLayer::Inline => "inline",
        };
        write!(f, "{value}")
    }
}

impl fmt::Display for ModuleOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleOrigin::Local {
                collection_root,
                module_dir,
            } => write!(
                f,
                "local:{} ({})",
                collection_root.display(),
                module_dir.display()
            ),
            ModuleOrigin::Git {
                repo_url,
                ref_,
                commit,
                module_dir,
                ..
            } => write!(
                f,
                "git:{}@{} ({}) [{}]",
                repo_url,
                ref_,
                commit,
                module_dir.display()
            ),
            ModuleOrigin::Inline { config_path } => write!(f, "inline:{config_path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_specs() {
        let resolver = DefaultSourceResolver;

        assert_eq!(
            resolver.parse("local://services/jellyfin").unwrap(),
            SourceSpec::LocalPath {
                path: PathBuf::from("services/jellyfin")
            }
        );
        assert_eq!(resolver.parse("inline").unwrap(), SourceSpec::Inline);

        assert_eq!(
            resolver
                .parse("git::https://github.com/example/vmctl-modules//jellyfin?ref=v1")
                .unwrap(),
            SourceSpec::Git {
                repo_url: "https://github.com/example/vmctl-modules".to_string(),
                ref_: "v1".to_string(),
                subdir: Some("jellyfin".to_string()),
            }
        );

        assert_eq!(
            resolver
                .parse("https://github.com/example/vmctl-modules")
                .unwrap(),
            SourceSpec::Git {
                repo_url: "https://github.com/example/vmctl-modules".to_string(),
                ref_: "main".to_string(),
                subdir: None,
            }
        );
        assert_eq!(
            resolver.parse("./resources/media-stack").unwrap(),
            SourceSpec::LocalPath {
                path: PathBuf::from("./resources/media-stack")
            }
        );
        assert_eq!(
            resolver.parse("resources/media-stack").unwrap(),
            SourceSpec::LocalPath {
                path: PathBuf::from("resources/media-stack")
            }
        );
        assert_eq!(
            resolver
                .parse("git@github.com:example/private-modules.git")
                .unwrap(),
            SourceSpec::Git {
                repo_url: "git@github.com:example/private-modules.git".to_string(),
                ref_: "main".to_string(),
                subdir: None,
            }
        );
    }

    #[test]
    fn rejects_git_source_without_ref() {
        let resolver = DefaultSourceResolver;
        let error = resolver
            .parse("git::https://github.com/example/vmctl-modules//jellyfin")
            .unwrap_err();
        assert!(error.to_string().contains("?ref="));
    }

    #[test]
    fn rejects_unsafe_git_subdir() {
        let resolver = DefaultSourceResolver;
        let error = resolver
            .parse("git::https://github.com/example/vmctl-modules//../../x?ref=main")
            .unwrap_err();
        assert!(error.to_string().contains("may not include `..`"));
    }

    #[test]
    fn indexes_multi_module_collection() {
        let root = unique_temp_dir("module-index");
        fs::create_dir_all(root.join("services/jellyfin")).unwrap();
        fs::create_dir_all(root.join("resources/media-stack")).unwrap();
        fs::write(
            root.join("services/jellyfin/service.toml"),
            "name = \"jellyfin\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("resources/media-stack/resource.toml"),
            "name = \"media-stack\"\nkind = \"vm\"\n",
        )
        .unwrap();

        let indexer = FsModuleIndexer;
        let modules = indexer
            .index_collection(
                &root,
                &ModuleOrigin::Local {
                    collection_root: root.clone(),
                    module_dir: PathBuf::new(),
                },
            )
            .unwrap();

        assert_eq!(modules.len(), 2);
        assert!(modules
            .iter()
            .any(|module| { module.kind == ModuleKind::Service && module.name == "jellyfin" }));
        assert!(modules
            .iter()
            .any(|module| { module.kind == ModuleKind::Resource && module.name == "media-stack" }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_applies_precedence() {
        let mut builder = ModuleRegistryBuilder::default();
        builder
            .add_indexed(
                vec![IndexedModule {
                    kind: ModuleKind::Service,
                    name: "jellyfin".to_string(),
                    module_dir: PathBuf::from("remote/jellyfin"),
                    manifest_path: PathBuf::from("remote/jellyfin/service.toml"),
                    origin: ModuleOrigin::Git {
                        repo_url: "https://example/repo".to_string(),
                        ref_: "main".to_string(),
                        commit: "abc".to_string(),
                        checkout_root: PathBuf::from("remote"),
                        module_dir: PathBuf::from("remote/jellyfin"),
                    },
                    version: Some("1.0.0".to_string()),
                }],
                ModuleLayer::Remote,
            )
            .unwrap();
        builder
            .add_indexed(
                vec![IndexedModule {
                    kind: ModuleKind::Service,
                    name: "jellyfin".to_string(),
                    module_dir: PathBuf::from("local/jellyfin"),
                    manifest_path: PathBuf::from("local/jellyfin/service.toml"),
                    origin: ModuleOrigin::Local {
                        collection_root: PathBuf::from("local"),
                        module_dir: PathBuf::from("local/jellyfin"),
                    },
                    version: Some("1.0.1".to_string()),
                }],
                ModuleLayer::Local,
            )
            .unwrap();

        let registry = builder.build();
        let origin = &registry.services["jellyfin"].origin;
        assert!(matches!(origin, ModuleOrigin::Local { .. }));
    }

    #[test]
    fn repo_manager_dedup_and_offline() {
        let root = unique_temp_dir("repo-manager");
        let remote = root.join("remote.git");
        let cache = root.join("cache");
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();

        run_git(CommandOptions::new(
            "git",
            ["init", "--bare", remote.to_string_lossy().as_ref()],
        ))
        .unwrap();

        let seed = root.join("seed");
        run_git(CommandOptions::new(
            "git",
            ["init", seed.to_string_lossy().as_ref()],
        ))
        .unwrap();
        fs::write(seed.join("README.md"), "hello\n").unwrap();
        run_git(CommandOptions::new(
            "git",
            ["-C", seed.to_string_lossy().as_ref(), "add", "README.md"],
        ))
        .unwrap();
        run_git(CommandOptions::new(
            "git",
            [
                "-C",
                seed.to_string_lossy().as_ref(),
                "-c",
                "user.name=vmctl",
                "-c",
                "user.email=vmctl@example.com",
                "commit",
                "-m",
                "seed",
            ],
        ))
        .unwrap();
        run_git(CommandOptions::new(
            "git",
            [
                "-C",
                seed.to_string_lossy().as_ref(),
                "remote",
                "add",
                "origin",
                remote.to_string_lossy().as_ref(),
            ],
        ))
        .unwrap();
        run_git(CommandOptions::new(
            "git",
            [
                "-C",
                seed.to_string_lossy().as_ref(),
                "push",
                "origin",
                "HEAD:main",
            ],
        ))
        .unwrap();

        let manager = GitRepoManager::new(cache);
        let repo = RepoRef {
            repo_url: remote.to_string_lossy().to_string(),
            ref_: "main".to_string(),
        };

        let first = manager.ensure_repo(&repo, false).unwrap();
        let second = manager.ensure_repo(&repo, false).unwrap();
        assert_eq!(first.checkout_root, second.checkout_root);

        let offline = manager.ensure_repo(&repo, true).unwrap();
        assert_eq!(offline.commit, second.commit);

        fs::remove_dir_all(root).unwrap();
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "vmctl-modules-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir
    }
}
