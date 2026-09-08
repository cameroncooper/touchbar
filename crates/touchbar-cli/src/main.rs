use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Serialize;
use serde_json::json;
use touchbar_catalog::{Catalog, CatalogEntry, CatalogState, CatalogTier};
use touchbar_package::{
    GithubSource, MAX_TOUCHBAR_WIDTH, PluginManifest, PresentationBar, PresentationBarElement,
    PresentationGroupElement, REFERENCE_TOUCHBAR_WIDTH, RuntimeSpec,
};
use touchbar_plugin_store::{
    InstalledOrigin, InstalledRuntime, PluginStore, ReleaseInstall, StorePaths, inspect_package,
    pack_directory,
};
use touchbar_policy::{
    CapabilityId, CapabilityRegistry, ClipboardBinding, Decision, FilesystemMountBinding,
    GrantBindings, GrantRecord, GrantStore, LocalEndpointBinding, PackageInstance,
    PermissionChangeKind, Provenance, ReusePolicy, RuntimeKind, SecretBinding, SessionGrants,
    calculate_effective_policy,
};
use url::Url;

mod github;

const CORE_REPOSITORY: &str = "https://github.com/cameroncooper/touchbar";
const CATALOG_REPOSITORY: &str = "https://github.com/cameroncooper/touchbar-plugins";
const DEFAULT_HARDWARE_SOCKET: &str = "/run/touchbar/hardware.sock";
const HARDWARE_RECOVERY_MARKER: &str = "/run/touchbar/recovery-fallback";
const TOOLCHAIN_TAG: &str = "v0.1.0";
static DEV_INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = r#"usage: touchbarctl plugin COMMAND [OPTIONS]
       touchbarctl session status|reload [--format text|json]
       touchbarctl session profile list|select NAME|automatic [--format text|json]
       touchbarctl hardware status [--format text|json]

  new NAME [--source github:OWNER/REPO] [--directory DIR] [--sdk PATH]
  build [--package DIR]
  context [--format markdown|json]
  check [--package DIR] [--format text|json]
  test [--package DIR] [--host PATH] [--format text|json]
  replay --scenario FILE [--screenshots DIR] [--package DIR] [--host PATH]
  dev [--package DIR] [--item ID] [--width PX] [--scale 1|2|4]
      [--host PATH] [--supervisor PATH] [--sessiond PATH]
  run [--package DIR] [--item ID] [--width PX]
      [--when-application CLASS] [--sandboxed]
      [--host PATH] [--supervisor PATH] [--sessiond PATH]
      [--hardware-socket PATH]
  pack [--package DIR] [--output FILE]
  release-check --tag TAG [--repository OWNER/REPO] [--package DIR]
  publish --tag TAG --repository OWNER/REPO [--package DIR]
  add (--path PACKAGE_OR_ARCHIVE | SOURCE_OR_ALIAS [--version VERSION])
  update SOURCE [--version VERSION]
  rollback SOURCE [--version VERSION]
  search [QUERY] [--category CATEGORY] [--format text|json]
  catalog-check [--catalog FILE] [--previous FILE] [--online] [--host PATH] [--format text|json]
  submit --alias ALIAS --categories CATEGORY,... [--package DIR] [--format toml|json]
  list [--format text|json]
  inspect SOURCE [--format text|json]
  permissions SOURCE [--format text|json]
  permission SOURCE CAPABILITY allow|deny|reset (--session|--persistent)
             [--reuse source|digest] [--format text|json]
             [--bind LABEL=DIR] [--endpoint LABEL=SOCKET]
             [--secret NAME=OBJECT_PATH] [--clipboard-socket SOCKET]
  enable SOURCE | disable SOURCE | remove SOURCE
  item SOURCE ITEM enable|disable|width [PX]
  profile SOURCE PROFILE enable|disable

TOUCHBAR_HOME overrides the local store root."#;

fn main() {
    if let Err(error) = run() {
        eprintln!("touchbarctl: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|v| v == "--help" || v == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let Some((scope, rest)) = args.split_first() else {
        bail!(USAGE)
    };
    if scope == "session" {
        return session(rest);
    }
    if scope == "hardware" {
        return hardware(rest);
    }
    if scope != "plugin" {
        bail!("unknown command group `{scope}`\n\n{USAGE}")
    }
    let Some((command, values)) = rest.split_first() else {
        bail!(USAGE)
    };
    match command.as_str() {
        "new" => new_plugin(values),
        "build" => build(values),
        "context" => context(values),
        "check" => check(values),
        "test" => test(values),
        "replay" => replay(values),
        "dev" => dev(values),
        "run" => run_on_hardware(values),
        "pack" => pack(values),
        "release-check" => release_check(values),
        "publish" => publish(values),
        "add" => add(values),
        "update" => update(values),
        "rollback" => rollback(values),
        "search" => search_catalog(values),
        "catalog-check" => catalog_check(values),
        "submit" => submit(values),
        "list" => list(values),
        "inspect" => inspect(values),
        "permissions" => permissions(values),
        "permission" => permission(values),
        "enable" => set_enabled(values, true),
        "disable" => set_enabled(values, false),
        "item" => item(values),
        "profile" => profile(values),
        "remove" => remove(values),
        other => bail!("unknown plugin command `{other}`\n\n{USAGE}"),
    }
}

fn new_plugin(args: &[String]) -> Result<()> {
    reject(args, &["--source", "--directory", "--sdk"], 1)?;
    let name = positional(args, 0).context("new requires NAME")?;
    validate_name(name)?;
    let source = opt(args, "--source")
        .map(source)
        .transpose()?
        .unwrap_or_else(|| GithubSource::new("local", name).expect("valid local source"));
    let dir = opt(args, "--directory")
        .map(PathBuf::from)
        .unwrap_or_else(|| name.into());
    if dir.exists() {
        bail!("{} already exists", dir.display())
    }
    fs::create_dir_all(dir.join("src"))?;
    fs::create_dir_all(dir.join("component"))?;
    fs::create_dir_all(dir.join("tests"))?;
    fs::create_dir_all(dir.join(".github/workflows"))?;
    let sdk_dependency = match opt(args, "--sdk") {
        Some(path) => {
            let sdk = Path::new(path)
                .canonicalize()
                .with_context(|| format!("open SDK path {path}"))?;
            format!("{{ path = {:?} }}", sdk.to_string_lossy())
        }
        None => format!("{{ git = \"{CORE_REPOSITORY}\", tag = \"{TOOLCHAIN_TAG}\" }}"),
    };
    let cargo = format!(
        "[package]\nname = \"touchbar-{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT OR Apache-2.0\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\ntouchbar-component-sdk = {sdk_dependency}\n\n[workspace]\n"
    );
    let manifest = format!(
        "manifest_version = 1\n\n[plugin]\nname = \"{}\"\nversion = \"0.1.0\"\ndescription = \"A TouchBar plugin\"\nlicense = \"MIT OR Apache-2.0\"\nsource = \"{}\"\napi = \"^1.0\"\n\n[runtime]\nkind = \"component\"\nentrypoint = \"component/plugin.wasm\"\nworld = \"touchbar:plugin/plugin@1.0.0\"\n\n[[items]]\nid = \"main\"\nlabel = \"{}\"\nexpanded_bar = \"main-actions\"\npress_and_hold_bar = \"main-actions\"\n\n[[items]]\nid = \"actions\"\nlabel = \"Actions\"\n\n[[bar]]\nid = \"main-actions\"\nminimum_width = 248\npreferred_width = 360\nmaximum_width = 640\ndismiss_on_selection = true\nprincipal_item = \"main\"\n\n[[bar.element]]\nkind = \"item\"\nitem = \"main\"\nminimum_width = 120\npreferred_width = 240\nmaximum_width = 480\n\n[[bar.element]]\nkind = \"fixed-space\"\nwidth = 8\n\n[[bar.element]]\nkind = \"item\"\nitem = \"actions\"\nminimum_width = 96\npreferred_width = 112\nmaximum_width = 152\n",
        title(name),
        source,
        title(name)
    );
    write_new(&dir.join("Cargo.toml"), cargo.as_bytes())?;
    write_new(&dir.join("touchbar-plugin.toml"), manifest.as_bytes())?;
    write_new(
        &dir.join("src/lib.rs"),
        SCAFFOLD.replace("__LABEL__", &title(name)).as_bytes(),
    )?;
    write_new(
        &dir.join("tests/interaction.json"),
        REPLAY_SCENARIO.as_bytes(),
    )?;
    write_new(
        &dir.join("README.md"),
        format!(
            "# {}\n\nGenerated with `touchbarctl plugin new`.\n",
            title(name)
        )
        .as_bytes(),
    )?;
    write_new(
        &dir.join(".gitignore"),
        b"/target\n/component/*.wasm\n/*.touchbar\n",
    )?;
    write_new(&dir.join("AGENTS.md"), AGENT_GUIDE.as_bytes())?;
    write_new(
        &dir.join(".github/workflows/check.yml"),
        CHECK_WORKFLOW.as_bytes(),
    )?;
    write_new(
        &dir.join(".github/workflows/release.yml"),
        RELEASE_WORKFLOW.as_bytes(),
    )?;
    println!(
        "Created {source}\n\n  cd {}\n  touchbarctl plugin build\n  touchbarctl plugin test --format json\n  touchbarctl plugin replay --scenario tests/interaction.json\n  touchbarctl plugin dev",
        dir.display()
    );
    Ok(())
}

fn build(args: &[String]) -> Result<()> {
    reject(args, &["--package"], 0)?;
    let root = package_root(args)?;
    build_component(&root)
}

fn build_component(root: &Path) -> Result<()> {
    let cargo_path = root.join("Cargo.toml");
    let toolchain = ComponentToolchain::resolve(root)?;
    let status = toolchain
        .cargo_command()
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip2",
            "--manifest-path",
        ])
        .arg(&cargo_path)
        .status()
        .context("run cargo build")?;
    if !status.success() {
        bail!("component build failed")
    }
    let manifest = read_manifest(&root)?;
    let RuntimeSpec::Component { entrypoint, .. } = &manifest.runtime else {
        bail!("native plugins own their build process")
    };
    let cargo: toml::Value = toml::from_str(&fs::read_to_string(&cargo_path)?)?;
    let crate_name = cargo
        .get("package")
        .and_then(|v| v.get("name"))
        .and_then(toml::Value::as_str)
        .context("Cargo.toml package.name is required")?
        .replace('-', "_");
    let metadata = toolchain
        .cargo_command()
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&cargo_path)
        .output()
        .context("read cargo metadata")?;
    if !metadata.status.success() {
        bail!("cargo metadata failed")
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let target_directory = metadata
        .get("target_directory")
        .and_then(serde_json::Value::as_str)
        .context("cargo metadata omitted target_directory")?;
    let built = Path::new(target_directory)
        .join("wasm32-wasip2/release")
        .join(format!("{crate_name}.wasm"));
    let target = root.join(entrypoint);
    fs::create_dir_all(target.parent().context("invalid component entrypoint")?)?;
    fs::copy(&built, &target)
        .with_context(|| format!("copy {} to {}", built.display(), target.display()))?;
    let package = inspect_package(&root)?;
    println!("Built {}\n{}", target.display(), package.package_digest);
    Ok(())
}

struct ComponentToolchain {
    cargo: PathBuf,
    rustc: PathBuf,
}

impl ComponentToolchain {
    fn resolve(root: &Path) -> Result<Self> {
        match (env::var_os("CARGO"), env::var_os("RUSTC")) {
            (Some(cargo), Some(rustc)) => {
                let toolchain = Self {
                    cargo: PathBuf::from(cargo),
                    rustc: PathBuf::from(rustc),
                };
                toolchain.ensure_wasi_target()?;
                return Ok(toolchain);
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("component builds require CARGO and RUSTC to be set together")
            }
            (None, None) => {}
        }
        if let Some(toolchain) = Self::from_rustup(root)? {
            toolchain.ensure_wasi_target()?;
            return Ok(toolchain);
        }

        let toolchain = Self {
            cargo: PathBuf::from("cargo"),
            rustc: PathBuf::from("rustc"),
        };
        toolchain.ensure_wasi_target()?;
        Ok(toolchain)
    }

