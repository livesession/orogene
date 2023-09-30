use indexmap::IndexMap;
use kdl::{KdlDocument, KdlNode};
use nassun::{client::Nassun, package::Package, PackageResolution};
use node_semver::Version;
use oro_common::CorgiManifest;
use oro_package_spec::PackageSpec;
use serde::{Deserialize, Serialize};
use ssri::Integrity;
use unicase::UniCase;

use crate::{error::NodeMaintainerError, graph::DepType, IntoKdl};

/// A representation of a resolved lockfile.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub(crate) version: u64,
    pub(crate) root: LockfileNode,
    pub(crate) packages: IndexMap<UniCase<String>, LockfileNode>,
}

impl Lockfile {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn root(&self) -> &LockfileNode {
        &self.root
    }

    pub fn packages(&self) -> &IndexMap<UniCase<String>, LockfileNode> {
        &self.packages
    }

    pub fn to_kdl(&self) -> KdlDocument {
        let mut doc = KdlDocument::new();
        doc.set_leading(
            "// This file is automatically generated and not intended for manual editing.",
        );
        let mut version_node = KdlNode::new("lockfile-version");
        version_node.push(self.version as i64);
        doc.nodes_mut().push(version_node);
        doc.nodes_mut().push(self.root.to_kdl());
        let mut packages = self.packages.iter().collect::<Vec<_>>();
        packages.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (_, pkg) in packages {
            doc.nodes_mut().push(pkg.to_kdl());
        }
        doc.fmt();
        doc
    }

    pub fn from_kdl(kdl: impl IntoKdl) -> Result<Self, NodeMaintainerError> {
        let kdl: KdlDocument = kdl.into_kdl()?;
        fn inner(kdl: KdlDocument) -> Result<Lockfile, NodeMaintainerError> {
            let packages = kdl
                .nodes()
                .iter()
                .filter(|node| node.name().to_string() == "pkg")
                .map(|node| LockfileNode::from_kdl(node, false))
                .map(|node| {
                    let node = node?;
                    let path_str = node
                        .path
                        .iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join("/node_modules/");
                    Ok((UniCase::from(path_str), node))
                })
                .collect::<Result<IndexMap<UniCase<String>, LockfileNode>, NodeMaintainerError>>(
                )?;
            Ok(Lockfile {
                version: kdl
                    .get_arg("lockfile-version")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.try_into())
                    .transpose()
                    // TODO: add a miette span here
                    .map_err(|_| NodeMaintainerError::InvalidLockfileVersion)?
                    .unwrap_or(1),
                root: kdl
                    .get("root")
                    // TODO: add a miette span here
                    .ok_or_else(|| NodeMaintainerError::KdlLockMissingRoot(kdl.clone()))
                    .and_then(|node| LockfileNode::from_kdl(node, true))?,
                packages,
            })
        }
        inner(kdl)
    }

    pub fn from_npm(npm: impl AsRef<str>) -> Result<Self, NodeMaintainerError> {
        let pkglock: NpmPackageLock = serde_json::from_str(npm.as_ref())?;
        fn inner(npm: NpmPackageLock) -> Result<Lockfile, NodeMaintainerError> {
            let packages = npm
                .packages
                .iter()
                .map(|(path, entry)| LockfileNode::from_npm(path, entry))
                .map(|node| {
                    let node = node?;
                    let path_str = node
                        .path
                        .iter()
                        .map(|x| x.to_string())
                        .collect::<Vec<_>>()
                        .join("/node_modules/");
                    Ok((UniCase::from(path_str), node))
                })
                .collect::<Result<IndexMap<UniCase<String>, LockfileNode>, NodeMaintainerError>>(
                )?;
            Ok(Lockfile {
                version: npm
                    .lockfile_version
                    .map(|v| v.try_into())
                    .transpose()
                    // TODO: add a miette span here
                    .map_err(|_| NodeMaintainerError::InvalidLockfileVersion)?
                    .unwrap_or(3),
                root: npm
                    .packages
                    .get("")
                    .ok_or_else(|| NodeMaintainerError::NpmLockMissingRoot(npm.clone()))
                    .and_then(|node| LockfileNode::from_npm("", node))?,
                packages,
            })
        }
        inner(pkglock)
    }
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct LockfileNode {
    pub name: UniCase<String>,
    pub is_root: bool,
    pub path: Vec<UniCase<String>>,
    pub resolved: Option<String>,
    pub version: Option<Version>,
    pub integrity: Option<Integrity>,
    pub dependencies: IndexMap<String, String>,
    pub dev_dependencies: IndexMap<String, String>,
    pub peer_dependencies: IndexMap<String, String>,
    pub optional_dependencies: IndexMap<String, String>,
}

