use std::{env, fs, path::PathBuf, process::ExitCode};

use touchbar_package::PluginManifest;
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, Decision, GrantRecord, GrantStore, ReusePolicy,
};

const USAGE: &str =
    "usage: write_broker_demo_grants MANIFEST SHA256_DIGEST OUTPUT_PERMISSIONS_TOML";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("write-broker-demo-grants: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let manifest_path = PathBuf::from(arguments.next().ok_or(USAGE)?);
    let digest = arguments
        .next()
        .ok_or(USAGE)?
        .into_string()
        .map_err(|_| "digest must be UTF-8")?;
    let output = PathBuf::from(arguments.next().ok_or(USAGE)?);
    if arguments.next().is_some() {
        return Err(USAGE.into());
    }

    let manifest = PluginManifest::from_toml(&fs::read_to_string(manifest_path)?)?;
    let requests = CapabilityRegistry::default()
        .normalize(&manifest)
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })?;
    if manifest.plugin.source.to_string() != "github:cameroncooper/touchbar-broker-component-demo"
        || manifest.plugin.version != semver::Version::new(0, 1, 0)
        || requests.len() != 2
        || !requests
            .iter()
            .any(|request| request.capability == CapabilityId::DbusCallV1)
        || !requests
            .iter()
            .any(|request| request.capability == CapabilityId::DbusSubscribeV1)
    {
        return Err("refusing to grant anything except the fixed broker reference demo".into());
    }
    let mut grants = GrantStore::default();
    for request in requests {
        grants.insert(GrantRecord {
            source: manifest.plugin.source.clone(),
            capability: request.capability,
            approved_scope: request.scope,
            bindings: Default::default(),
            decision: Decision::Allow,
            reuse: ReusePolicy::ExactDigest,
            approved_version: manifest.plugin.version.clone(),
            approved_digest: digest.clone(),
        })?;
    }
    grants.save(output)?;
    Ok(())
}
