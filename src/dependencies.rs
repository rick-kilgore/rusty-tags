use std::path::PathBuf;

use serde_json;
use fnv::FnvHashMap;

use rt_result::RtResult;
use types::{DepTree, Source, SourceVersion, SourceId};
use config::Config;

type JsonValue = serde_json::Value;
type JsonObject = serde_json::Map<String, JsonValue>;

/// Returns the dependency tree of the whole cargo workspace.
pub fn dependency_tree(config: &Config, metadata: &JsonValue) -> RtResult<DepTree> {
    let mut dep_tree = DepTree::new();
    let packages = packages(config, metadata, &mut dep_tree)?;

    build_dep_tree(config, metadata, &packages, &mut dep_tree)?;
    dep_tree.compute_depths();

    Ok(dep_tree)
}

fn workspace_members(metadata: &JsonValue) -> RtResult<Vec<SourceVersion>> {
    let members = as_array_from_value("workspace_members", metadata)?;
    let mut source_versions = Vec::with_capacity(members.len() * 2);
    for member in members {
        let member_str = member.as_str()
            .ok_or(format!("Expected 'workspace_members' of type string but found: {}", to_string_pretty(member)))?;

        source_versions.push(SourceVersion::parse_from_id(member_str.to_owned())?)
    }

    Ok(source_versions)
}

#[derive(Debug)]
struct Package {
    pub source_id: SourceId,
    pub source_paths: Vec<PathBuf>,
}

type Packages<'a> = FnvHashMap<SourceVersion, Package>;

fn packages<'a>(config: &Config,
                metadata: &'a JsonValue,
                dep_tree: &mut DepTree)
                -> RtResult<Packages<'a>> {
    let packages = as_array_from_value("packages", metadata)?;
    dep_tree.reserve_num_sources(packages.len());
    let mut package_map: Packages = FnvHashMap::default();
    for package in packages {
        let id = as_str_from_value("id", package)?;

        for dotarget in [ false, true ] {
            let source_version = SourceVersion::parse_from_id(id.to_owned())?;
            let source_path = {
                let path = source_path(config, package, dotarget)?;
                if path == None {
                    continue;
                }

                path.unwrap()
            };

            verbose!(config, "Found source {} for package {}", source_path.display(), source_version);

            package_map.entry(source_version).or_insert({
              let source_id = dep_tree.new_source();
              let source_paths = vec![];
              Package { source_id, source_paths }
            }).source_paths.push(source_path);
        }
    }

    Ok(package_map)
}

fn build_dep_tree(config: &Config,
                  metadata: &JsonValue,
                  packages: &Packages,
                  dep_tree: &mut DepTree)
                  -> RtResult<()> {
    let root_ids = {
        let workspace_members = workspace_members(metadata)?;
        verbose!(config, "Found workspace members: {:?}", workspace_members);

        let mut ids = Vec::with_capacity(workspace_members.len());
        for member in &workspace_members {
            let member_package = package(member, packages)?;
            ids.push(member_package.source_id);
            if config.omit_deps {
                let is_root = true;
                let source = Source::new(member_package.source_id, member,
                                         member_package.source_paths.to_owned(),
                                         is_root, config)?;
                dep_tree.set_source(source, vec![]);
            }
        }

        ids
    };

    dep_tree.set_roots(root_ids.clone());
    if config.omit_deps {
        return Ok(());
    }

    let nodes = {
        let resolve = as_object_from_value("resolve", metadata)?;
        as_array_from_object("nodes", resolve)?
    };

    for node in nodes {
        let node_version = {
            let id = as_str_from_value("id", node)?;
            SourceVersion::parse_from_id(id.to_owned())?
        };

        let node_package = package(&node_version, packages)?;

        let dep_ids = {
            let dependencies = as_array_from_value("dependencies", node)?;

            let dep_versions = {
                let mut vers = Vec::with_capacity(dependencies.len());
                for dep in dependencies {
                    let id = dep.as_str()
                        .ok_or(format!("Couldn't find string in dependency:\n{}", to_string_pretty(dep)))?;

                    vers.push(SourceVersion::parse_from_id(id.to_owned())?);
                }

                vers
            };

            if ! dep_versions.is_empty() {
                verbose!(config, "Found dependencies of {}: {:?}", node_version, dep_versions);
            }

            let mut ids = Vec::with_capacity(dep_versions.len());
            for version in &dep_versions {
                ids.push(package(version, packages)?.source_id);
            }

            ids
        };

        verbose!(config, "Building tree for {}", node_version);

        let is_root = root_ids.iter().find(|id| **id == node_package.source_id) != None;
        let source = Source::new(node_package.source_id, &node_version, node_package.source_paths.to_owned(), is_root, config)?;
        dep_tree.set_source(source, dep_ids);
    }

    Ok(())
}