    fn from_rustup(root: &Path) -> Result<Option<Self>> {
        let mut candidates = vec![PathBuf::from("rustup")];
        if let Some(home) = env::var_os("HOME") {
            let user_rustup = PathBuf::from(home).join(".cargo/bin/rustup");
            if user_rustup.is_file() {
                candidates.push(user_rustup);
            }
        }
        for rustup in candidates {
            let cargo = match rustup_which(&rustup, root, "cargo") {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("run {}", rustup.display()));
                }
            };
            let rustc = rustup_which(&rustup, root, "rustc")
                .with_context(|| format!("select rustc through {}", rustup.display()))?;
            return Ok(Some(Self { cargo, rustc }));
        }
        Ok(None)
    }

    fn ensure_wasi_target(&self) -> Result<()> {
        let output = Command::new(&self.rustc)
            .args(["--print", "target-libdir", "--target", "wasm32-wasip2"])
            .output()
            .with_context(|| format!("inspect Rust compiler {}", self.rustc.display()))?;
        let target_libdir = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success()
            || target_libdir.is_empty()
            || !Path::new(&target_libdir).is_dir()
        {
            bail!(
                "Rust compiler {} does not have the wasm32-wasip2 standard library; run `rustup target add wasm32-wasip2`, or set CARGO and RUSTC to a matched toolchain",
                self.rustc.display()
            );
        }
        Ok(())
    }

    fn cargo_command(&self) -> Command {
        let mut command = Command::new(&self.cargo);
        // Cargo otherwise searches PATH for rustc. An Arch cargo paired with a
        // rustup rustc (or the reverse) can silently select the wrong sysroot.
        command.env("RUSTC", &self.rustc);
        command
    }
}

fn rustup_which(rustup: &Path, root: &Path, binary: &str) -> io::Result<PathBuf> {
    let output = Command::new(rustup)
        .args(["which", binary])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "rustup could not select {binary}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "rustup selected missing {binary} executable {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn context(args: &[String]) -> Result<()> {
    reject(args, &["--format"], 0)?;
    match output_format(args)? {
        Format::Text => print!("{}", include_str!("../../../docs/plugin-author-context.md")),
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "manifest_version": 1, "host_api": touchbar_package::SUPPORTED_HOST_API_VERSION, "component_world": touchbar_package::SUPPORTED_COMPONENT_WORLD,
                "commands": ["new", "build", "context", "check", "test", "replay", "dev", "run", "pack", "release-check", "publish", "add", "update", "rollback", "search", "catalog-check", "submit", "list", "inspect", "permissions", "permission", "enable", "disable", "item", "profile", "remove"],
                "principles": ["stable item ids", "responsive rendering through 2008 pixels", "package-local automatic profiles", "package-local presentation bars", "presentation width matrices", "theme roles", "sealed logical assets", "semantic image tint", "stable animation ids", "host-timed animations", "bounded validated GPU effects", "brokered capabilities", "scope-checked offline broker fixtures", "headless tests"]
            }))?
        ),
    }
    Ok(())
}

fn check(args: &[String]) -> Result<()> {
    reject(args, &["--package", "--format"], 0)?;
    let package = inspect_package(&package_root(args)?)?;
    match output_format(args)? {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true, "source": package.manifest.plugin.source, "version": package.manifest.plugin.version,
                "runtime": if matches!(&package.manifest.runtime, RuntimeSpec::Component { .. }) { "component" } else { "native" },
                "items": package.manifest.items, "profiles": package.manifest.profiles, "permissions": package.requests,
                "package_digest": package.package_digest, "artifacts": package.artifacts
            }))?
        ),
        Format::Text => {
            println!(
                "OK  {} {}",
                package.manifest.plugin.source, package.manifest.plugin.version
            );
            println!(
                "    {} item(s), {} automatic profile(s), {} permission request(s)",
                package.manifest.items.len(),
                package.manifest.profiles.len(),
                package.requests.len()
            );
            println!("    {}", package.package_digest);
        }
    }
    Ok(())
}