impl From<LockfileNode> for CorgiManifest {
    fn from(value: LockfileNode) -> Self {
        CorgiManifest {
            name: Some(value.name.to_string()),
            version: value.version,
            dependencies: value.dependencies,
            dev_dependencies: value.dev_dependencies,
            peer_dependencies: value.peer_dependencies,
            optional_dependencies: value.optional_dependencies,
            bundled_dependencies: Vec::new(),
        }
    }
}

impl LockfileNode {
    pub(crate) async fn to_package(
        &self,
        nassun: &Nassun,
    ) -> Result<Option<Package>, NodeMaintainerError> {
        let spec = match (self.resolved.as_ref(), self.version.as_ref()) {
            (Some(resolved), Some(version)) if resolved.starts_with("http") => {
                format!("{}@{version}", self.name)
            }
            (Some(resolved), _) => format!("{}@{resolved}", self.name),
            (_, Some(version)) => format!("{}@{version}", self.name),
            _ => {
                // Nothing we can do here, we don't have enough information to resolve the package.
                return Ok(None);
            }
        };
        let spec: PackageSpec = spec.parse()?;
        let package = match &spec.target() {
            PackageSpec::Dir { path } => {
                let resolution = PackageResolution::Dir {
                    name: self.name.to_string(),
                    path: path.clone(),
                };
                nassun.resolve_from(self.name.to_string(), spec, resolution)
            }
            PackageSpec::Npm { name, .. } => {
                let version = if let Some(ref version) = self.version {
                    version
                } else {
                    return Err(NodeMaintainerError::MissingVersion);
                };
                if let Some(ref url) = self.resolved {
                    let resolution = PackageResolution::Npm {
                        name: name.clone(),
                        version: version.clone(),
                        tarball: url
                            .parse()
                            .map_err(|e| NodeMaintainerError::UrlParseError(url.clone(), e))?,
                        integrity: self.integrity.clone(),
                    };
                    nassun.resolve_from(self.name.to_string(), spec, resolution)
                } else {
                    nassun.resolve(spec.to_string()).await?
                }
            }
            PackageSpec::Git(info) => {
                if info.committish().is_some() {
                    let resolution = PackageResolution::Git {
                        name: self.name.to_string(),
                        info: info.clone(),
                    };
                    nassun.resolve_from(self.name.to_string(), spec, resolution)
                } else {
                    nassun.resolve(spec.to_string()).await?
                }
            }
            PackageSpec::Alias { .. } => {
                unreachable!("Alias should have already been resolved by the .target() call above.")
            }
        };
        Ok(Some(package))
    }

    fn from_kdl(node: &KdlNode, is_root: bool) -> Result<Self, NodeMaintainerError> {
        let children = node.children().cloned().unwrap_or_else(KdlDocument::new);
        let path = node
            .entries()
            .iter()
            .filter(|e| e.value().is_string() && e.name().is_none())
            .map(|e| {
                UniCase::new(
                    e.value()
                        .as_string()
                        .expect("We already checked that it's a string, above.")
                        .into(),
                )
            })
            .collect::<Vec<_>>();
        let name = if is_root {
            UniCase::new("".into())
        } else {
            path.last()
                .cloned()
                // TODO: add a miette span here
                .ok_or_else(|| NodeMaintainerError::KdlLockMissingName(node.clone()))?
        };
        let integrity = children
            .get_arg("integrity")
            .and_then(|i| i.as_string())
            .map(|i| i.parse())
            .transpose()
            .map_err(|e| NodeMaintainerError::KdlLockfileIntegrityParseError(node.clone(), e))?;
        let version = children
            .get_arg("version")
            .and_then(|val| val.as_string())
            .map(|val| {
                val.parse()
                    // TODO: add a miette span here
                    .map_err(NodeMaintainerError::SemverParseError)
            })
            .transpose()?;
        let resolved = children
            .get_arg("resolved")
            .and_then(|resolved| resolved.as_string())
            .map(|resolved| resolved.to_string());
        Ok(Self {
            name,
            is_root,
            path,
            integrity,
            resolved,
            version,
            dependencies: Self::from_kdl_deps(&children, &DepType::Prod)?,
            dev_dependencies: Self::from_kdl_deps(&children, &DepType::Dev)?,
            optional_dependencies: Self::from_kdl_deps(&children, &DepType::Opt)?,
            peer_dependencies: Self::from_kdl_deps(&children, &DepType::Peer)?,
        })
    }

    fn from_kdl_deps(
        children: &KdlDocument,
        dep_type: &DepType,
    ) -> Result<IndexMap<String, String>, NodeMaintainerError> {
        use DepType::*;
        let type_name = match dep_type {
            Prod => "dependencies",
            Dev => "dev-dependencies",
            Peer => "peer-dependencies",
            Opt => "optional-dependencies",
        };
        let mut deps = IndexMap::new();
        if let Some(node) = children.get(type_name) {
            if let Some(children) = node.children() {
                for dep in children.nodes() {
                    let name = dep.name().value().to_string();
                    let spec = dep.get(0).and_then(|spec| spec.as_string()).unwrap_or("*");
                    deps.insert(name.clone(), spec.into());
                }
            }
        }
        Ok(deps)
    }