fn package<'a>(source_version: &SourceVersion, packages: &'a Packages) -> RtResult<&'a Package> {
    packages.get(&source_version)
        .ok_or(format!("Couldn't find package for {}", source_version).into())
}

fn source_path<'a>(config: &Config, package: &'a JsonValue, dotarget: bool) -> RtResult<Option<PathBuf>> {
    let targets = as_array_from_value("targets", package)?;

    let manifest_dir = {
        let manifest_path = as_str_from_value("manifest_path", package).map(PathBuf::from)?;

        manifest_path.parent()
            .ok_or(format!("Couldn't get directory of path '{:?}'", manifest_path.display()))?.to_path_buf()
    };

    for target in targets {
        let kinds = as_array_from_value("kind", target)?;

        for kind in kinds {
            let kind_str = kind.as_str()
                .ok_or(format!("Expected 'kind' of type string but found: {}", to_string_pretty(kind)))?;

            if kind_str != "bin" && ! kind_str.contains("lib") && kind_str != "proc-macro" && kind_str != "test" {
                verbose!(config, "Unsupported target kind: {}", kind_str);
                continue;
            }

            let mut src_path = as_str_from_value("src_path", target).map(PathBuf::from)?;
            if src_path.is_absolute() && src_path.is_file() {
                src_path = src_path.parent()
                    .ok_or(format!("Couldn't get directory of path '{:?}' in target:\n{}\nof package:\n{}",
                                   src_path.display(), to_string_pretty(target), to_string_pretty(package)))?.to_path_buf();
                if dotarget {
                    let pkg_path = src_path.parent()
                        .ok_or(format!("Couldn't get package directory of path '{:?}' in target:\n{}\nof package:\n{}",
                                       src_path.display(), to_string_pretty(target), to_string_pretty(package)))?.to_path_buf();
                    src_path = pkg_path.join("target");
                }
            }

            if src_path.is_relative() {
                src_path = manifest_dir;
            }

            if ! src_path.is_dir() {
                return if dotarget { Ok(None) } else { Err(format!("Invalid source path directory '{:?}'", src_path.display()).into()) }
            }

            return Ok(Some(src_path));
        }
    }

    Ok(None)
}

fn to_string_pretty(value: &JsonValue) -> String {
    serde_json::to_string_pretty(value).unwrap_or(String::new())
}

fn as_array_from_value<'a>(entry: &str, value: &'a JsonValue) -> RtResult<&'a Vec<JsonValue>> {
    value.get(entry)
         .and_then(JsonValue::as_array)
         .ok_or(format!("Couldn't find array entry '{}' in:\n{}", entry, to_string_pretty(value)).into())
}

fn as_str_from_value<'a>(entry: &str, value: &'a JsonValue) -> RtResult<&'a str> {
    value.get(entry)
         .and_then(JsonValue::as_str)
         .ok_or(format!("Couldn't find string entry '{}' in:\n{}", entry, to_string_pretty(value)).into())
}

fn as_object_from_value<'a>(entry: &str, value: &'a JsonValue) -> RtResult<&'a JsonObject> {
    value.get(entry)
         .and_then(JsonValue::as_object)
         .ok_or(format!("Couldn't find object entry '{}' in:\n{}", entry, to_string_pretty(value)).into())
}

fn as_array_from_object<'a>(entry: &str, object: &'a JsonObject) -> RtResult<&'a Vec<JsonValue>> {
    object.get(entry)
          .and_then(JsonValue::as_array)
          .ok_or(format!("Couldn't find array entry '{}' in:\n{:?}", entry, object).into())
}