fn test(args: &[String]) -> Result<()> {
    reject(args, &["--package", "--host", "--format"], 0)?;
    let root = package_root(args)?;
    let package = inspect_package(&root)?;
    let host = binary(
        args,
        "--host",
        "TOUCHBAR_PLUGIN_HOST",
        "touchbar-plugin-host",
    )?;
    let items = test_component_package(&root, &package, &host)?;
    let render_cases = items.iter().map(|item| item.widths.len()).sum::<usize>();
    let report = PluginTestReport {
        report_version: 1,
        ok: true,
        source: package.manifest.plugin.source.to_string(),
        plugin_version: package.manifest.plugin.version.to_string(),
        runtime: "component",
        items,
        presentation_bars: &package.manifest.bars,
        render_cases,
    };
    match output_format(args)? {
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        Format::Text => {
            println!("PASS  {} {}", report.source, report.plugin_version);
            println!(
                "    {} item(s), {} presentation bar(s), {} render case(s)",
                report.items.len(),
                report.presentation_bars.len(),
                report.render_cases
            );
            for item in &report.items {
                println!(
                    "    {}: {} px",
                    item.id,
                    item.widths
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct TestedItem {
    id: String,
    widths: Vec<u32>,
}

#[derive(Serialize)]
struct PluginTestReport<'a> {
    report_version: u32,
    ok: bool,
    source: String,
    plugin_version: String,
    runtime: &'static str,
    items: Vec<TestedItem>,
    presentation_bars: &'a [PresentationBar],
    render_cases: usize,
}

fn test_component_package(
    root: &Path,
    package: &touchbar_plugin_store::PackageInspection,
    host: &Path,
) -> Result<Vec<TestedItem>> {
    if !matches!(&package.manifest.runtime, RuntimeSpec::Component { .. }) {
        bail!("native plugin tests are package-owned")
    }
    let widths = component_test_matrix(&package.manifest);
    let mut tested = Vec::with_capacity(package.manifest.items.len());
    for item in &package.manifest.items {
        let item_widths = &widths[item.id.as_str()];
        for width in item_widths {
            let status = Command::new(host)
                .arg(root)
                .arg(&item.id)
                .arg(width.to_string())
                // Package checks may run in CI with repository credentials in
                // the environment. The component host needs none of them.
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .with_context(|| format!("run {}", host.display()))?;
            if !status.success() {
                bail!("item {} failed at width {width}", item.id)
            }
        }
        tested.push(TestedItem {
            id: item.id.clone(),
            widths: item_widths.iter().copied().collect(),
        });
    }
    Ok(tested)
}

fn component_test_matrix(manifest: &PluginManifest) -> BTreeMap<&str, BTreeSet<u32>> {
    let mut widths = manifest
        .items
        .iter()
        .map(|item| {
            (
                item.id.as_str(),
                BTreeSet::from([80_u32, 160, 320, 1004, REFERENCE_TOUCHBAR_WIDTH]),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for bar in &manifest.bars {
        for element in &bar.elements {
            collect_presentation_test_widths(element, &mut widths);
        }
    }
    widths
}

fn collect_presentation_test_widths<'a>(
    element: &'a PresentationBarElement,
    widths: &mut BTreeMap<&'a str, BTreeSet<u32>>,
) {
    match element {
        PresentationBarElement::Item {
            item,
            minimum_width,
            preferred_width,
            maximum_width,
        } => extend_test_widths(
            widths,
            item,
            *minimum_width,
            *preferred_width,
            *maximum_width,
        ),
        PresentationBarElement::Group { elements, .. } => {
            for element in elements {
                collect_group_test_widths(element, widths);
            }
        }
        PresentationBarElement::FixedSpace { .. }
        | PresentationBarElement::FlexibleSpace { .. } => {}
    }
}

fn collect_group_test_widths<'a>(
    element: &'a PresentationGroupElement,
    widths: &mut BTreeMap<&'a str, BTreeSet<u32>>,
) {
    match element {
        PresentationGroupElement::Item {
            item,
            minimum_width,
            preferred_width,
            maximum_width,
        } => extend_test_widths(
            widths,
            item,
            *minimum_width,
            *preferred_width,
            *maximum_width,
        ),
        PresentationGroupElement::Group { elements, .. } => {
            for element in elements {
                collect_group_test_widths(element, widths);
            }
        }
    }
}

fn extend_test_widths<'a>(
    widths: &mut BTreeMap<&'a str, BTreeSet<u32>>,
    item: &'a str,
    minimum_width: u32,
    preferred_width: u32,
    maximum_width: u32,
) {
    widths
        .get_mut(item)
        .expect("validated presentation references a declared item")
        .extend([minimum_width, preferred_width, maximum_width]);
}

fn replay(args: &[String]) -> Result<()> {
    reject(
        args,
        &["--package", "--scenario", "--screenshots", "--host"],
        0,
    )?;
    let root = package_root(args)?;
    let scenario = Path::new(opt(args, "--scenario").context("replay requires --scenario FILE")?)
        .canonicalize()
        .context("open replay scenario")?;
    let host = binary(
        args,
        "--host",
        "TOUCHBAR_PLUGIN_HOST",
        "touchbar-plugin-host",
    )?;
    let screenshots = opt(args, "--screenshots")
        .map(|path| {
            fs::create_dir_all(path)
                .with_context(|| format!("create screenshot directory {path}"))?;
            Path::new(path)
                .canonicalize()
                .with_context(|| format!("open screenshot directory {path}"))
        })
        .transpose()?;
    let mut command = Command::new(&host);
    command.arg(root).arg("--replay").arg(&scenario);
    if let Some(screenshots) = &screenshots {
        command.arg("--screenshots").arg(screenshots);
    }
    let status = command
        .env_clear()
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("run {}", host.display()))?;
    if !status.success() {
        bail!("component replay failed for {}", scenario.display());
    }
    Ok(())
}

fn dev(args: &[String]) -> Result<()> {
    reject(
        args,
        &[
            "--package",
            "--item",
            "--width",
            "--scale",
            "--host",
            "--supervisor",
            "--sessiond",
        ],
        0,
    )?;
    let root = package_root(args)?;
    let package = inspect_package(&root)?;
    let RuntimeSpec::Component { .. } = &package.manifest.runtime else {
        bail!("sandboxed dev mode supports component plugins")
    };
    if package.manifest.items.is_empty() {
        bail!("plugin has no items");
    }
    let selected = opt(args, "--item");
    if selected.is_some_and(|selected| {
        !package
            .manifest
            .items
            .iter()
            .any(|item| item.id == selected)
    }) {
        bail!("plugin has no item `{}`", selected.unwrap());
    }
    let width = opt(args, "--width")
        .map(|width| width.parse::<u32>().context("--width must be an integer"))
        .transpose()?;
    if width.is_some_and(|width| !(1..=MAX_TOUCHBAR_WIDTH).contains(&width)) {
        bail!("--width must be between 1 and {MAX_TOUCHBAR_WIDTH}");
    }
    let host = binary(
        args,
        "--host",
        "TOUCHBAR_PLUGIN_HOST",
        "touchbar-plugin-host",
    )?;
    let supervisor = binary(
        args,
        "--supervisor",
        "TOUCHBAR_PLUGIN_SUPERVISOR",
        "touchbar-plugin-supervisor",
    )?;
    let sessiond = binary(args, "--sessiond", "TOUCHBAR_SESSIOND", "touchbar-sessiond")?;
    let scale = opt(args, "--scale")
        .unwrap_or("2")
        .parse::<u32>()
        .context("--scale must be 1, 2, or 4")?;
    if !matches!(scale, 1 | 2 | 4) {
        bail!("--scale must be 1, 2, or 4");
    }
    let temporary = tempfile::tempdir().context("create isolated plugin preview store")?;
    let paths = StorePaths::under(temporary.path().join("store"));
    let mut store = PluginStore::open(paths.clone())?;
    let installed = store.install_directory(&root)?;
    store.set_enabled(&installed.source, true)?;
    for item in &installed.items {
        if let Some(selected) = selected {
            store.set_item_enabled(&installed.source, &item.id, item.id == selected)?;
        }
        if let Some(width) = width
            && selected.is_none_or(|selected| item.id == selected)
        {
            store.set_item_width(&installed.source, &item.id, width)?;
        }
    }
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is required for the desktop preview")?;
    let socket_name = format!("touchbar-dev-{}", std::process::id());
    let socket_path = runtime_dir.join(&socket_name);
    if socket_path.exists() {
        bail!(
            "refusing to replace existing preview socket {}",
            socket_path.display()
        );
    }
    install_dev_signal_handlers()?;
    let _socket_cleanup = DevSocketCleanup::new(socket_path.clone());
    let mut compositor = ChildGuard::new(
        Command::new(&sessiond)
            .args([
                "--socket",
                &socket_name,
                "--preview-scale",
                &scale.to_string(),
                "--plugin-host",
            ])
            .arg(&host)
            .arg("--plugin-supervisor")
            .arg(&supervisor)
            .env("TOUCHBAR_HOME", &paths.root)
            .spawn()
            .with_context(|| format!("launch preview compositor {}", sessiond.display()))?,
    );
    wait_for_preview_socket(&socket_path, compositor.child_mut())?;
    println!(
        "plugin-preview=ready package={} items={} selected={} width={} scale={} input=synthetic close=escape",
        installed.source,
        installed.items.len(),
        selected.unwrap_or("all"),
        width.map_or_else(|| "manifest".to_owned(), |width| width.to_string()),
        scale,
    );

    loop {
        if DEV_INTERRUPTED.load(Ordering::Relaxed) {
            println!("plugin-preview=interrupted");
            return Ok(());
        }
        if let Some(status) = compositor
            .child_mut()
            .try_wait()
            .context("poll preview compositor")?
        {
            if status.success() {
                println!("plugin-preview=closed");
                return Ok(());
            }
            return finish_dev_process("preview compositor", status);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_on_hardware(args: &[String]) -> Result<()> {
    reject_with_flags(
        args,
        &[
            "--package",
            "--item",
            "--width",
            "--when-application",
            "--host",
            "--supervisor",
            "--sessiond",
            "--hardware-socket",
        ],
        &["--sandboxed"],
        0,
    )?;
    let root = package_root(args)?;
    let initial_manifest = read_manifest(&root)?;
    if initial_manifest.items.is_empty() {
        bail!("plugin has no items");
    }
    let selected = opt(args, "--item");
    if selected.is_some_and(|selected| {
        !initial_manifest
            .items
            .iter()
            .any(|item| item.id == selected)
    }) {
        bail!("plugin has no item `{}`", selected.unwrap());
    }
    let width = opt(args, "--width")
        .map(|width| width.parse::<u32>().context("--width must be an integer"))
        .transpose()?;
    if width.is_some_and(|width| !(1..=MAX_TOUCHBAR_WIDTH).contains(&width)) {
        bail!("--width must be between 1 and {MAX_TOUCHBAR_WIDTH}");
    }
    let when_application = opt(args, "--when-application");
    if when_application.is_some() && selected.is_none() {
        bail!("--when-application requires --item");
    }
    if let Some(application) = when_application
        && (application.is_empty()
            || application.len() > 128
            || !application
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        bail!(
            "--when-application must be an application class using letters, digits, '.', '_', or '-'"
        );
    }
    let hardware_socket =
        PathBuf::from(opt(args, "--hardware-socket").unwrap_or(DEFAULT_HARDWARE_SOCKET));
    if !hardware_socket.is_absolute()
        || hardware_socket.components().any(|component| {
            !matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        bail!("--hardware-socket must be a normalized absolute path");
    }
    if matches!(initial_manifest.runtime, RuntimeSpec::Component { .. })
        && root.join("Cargo.toml").is_file()
    {
        build_component(&root)?;
    }
    inspect_package(&root)?;
    let host = binary(
        args,
        "--host",
        "TOUCHBAR_PLUGIN_HOST",
        "touchbar-plugin-host",
    )?;
    let supervisor = binary(
        args,
        "--supervisor",
        "TOUCHBAR_PLUGIN_SUPERVISOR",
        "touchbar-plugin-supervisor",
    )?;
    let sessiond = binary(args, "--sessiond", "TOUCHBAR_SESSIOND", "touchbar-sessiond")?;
    for (label, path) in [
        ("component host", &host),
        ("plugin supervisor", &supervisor),
        ("session daemon", &sessiond),
    ] {
        if !path.is_file() {
            bail!("{label} does not exist: {}", path.display());
        }
    }
    println!(
        "plugin-run-runtime sessiond={} host={} supervisor={}",
        sessiond.display(),
        host.display(),
        supervisor.display()
    );

    let temporary = tempfile::tempdir().context("create isolated physical plugin store")?;
    let paths = StorePaths::under(temporary.path().join("store"));
    let mut store = PluginStore::open(paths.clone())?;
    let installed = store.install_directory(&root)?;
    store.set_enabled(&installed.source, true)?;
    for item in &installed.items {
        if let Some(selected) = selected {
            store.set_item_enabled(&installed.source, &item.id, item.id == selected)?;
        }
        if let Some(width) = width
            && selected.is_none_or(|selected| item.id == selected)
        {
            store.set_item_width(&installed.source, &item.id, width)?;
        }
    }

    let profile_path = when_application
        .map(|application| {
            let selected = selected.expect("contextual run requires a selected item");
            let path = temporary.path().join("profiles.toml");
            write_contextual_run_profile(
                &path,
                &installed.source.to_string(),
                selected,
                &application.to_ascii_lowercase(),
            )?;
            Ok::<_, anyhow::Error>(path)
        })
        .transpose()?;

    let active_paths = StorePaths::discover()?;
    let hardware_lease = if active_paths.control.exists() {
        let (response, lease) = touchbar_control::acquire_hardware_yield(&active_paths.control)
            .context("ask the installed user session to yield the Touch Bar")?;
        if !response.ok {
            bail!(
                "installed user session refused hardware handoff: {}",
                response.message
            );
        }
        println!("plugin-run-handoff=yielded installed-session=true");
        Some(lease)
    } else {
        println!("plugin-run-handoff=available installed-session=false");
        None
    };

    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is required for physical plugin runs")?;
    let socket_name = format!("touchbar-run-{}", std::process::id());
    let socket_path = runtime_dir.join(&socket_name);
    if socket_path.exists() {
        bail!(
            "refusing to replace existing development socket {}",
            socket_path.display()
        );
    }
    install_dev_signal_handlers()?;
    let _socket_cleanup = DevSocketCleanup::new(socket_path);
    let mut command = Command::new(&sessiond);
    command
        .arg("--socket")
        .arg(&socket_name)
        .arg("--hardware-socket")
        .arg(&hardware_socket)
        .arg("--system-bar")
        .arg("--plugin-host")
        .arg(&host)
        .arg("--plugin-supervisor")
        .arg(&supervisor)
        .env("TOUCHBAR_HOME", &paths.root);
    if !has_flag(args, "--sandboxed") {
        command.arg("--trusted-plugins");
    }
    if let Some(profile_path) = &profile_path {
        command
            .arg("--profiles")
            .arg(profile_path)
            .arg("--hyprland-context");
    }
    terminate_with_parent(&mut command);
    let mut compositor =
        ChildGuard::new(command.spawn().with_context(|| {
            format!("launch physical development session {}", sessiond.display())
        })?);
    let response = wait_for_physical_run(&paths.control, compositor.child_mut())?;
    println!(
        "plugin-run=ready package={} items={} selected={} width={} mode={} input=physical close=ctrl-c",
        installed.source,
        installed.items.len(),
        selected.unwrap_or("all"),
        width.map_or_else(|| "manifest".to_owned(), |width| width.to_string()),
        if has_flag(args, "--sandboxed") {
            "sandboxed"
        } else {
            "trusted-local"
        },
    );
    if !response.processes.is_empty() {
        println!("plugin-run-processes={}", response.processes.len());
    }

    let run_result = loop {
        if DEV_INTERRUPTED.load(Ordering::Relaxed) {
            println!("plugin-run=interrupted");
            break Ok(());
        }
        if let Some(status) = compositor
            .child_mut()
            .try_wait()
            .context("poll physical development session")?
        {
            break finish_dev_process("physical development session", status);
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(compositor);
    drop(hardware_lease);
    wait_for_installed_session_restore(&active_paths.control)?;
    println!("plugin-run-handoff=restored");
    run_result
}

fn terminate_with_parent(command: &mut Command) {
    let parent = unsafe { libc::getpid() };
    // SAFETY: only async-signal-safe libc calls run between fork and exec. The
    // parent check closes the small race where the CLI exits before prctl.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }
}

fn write_contextual_run_profile(
    path: &Path,
    source: &str,
    item: &str,
    application: &str,
) -> Result<()> {
    let document = format!(
        r#"version = 1
fallback = "default"

[[contribution]]
id = "physical-preview-item"
scope = "application"
when = {{ kind = "text-equals", key = "application.id", value = "{application}" }}
items = [{{ plugin = "{source}", item = "{item}", required = true }}]

[[profile]]
id = "default"

[[profile.element]]
kind = "slot"
id = "inactive-preview"
policy = "collect"
contributions = ["physical-preview-item"]

[[profile]]
id = "physical-preview"

[[profile.element]]
kind = "slot"
id = "physical-preview"
policy = "fixed"
contributions = ["physical-preview-item"]

[[rule]]
profile = "physical-preview"
priority = 1000
when = {{ kind = "text-equals", key = "application.id", value = "{application}" }}
"#,
    );
    fs::write(path, document).with_context(|| format!("write {}", path.display()))
}

fn wait_for_physical_run(control: &Path, child: &mut Child) -> Result<touchbar_control::Response> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child
            .try_wait()
            .context("poll physical development session startup")?
        {
            bail!("physical development session exited during startup with {status}");
        }
        if control.exists()
            && let Ok(response) = touchbar_control::call(
                control,
                &touchbar_control::Request::Status {
                    version: touchbar_control::VERSION,
                },
            )
            && response.ok
            && response.runtime.hardware_connected
            && response.runtime.user_content_visible
            && response
                .processes
                .iter()
                .any(|process| process.state == "running")
        {
            return Ok(response);
        }
        if Instant::now() >= deadline {
            bail!(
                "physical development session did not connect hardware and start a plugin within 15 seconds"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_installed_session_restore(control: &Path) -> Result<()> {
    if !control.exists() {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(response) = touchbar_control::call(
            control,
            &touchbar_control::Request::Status {
                version: touchbar_control::VERSION,
            },
        ) && response.ok
            && response.runtime.hardware_connected
            && !response.runtime.hardware_yielded
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("installed user session did not reclaim the Touch Bar within 10 seconds");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

extern "C" fn interrupt_dev(_: libc::c_int) {
    DEV_INTERRUPTED.store(true, Ordering::Relaxed);
}

fn install_dev_signal_handlers() -> Result<()> {
    DEV_INTERRUPTED.store(false, Ordering::Relaxed);
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the signal handler performs only one lock-free atomic store.
        let previous =
            unsafe { libc::signal(signal, interrupt_dev as *const () as libc::sighandler_t) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error())
                .context("install developer-preview signal handler");
        }
    }
    Ok(())
}

struct DevSocketCleanup {
    socket: PathBuf,
    lock: PathBuf,
}

impl DevSocketCleanup {
    fn new(socket: PathBuf) -> Self {
        let lock = socket.with_file_name(format!(
            "{}.lock",
            socket
                .file_name()
                .expect("development socket has a filename")
                .to_string_lossy()
        ));
        Self { socket, lock }
    }
}

impl Drop for DevSocketCleanup {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.socket);
        }
        if fs::symlink_metadata(&self.lock).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&self.lock);
        }
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0
            .as_mut()
            .expect("child guard always owns its process")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn wait_for_preview_socket(path: &Path, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .context("poll preview compositor startup")?
        {
            bail!("preview compositor exited during startup with {status}");
        }
        if Instant::now() >= deadline {
            bail!("preview compositor did not create {}", path.display());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn finish_dev_process(name: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        println!("plugin-preview=finished process={name}");
        Ok(())
    } else {
        bail!("{name} exited with {status}")
    }
}

fn pack(args: &[String]) -> Result<()> {
    reject(args, &["--package", "--output"], 0)?;
    let root = package_root(args)?;
    let default = github::RELEASE_ASSET_NAME.to_owned();
    let output = opt(args, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(default));
    if output.exists() {
        bail!("{} already exists", output.display())
    }
    let package = pack_directory(&root, &output)?;
    println!("Packed {}\n{}", output.display(), package.package_digest);
    Ok(())
}

fn release_check(args: &[String]) -> Result<()> {
    reject(args, &["--package", "--tag", "--repository"], 0)?;
    let tag = opt(args, "--tag").context("release-check requires --tag vMAJOR.MINOR.PATCH")?;
    let root = package_root(args)?;
    let package = inspect_package(&root)?;
    let version = &package.manifest.plugin.version;
    if !version.pre.is_empty() || !version.build.is_empty() || tag != format!("v{version}") {
        bail!("release tag must exactly match the plain manifest version as vMAJOR.MINOR.PATCH");
    }
    if let Some(repository) = opt(args, "--repository") {
        let repository = repository.to_ascii_lowercase();
        let expected = format!("github:{repository}")
            .parse::<GithubSource>()
            .map_err(anyhow::Error::msg)?;
        if package.manifest.plugin.source != expected {
            bail!("manifest source does not match the release repository");
        }
    }
    if package.manifest.plugin.source.owner() == "local" {
        bail!("release packages require an explicit non-local GitHub source");
    }
    let cargo_path = root.join("Cargo.toml");
    if cargo_path.is_file() {
        let cargo: toml::Value = toml::from_str(&fs::read_to_string(&cargo_path)?)?;
        let cargo_version = cargo
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(toml::Value::as_str)
            .context("Cargo.toml package.version is required")?
            .parse::<Version>()
            .context("Cargo.toml package.version is not semantic")?;
        if cargo_version != *version {
            bail!("Cargo.toml and touchbar-plugin.toml versions do not match");
        }
    }
    println!(
        "Release-ready {} {} {}",
        package.manifest.plugin.source, version, package.package_digest
    );
    Ok(())
}

fn publish(args: &[String]) -> Result<()> {
    reject(args, &["--package", "--tag", "--repository"], 0)?;
    let tag = opt(args, "--tag").context("publish requires --tag vMAJOR.MINOR.PATCH")?;
    let repository =
        opt(args, "--repository").context("publish requires --repository OWNER/REPO")?;
    release_check(args)?;

    let root = package_root(args)?;
    let package = inspect_package(&root)?;
    let expected = format!("github:{}", repository.to_ascii_lowercase())
        .parse::<GithubSource>()
        .map_err(anyhow::Error::msg)?;
    if package.manifest.plugin.source != expected {
        bail!("manifest source does not match the publish repository");
    }
    let asset = root.join(github::RELEASE_ASSET_NAME);
    let published = github::GithubPublisher::new()?.publish(&expected, tag, &asset)?;
    println!(
        "Published {} {} release_id={} asset_id={} asset={} bytes={} {}",
        expected,
        published.tag,
        published.release_id,
        published.asset_id,
        published.asset_name,
        published.asset_size,
        published.asset_digest
    );
    Ok(())
}

fn search_catalog(args: &[String]) -> Result<()> {
    reject(args, &["--category", "--format"], 1)?;
    let query = positional(args, 0).unwrap_or("");
    let category = opt(args, "--category");
    let catalog = Catalog::bundled()?;
    let matches = catalog.search(query, category);
    match output_format(args)? {
        Format::Json => println!("{}", serde_json::to_string_pretty(&matches)?),
        Format::Text if matches.is_empty() => println!("No listed plugins match."),
        Format::Text => {
            for entry in matches {
                println!(
                    "{}  {}  {}  [{}]",
                    entry.alias,
                    entry.tier,
                    entry.source,
                    entry.categories.join(",")
                );
                println!("    {} — {}", entry.name, entry.description);
            }
        }
    }
    Ok(())
}

fn catalog_check(args: &[String]) -> Result<()> {
    reject_with_flags(
        args,
        &["--catalog", "--previous", "--host", "--format"],
        &["--online"],
        0,
    )?;
    let catalog = match opt(args, "--catalog") {
        Some(path) => Catalog::from_toml(
            &fs::read_to_string(path).with_context(|| format!("read catalog {path}"))?,
        )?,
        None => Catalog::bundled()?,
    };
    if let Some(path) = opt(args, "--previous") {
        let previous = Catalog::from_toml(
            &fs::read_to_string(path).with_context(|| format!("read previous catalog {path}"))?,
        )?;
        catalog.validate_transition(&previous)?;
    }
    let online = has_flag(args, "--online");
    let mut releases = Vec::new();
    if online {
        let host = binary(
            args,
            "--host",
            "TOUCHBAR_PLUGIN_HOST",
            "touchbar-plugin-host",
        )?;
        releases = validate_catalog_releases(&catalog, &host)?;
    }
    match output_format(args)? {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "catalog_version": catalog.catalog_version,
                "entries": catalog.plugins.len(),
                "online": online,
                "releases": releases,
            }))?
        ),
        Format::Text => println!(
            "Catalog OK  {} entr{}{}",
            catalog.plugins.len(),
            if catalog.plugins.len() == 1 {
                "y"
            } else {
                "ies"
            },
            if online { "  releases=verified" } else { "" }
        ),
    }
    Ok(())
}

fn validate_catalog_releases(catalog: &Catalog, host: &Path) -> Result<Vec<serde_json::Value>> {
    let temporary = tempfile::tempdir().context("create isolated catalog verifier")?;
    let mut store = PluginStore::open(StorePaths::under(temporary.path().join("store")))?;
    let client = github::GithubClient::new()?;
    let mut verified = Vec::with_capacity(catalog.plugins.len());
    for entry in catalog
        .plugins
        .iter()
        .filter(|entry| entry.state == CatalogState::Active)
    {
        let release = client
            .resolve(&entry.source, None)
            .with_context(|| format!("resolve catalog entry `{}`", entry.alias))?;
        let archive = client
            .download(&release, &store.paths().root)
            .with_context(|| format!("download catalog entry `{}`", entry.alias))?;
        let attested = client
            .verify_attestation_if_present(&release, archive.path())
            .with_context(|| format!("verify catalog entry `{}`", entry.alias))?;
        let outcome = store
            .install_release_archive(
                archive.path(),
                &entry.source,
                &release.version,
                release.installed_origin(attested),
            )
            .with_context(|| format!("inspect catalog entry `{}`", entry.alias))?;
        let package = inspect_package(&store.package_path(&outcome.installed)?)?;
        if package.manifest.plugin.name != entry.name
            || package.manifest.plugin.description != entry.description
        {
            bail!(
                "catalog entry `{}` name or description differs from its latest package manifest",
                entry.alias
            );
        }
        if matches!(package.manifest.runtime, RuntimeSpec::Component { .. }) {
            test_component_package(&package.root, &package, host)
                .with_context(|| format!("run catalog entry `{}`", entry.alias))?;
        }
        verified.push(json!({
            "alias": entry.alias,
            "source": entry.source,
            "version": release.version,
            "immutable": release.immutable,
            "attested": attested,
            "digest": release.asset_digest,
        }));
    }
    Ok(verified)
}

#[derive(Serialize)]
struct SubmissionDocument {
    #[serde(rename = "plugin")]
    plugins: Vec<CatalogEntry>,
}

fn submit(args: &[String]) -> Result<()> {
    reject(
        args,
        &["--alias", "--categories", "--package", "--format"],
        0,
    )?;
    let alias = opt(args, "--alias").context("submit requires --alias ALIAS")?;
    let categories = opt(args, "--categories")
        .context("submit requires --categories CATEGORY,...")?
        .split(',')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let package = inspect_package(&package_root(args)?)?;
    if package.manifest.plugin.source.owner() == "local" {
        bail!("catalog submissions require an explicit public GitHub source");
    }
    let entry = CatalogEntry {
        alias: alias.to_owned(),
        source: package.manifest.plugin.source.clone(),
        name: package.manifest.plugin.name.clone(),
        description: package.manifest.plugin.description.clone(),
        categories,
        tier: CatalogTier::Listed,
        state: CatalogState::Active,
    };
    Catalog {
        catalog_version: touchbar_catalog::CATALOG_VERSION,
        plugins: vec![entry.clone()],
    }
    .validate()?;
    let bundled = Catalog::bundled()?;
    if bundled.find(&entry.alias).is_some() {
        bail!("catalog alias `{}` is already listed", entry.alias);
    }
    if let Some(existing) = bundled.find_source(&entry.source) {
        bail!("{} is already listed as `{}`", entry.source, existing.alias);
    }
    match opt(args, "--format").unwrap_or("toml") {
        "toml" => print!(
            "{}",
            toml::to_string_pretty(&SubmissionDocument {
                plugins: vec![entry]
            })?
        ),
        "json" => println!("{}", serde_json::to_string_pretty(&entry)?),
        value => bail!("unsupported submission format `{value}`"),
    }
    eprintln!(
        "Submit this entry in a pull request to {CATALOG_REPOSITORY}/blob/main/catalog/plugins.toml; catalog review does not replace release provenance verification."
    );
    Ok(())
}

fn add(args: &[String]) -> Result<()> {
    reject(args, &["--path", "--version"], 1)?;
    let path = opt(args, "--path");
    let remote = positional(args, 0);
    if path.is_some() == remote.is_some() {
        bail!("add requires exactly one --path PACKAGE_OR_ARCHIVE or GitHub SOURCE");
    }
    if path.is_some() && opt(args, "--version").is_some() {
        bail!("--version is only valid for a GitHub release install");
    }
    if remote.is_some() {
        return install_github(args, false);
    }
    let path = Path::new(path.expect("validated local path"));
    let mut store = store()?;
    let installed = if path.is_dir() {
        store.install_directory(path)?
    } else {
        store.install_archive(path)?
    };
    println!(
        "Installed {} {} (disabled)",
        installed.source, installed.version
    );
    reload_if_running();
    Ok(())
}

fn update(args: &[String]) -> Result<()> {
    reject(args, &["--version"], 1)?;
    if positional(args, 0).is_none() {
        bail!("update requires SOURCE");
    }
    install_github(args, true)
}

fn install_github(args: &[String], require_installed: bool) -> Result<()> {
    let requested_source =
        positional(args, 0).context("GitHub install requires SOURCE_OR_ALIAS")?;
    let (source, catalog_entry) = if require_installed {
        (source(requested_source)?, None)
    } else {
        install_source(requested_source)?
    };
    if let Some(entry) = &catalog_entry {
        println!(
            "Catalog alias `{}` resolves to {} ({})",
            entry.alias, entry.source, entry.tier
        );
    }
    let requested = opt(args, "--version")
        .map(str::parse::<Version>)
        .transpose()
        .context("--version must be a semantic version")?;
    let mut store = store()?;
    if require_installed && store.get(&source).is_none() {
        bail!("plugin {source} is not installed");
    }
    let client = github::GithubClient::new()?;
    let release = client.resolve(&source, requested.as_ref())?;
    let archive = client.download(&release, &store.paths().root)?;
    let attested = client.verify_attestation_if_present(&release, archive.path())?;
    let outcome = store.install_release_archive(
        archive.path(),
        &source,
        &release.version,
        release.installed_origin(attested),
    )?;
    print_release_install(&outcome);
    println!(
        "release: {} asset={} digest={} immutable={} attested={}",
        release.tag, release.asset_name, release.asset_digest, release.immutable, attested
    );
    if !release.immutable {
        eprintln!(
            "WARNING: this GitHub release is mutable; it is treated as unverified and grants will not carry forward as verified-source authority"
        );
    }
    reload_if_running();
    Ok(())
}

fn rollback(args: &[String]) -> Result<()> {
    reject(args, &["--version"], 1)?;
    let source = source(positional(args, 0).context("rollback requires SOURCE")?)?;
    let version = opt(args, "--version")
        .map(str::parse::<Version>)
        .transpose()
        .context("--version must be a semantic version")?;
    let outcome = store()?.rollback_release(&source, version.as_ref())?;
    println!(
        "Rolled back {} to {} ({})",
        outcome.installed.source,
        outcome.installed.version,
        if outcome.installed.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    print_permission_changes(&outcome);
    reload_if_running();
    Ok(())
}

fn print_release_install(outcome: &ReleaseInstall) {
    println!(
        "Installed {} {} ({}, {})",
        outcome.installed.source,
        outcome.installed.version,
        origin_label(&outcome.installed.origin),
        if outcome.installed.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    print_permission_changes(outcome);
    if outcome.disabled_for_consent {
        eprintln!(
            "The plugin was disabled because this update adds authority or replaces enabled native code; inspect it and explicitly enable it after consent."
        );
    }
}

fn print_permission_changes(outcome: &ReleaseInstall) {
    for change in &outcome.permission_changes {
        if change.kind != PermissionChangeKind::Unchanged {
            println!("permission: {} {:?}", change.capability, change.kind);
        }
    }
}

fn origin_label(origin: &InstalledOrigin) -> &'static str {
    match origin {
        InstalledOrigin::LocalDevelopment => "local-development",
        InstalledOrigin::GithubRelease {
            immutable: true, ..
        } => "verified-release",
        InstalledOrigin::GithubRelease {
            immutable: false, ..
        } => "unverified-release",
    }
}

fn list(args: &[String]) -> Result<()> {
    reject(args, &["--format"], 0)?;
    let plugins = store()?.plugins().cloned().collect::<Vec<_>>();
    match output_format(args)? {
        Format::Json => println!("{}", serde_json::to_string_pretty(&plugins)?),
        Format::Text if plugins.is_empty() => println!("No plugins installed."),
        Format::Text => {
            for p in plugins {
                println!(
                    "{}  {}  {}  {}",
                    if p.enabled { "enabled " } else { "disabled" },
                    p.version,
                    p.source,
                    origin_label(&p.origin)
                )
            }
        }
    }
    Ok(())
}

fn inspect(args: &[String]) -> Result<()> {
    reject(args, &["--format"], 1)?;
    let source = source(positional(args, 0).context("inspect requires SOURCE")?)?;
    let store = store()?;
    let installed = store
        .get(&source)
        .with_context(|| format!("plugin {source} is not installed"))?;
    let path = store.package_path(installed)?;
    let package = inspect_package(&path)?;
    let retained = store.release_history(&source).cloned().collect::<Vec<_>>();
    match output_format(args)? {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"installed": installed, "manifest": package.manifest, "permissions": package.requests, "package_path": path, "release_history": retained})
            )?
        ),
        Format::Text => {
            println!("{} {}", installed.source, installed.version);
            println!(
                "state: {}",
                if installed.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!(
                "runtime: {:?}\norigin: {}\npackage: {}\ndigest: {}",
                installed.runtime,
                origin_label(&installed.origin),
                path.display(),
                installed.package_digest
            );
            if let InstalledOrigin::GithubRelease {
                release_id,
                tag,
                asset_id,
                asset_name,
                asset_digest,
                immutable,
                attested,
            } = &installed.origin
            {
                println!(
                    "release: {tag} id={release_id} immutable={immutable} attested={attested}\nasset: {asset_name} id={asset_id} {asset_digest}"
                );
            }
            for item in &installed.items {
                println!(
                    "item: {} width={} {}",
                    item.id,
                    item.width,
                    if item.enabled { "enabled" } else { "disabled" }
                )
            }
            for profile in &installed.profiles {
                println!(
                    "profile: {} {} ({})",
                    profile.id,
                    if profile.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    profile.label
                )
            }
            for request in &package.requests {
                println!(
                    "permission: {} ({})",
                    request.capability,
                    if request.required {
                        "required"
                    } else {
                        "optional"
                    }
                )
            }
            for snapshot in retained {
                println!(
                    "retained: {} {} {}",
                    snapshot.version,
                    snapshot.package_digest,
                    origin_label(&snapshot.origin)
                );
            }
        }
    }
    Ok(())
}

fn permissions(args: &[String]) -> Result<()> {
    reject(args, &["--format"], 1)?;
    let source = source(positional(args, 0).context("permissions requires SOURCE")?)?;
    let (installed, package, instance) = installed_policy_context(&source)?;
    let paths = StorePaths::discover()?;
    let grants = GrantStore::load(&paths.grants)?;
    let session_store = GrantStore::load(&paths.session_grants)?;
    let session_grants =
        SessionGrants::from_records(session_store.records()).map_err(anyhow::Error::msg)?;
    let policy = calculate_effective_policy(
        &instance,
        &package.requests,
        &grants,
        &session_grants,
        &CapabilityRegistry::v1(),
    );
    match output_format(args)? {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "source": source,
                "version": installed.version,
                "package_digest": installed.package_digest,
                "provenance": instance.provenance,
                "runtime": instance.runtime,
                "enabled": installed.enabled,
                "policy": policy,
            }))?
        ),
        Format::Text => {
            println!(
                "{} {}  {}  {}",
                source,
                installed.version,
                origin_label(&installed.origin),
                if installed.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if policy.grants.is_empty() {
                println!("No capabilities requested.");
            }
            for grant in &policy.grants {
                println!(
                    "{}  {}  {}  risk={}",
                    grant.request.capability,
                    if grant.request.required {
                        "required"
                    } else {
                        "optional"
                    },
                    enum_label(&grant.status)?,
                    enum_label(&grant.request.scope.risk(&grant.request.capability))?
                );
                println!("    {}", grant.request.reason);
                println!("    scope={}", serde_json::to_string(&grant.request.scope)?);
            }
            for warning in &policy.trust.warnings {
                println!("warning: {warning}");
            }
        }
    }
    Ok(())
}

