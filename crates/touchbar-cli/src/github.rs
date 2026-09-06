use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use attestation_verify::{
    Bundle, CheckpointOriginPolicy, GithubPolicy, RefPolicy, RepositoryIdentity, SignerPolicy,
    SourcePolicy, Subject, TrustStore, Verifier, WorkflowPath, WorkflowRevisionPolicy,
};
use reqwest::{
    blocking::{Client, Response},
    header::{
        ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap,
        HeaderValue, USER_AGENT,
    },
    redirect,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use touchbar_package::GithubSource;
use touchbar_plugin_store::InstalledOrigin;

pub const RELEASE_ASSET_NAME: &str = "touchbar-plugin.touchbar";
const API_ROOT: &str = "https://api.github.com";
const UPLOAD_ROOT: &str = "https://uploads.github.com";
const API_VERSION: &str = "2026-03-10";
const MAX_RELEASE_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RELEASE_ASSET_BYTES: u64 = 264 * 1024 * 1024;
const MAX_ATTESTATION_COMPRESSED_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ATTESTATION_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: u64 = 8 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 256;
const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";
const PUBLIC_REKOR_ORIGIN_PREFIX: &str = "rekor.sigstore.dev - ";

pub struct GithubClient {
    metadata: Client,
    download: Client,
    attestation_download: Client,
}

pub struct GithubPublisher {
    api: Client,
    upload: Client,
    api_root: url::Url,
    upload_root: url::Url,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedRelease {
    pub release_id: u64,
    pub asset_id: u64,
    pub tag: String,
    pub asset_name: String,
    pub asset_size: u64,
    pub asset_digest: String,
}

#[derive(Clone, Debug)]
pub struct ResolvedRelease {
    pub source: GithubSource,
    pub version: Version,
    pub release_id: u64,
    pub tag: String,
    pub immutable: bool,
    pub asset_id: u64,
    pub asset_name: String,
    pub asset_size: u64,
    pub asset_digest: String,
    pub repository_id: u64,
    pub owner_id: u64,
}

impl ResolvedRelease {
    pub fn installed_origin(&self, attested: bool) -> InstalledOrigin {
        InstalledOrigin::GithubRelease {
            release_id: self.release_id,
            tag: self.tag.clone(),
            asset_id: self.asset_id,
            asset_name: self.asset_name.clone(),
            asset_digest: self.asset_digest.clone(),
            immutable: self.immutable,
            attested,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    immutable: bool,
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    id: u64,
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiAttestations {
    attestations: Vec<ApiAttestation>,
}

#[derive(Debug, Deserialize)]
struct ApiAttestation {
    repository_id: u64,
    bundle_url: Option<String>,
    initiator: String,
    bundle: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ApiRepository {
    id: u64,
    owner: ApiRepositoryOwner,
    private: bool,
    archived: bool,
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct ApiRepositoryOwner {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct ApiReference {
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Debug, Deserialize)]
struct ApiPublishRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct ApiPublishedAsset {
    id: u64,
    name: String,
    state: String,
    size: u64,
    digest: String,
}

#[derive(Debug, Serialize)]
struct CreateReleaseRequest<'a> {
    tag_name: &'a str,
    name: &'a str,
    draft: bool,
    prerelease: bool,
    generate_release_notes: bool,
}

#[derive(Debug, Serialize)]
struct PublishReleaseRequest {
    draft: bool,
}

impl GithubClient {
    pub fn new() -> Result<Self> {
        let mut public_headers = HeaderMap::new();
        public_headers.insert(
            USER_AGENT,
            HeaderValue::from_static("touchbar-installer/0.1"),
        );
        public_headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static(API_VERSION),
        );
        public_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

        let mut metadata_headers = public_headers.clone();
        if let Some(authorization) = github_token_header(std::env::var_os("GITHUB_TOKEN"))? {
            metadata_headers.insert(AUTHORIZATION, authorization);
        }

        let metadata = Client::builder()
            .default_headers(metadata_headers)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            // Repository transfers are identity changes. Do not silently
            // follow GitHub's repository redirect for metadata lookup.
            .redirect(redirect::Policy::none())
            .build()
            .context("build GitHub metadata client")?;
        let download = Client::builder()
            // Never put API credentials in redirectable asset requests. Stable
            // installation targets public releases only.
            .default_headers(public_headers.clone())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .redirect(redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many GitHub asset redirects");
                }
                let url = attempt.url();
                let allowed =
                    url.scheme() == "https" && url.host_str().is_some_and(allowed_download_host);
                if allowed {
                    attempt.follow()
                } else {
                    attempt.error("GitHub asset redirected outside approved HTTPS hosts")
                }
            }))
            .build()
            .context("build GitHub asset client")?;
        let attestation_download = Client::builder()
            .default_headers(public_headers)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .redirect(redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 2 {
                    return attempt.error("too many attestation bundle redirects");
                }
                if attempt.url().scheme() == "https"
                    && attempt
                        .url()
                        .host_str()
                        .is_some_and(allowed_attestation_host)
                {
                    attempt.follow()
                } else {
                    attempt.error("attestation bundle redirected outside approved HTTPS hosts")
                }
            }))
            .build()
            .context("build GitHub attestation client")?;
        Ok(Self {
            metadata,
            download,
            attestation_download,
        })
    }

    pub fn resolve(
        &self,
        source: &GithubSource,
        requested: Option<&Version>,
    ) -> Result<ResolvedRelease> {
        let repository_endpoint = format!(
            "{API_ROOT}/repos/{}/{}",
            source.owner(),
            source.repository()
        );
        let repository_response = self
            .metadata
            .get(repository_endpoint)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .with_context(|| format!("query repository identity for {source}"))?;
        let repository: ApiRepository =
            read_json_value_bounded(require_success(repository_response, "GitHub repository")?)?;
        if repository.id == 0 || repository.owner.id == 0 {
            bail!("GitHub repository identity contains a zero numeric id");
        }
        if repository.private || repository.archived || repository.disabled {
            bail!("stable installs require an active public GitHub repository");
        }
        let endpoint = match requested {
            Some(version) => format!(
                "{API_ROOT}/repos/{}/{}/releases/tags/v{version}",
                source.owner(),
                source.repository()
            ),
            None => format!(
                "{API_ROOT}/repos/{}/{}/releases/latest",
                source.owner(),
                source.repository()
            ),
        };
        let response = self
            .metadata
            .get(&endpoint)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .with_context(|| format!("query release metadata for {source}"))?;
        let response = require_success(response, "GitHub release metadata")?;
        let release: ApiRelease = read_json_bounded(response)?;
        let mut resolved = resolve_metadata(source, requested, release)?;
        resolved.repository_id = repository.id;
        resolved.owner_id = repository.owner.id;
        Ok(resolved)
    }

    pub fn download(&self, release: &ResolvedRelease, directory: &Path) -> Result<NamedTempFile> {
        fs::create_dir_all(directory)
            .with_context(|| format!("create download directory {}", directory.display()))?;
        let endpoint = format!(
            "{API_ROOT}/repos/{}/{}/releases/assets/{}",
            release.source.owner(),
            release.source.repository(),
            release.asset_id
        );
        let response = self
            .download
            .get(endpoint)
            .header(ACCEPT, "application/octet-stream")
            .send()
            .with_context(|| format!("download {}", release.asset_name))?;
        let mut response = require_success(response, "GitHub release asset")?;
        let content_length = response.content_length();
        write_verified_asset(&mut response, content_length, release, directory)
    }

    pub fn verify_attestation_if_present(
        &self,
        release: &ResolvedRelease,
        _artifact: &Path,
    ) -> Result<bool> {
        let endpoint = format!(
            "{API_ROOT}/repos/{}/{}/attestations/{}?predicate_type=https%3A%2F%2Fslsa.dev%2Fprovenance%2Fv1&per_page=100",
            release.source.owner(),
            release.source.repository(),
            release.asset_digest
        );
        let response = self
            .metadata
            .get(endpoint)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .context("query GitHub artifact attestations")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let response = require_success(response, "GitHub artifact attestation")?;
        let attestations: ApiAttestations = read_json_value_bounded(response)?;
        if attestations.attestations.is_empty() {
            return Ok(false);
        }

        let user_attestations = attestations
            .attestations
            .into_iter()
            .filter(|attestation| attestation.initiator == "user")
            .collect::<Vec<_>>();
        if user_attestations.is_empty() {
            return Ok(false);
        }
        let mut failures = Vec::new();
        for attestation in user_attestations {
            if attestation.repository_id != release.repository_id {
                failures.push("attestation repository id differs from resolved repository".into());
                continue;
            }
            let result = self
                .load_attestation_bundle(attestation)
                .and_then(|bundle| verify_build_provenance(release, &bundle));
            match result {
                Ok(()) => return Ok(true),
                Err(error) => failures.push(format!("{error:#}")),
            }
        }
        bail!(
            "GitHub advertised build provenance, but native cryptographic verification failed: {}",
            failures.join("; ")
        )
    }

    fn load_attestation_bundle(&self, attestation: ApiAttestation) -> Result<Bundle> {
        if let Some(bundle) = attestation.bundle {
            let bytes = serde_json::to_vec(&bundle).context("encode inline attestation bundle")?;
            return Bundle::from_json(&bytes).context("parse inline attestation bundle");
        }
        let url = attestation
            .bundle_url
            .context("GitHub attestation has neither an inline bundle nor a bundle URL")?;
        let parsed = url::Url::parse(&url).context("parse GitHub attestation bundle URL")?;
        if parsed.scheme() != "https" || !parsed.host_str().is_some_and(allowed_attestation_host) {
            bail!("GitHub attestation bundle URL is outside the approved HTTPS host");
        }
        let response = self
            .attestation_download
            .get(parsed)
            .send()
            .context("download GitHub attestation bundle")?;
        let mut response = require_success(response, "GitHub attestation bundle")?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ATTESTATION_COMPRESSED_BYTES)
        {
            bail!("compressed attestation bundle exceeds its size limit");
        }
        let mut compressed = Vec::new();
        response
            .by_ref()
            .take(MAX_ATTESTATION_COMPRESSED_BYTES + 1)
            .read_to_end(&mut compressed)
            .context("read compressed attestation bundle")?;
        if compressed.len() as u64 > MAX_ATTESTATION_COMPRESSED_BYTES {
            bail!("compressed attestation bundle exceeds its size limit");
        }
        let decompressed_len =
            snap::raw::decompress_len(&compressed).context("read raw-Snappy attestation size")?;
        if decompressed_len > MAX_ATTESTATION_BYTES {
            bail!("decompressed attestation bundle exceeds its size limit");
        }
        let bytes = snap::raw::Decoder::new()
            .decompress_vec(&compressed)
            .context("decompress raw-Snappy attestation bundle")?;
        Bundle::from_json(&bytes).context("parse GitHub attestation bundle")
    }
}

