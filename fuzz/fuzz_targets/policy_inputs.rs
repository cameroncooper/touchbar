#![no_main]

use libfuzzer_sys::fuzz_target;
use touchbar_package::PluginManifest;
use touchbar_policy::{CapabilityRegistry, EffectivePolicy, GrantStore};

fuzz_target!(|bytes: &[u8]| {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    if let Ok(manifest) = PluginManifest::from_toml(text) {
        let _ = CapabilityRegistry::default().normalize(&manifest);
    }
    let _ = GrantStore::from_toml(text);
    let _ = toml::from_str::<EffectivePolicy>(text);
});