fn permission(args: &[String]) -> Result<()> {
    reject_with_flags(
        args,
        &[
            "--reuse",
            "--format",
            "--bind",
            "--endpoint",
            "--secret",
            "--clipboard-socket",
        ],
        &["--session", "--persistent"],
        3,
    )?;
    let source = source(
        positional_with_flags(args, 0, &["--session", "--persistent"])
            .context("permission requires SOURCE CAPABILITY ACTION")?,
    )?;
    let capability = positional_with_flags(args, 1, &["--session", "--persistent"])
        .context("permission requires CAPABILITY")?
        .parse::<CapabilityId>()
        .map_err(anyhow::Error::msg)?;
    let action = positional_with_flags(args, 2, &["--session", "--persistent"])
        .context("permission requires allow, deny, or reset")?;
    let session = has_flag(args, "--session");
    let persistent = has_flag(args, "--persistent");
    if session == persistent {
        bail!("permission requires exactly one of --session or --persistent");
    }
    let (installed, package, instance) = installed_policy_context(&source)?;
    if instance.runtime == RuntimeKind::Native {
        bail!(
            "native plugins are unrestricted processes; capability declarations are disclosure-only and cannot be granted through the sandbox policy"
        );
    }
    let request = package
        .requests
        .iter()
        .find(|request| request.capability == capability)
        .with_context(|| format!("{source} does not request {capability}"))?;
    let paths = StorePaths::discover()?;
    let decision_path = if session {
        ensure_live_session(&paths)?;
        &paths.session_grants
    } else {
        &paths.grants
    };
    match action {
        "reset" => {
            reject_allow_options(args)?;
            let changed = GrantStore::remove_record(decision_path, &source, &capability)?.is_some();
            permission_output(
                args,
                json!({"source": source, "capability": capability, "decision": null, "status": "needs-consent", "changed": changed, "persistent": persistent}),
                if changed {
                    format!("Reset {source} {capability} to needs-consent")
                } else {
                    format!("No stored decision for {source} {capability}")
                },
            )?;
        }
        "deny" => {
            reject_allow_options(args)?;
            GrantStore::update_record(
                decision_path,
                GrantRecord {
                    source: source.clone(),
                    capability: capability.clone(),
                    approved_scope: request.scope.clone(),
                    bindings: GrantBindings::default(),
                    decision: Decision::Deny,
                    reuse: ReusePolicy::ExactDigest,
                    approved_version: installed.version.clone(),
                    approved_digest: installed.package_digest.clone(),
                },
            )?;
            permission_output(
                args,
                json!({"source": source, "capability": capability, "decision": "deny", "status": "denied", "persistent": persistent}),
                format!("Denied {source} {capability}"),
            )?;
        }
        "allow" => {
            if !capability.is_known() || !CapabilityRegistry::v1().supports(&capability) {
                bail!("this host does not implement {capability}");
            }
            if session && opt(args, "--reuse").is_some() {
                bail!("--reuse is valid only with --persistent");
            }
            let reuse = match opt(args, "--reuse") {
                Some("source") => {
                    if instance.provenance != Provenance::VerifiedRelease {
                        bail!(
                            "source-wide reuse requires an immutable verified GitHub release; use --reuse digest"
                        );
                    }
                    ReusePolicy::VerifiedSameSource
                }
                Some("digest") => ReusePolicy::ExactDigest,
                Some(value) => bail!("--reuse must be source or digest, not `{value}`"),
                None if persistent && instance.provenance == Provenance::VerifiedRelease => {
                    ReusePolicy::VerifiedSameSource
                }
                None => ReusePolicy::ExactDigest,
            };
            let record = GrantRecord {
                source: source.clone(),
                capability: capability.clone(),
                approved_scope: request.scope.clone(),
                bindings: grant_bindings(args)?,
                decision: Decision::Allow,
                reuse,
                approved_version: installed.version.clone(),
                approved_digest: installed.package_digest.clone(),
            };
            record.validate().map_err(|error| {
                anyhow::anyhow!(
                    "{error}; this capability needs explicit host-resource bindings, which this first consent slice does not infer from package hints"
                )
            })?;
            GrantStore::update_record(decision_path, record)?;
            let reuse_label = match reuse {
                ReusePolicy::VerifiedSameSource => "verified-same-source",
                ReusePolicy::ExactDigest => "exact-digest",
            };
            permission_output(
                args,
                json!({"source": source, "capability": capability, "decision": "allow", "status": "granted", "persistent": persistent, "reuse": reuse_label}),
                format!("Allowed {source} {capability} ({reuse_label})"),
            )?;
            if persistent && instance.provenance == Provenance::LocalDevelopment {
                eprintln!(
                    "WARNING: persistent authority was recorded for local-development code and is limited to this exact package digest"
                );
            }
        }
        other => bail!("permission action must be allow, deny, or reset, not `{other}`"),
    }
    reload_if_running();
    Ok(())
}