impl GithubPublisher {
    pub fn new() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .context("GITHUB_TOKEN is required to publish a GitHub release")?;
        if token.is_empty() {
            bail!("GITHUB_TOKEN must not be empty");
        }
        Self::build(&token, API_ROOT, UPLOAD_ROOT, true)
    }

    fn build(token: &str, api_root: &str, upload_root: &str, require_https: bool) -> Result<Self> {
        let api_root = normalized_origin(api_root, "GitHub API", require_https)?;
        let upload_root = normalized_origin(upload_root, "GitHub upload API", require_https)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("touchbar-publisher/0.1"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("GITHUB_TOKEN is not a valid HTTP header value")?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);

        let api = Client::builder()
            .default_headers(headers.clone())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .redirect(redirect::Policy::none())
            .build()
            .context("build GitHub publishing client")?;
        let upload = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(180))
            .no_proxy()
            .redirect(redirect::Policy::none())
            .build()
            .context("build GitHub asset-upload client")?;
        Ok(Self {
            api,
            upload,
            api_root,
            upload_root,
        })
    }

    pub fn publish(
        &self,
        source: &GithubSource,
        tag: &str,
        asset_path: &Path,
    ) -> Result<PublishedRelease> {
        validate_publish_tag(tag)?;
        let staged = stage_publish_asset(asset_path)?;
        let asset_size = staged.as_file().metadata()?.len();
        let asset_digest = sha256_file(staged.as_file())?;
        self.require_existing_tag(source, tag)?;

        let create = CreateReleaseRequest {
            tag_name: tag,
            name: tag,
            draft: true,
            prerelease: false,
            generate_release_notes: true,
        };
        let response = self
            .api
            .post(self.api_endpoint(source, "releases")?)
            .header(ACCEPT, "application/vnd.github+json")
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&create).context("encode release request")?)
            .send()
            .context("create draft GitHub release")?;
        let created: ApiPublishRelease =
            read_json_value_bounded(require_success(response, "create GitHub release")?)?;
        if created.id == 0 {
            bail!("GitHub created a release with a zero id");
        }

        let result = (|| {
            if created.tag_name != tag || !created.draft {
                bail!("GitHub did not create the requested draft release");
            }
            let upload_url = self.validated_upload_url(source, created.id, &created.upload_url)?;
            let upload_file = staged
                .reopen()
                .context("reopen staged release asset for upload")?;
            let response = self
                .upload
                .post(upload_url)
                .header(ACCEPT, "application/vnd.github+json")
                .header(CONTENT_TYPE, "application/octet-stream")
                .header(CONTENT_LENGTH, asset_size)
                .body(reqwest::blocking::Body::new(upload_file))
                .send()
                .context("upload GitHub release asset")?;
            let asset: ApiPublishedAsset =
                read_json_value_bounded(require_success(response, "upload GitHub release asset")?)?;
            if asset.id == 0
                || asset.name != RELEASE_ASSET_NAME
                || asset.state != "uploaded"
                || asset.size != asset_size
            {
                bail!("GitHub returned inconsistent release-asset metadata");
            }
            validate_sha256(&asset.digest)?;
            if asset.digest != asset_digest {
                bail!("GitHub release-asset digest differs from the staged package");
            }

            let publish = PublishReleaseRequest { draft: false };
            let response = self
                .api
                .patch(self.api_endpoint(source, &format!("releases/{}", created.id))?)
                .header(ACCEPT, "application/vnd.github+json")
                .header(CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(&publish).context("encode publish request")?)
                .send()
                .context("publish GitHub release")?;
            let published: ApiPublishRelease =
                read_json_value_bounded(require_success(response, "publish GitHub release")?)?;
            if published.id != created.id || published.tag_name != tag || published.draft {
                bail!("GitHub returned inconsistent published-release metadata");
            }
            Ok(PublishedRelease {
                release_id: created.id,
                asset_id: asset.id,
                tag: tag.to_owned(),
                asset_name: asset.name,
                asset_size,
                asset_digest,
            })
        })();

        if let Err(error) = result {
            let cleanup = self.delete_release(source, created.id);
            return match cleanup {
                Ok(()) => Err(error.context("draft release was removed")),
                Err(cleanup) => Err(error.context(format!(
                    "draft release {} could not be removed: {cleanup:#}",
                    created.id
                ))),
            };
        }
        result
    }

    fn require_existing_tag(&self, source: &GithubSource, tag: &str) -> Result<()> {
        let response = self
            .api
            .get(self.api_endpoint(source, &format!("git/ref/tags/{tag}"))?)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .context("verify GitHub release tag")?;
        let reference: ApiReference =
            read_json_value_bounded(require_success(response, "verify GitHub release tag")?)?;
        if reference.reference != format!("refs/tags/{tag}") {
            bail!("GitHub returned a different release tag reference");
        }
        Ok(())
    }

    fn validated_upload_url(
        &self,
        source: &GithubSource,
        release_id: u64,
        advertised: &str,
    ) -> Result<url::Url> {
        let advertised = advertised.split('{').next().unwrap_or(advertised);
        let parsed = url::Url::parse(advertised).context("parse GitHub release upload URL")?;
        if !same_origin(&parsed, &self.upload_root)
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.path()
                != format!(
                    "/repos/{}/{}/releases/{release_id}/assets",
                    source.owner(),
                    source.repository()
                )
        {
            bail!("GitHub release upload URL is outside the exact repository endpoint");
        }
        let mut upload = parsed;
        upload
            .query_pairs_mut()
            .append_pair("name", RELEASE_ASSET_NAME);
        Ok(upload)
    }

    fn api_endpoint(&self, source: &GithubSource, suffix: &str) -> Result<url::Url> {
        self.api_root
            .join(&format!(
                "repos/{}/{}/{suffix}",
                source.owner(),
                source.repository()
            ))
            .context("construct GitHub API endpoint")
    }

    fn delete_release(&self, source: &GithubSource, release_id: u64) -> Result<()> {
        let response = self
            .api
            .delete(self.api_endpoint(source, &format!("releases/{release_id}"))?)
            .header(ACCEPT, "application/vnd.github+json")
            .send()
            .context("remove incomplete draft GitHub release")?;
        require_success(response, "remove incomplete draft GitHub release")?;
        Ok(())
    }
}