    fn to_kdl(&self) -> KdlNode {
        let mut kdl_node = if self.is_root {
            KdlNode::new("root")
        } else {
            KdlNode::new("pkg")
        };
        for name in &self.path {
            kdl_node.push(name.as_ref());
        }
        if let Some(ref version) = self.version {
            let mut vnode = KdlNode::new("version");
            vnode.push(version.to_string());
            kdl_node.ensure_children().nodes_mut().push(vnode);
        }
        if let Some(resolved) = &self.resolved {
            if !self.is_root {
                let mut rnode = KdlNode::new("resolved");
                rnode.push(resolved.to_string());
                kdl_node.ensure_children().nodes_mut().push(rnode);

                if let Some(integrity) = &self.integrity {
                    let mut inode = KdlNode::new("integrity");
                    inode.push(integrity.to_string());
                    kdl_node.ensure_children().nodes_mut().push(inode);
                }
            }
        }
        if !self.dependencies.is_empty() {
            kdl_node
                .ensure_children()
                .nodes_mut()
                .push(self.to_kdl_deps(&DepType::Prod, &self.dependencies));
        }
        if !self.dev_dependencies.is_empty() {
            kdl_node
                .ensure_children()
                .nodes_mut()
                .push(self.to_kdl_deps(&DepType::Dev, &self.dev_dependencies));
        }
        if !self.peer_dependencies.is_empty() {
            kdl_node
                .ensure_children()
                .nodes_mut()
                .push(self.to_kdl_deps(&DepType::Peer, &self.peer_dependencies));
        }
        if !self.optional_dependencies.is_empty() {
            kdl_node
                .ensure_children()
                .nodes_mut()
                .push(self.to_kdl_deps(&DepType::Opt, &self.optional_dependencies));
        }
        kdl_node
    }

    fn to_kdl_deps(&self, dep_type: &DepType, deps: &IndexMap<String, String>) -> KdlNode {
        use DepType::*;
        let type_name = match dep_type {
            Prod => "dependencies",
            Dev => "dev-dependencies",
            Peer => "peer-dependencies",
            Opt => "optional-dependencies",
        };
        let mut deps_node = KdlNode::new(type_name);
        for (name, requested) in deps {
            let children = deps_node.ensure_children();
            let mut ddnode = KdlNode::new(name.clone());
            ddnode.push(requested.clone());
            children.nodes_mut().push(ddnode);
        }
        deps_node
            .ensure_children()
            .nodes_mut()
            .sort_by_key(|n| n.name().value().to_string());
        deps_node
    }

    fn from_npm(path_str: &str, npm: &NpmPackageLockEntry) -> Result<Self, NodeMaintainerError> {
        let mut path = "/".to_string();
        path.push_str(path_str);
        let path = path
            .split("/node_modules/")
            .skip(1)
            .map(|s| s.into())
            .collect::<Vec<_>>();
        let name = if path_str.is_empty() {
            UniCase::new("".into())
        } else {
            npm.name
                .clone()
                .map(UniCase::new)
                .or_else(|| path.last().cloned())
                .ok_or_else(|| NodeMaintainerError::NpmLockMissingName(Box::new(npm.clone())))?
        };
        let integrity = npm
            .integrity
            .as_ref()
            .map(|i| i.parse())
            .transpose()
            .map_err(|e| {
                NodeMaintainerError::NpmLockfileIntegrityParseError(Box::new(npm.clone()), e)
            })?;
        let version = npm
            .version
            .as_ref()
            .map(|val| val.parse().map_err(NodeMaintainerError::SemverParseError))
            .transpose()?;
        Ok(Self {
            name,
            is_root: path.is_empty(),
            path,
            integrity,
            resolved: npm.resolved.clone(),
            version,
            dependencies: npm.dependencies.clone(),
            dev_dependencies: npm.dev_dependencies.clone(),
            optional_dependencies: npm.optional_dependencies.clone(),
            peer_dependencies: npm.peer_dependencies.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmPackageLock {
    #[serde(default)]
    pub lockfile_version: Option<usize>,
    #[serde(default)]
    pub requires: bool,
    #[serde(default)]
    pub packages: IndexMap<String, NpmPackageLockEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NpmPackageLockEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub resolved: Option<String>,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub dependencies: IndexMap<String, String>,
    #[serde(default)]
    pub dev_dependencies: IndexMap<String, String>,
    #[serde(default)]
    pub optional_dependencies: IndexMap<String, String>,
    #[serde(default)]
    pub peer_dependencies: IndexMap<String, String>,
}