fn ensure_live_session(paths: &StorePaths) -> Result<()> {
    let response = touchbar_control::call(
        &paths.control,
        &touchbar_control::Request::Status {
            version: touchbar_control::VERSION,
        },
    )
    .context("session-only decisions require a running touchbar-sessiond")?;
    if !response.ok {
        bail!("touchbar-sessiond rejected the session permission request");
    }
    Ok(())
}

fn permission_output(args: &[String], structured: serde_json::Value, text: String) -> Result<()> {
    match output_format(args)? {
        Format::Json => println!("{}", serde_json::to_string_pretty(&structured)?),
        Format::Text => println!("{text}"),
    }
    Ok(())
}

fn reject_allow_options(args: &[String]) -> Result<()> {
    for option in [
        "--reuse",
        "--format",
        "--bind",
        "--endpoint",
        "--secret",
        "--clipboard-socket",
    ] {
        if option != "--format" && opt(args, option).is_some() {
            bail!("{option} is valid only with allow");
        }
    }
    Ok(())
}

fn grant_bindings(args: &[String]) -> Result<GrantBindings> {
    let mut filesystem_mounts = BTreeMap::new();
    for value in options(args, "--bind") {
        let (label, path) = binding_pair(value, "--bind LABEL=DIR")?;
        let binding = FilesystemMountBinding::from_directory(path)
            .with_context(|| format!("bind filesystem label `{label}` to {path}"))?;
        if filesystem_mounts
            .insert(label.to_owned(), binding)
            .is_some()
        {
            bail!("duplicate filesystem binding `{label}`");
        }
    }

    let mut local_endpoints = BTreeMap::new();
    for value in options(args, "--endpoint") {
        let (label, path) = binding_pair(value, "--endpoint LABEL=SOCKET")?;
        let path = validated_user_socket(path)?;
        if local_endpoints
            .insert(label.to_owned(), LocalEndpointBinding::UnixStream { path })
            .is_some()
        {
            bail!("duplicate local endpoint binding `{label}`");
        }
    }

    let mut secrets = BTreeMap::new();
    for value in options(args, "--secret") {
        let (name, object_path) = binding_pair(value, "--secret NAME=OBJECT_PATH")?;
        if secrets
            .insert(
                name.to_owned(),
                SecretBinding::SecretServiceItem {
                    object_path: object_path.to_owned(),
                },
            )
            .is_some()
        {
            bail!("duplicate secret binding `{name}`");
        }
    }

    let clipboard_values = options(args, "--clipboard-socket");
    if clipboard_values.len() > 1 {
        bail!("--clipboard-socket may be supplied only once");
    }
    let clipboard = match clipboard_values.first() {
        Some(path) => Some(ClipboardBinding::WaylandDataControl {
            socket: validated_user_socket(path)?,
        }),
        None => None,
    };
    Ok(GrantBindings {
        filesystem_mounts,
        secrets,
        local_endpoints,
        clipboard,
    })
}