fn normalized_origin(value: &str, label: &str, require_https: bool) -> Result<url::Url> {
    let mut origin = url::Url::parse(value).with_context(|| format!("parse {label} origin"))?;
    if origin.cannot_be_a_base()
        || origin.host_str().is_none()
        || (require_https && origin.scheme() != "https")
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        bail!("{label} origin is invalid");
    }
    origin.set_path("/");
    Ok(origin)
}

fn same_origin(left: &url::Url, right: &url::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn validate_publish_tag(tag: &str) -> Result<Version> {
    let version = tag
        .strip_prefix('v')
        .context("release tag must have the exact form vMAJOR.MINOR.PATCH")?
        .parse::<Version>()
        .context("release tag is not a semantic version")?;
    if tag != format!("v{version}") || !version.pre.is_empty() || !version.build.is_empty() {
        bail!("release tag must have the exact form vMAJOR.MINOR.PATCH");
    }
    Ok(version)
}

fn stage_publish_asset(path: &Path) -> Result<NamedTempFile> {
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open release asset {}", path.display()))?;
    if !source.metadata()?.is_file() {
        bail!("release asset must be a regular file");
    }
    let mut staged = NamedTempFile::new().context("create private staged release asset")?;
    let mut limited = source.take(MAX_RELEASE_ASSET_BYTES + 1);
    let copied =
        std::io::copy(&mut limited, staged.as_file_mut()).context("stage release asset")?;
    if copied == 0 || copied > MAX_RELEASE_ASSET_BYTES {
        bail!("release asset must be between 1 byte and 264 MiB");
    }
    staged.as_file_mut().sync_all()?;
    staged.as_file_mut().seek(SeekFrom::Start(0))?;
    Ok(staged)
}

fn sha256_file(file: &std::fs::File) -> Result<String> {
    let mut file = file.try_clone().context("clone staged release asset")?;
    file.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn verify_build_provenance(release: &ResolvedRelease, bundle: &Bundle) -> Result<()> {
    let repository = format!("{}/{}", release.source.owner(), release.source.repository());
    let identity = RepositoryIdentity::parse(&repository)
        .context("construct attestation repository identity")?;
    let policy = GithubPolicy::builder()
        .source(SourcePolicy {
            repository: identity
                .clone()
                .with_owner_id(release.owner_id)
                .with_repository_id(release.repository_id),
            git_ref: RefPolicy::Exact(format!("refs/tags/{}", release.tag)),
            commit: None,
        })
        .signer(SignerPolicy {
            repository: identity,
            path: WorkflowPath::new(RELEASE_WORKFLOW)
                .context("construct release workflow identity")?,
            revision: WorkflowRevisionPolicy::Any,
        })
        .build()
        .context("construct GitHub provenance policy")?;
    let trust_store = TrustStore::embedded_public_good()
        .context("load embedded Sigstore public-good trust root")?;
    let entry = bundle
        .verification_material
        .tlog_entries
        .first()
        .context("build-provenance bundle has no transparency-log entry")?;
    let log = trust_store
        .tlogs
        .iter()
        .find(|log| log.log_id_key_id == entry.log_id_key_id)
        .context("attestation uses an unknown transparency-log key")?;
    let checkpoint = entry
        .inclusion_proof
        .as_ref()
        .context("attestation has no Rekor inclusion proof")?;
    let origin = checkpoint
        .checkpoint
        .envelope
        .lines()
        .next()
        .context("attestation checkpoint has no signed origin")?;
    if !origin.starts_with(PUBLIC_REKOR_ORIGIN_PREFIX)
        || origin[PUBLIC_REKOR_ORIGIN_PREFIX.len()..]
            .bytes()
            .any(|byte| !byte.is_ascii_digit())
    {
        bail!("attestation checkpoint is not from the public Rekor deployment");
    }
    let origin_policy = CheckpointOriginPolicy::for_log(log, [origin])
        .context("construct transparency checkpoint policy")?;
    let verifier = Verifier::builder()
        .trust_store(trust_store)
        .github_policy(policy)
        .checkpoint_origin_policy(origin_policy)
        .build()
        .context("construct native attestation verifier")?;
    let digest = release
        .asset_digest
        .strip_prefix("sha256:")
        .context("release digest lost its sha256 prefix")?;
    let subject = Subject::from_digest_hex(digest).context("decode release digest")?;
    verifier
        .verify_digest(&subject, bundle)
        .context("verify GitHub build provenance")?;
    Ok(())
}

fn write_verified_asset(
    reader: &mut impl Read,
    content_length: Option<u64>,
    release: &ResolvedRelease,
    directory: &Path,
) -> Result<NamedTempFile> {
    if content_length.is_some_and(|length| length != release.asset_size) {
        bail!("GitHub asset Content-Length differs from release metadata");
    }
    let mut file = NamedTempFile::new_in(directory)
        .with_context(|| format!("create private download in {}", directory.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .context("read GitHub release asset")?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("GitHub asset size overflow")?;
        if total > MAX_RELEASE_ASSET_BYTES || total > release.asset_size {
            bail!("GitHub release asset exceeds its declared size or host limit");
        }
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
    }
    if total != release.asset_size {
        bail!("GitHub release asset is truncated");
    }
    let actual = format!("sha256:{:x}", hasher.finalize());
    if actual != release.asset_digest {
        bail!("GitHub release asset digest does not match its API metadata");
    }
    file.as_file().sync_all()?;
    Ok(file)
}

fn github_token_header(token: Option<OsString>) -> Result<Option<HeaderValue>> {
    let Some(token) = token else {
        return Ok(None);
    };
    let token = token
        .into_string()
        .map_err(|_| anyhow::anyhow!("GITHUB_TOKEN must be UTF-8"))?;
    if token.is_empty() {
        bail!("GITHUB_TOKEN must not be empty");
    }
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .context("GITHUB_TOKEN is not a valid HTTP header value")?;
    authorization.set_sensitive(true);
    Ok(Some(authorization))
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: Option<String>,
}

fn require_success(mut response: Response, what: &str) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let headers = response.headers().clone();
    let mut body = Vec::new();
    response
        .by_ref()
        .take(MAX_ERROR_BODY_BYTES + 1)
        .read_to_end(&mut body)
        .context("read bounded GitHub error response")?;
    bail!(
        "{what} request failed with HTTP {status}{}",
        github_error_details(status, &headers, &body)
    );
}

fn github_error_details(status: reqwest::StatusCode, headers: &HeaderMap, body: &[u8]) -> String {
    let mut details = Vec::new();
    if body.len() as u64 <= MAX_ERROR_BODY_BYTES
        && let Ok(error) = serde_json::from_slice::<ApiError>(body)
        && let Some(message) = error.message.as_deref().and_then(safe_error_value)
    {
        details.push(message);
    }
    if let Some(request_id) = header_value(headers, "x-github-request-id") {
        details.push(format!("request {request_id}"));
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || (status == reqwest::StatusCode::FORBIDDEN
            && header_value(headers, "x-ratelimit-remaining").as_deref() == Some("0"))
    {
        let mut rate = String::from("GitHub API rate limit exhausted");
        if let Some(retry_after) = header_value(headers, "retry-after") {
            rate.push_str(&format!("; retry after {retry_after}"));
        } else if let Some(reset) = header_value(headers, "x-ratelimit-reset") {
            rate.push_str(&format!("; reset at Unix time {reset}"));
        }
        rate.push_str("; set GITHUB_TOKEN to use an authenticated API limit");
        details.push(rate);
    }

    if details.is_empty() {
        String::new()
    } else {
        format!(": {}", details.join("; "))
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(safe_error_value)
}

fn safe_error_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.chars().take(MAX_ERROR_MESSAGE_CHARS).collect())
}

fn read_json_bounded(response: Response) -> Result<ApiRelease> {
    read_json_value_bounded(response)
}

fn read_json_value_bounded<T: for<'de> Deserialize<'de>>(mut response: Response) -> Result<T> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_RELEASE_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read GitHub release metadata")?;
    if bytes.len() as u64 > MAX_RELEASE_METADATA_BYTES {
        bail!("GitHub release metadata exceeds its size limit");
    }
    serde_json::from_slice(&bytes).context("decode bounded GitHub API response")
}

fn resolve_metadata(
    source: &GithubSource,
    requested: Option<&Version>,
    release: ApiRelease,
) -> Result<ResolvedRelease> {
    if release.id == 0 || release.draft || release.prerelease {
        bail!("stable installs require a published non-prerelease GitHub release");
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .context("release tag must have the exact form vMAJOR.MINOR.PATCH")?
        .parse::<Version>()
        .context("release tag is not a semantic version")?;
    if release.tag_name != format!("v{version}") {
        bail!("release tag is not the canonical vMAJOR.MINOR.PATCH form");
    }
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("stable installs require a plain MAJOR.MINOR.PATCH release version");
    }
    if requested.is_some_and(|requested| requested != &version) {
        bail!("GitHub returned a release other than the requested version");
    }
    let mut matching = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == RELEASE_ASSET_NAME);
    let asset = matching
        .next()
        .with_context(|| format!("release has no {RELEASE_ASSET_NAME} asset"))?;
    if matching.next().is_some() {
        bail!("release contains duplicate standard package assets");
    }
    if asset.id == 0 || asset.state != "uploaded" || asset.size == 0 {
        bail!("release package asset is incomplete");
    }
    if asset.size > MAX_RELEASE_ASSET_BYTES {
        bail!("release package asset exceeds the 264 MiB download limit");
    }
    let asset_digest = asset
        .digest
        .context("GitHub did not provide a digest for the release package asset")?;
    validate_sha256(&asset_digest)?;

    Ok(ResolvedRelease {
        source: source.clone(),
        version,
        release_id: release.id,
        tag: release.tag_name,
        immutable: release.immutable,
        asset_id: asset.id,
        asset_name: asset.name,
        asset_size: asset.size,
        asset_digest,
        repository_id: 1,
        owner_id: 1,
    })
}

