use std::{env, fs, path::PathBuf};

use touchbar_package::{GithubSource, PluginManifest};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, Decision, FilesystemMountBinding, GrantRecord, GrantStore,
    ReusePolicy,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("write-filesystem-demo-grants: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let manifest_path = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: write_filesystem_demo_grants MANIFEST DIGEST OUTPUT DIRECTORY")?,
    );
    let digest = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or("digest must be UTF-8")?;
    let output = PathBuf::from(arguments.next().ok_or("missing output path")?);
    let selected = PathBuf::from(arguments.next().ok_or("missing selected directory")?);
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }
    let selected = selected
        .canonicalize()
        .map_err(|error| format!("resolve selected directory: {error}"))?;
    if !fs::metadata(&selected)
        .map_err(|error| format!("inspect selected directory: {error}"))?
        .is_dir()
    {
        return Err("selected filesystem grant root is not a directory".into());
    }
    let manifest = PluginManifest::from_toml(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("read {}: {error}", manifest_path.display()))?,
    )?;
    let expected = GithubSource::new("cameroncooper", "touchbar-filesystem-component-demo")?;
    let requests = CapabilityRegistry::default()
        .normalize(&manifest)
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })?;
    if manifest.plugin.source != expected
        || manifest.plugin.version != semver::Version::new(0, 1, 0)
        || requests.len() != 1
        || requests[0].capability != CapabilityId::FilesystemReadV1
    {
        return Err("refusing to grant anything except the fixed filesystem reference demo".into());
    }
    let request = requests.into_iter().next().unwrap();
    let mut grants = GrantStore::default();
    grants.insert(GrantRecord {
        source: manifest.plugin.source,
        capability: request.capability,
        approved_scope: request.scope,
        bindings: touchbar_policy::GrantBindings {
            filesystem_mounts: [(
                "gallery".into(),
                FilesystemMountBinding::from_directory(selected)?,
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        decision: Decision::Allow,
        reuse: ReusePolicy::ExactDigest,
        approved_version: manifest.plugin.version,
        approved_digest: digest,
    })?;
    grants.save(output)?;
    Ok(())
}