fn binding_pair<'a>(value: &'a str, usage: &str) -> Result<(&'a str, &'a str)> {
    let (name, target) = value
        .split_once('=')
        .with_context(|| format!("binding must be {usage}"))?;
    if name.is_empty() || target.is_empty() {
        bail!("binding must be {usage}");
    }
    Ok((name, target))
}

fn validated_user_socket(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.components().any(|part| {
            !matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        bail!("socket binding must be a normalized absolute path");
    }
    let parent = path.parent().context("socket binding has no parent")?;
    if parent.canonicalize()? != parent {
        bail!("socket binding parent must not contain symlinks");
    }
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect socket binding {}", path.display()))?;
    // SAFETY: geteuid has no preconditions and retains no pointers.
    let current_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_socket() || metadata.file_type().is_symlink() {
        bail!("socket binding must name an existing Unix socket, not a symlink");
    }
    if metadata.uid() != current_uid {
        bail!("socket binding must be owned by the current user");
    }
    Ok(path)
}

fn installed_policy_context(
    source: &GithubSource,
) -> Result<(
    touchbar_plugin_store::InstalledPlugin,
    touchbar_plugin_store::PackageInspection,
    PackageInstance,
)> {
    let store = store()?;
    let installed = store
        .get(source)
        .cloned()
        .with_context(|| format!("plugin {source} is not installed"))?;
    let package = inspect_package(&store.package_path(&installed)?)?;
    let instance = PackageInstance {
        source: installed.source.clone(),
        version: installed.version.clone(),
        digest: installed.package_digest.clone(),
        provenance: installed.origin.provenance(),
        runtime: match installed.runtime {
            InstalledRuntime::Component => RuntimeKind::Component,
            InstalledRuntime::Native => RuntimeKind::Native,
        },
    };
    Ok((installed, package, instance))
}

fn enum_label(value: &impl Serialize) -> Result<String> {
    let value = serde_json::to_value(value)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        _ => bail!("internal enum did not serialize as a string"),
    }
}

fn set_enabled(args: &[String], enabled: bool) -> Result<()> {
    reject(args, &[], 1)?;
    let source = source(positional(args, 0).context("command requires SOURCE")?)?;
    store()?.set_enabled(&source, enabled)?;
    println!("{} {source}", if enabled { "Enabled" } else { "Disabled" });
    reload_if_running();
    Ok(())
}

fn item(args: &[String]) -> Result<()> {
    let source = source(args.first().context("item requires SOURCE ITEM ACTION")?)?;
    let item = args.get(1).context("item requires ITEM")?;
    let action = args.get(2).context("item requires ACTION")?;
    let mut store = store()?;
    match (action.as_str(), args.len()) {
        ("enable", 3) => store.set_item_enabled(&source, item, true)?,
        ("disable", 3) => store.set_item_enabled(&source, item, false)?,
        ("width", 4) => store.set_item_width(
            &source,
            item,
            args[3].parse().context("PX must be an integer")?,
        )?,
        _ => bail!("usage: touchbarctl plugin item SOURCE ITEM enable|disable|width [PX]"),
    }
    println!("Updated {source} item {item}");
    reload_if_running();
    Ok(())
}

fn profile(args: &[String]) -> Result<()> {
    let source = source(
        args.first()
            .context("profile requires SOURCE PROFILE ACTION")?,
    )?;
    let profile = args.get(1).context("profile requires PROFILE")?;
    let action = args.get(2).context("profile requires ACTION")?;
    if args.len() != 3 {
        bail!("usage: touchbarctl plugin profile SOURCE PROFILE enable|disable");
    }
    let enabled = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => bail!("usage: touchbarctl plugin profile SOURCE PROFILE enable|disable"),
    };
    store()?.set_profile_enabled(&source, profile, enabled)?;
    println!("Updated {source} profile {profile}");
    reload_if_running();
    Ok(())
}

fn remove(args: &[String]) -> Result<()> {
    reject(args, &[], 1)?;
    let source = source(positional(args, 0).context("remove requires SOURCE")?)?;
    if !store()?.remove(&source)? {
        bail!("plugin {source} is not installed")
    }
    println!("Removed {source}");
    reload_if_running();
    Ok(())
}

fn session(args: &[String]) -> Result<()> {
    let (request, format, profile_list) = session_request(args)?;
    let paths = StorePaths::discover()?;
    let response = touchbar_control::call(&paths.control, &request)?;
    if matches!(format, Format::Json) {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else if profile_list {
        print_profile_list(&response.runtime.profile);
    } else {
        let runtime = &response.runtime;
        println!(
            "{} hardware={} fn={} system_scene={}",
            response.message,
            if runtime.hardware_yielded {
                "yielded"
            } else if runtime.hardware_connected {
                "connected"
            } else {
                "disconnected"
            },
            if runtime.fn_pressed {
                "pressed"
            } else {
                "released"
            },
            if runtime.system_scene_visible {
                "visible"
            } else {
                "hidden"
            }
        );
        print_profile_status(&runtime.profile);
        for process in &response.processes {
            println!(
                "{}  {}:{}  pid={} restarts={}{}",
                process.state,
                process.source,
                process.item,
                process
                    .pid
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".into()),
                process.restarts,
                process
                    .detail
                    .as_ref()
                    .map(|detail| format!("  {detail}"))
                    .unwrap_or_default()
            );
        }
    }
    if !response.ok {
        bail!("session daemon rejected request: {}", response.message)
    }
    Ok(())
}

fn session_request(args: &[String]) -> Result<(touchbar_control::Request, Format, bool)> {
    let Some(command) = args.first() else {
        bail!("session requires status, reload, or profile")
    };
    match command.as_str() {
        "status" | "reload" => {
            reject(&args[1..], &["--format"], 0)?;
            let request = if command == "status" {
                touchbar_control::Request::Status {
                    version: touchbar_control::VERSION,
                }
            } else {
                touchbar_control::Request::Reload {
                    version: touchbar_control::VERSION,
                }
            };
            Ok((request, output_format(&args[1..])?, false))
        }
        "profile" => {
            let Some(operation) = args.get(1) else {
                bail!("session profile requires list, select NAME, or automatic")
            };
            let values = &args[2..];
            let (request, profile_list) = match operation.as_str() {
                "list" => {
                    reject(values, &["--format"], 0)?;
                    (
                        touchbar_control::Request::Status {
                            version: touchbar_control::VERSION,
                        },
                        true,
                    )
                }
                "select" => {
                    reject(values, &["--format"], 1)?;
                    let profile = positional(values, 0)
                        .context("session profile select requires NAME")?
                        .to_owned();
                    (
                        touchbar_control::Request::ProfileSelect {
                            version: touchbar_control::VERSION,
                            profile,
                        },
                        false,
                    )
                }
                "automatic" => {
                    reject(values, &["--format"], 0)?;
                    (
                        touchbar_control::Request::ProfileAutomatic {
                            version: touchbar_control::VERSION,
                        },
                        false,
                    )
                }
                other => bail!("unknown session profile command `{other}`"),
            };
            Ok((request, output_format(values)?, profile_list))
        }
        other => bail!("unknown session command `{other}`"),
    }
}

fn print_profile_status(profile: &touchbar_control::ProfileRuntimeStatus) {
    if !profile.configured {
        println!("profile=all-items configured=false");
        return;
    }
    println!(
        "profile={} mode={} active={} available={}{}",
        if profile.ready { "ready" } else { "waiting" },
        if profile.automatic {
            "automatic"
        } else {
            "manual"
        },
        profile.active.as_deref().unwrap_or("-"),
        profile.available.join(","),
        if profile.missing_required_items.is_empty() {
            String::new()
        } else {
            format!(" missing={}", profile.missing_required_items.join(","))
        }
    );
}

fn print_profile_list(profile: &touchbar_control::ProfileRuntimeStatus) {
    if !profile.configured {
        println!("No profile configuration is loaded; all connected items are composed.");
        return;
    }
    println!(
        "mode={} state={}",
        if profile.automatic {
            "automatic"
        } else {
            "manual"
        },
        if profile.ready { "ready" } else { "waiting" }
    );
    for id in &profile.available {
        println!(
            "{} {}",
            if profile.active.as_deref() == Some(id) {
                "*"
            } else {
                " "
            },
            id
        );
    }
    if !profile.missing_required_items.is_empty() {
        println!(
            "missing required items: {}",
            profile.missing_required_items.join(", ")
        );
    }
}

fn hardware(args: &[String]) -> Result<()> {
    let Some(command) = args.first() else {
        bail!("hardware requires status")
    };
    if command != "status" {
        bail!("unknown hardware command `{command}`")
    }
    reject(&args[1..], &["--format"], 0)?;
    let output = Command::new("systemctl")
        .args([
            "show",
            "touchbar.service",
            "--no-pager",
            "--property=LoadState,ActiveState,SubState,MainPID",
        ])
        .output()
        .context("query touchbar.service")?;
    if !output.status.success() {
        bail!(
            "systemctl could not query touchbar.service: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    let properties = String::from_utf8(output.stdout).context("systemctl returned non-UTF-8")?;
    let property = |name: &str| {
        properties
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or("unknown")
            .to_owned()
    };
    let load = property("LoadState");
    let active = property("ActiveState");
    let sub = property("SubState");
    let pid = property("MainPID");
    let socket = Path::new("/run/touchbar/hardware.sock").exists();
    let recovery = recovery_marker_status(Path::new(HARDWARE_RECOVERY_MARKER), 0);
    if matches!(output_format(&args[1..])?, Format::Json) {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "service": "touchbar.service",
                "load_state": load,
                "active_state": active,
                "sub_state": sub,
                "main_pid": pid.parse::<u32>().unwrap_or(0),
                "hardware_socket": socket,
                "recovery": recovery,
            }))?
        );
    } else {
        println!(
            "touchbar.service load={load} active={active} sub={sub} pid={pid} socket={} recovery={recovery}",
            if socket { "ready" } else { "absent" },
        );
    }
    if active != "active" || !socket {
        bail!("hardware service is not ready")
    }
    Ok(())
}