fn validate_sha256(value: &str) -> Result<()> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if !valid {
        bail!("GitHub asset digest must be sha256 followed by 64 lowercase hexadecimal digits");
    }
    Ok(())
}

fn allowed_download_host(host: &str) -> bool {
    host == "api.github.com"
        || host == "github.com"
        || host == "objects.githubusercontent.com"
        || host == "github-releases.githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

fn allowed_attestation_host(host: &str) -> bool {
    host == "tmaproduction.blob.core.windows.net"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        net::{TcpListener, TcpStream},
        thread,
    };

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        target: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    fn read_request(mut stream: &TcpStream) -> RecordedRequest {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending complete headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(
                bytes.len() <= 64 * 1024,
                "test request headers are too large"
            );
        };
        let headers_text = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = headers_text.split("\r\n");
        let request_line = lines.next().unwrap();
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().unwrap().to_owned();
        let target = request_parts.next().unwrap().to_owned();
        let mut headers = BTreeMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let content_length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending complete body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        RecordedRequest {
            method,
            target,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn serve(
        listener: TcpListener,
        responses: Vec<(&'static str, String)>,
    ) -> thread::JoinHandle<Vec<RecordedRequest>> {
        thread::spawn(move || {
            responses
                .into_iter()
                .map(|(status, body)| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request = read_request(&stream);
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    request
                })
                .collect()
        })
    }

    fn source() -> GithubSource {
        "github:alice/demo".parse().unwrap()
    }

    #[test]
    fn installer_token_is_optional_but_never_accepted_empty() {
        assert!(github_token_header(None).unwrap().is_none());
        assert!(github_token_header(Some(OsString::new())).is_err());
        let header = github_token_header(Some(OsString::from("token-value")))
            .unwrap()
            .unwrap();
        assert_eq!(header.to_str().unwrap(), "Bearer token-value");
        assert!(header.is_sensitive());
    }

    #[test]
    fn rate_limit_error_is_bounded_and_actionable() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1770000123"));
        headers.insert("x-github-request-id", HeaderValue::from_static("ABC:123"));
        let details = github_error_details(
            reqwest::StatusCode::FORBIDDEN,
            &headers,
            br#"{"message":"API rate limit exceeded"}"#,
        );
        assert!(details.contains("API rate limit exceeded"));
        assert!(details.contains("request ABC:123"));
        assert!(details.contains("reset at Unix time 1770000123"));
        assert!(details.contains("set GITHUB_TOKEN"));

        let oversized = vec![b'x'; MAX_ERROR_BODY_BYTES as usize + 1];
        let details = github_error_details(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            &oversized,
        );
        assert!(!details.contains(&"x".repeat(32)));
        assert!(details.contains("GitHub API rate limit exhausted"));
    }

    #[test]
    fn unsafe_github_error_metadata_is_not_reflected() {
        let details = github_error_details(
            reqwest::StatusCode::BAD_REQUEST,
            &HeaderMap::new(),
            br#"{"message":"unsafe\nterminal text"}"#,
        );
        assert!(details.is_empty());
        assert!(safe_error_value("").is_none());
        assert!(safe_error_value("unsafe\rvalue").is_none());
    }

    fn release(immutable: bool) -> ApiRelease {
        ApiRelease {
            id: 7,
            tag_name: "v1.2.3".into(),
            draft: false,
            prerelease: false,
            immutable,
            assets: vec![ApiAsset {
                id: 9,
                name: RELEASE_ASSET_NAME.into(),
                state: "uploaded".into(),
                size: 128,
                digest: Some(format!("sha256:{}", "a".repeat(64))),
            }],
        }
    }

    #[test]
    fn exact_release_asset_becomes_typed_installer_provenance() {
        let resolved =
            resolve_metadata(&source(), Some(&Version::new(1, 2, 3)), release(true)).unwrap();
        assert_eq!(resolved.version, Version::new(1, 2, 3));
        assert_eq!(
            resolved.installed_origin(true).provenance(),
            touchbar_policy::Provenance::VerifiedRelease
        );

        let mutable = resolve_metadata(&source(), None, release(false)).unwrap();
        assert_eq!(
            mutable.installed_origin(false).provenance(),
            touchbar_policy::Provenance::UnverifiedRelease
        );
    }

    #[test]
    fn malformed_or_ambiguous_release_metadata_fails_closed() {
        let mut wrong_tag = release(true);
        wrong_tag.tag_name = "1.2.3".into();
        assert!(resolve_metadata(&source(), None, wrong_tag).is_err());

        let mut duplicate = release(true);
        duplicate.assets.push(ApiAsset {
            id: 10,
            name: RELEASE_ASSET_NAME.into(),
            state: "uploaded".into(),
            size: 128,
            digest: Some(format!("sha256:{}", "b".repeat(64))),
        });
        assert!(resolve_metadata(&source(), None, duplicate).is_err());

        let mut bad_digest = release(true);
        bad_digest.assets[0].digest = Some(format!("sha256:{}", "A".repeat(64)));
        assert!(resolve_metadata(&source(), None, bad_digest).is_err());
    }

    #[test]
    fn redirect_allowlist_is_https_github_only() {
        assert!(allowed_download_host("objects.githubusercontent.com"));
        assert!(allowed_download_host(
            "release-assets.githubusercontent.com"
        ));
        assert!(!allowed_download_host(
            "githubusercontent.com.attacker.test"
        ));
        assert!(!allowed_download_host("example.com"));
        assert!(allowed_attestation_host(
            "tmaproduction.blob.core.windows.net"
        ));
        assert!(!allowed_attestation_host(
            "tmaproduction.blob.core.windows.net.attacker.test"
        ));
    }

    #[test]
    fn downloaded_bytes_must_match_declared_size_and_github_digest() {
        let bytes = b"package archive";
        let mut resolved = resolve_metadata(&source(), None, release(true)).unwrap();
        resolved.asset_size = bytes.len() as u64;
        resolved.asset_digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let directory = tempfile::tempdir().unwrap();
        let mut reader = &bytes[..];
        let file = write_verified_asset(
            &mut reader,
            Some(bytes.len() as u64),
            &resolved,
            directory.path(),
        )
        .unwrap();
        assert_eq!(fs::read(file.path()).unwrap(), bytes);

        let mut truncated = &bytes[..bytes.len() - 1];
        assert!(write_verified_asset(&mut truncated, None, &resolved, directory.path()).is_err());
        let mut corrupted = b"package archivf".as_slice();
        assert!(write_verified_asset(&mut corrupted, None, &resolved, directory.path()).is_err());
    }

    #[test]
    fn native_publisher_uses_a_draft_transaction_and_fixed_asset() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://{}/", listener.local_addr().unwrap());
        let bytes = b"package";
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let upload_template = format!("{root}repos/alice/demo/releases/42/assets{{?name,label}}");
        let server = serve(
            listener,
            vec![
                ("200 OK", r#"{"ref":"refs/tags/v1.2.3"}"#.to_owned()),
                (
                    "201 Created",
                    format!(
                        r#"{{"id":42,"tag_name":"v1.2.3","draft":true,"upload_url":"{upload_template}"}}"#
                    ),
                ),
                (
                    "201 Created",
                    format!(
                        r#"{{"id":9,"name":"{RELEASE_ASSET_NAME}","state":"uploaded","size":{},"digest":"{digest}"}}"#,
                        bytes.len()
                    ),
                ),
                (
                    "200 OK",
                    format!(
                        r#"{{"id":42,"tag_name":"v1.2.3","draft":false,"upload_url":"{upload_template}"}}"#
                    ),
                ),
            ],
        );
        let directory = tempfile::tempdir().unwrap();
        let asset = directory.path().join(RELEASE_ASSET_NAME);
        fs::write(&asset, bytes).unwrap();
        let publisher = GithubPublisher::build("test-token", &root, &root, false).unwrap();
        let published = publisher.publish(&source(), "v1.2.3", &asset).unwrap();
        assert_eq!(published.release_id, 42);
        assert_eq!(published.asset_id, 9);
        assert_eq!(published.asset_digest, digest);

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/repos/alice/demo/git/ref/tags/v1.2.3");
        assert_eq!(requests[1].method, "POST");
        assert_eq!(requests[1].target, "/repos/alice/demo/releases");
        assert!(String::from_utf8_lossy(&requests[1].body).contains(r#""draft":true"#));
        assert_eq!(requests[2].method, "POST");
        assert_eq!(
            requests[2].target,
            format!("/repos/alice/demo/releases/42/assets?name={RELEASE_ASSET_NAME}")
        );
        assert_eq!(requests[2].body, bytes);
        assert_eq!(requests[3].method, "PATCH");
        assert_eq!(requests[3].target, "/repos/alice/demo/releases/42");
        assert!(String::from_utf8_lossy(&requests[3].body).contains(r#""draft":false"#));
        for request in &requests {
            assert_eq!(
                request.headers.get("authorization").map(String::as_str),
                Some("Bearer test-token")
            );
        }
    }

    #[test]
    fn failed_asset_upload_removes_the_incomplete_draft() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://{}/", listener.local_addr().unwrap());
        let upload_template = format!("{root}repos/alice/demo/releases/42/assets{{?name,label}}");
        let server = serve(
            listener,
            vec![
                ("200 OK", r#"{"ref":"refs/tags/v1.2.3"}"#.to_owned()),
                (
                    "201 Created",
                    format!(
                        r#"{{"id":42,"tag_name":"v1.2.3","draft":true,"upload_url":"{upload_template}"}}"#
                    ),
                ),
                ("500 Internal Server Error", String::new()),
                ("204 No Content", String::new()),
            ],
        );
        let directory = tempfile::tempdir().unwrap();
        let asset = directory.path().join(RELEASE_ASSET_NAME);
        fs::write(&asset, b"package").unwrap();
        let publisher = GithubPublisher::build("test-token", &root, &root, false).unwrap();
        let error = publisher.publish(&source(), "v1.2.3", &asset).unwrap_err();
        assert!(error.to_string().contains("draft release was removed"));
        let requests = server.join().unwrap();
        assert_eq!(requests[3].method, "DELETE");
        assert_eq!(requests[3].target, "/repos/alice/demo/releases/42");
    }

    #[test]
    fn publisher_rejects_upload_origin_and_repository_confusion() {
        let publisher = GithubPublisher::build("test-token", API_ROOT, UPLOAD_ROOT, true).unwrap();
        assert!(
            publisher
                .validated_upload_url(
                    &source(),
                    42,
                    "https://uploads.github.com.attacker.test/repos/alice/demo/releases/42/assets{?name,label}",
                )
                .is_err()
        );
        assert!(
            publisher
                .validated_upload_url(
                    &source(),
                    42,
                    "https://uploads.github.com/repos/mallory/demo/releases/42/assets{?name,label}",
                )
                .is_err()
        );
        assert!(
            publisher
                .validated_upload_url(
                    &source(),
                    42,
                    "https://uploads.github.com/repos/alice/demo/releases/43/assets{?name,label}",
                )
                .is_err()
        );
    }

    #[test]
    fn publisher_stages_only_nonempty_regular_nonsymlink_assets() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty");
        fs::write(&empty, []).unwrap();
        assert!(stage_publish_asset(&empty).is_err());

        let regular = directory.path().join("regular");
        fs::write(&regular, b"package").unwrap();
        let link = directory.path().join("link");
        symlink(&regular, &link).unwrap();
        assert!(stage_publish_asset(&link).is_err());
        assert_eq!(
            stage_publish_asset(&regular)
                .unwrap()
                .as_file()
                .metadata()
                .unwrap()
                .len(),
            7
        );
    }
}