fn recovery_marker_status(path: &Path, expected_owner: u32) -> &'static str {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => "normal",
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.uid() == expected_owner
                && metadata.mode() & 0o022 == 0 =>
        {
            "fallback-locked"
        }
        Ok(_) => "invalid-marker",
        Err(_) => "unknown",
    }
}

fn reload_if_running() {
    let Ok(paths) = StorePaths::discover() else {
        return;
    };
    if !paths.control.exists() {
        return;
    }
    match touchbar_control::call(
        &paths.control,
        &touchbar_control::Request::Reload {
            version: touchbar_control::VERSION,
        },
    ) {
        Ok(response) if response.ok => {}
        Ok(response) => eprintln!(
            "touchbarctl: warning: session reload failed: {}",
            response.message
        ),
        Err(error) => eprintln!(
            "touchbarctl: warning: installed state changed but session reload failed: {error:#}"
        ),
    }
}

fn store() -> Result<PluginStore> {
    PluginStore::open(StorePaths::discover()?)
}
fn read_manifest(root: &Path) -> Result<PluginManifest> {
    Ok(PluginManifest::from_toml(&fs::read_to_string(
        root.join("touchbar-plugin.toml"),
    )?)?)
}
fn package_root(args: &[String]) -> Result<PathBuf> {
    opt(args, "--package")
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?)
        .canonicalize()
        .context("open plugin package")
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Text,
    Json,
}
fn output_format(args: &[String]) -> Result<Format> {
    match opt(args, "--format").unwrap_or("text") {
        "text" | "markdown" => Ok(Format::Text),
        "json" => Ok(Format::Json),
        value => bail!("unsupported format `{value}`"),
    }
}
fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|v| v[0] == name)
        .map(|v| v[1].as_str())
}
fn options<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|values| values[0] == name)
        .map(|values| values[1].as_str())
        .collect()
}
fn positional(args: &[String], wanted: usize) -> Option<&str> {
    positional_with_flags(args, wanted, &[])
}
fn positional_with_flags<'a>(args: &'a [String], wanted: usize, flags: &[&str]) -> Option<&'a str> {
    let (mut index, mut number) = (0, 0);
    while index < args.len() {
        if flags.contains(&args[index].as_str()) {
            index += 1;
            continue;
        }
        if args[index].starts_with("--") {
            index += 2;
            continue;
        }
        if number == wanted {
            return Some(&args[index]);
        }
        number += 1;
        index += 1
    }
    None
}
fn reject(args: &[String], options: &[&str], max_positionals: usize) -> Result<()> {
    let (mut index, mut count) = (0, 0);
    while index < args.len() {
        if args[index].starts_with("--") {
            if !options.contains(&args[index].as_str()) {
                bail!("unknown option `{}`", args[index])
            }
            if index + 1 >= args.len() || args[index + 1].starts_with("--") {
                bail!("{} requires a value", args[index])
            }
            index += 2
        } else {
            count += 1;
            index += 1
        }
    }
    if count > max_positionals {
        bail!("too many arguments")
    }
    Ok(())
}
fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|argument| argument == name)
}
fn reject_with_flags(
    args: &[String],
    options: &[&str],
    flags: &[&str],
    max_positionals: usize,
) -> Result<()> {
    let (mut index, mut count) = (0, 0);
    while index < args.len() {
        if flags.contains(&args[index].as_str()) {
            index += 1;
        } else if args[index].starts_with("--") {
            if !options.contains(&args[index].as_str()) {
                bail!("unknown option `{}`", args[index])
            }
            if index + 1 >= args.len() || args[index + 1].starts_with("--") {
                bail!("{} requires a value", args[index])
            }
            index += 2;
        } else {
            count += 1;
            index += 1;
        }
    }
    if count > max_positionals {
        bail!("too many arguments")
    }
    Ok(())
}
fn source(value: &str) -> Result<GithubSource> {
    if value.starts_with("github:") {
        return value.parse().map_err(anyhow::Error::msg);
    }
    let url = Url::parse(value)
        .context("source must be github:owner/repository or an HTTPS GitHub URL")?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("GitHub URL must be exactly https://github.com/owner/repository");
    }
    let segments = url
        .path_segments()
        .context("GitHub URL has no repository path")?
        .collect::<Vec<_>>();
    if segments.len() != 2 || segments.iter().any(|segment| segment.is_empty()) {
        bail!("GitHub URL must be exactly https://github.com/owner/repository");
    }
    GithubSource::new(segments[0], segments[1]).map_err(anyhow::Error::msg)
}
fn install_source(value: &str) -> Result<(GithubSource, Option<CatalogEntry>)> {
    let catalog = Catalog::bundled()?;
    install_source_from_catalog(value, &catalog)
}
fn install_source_from_catalog(
    value: &str,
    catalog: &Catalog,
) -> Result<(GithubSource, Option<CatalogEntry>)> {
    if value.starts_with("github:") || value.starts_with("https://") {
        return Ok((source(value)?, None));
    }
    validate_name(value)
        .context("install target must be a GitHub source, URL, or catalog alias")?;
    let entry = catalog.resolve(value).cloned().with_context(|| {
        format!("unknown catalog alias `{value}`; use `touchbarctl plugin search`")
    })?;
    Ok((entry.source.clone(), Some(entry)))
}
fn binary(args: &[String], flag: &str, variable: &str, name: &str) -> Result<PathBuf> {
    if let Some(v) = opt(args, flag) {
        return Ok(v.into());
    }
    if let Some(v) = env::var_os(variable) {
        return Ok(v.into());
    }
    let sibling = env::current_exe()?.with_file_name(name);
    if sibling.is_file() {
        return Ok(sibling);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // An installed release CLI has no sibling runtime in /usr/bin. Prefer the
    // workspace's coherent release set in that case: debug artifacts may be
    // older and may not support a component just built by the release path.
    for mode in ["release", "debug"] {
        let candidate = root.join("target").join(mode).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let installed = Path::new("/usr/lib/touchbar").join(name);
    if installed.is_file() {
        return Ok(installed);
    }
    Ok(name.into())
}
fn validate_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || v == b'-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        bail!("NAME must be a lowercase kebab-case identifier")
    }
    Ok(())
}
fn title(value: &str) -> String {
    value
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

const SCAFFOLD: &str = r#"use touchbar_component_sdk::kit::{ColorRole, InputEvent, InputKind, Pressable, ViewBuilder};
use touchbar_component_sdk::touchbar::plugin::ui::{PresentationBegin, PresentationCommand, PresentationLifecycle, PresentationPlacement};
use touchbar_component_sdk::{Guest, HostEvent, Item, PresentationEvent, RenderRequest, Update, View};
struct Plugin;
impl Guest for Plugin {
    fn items() -> Vec<Item> {
        vec![
            Item { id: "main".into(), label: "__LABEL__".into() },
            Item { id: "actions".into(), label: "Actions".into() },
        ]
    }
    fn render(request: RenderRequest) -> Result<View, String> {
        let mut view = ViewBuilder::new();
        let (widget, text, label) = if request.item_id == "main" {
            (1, "OPEN", "Open actions")
        } else if request.item_id == "actions" {
            (2, "ACTION", "Run action")
        } else {
            return Err(format!("unknown item {}", request.item_id));
        };
        let content = view.label(text, 11.0, ColorRole::Foreground);
        let control = view.pressable(
            widget,
            label,
            content,
            Pressable { hold_ms: (widget == 1).then_some(420), ..Pressable::control() },
        );
        Ok(view.finish(control))
    }
    fn handle_event(event: InputEvent) -> Result<Update, String> {
        let presentation = match (event.widget_id, event.kind) {
            (1, InputKind::Activated) => Some(PresentationCommand::Begin(PresentationBegin {
                placement: PresentationPlacement::Anchored,
                lifecycle: PresentationLifecycle::Persistent,
                target: None,
            })),
            (1, InputKind::LongPressed) => Some(PresentationCommand::Begin(PresentationBegin {
                placement: PresentationPlacement::Anchored,
                lifecycle: PresentationLifecycle::Transient,
                target: None,
            })),
            _ => None,
        };
        Ok(Update { rerender: true, presentation })
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(Update { rerender: true, presentation: None })
    }
    fn handle_host_event(_event: HostEvent) -> Result<Update, String> {
        Ok(Update { rerender: false, presentation: None })
    }
}
touchbar_component_sdk::export!(Plugin);
"#;

const REPLAY_SCENARIO: &str = r##"{
  "version": 1,
  "item": "main",
  "width": 160,
  "appearance": {
    "preset": "dark",
    "motion": "full",
    "colors": {}
  },
  "steps": [
    { "kind": "touch", "contact_id": 1, "phase": "down", "x": 80, "y": 30, "time_ms": 0 },
    { "kind": "touch", "contact_id": 1, "phase": "up", "x": 80, "y": 30, "time_ms": 50 },
    { "kind": "presentation", "event": { "kind": "started" } },
    { "kind": "presentation", "event": { "kind": "ended", "reason": "selection" } },
    {
      "kind": "appearance",
      "preset": "light",
      "revision": 2,
      "motion": "reduced",
      "colors": { "accent": "#246bfe" }
    },
    { "kind": "touch", "contact_id": 2, "phase": "down", "x": 80, "y": 30, "time_ms": 600 },
    { "kind": "advance", "time_ms": 1100 },
    { "kind": "touch", "contact_id": 2, "phase": "up", "x": 80, "y": 30, "time_ms": 1200 },
    { "kind": "snapshot", "name": "final" }
  ]
}
"##;

const AGENT_GUIDE: &str = r#"# TouchBar plugin agent guide

This repository is one sandboxed TouchBar component pack. Keep every
item ID stable after release and render correctly at every assigned width at
the fixed 60-pixel height. The canvas follows the attached panel, so never
assume a particular total strip width.

Use semantic appearance roles from `RenderRequest`; never assume a fixed
background or pair a hard-coded foreground with a theme-controlled surface.
Text, geometry, SVG masks, image tint, graphs, and animation should all react
to the supplied theme and motion policy.

The generated `main-actions` presentation demonstrates both tap-to-open and
hold-slide interaction. Keep presentation references package-local, declare
ordered min/preferred/max sizing for the bar and every item, and handle
presentation lifecycle events instead of assuming a request was accepted.

Declare only the narrow capabilities the component actually needs. The guest
has no ambient environment, filesystem, network, D-Bus, command execution,
clipboard, or desktop access. Optional capabilities must degrade gracefully;
required capabilities prevent launch when denied.

Before handing work back, run:

```text
cargo generate-lockfile
touchbarctl plugin build
touchbarctl plugin check
touchbarctl plugin test --format json
touchbarctl plugin replay --scenario tests/interaction.json
touchbarctl plugin replay --scenario tests/interaction.json --screenshots screenshots
```

The replay scenario drives coordinates through the real hit-testing, capture,
slider, and hold recognizers with a deterministic clock. It also demonstrates
presentation lifecycle callbacks and a dynamic light/reduced-motion theme. Its
versioned JSON output is suitable for semantic snapshot assertions. The optional
screenshots command renders named checkpoints through the production GLES UI
renderer and refuses to overwrite existing PNGs.

Run `touchbarctl plugin dev` to open the complete pack in the real compositor's
desktop preview. Use `--item ID --width PX` for one responsive item and
`--scale 1|2|4` for display sizing. Mouse and Wayland touch exercise the normal
capture and presentation router, but are explicitly synthetic and cannot
authorize activation-gated OS operations.

On supported hardware, run `touchbarctl plugin run --item ID --width PX` to
launch the workspace session compositor without stopping the hardware service
or changing installed plugins. Ctrl-C returns the strip to the installed user
session. This is trusted local hosting by default; add `--sandboxed` when the
test specifically covers production permission or broker behavior.

For capability-driven widgets, add exact typed D-Bus call/subscription, HTTP
inline/stream, constrained-command, filesystem-read, framed local-service,
notification, URI-open, clipboard, or secret-read
fixtures to the replay scenario. They traverse the real asynchronous broker ABI
and production scope/activation checks without contacting the desktop/network,
launching a process, reading a host file, or opening a Unix socket. Every
expected response must be consumed, so unexpected or missing integration calls
fail the replay instead of silently producing a plausible screenshot.
Clipboard and URI fixtures remain physical-activation gated; use only fake
clipboard values in a committed scenario. Secret fixtures are activation-gated,
never reported by the host, and must contain conspicuously synthetic values only.

For release, update both Cargo.toml and touchbar-plugin.toml to the same plain
semantic version, commit Cargo.lock, tag that commit as `vMAJOR.MINOR.PATCH`,
and push the tag. The generated workflow publishes exactly
`touchbar-plugin.touchbar` and creates a GitHub provenance attestation.
After that release succeeds, `touchbarctl plugin submit --alias ALIAS
--categories CATEGORY,...` emits a reviewed-catalog entry without publishing
or changing anything remotely.
"#;

const CHECK_WORKFLOW: &str = r#"name: TouchBar plugin checks

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - name: Install Rust component target
        run: rustup target add wasm32-wasip2
      - name: Install TouchBar toolchain
        run: |
          cargo install --locked --git https://github.com/cameroncooper/touchbar --tag v0.1.0 touchbar-cli
          cargo install --locked --git https://github.com/cameroncooper/touchbar --tag v0.1.0 touchbar-plugin-host
      - name: Build and validate every responsive width
        run: |
          touchbarctl plugin build
          touchbarctl plugin check
          touchbarctl plugin test
"#;

const RELEASE_WORKFLOW: &str = r#"name: Publish TouchBar plugin

on:
  push:
    tags: ["v*.*.*"]

permissions:
  contents: write
  id-token: write
  attestations: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
      - name: Install Rust component target
        run: rustup target add wasm32-wasip2
      - name: Install TouchBar toolchain
        run: |
          cargo install --locked --git https://github.com/cameroncooper/touchbar --tag v0.1.0 touchbar-cli
          cargo install --locked --git https://github.com/cameroncooper/touchbar --tag v0.1.0 touchbar-plugin-host
      - name: Build, test, and package the tagged source
        run: |
          cargo build --release --locked --target wasm32-wasip2
          touchbarctl plugin build
          touchbarctl plugin check
          touchbarctl plugin test
          touchbarctl plugin release-check --tag "$GITHUB_REF_NAME" --repository "$GITHUB_REPOSITORY"
          touchbarctl plugin pack
      - name: Attest package provenance
        uses: actions/attest@v4
        with:
          subject-path: touchbar-plugin.touchbar
      - name: Create GitHub Release with the standard package asset
        env:
          GITHUB_TOKEN: ${{ github.token }}
        run: >-
          touchbarctl plugin publish
          --tag "$GITHUB_REF_NAME"
          --repository "$GITHUB_REPOSITORY"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    #[test]
    fn parses_options() {
        let args = vec!["demo".into(), "--format".into(), "json".into()];
        assert_eq!(positional(&args, 0), Some("demo"));
        assert_eq!(positional(&args, 1), None);
        assert_eq!(opt(&args, "--format"), Some("json"));
    }

    #[test]
    fn development_socket_cleanup_removes_wayland_socket_and_lock() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("touchbar-run-test");
        let lock = temporary.path().join("touchbar-run-test.lock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fs::write(&lock, []).unwrap();
        drop(DevSocketCleanup::new(socket.clone()));
        assert!(!socket.exists());
        assert!(!lock.exists());
        drop(listener);
    }

    #[test]
    fn component_test_matrix_includes_standard_and_presentation_widths() {
        let manifest = PluginManifest::from_toml(include_str!(
            "../../touchbar-package/tests/fixtures/valid-component.toml"
        ))
        .unwrap();
        let matrix = component_test_matrix(&manifest);
        assert_eq!(
            matrix["now-playing"].iter().copied().collect::<Vec<_>>(),
            [80, 128, 160, 240, 320, 480, 1004, 2008]
        );
        assert_eq!(
            matrix["media-timeline"].iter().copied().collect::<Vec<_>>(),
            [80, 120, 160, 240, 320, 480, 1004, 2008]
        );
    }

    #[test]
    fn parses_fresh_v1_profile_control_commands() {
        let (request, format, list) = session_request(&[
            "profile".into(),
            "select".into(),
            "coding".into(),
            "--format".into(),
            "json".into(),
        ])
        .unwrap();
        assert_eq!(
            request,
            touchbar_control::Request::ProfileSelect {
                version: touchbar_control::VERSION,
                profile: "coding".into(),
            }
        );
        assert_eq!(format, Format::Json);
        assert!(!list);

        let (request, _, list) = session_request(&["profile".into(), "list".into()]).unwrap();
        assert!(matches!(request, touchbar_control::Request::Status { .. }));
        assert!(list);

        let (request, _, _) = session_request(&["profile".into(), "automatic".into()]).unwrap();
        assert!(matches!(
            request,
            touchbar_control::Request::ProfileAutomatic { .. }
        ));
        assert!(
            session_request(&[
                "profile".into(),
                "select".into(),
                "coding".into(),
                "extra".into()
            ])
            .is_err()
        );
    }
    #[test]
    fn validates_names() {
        assert!(validate_name("media-controls").is_ok());
        for value in ["", "Media", "media_controls", "-media", "media-"] {
            assert!(validate_name(value).is_err())
        }
    }
    #[test]
    fn accepts_only_canonical_github_identity_inputs() {
        assert_eq!(
            source("https://github.com/cameroncooper/touchbar-demo")
                .unwrap()
                .to_string(),
            "github:cameroncooper/touchbar-demo"
        );
        assert_eq!(
            source("github:cameroncooper/touchbar-demo")
                .unwrap()
                .to_string(),
            "github:cameroncooper/touchbar-demo"
        );
        for invalid in [
            "http://github.com/cameroncooper/touchbar-demo",
            "https://github.com/cameroncooper/touchbar-demo.git",
            "https://github.com/cameroncooper/touchbar-demo/releases",
            "https://github.com/cameroncooper/touchbar-demo?ref=main",
            "https://example.com/cameroncooper/touchbar-demo",
        ] {
            assert!(source(invalid).is_err(), "accepted {invalid}");
        }
    }
    #[test]
    fn catalog_aliases_resolve_without_replacing_canonical_identity() {
        let source = GithubSource::new("alice", "touchbar-media").unwrap();
        let catalog = Catalog {
            catalog_version: touchbar_catalog::CATALOG_VERSION,
            plugins: vec![CatalogEntry {
                alias: "media-controls".into(),
                source: source.clone(),
                name: "Media Controls".into(),
                description: "Playback and timeline controls".into(),
                categories: vec!["media".into()],
                tier: CatalogTier::Listed,
                state: CatalogState::Active,
            }],
        };
        let (resolved, entry) = install_source_from_catalog("media-controls", &catalog).unwrap();
        assert_eq!(resolved, source);
        assert_eq!(entry.unwrap().alias, "media-controls");
        let (direct, entry) =
            install_source_from_catalog("github:alice/touchbar-media", &catalog).unwrap();
        assert_eq!(direct, source);
        assert!(entry.is_none());
        assert!(install_source_from_catalog("missing", &catalog).is_err());
    }
    #[test]
    fn consent_bindings_are_explicit_canonical_and_user_owned() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let socket = directory.join("service.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let args = vec![
            "--bind".into(),
            format!("gallery={}", directory.display()),
            "--endpoint".into(),
            format!("service={}", socket.display()),
            "--secret".into(),
            "token=/org/freedesktop/secrets/collection/login/1".into(),
            "--clipboard-socket".into(),
            socket.to_string_lossy().into_owned(),
        ];
        let bindings = grant_bindings(&args).unwrap();
        assert_eq!(bindings.filesystem_mounts["gallery"].path, directory);
        assert!(bindings.local_endpoints.contains_key("service"));
        assert!(bindings.secrets.contains_key("token"));
        assert!(bindings.clipboard.is_some());

        let relative = vec!["--endpoint".into(), "service=relative.sock".into()];
        assert!(grant_bindings(&relative).is_err());
        let symlink = directory.join("linked.sock");
        std::os::unix::fs::symlink(&socket, &symlink).unwrap();
        let linked = vec![
            "--clipboard-socket".into(),
            symlink.to_string_lossy().into_owned(),
        ];
        assert!(grant_bindings(&linked).is_err());
    }
    #[test]
    fn session_decisions_require_and_accept_a_live_session_owner() {
        let temporary = tempfile::tempdir().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StorePaths::under(temporary.path().to_path_buf());
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let socket = paths.control.clone();
        let server = thread::spawn(move || {
            let server = touchbar_control::Server::bind(&socket).unwrap();
            ready_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some((mut stream, request)) = server.poll().unwrap().into_iter().next() {
                    assert!(matches!(request, touchbar_control::Request::Status { .. }));
                    touchbar_control::write_response(
                        &mut stream,
                        &touchbar_control::Response {
                            version: touchbar_control::VERSION,
                            ok: true,
                            message: "running".into(),
                            runtime: touchbar_control::SessionRuntimeStatus {
                                hardware_connected: false,
                                hardware_yielded: false,
                                user_content_visible: false,
                                fn_pressed: false,
                                system_scene_visible: false,
                                profile: touchbar_control::ProfileRuntimeStatus::default(),
                                plugin_placeholder: None,
                                power_source: touchbar_control::PowerSourceStatus::Unknown,
                                animation_frame_rate_hz: 60,
                            },
                            processes: Vec::new(),
                        },
                    )
                    .unwrap();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "CLI never contacted session owner"
                );
                thread::sleep(Duration::from_millis(1));
            }
        });
        ready_rx.recv().unwrap();

        ensure_live_session(&paths).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn hardware_recovery_status_requires_a_regular_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("recovery-fallback");
        let owner = fs::metadata(temporary.path()).unwrap().uid();
        assert_eq!(recovery_marker_status(&marker, owner), "normal");

        fs::write(&marker, b"fallback-locked\n").unwrap();
        assert_eq!(recovery_marker_status(&marker, owner), "fallback-locked");
        assert_eq!(recovery_marker_status(&marker, owner + 1), "invalid-marker");

        fs::set_permissions(&marker, fs::Permissions::from_mode(0o622)).unwrap();
        assert_eq!(recovery_marker_status(&marker, owner), "invalid-marker");

        fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink("missing", &marker).unwrap();
        assert_eq!(recovery_marker_status(&marker, owner), "invalid-marker");
    }
    #[test]
    fn scaffold_is_standalone_theme_aware_and_release_ready() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("demo");
        new_plugin(&[
            "demo".into(),
            "--source".into(),
            "https://github.com/alice/demo".into(),
            "--directory".into(),
            root.to_string_lossy().into_owned(),
        ])
        .unwrap();
        let cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(CORE_REPOSITORY));
        assert!(cargo.contains(TOOLCHAIN_TAG));
        assert!(!cargo.contains(temporary.path().to_string_lossy().as_ref()));
        let guide = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(guide.contains("semantic appearance roles"));
        let replay = fs::read_to_string(root.join("tests/interaction.json")).unwrap();
        assert!(replay.contains("\"kind\": \"advance\""));
        assert!(replay.contains("\"preset\": \"light\""));
        let release = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
        assert!(release.contains("actions/attest@v4"));
        assert!(release.contains(github::RELEASE_ASSET_NAME));
        assert!(release.contains("touchbarctl plugin publish"));
        assert!(!release.contains("gh release"));

        fs::write(root.join("component/plugin.wasm"), b"component").unwrap();
        release_check(&[
            "--package".into(),
            root.to_string_lossy().into_owned(),
            "--tag".into(),
            "v0.1.0".into(),
            "--repository".into(),
            "alice/demo".into(),
        ])
        .unwrap();
    }
}
