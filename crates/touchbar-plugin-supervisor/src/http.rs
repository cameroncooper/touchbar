use std::{
    collections::VecDeque,
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use reqwest::{
    Method,
    blocking::Client,
    header::{ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LOCATION},
    redirect::Policy,
};
use touchbar_broker_schema::{
    HttpRequest, HttpRequestMethod, HttpResponse, HttpStreamEvent, HttpStreamOpened,
    MAX_HTTP_INLINE_BODY_BYTES, MAX_HTTP_STREAM_CHUNK_BYTES, SchemaError,
};
use touchbar_policy::{
    CapabilityId, CapabilityScope, HttpMethod, HttpOriginRule, HttpRequestScope,
};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};
use url::{Host, Url};

use crate::{
    ActivationLedger, Backend, BackendRequest, CancellationToken, OpenedResource, ResourceBackend,
    ResourceEventSink, ResourceHandle, ResourceLimits,
};

pub const HTTP_REQUEST_OPERATION: &str = "request";
pub const HTTP_STREAM_OPERATION: &str = "request-stream";
const MAX_REDIRECTS: usize = 5;
const TRANSPORT_TIMEOUT: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SAFE_HEADER_BYTES: usize = 4096;
const RATE_WINDOW_MICROS: u64 = 60_000_000;
const HTTP_STREAM_RESERVED_BYTES: usize = 256 * 1024;
const HTTP_STREAM_TIMEOUT: Duration = Duration::from_secs(30);
const DNS_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(2);
const DNS_RESOLVER_WORKERS: usize = 2;
const DNS_RESOLVER_QUEUE: usize = 8;

pub trait HttpResolver: Send + Sync + 'static {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, BrokerErrorCode>;
}

struct ResolveJob {
    host: String,
    port: u16,
    response: SyncSender<Result<Vec<SocketAddr>, BrokerErrorCode>>,
}

pub struct SystemHttpResolver {
    jobs: Option<SyncSender<ResolveJob>>,
    timeout: Duration,
}

impl Default for SystemHttpResolver {
    fn default() -> Self {
        Self::start(
            Arc::new(|host: &str, port: u16| {
                (host, port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect())
                    .map_err(|_| BrokerErrorCode::Unavailable)
            }),
            DNS_RESOLVER_WORKERS,
            DNS_RESOLVER_QUEUE,
            DNS_RESOLUTION_TIMEOUT,
        )
    }
}

type ResolveFunction = dyn Fn(&str, u16) -> Result<Vec<SocketAddr>, BrokerErrorCode> + Send + Sync;

impl SystemHttpResolver {
    fn start(
        resolve: Arc<ResolveFunction>,
        workers: usize,
        queue: usize,
        timeout: Duration,
    ) -> Self {
        if workers == 0 || queue == 0 || timeout.is_zero() {
            return Self {
                jobs: None,
                timeout,
            };
        }
        let (sender, receiver) = mpsc::sync_channel::<ResolveJob>(queue);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers {
            let receiver = Arc::clone(&receiver);
            let resolve = Arc::clone(&resolve);
            if thread::Builder::new()
                .name(format!("touchbar-dns-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let receiver = receiver
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            receiver.recv()
                        };
                        let Ok(job) = job else { break };
                        let result = resolve(&job.host, job.port);
                        let _ = job.response.send(result);
                    }
                })
                .is_err()
            {
                return Self {
                    jobs: None,
                    timeout,
                };
            }
        }
        Self {
            jobs: Some(sender),
            timeout,
        }
    }
}

impl HttpResolver for SystemHttpResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, BrokerErrorCode> {
        let jobs = self.jobs.as_ref().ok_or(BrokerErrorCode::Unavailable)?;
        let (response, result) = mpsc::sync_channel(1);
        match jobs.try_send(ResolveJob {
            host: host.into(),
            port,
            response,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(BrokerErrorCode::RateLimited),
            Err(TrySendError::Disconnected(_)) => return Err(BrokerErrorCode::Unavailable),
        }
        match result.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(BrokerErrorCode::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(BrokerErrorCode::BackendFailed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpTransportResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub content_encoding: Option<String>,
    pub body: Vec<u8>,
}

pub struct HttpStreamCallbacks<'a> {
    pub metadata: &'a mut dyn FnMut(&HttpTransportResponse) -> Result<(), BrokerErrorCode>,
    pub chunk: &'a mut dyn FnMut(&[u8]) -> Result<(), BrokerErrorCode>,
}

pub trait HttpTransport: Send + Sync + 'static {
    fn send(
        &self,
        request: &HttpRequest,
        url: &Url,
        addresses: &[SocketAddr],
        maximum_response_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<HttpTransportResponse, BrokerErrorCode>;

    fn stream(
        &self,
        request: &HttpRequest,
        url: &Url,
        addresses: &[SocketAddr],
        maximum_response_bytes: u64,
        cancellation: &CancellationToken,
        callbacks: &mut HttpStreamCallbacks<'_>,
    ) -> Result<HttpTransportResponse, BrokerErrorCode> {
        let mut response = self.send(
            request,
            url,
            addresses,
            maximum_response_bytes,
            cancellation,
        )?;
        validate_transport_response(&response, maximum_response_bytes)?;
        (callbacks.metadata)(&response)?;
        if !is_redirect(response.status) {
            for chunk in response.body.chunks(MAX_HTTP_STREAM_CHUNK_BYTES) {
                (callbacks.chunk)(chunk)?;
            }
            response.body.clear();
        }
        Ok(response)
    }
}

#[derive(Default)]
pub struct ReqwestHttpTransport;

impl HttpTransport for ReqwestHttpTransport {
    fn send(
        &self,
        request: &HttpRequest,
        url: &Url,
        addresses: &[SocketAddr],
        maximum_response_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<HttpTransportResponse, BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let host = url.host_str().ok_or(BrokerErrorCode::InvalidRequest)?;
        let client = Client::builder()
            .tls_backend_rustls()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .pool_max_idle_per_host(0)
            .timeout(TRANSPORT_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .resolve_to_addrs(host, addresses)
            .user_agent("touchbar-broker/0.1")
            .build()
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let mut builder = client
            .request(reqwest_method(request.method), url.clone())
            .header(ACCEPT_ENCODING, "identity");
        if let Some(accept) = &request.accept {
            builder = builder.header(ACCEPT, accept);
        }
        if let Some(content_type) = &request.content_type {
            builder = builder.header(CONTENT_TYPE, content_type);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body.clone());
        }
        let mut response = builder.send().map_err(reqwest_error)?;
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let status = response.status().as_u16();
        let location = safe_header(&response, LOCATION)?;
        let content_type = safe_header(&response, CONTENT_TYPE)?;
        let etag = safe_header(&response, ETAG)?;
        let content_encoding = safe_header(&response, reqwest::header::CONTENT_ENCODING)?;
        if let Some(length) = response.headers().get(CONTENT_LENGTH) {
            let length = length
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or(BrokerErrorCode::BackendFailed)?;
            if length > maximum_response_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
        }
        let mut body = Vec::new();
        if !is_redirect(status) {
            response
                .by_ref()
                .take(maximum_response_bytes.saturating_add(1))
                .read_to_end(&mut body)
                .map_err(|_| BrokerErrorCode::BackendFailed)?;
            if body.len() as u64 > maximum_response_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
        }
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        Ok(HttpTransportResponse {
            status,
            location,
            content_type,
            etag,
            content_encoding,
            body,
        })
    }

    fn stream(
        &self,
        request: &HttpRequest,
        url: &Url,
        addresses: &[SocketAddr],
        maximum_response_bytes: u64,
        cancellation: &CancellationToken,
        callbacks: &mut HttpStreamCallbacks<'_>,
    ) -> Result<HttpTransportResponse, BrokerErrorCode> {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        let host = url.host_str().ok_or(BrokerErrorCode::InvalidRequest)?;
        let client = Client::builder()
            .tls_backend_rustls()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .pool_max_idle_per_host(0)
            .timeout(TRANSPORT_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .resolve_to_addrs(host, addresses)
            .user_agent("touchbar-broker/0.1")
            .build()
            .map_err(|_| BrokerErrorCode::BackendFailed)?;
        let mut builder = client
            .request(reqwest_method(request.method), url.clone())
            .header(ACCEPT_ENCODING, "identity");
        if let Some(accept) = &request.accept {
            builder = builder.header(ACCEPT, accept);
        }
        if let Some(content_type) = &request.content_type {
            builder = builder.header(CONTENT_TYPE, content_type);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body.clone());
        }
        let mut response = builder.send().map_err(reqwest_error)?;
        let status = response.status().as_u16();
        let response_metadata = HttpTransportResponse {
            status,
            location: safe_header(&response, LOCATION)?,
            content_type: safe_header(&response, CONTENT_TYPE)?,
            etag: safe_header(&response, ETAG)?,
            content_encoding: safe_header(&response, reqwest::header::CONTENT_ENCODING)?,
            body: Vec::new(),
        };
        validate_transport_response(&response_metadata, maximum_response_bytes)?;
        if let Some(length) = response.headers().get(CONTENT_LENGTH) {
            let length = length
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or(BrokerErrorCode::BackendFailed)?;
            if length > maximum_response_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
        }
        (callbacks.metadata)(&response_metadata)?;
        if !is_redirect(status) {
            let mut total = 0_u64;
            let mut buffer = [0_u8; MAX_HTTP_STREAM_CHUNK_BYTES];
            loop {
                if let Some(reason) = cancellation.reason() {
                    return Err(reason);
                }
                let read = response
                    .read(&mut buffer)
                    .map_err(|_| BrokerErrorCode::BackendFailed)?;
                if read == 0 {
                    break;
                }
                total = total
                    .checked_add(read as u64)
                    .ok_or(BrokerErrorCode::QuotaExceeded)?;
                if total > maximum_response_bytes {
                    return Err(BrokerErrorCode::QuotaExceeded);
                }
                (callbacks.chunk)(&buffer[..read])?;
            }
        }
        Ok(response_metadata)
    }
}

pub struct HttpRequestBackend<R = SystemHttpResolver, T = ReqwestHttpTransport> {
    resolver: Arc<R>,
    transport: Arc<T>,
    rate: Arc<Mutex<VecDeque<u64>>>,
}

impl HttpRequestBackend<SystemHttpResolver, ReqwestHttpTransport> {
    pub fn new() -> Self {
        Self::with_parts(SystemHttpResolver::default(), ReqwestHttpTransport)
    }
}

impl Default for HttpRequestBackend<SystemHttpResolver, ReqwestHttpTransport> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, T> HttpRequestBackend<R, T> {
    pub fn with_parts(resolver: R, transport: T) -> Self {
        Self {
            resolver: Arc::new(resolver),
            transport: Arc::new(transport),
            rate: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl<R: HttpResolver, T: HttpTransport> HttpRequestBackend<R, T> {
    fn authorize_operation(
        &self,
        request: &BackendRequest,
        operation: &str,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        let (wire, scope) = decode_request(request, operation)?;
        authorize_wire_request(&wire, scope)?;
        let mut rate = self.rate.lock().map_err(|_| BrokerErrorCode::Internal)?;
        while rate.front().is_some_and(|started| {
            now_monotonic_micros.saturating_sub(*started) >= RATE_WINDOW_MICROS
        }) {
            rate.pop_front();
        }
        if rate.len() >= usize::from(scope.maximum_requests_per_minute) {
            return Err(BrokerErrorCode::RateLimited);
        }
        rate.push_back(now_monotonic_micros);
        Ok(())
    }
}

impl<R: HttpResolver, T: HttpTransport> Backend for HttpRequestBackend<R, T> {
    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        self.authorize_operation(request, HTTP_REQUEST_OPERATION, now_monotonic_micros)
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        match self.execute_inner(request, cancellation) {
            Ok(response) => match response.encode() {
                Ok(payload) => BrokerResult::Success { payload },
                Err(_) => BrokerResult::Error(BrokerErrorCode::BackendFailed),
            },
            Err(error) => BrokerResult::Error(error),
        }
    }
}

struct HttpStreamHandle {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ResourceHandle for HttpStreamHandle {
    fn close(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // A blocking network read has its own short timeout. Detaching here
        // keeps revocation from blocking the supervisor event loop; events from
        // the removed resource ID are discarded by ResourceManager.
        self.worker.take();
    }
}

impl<R: HttpResolver, T: HttpTransport> ResourceBackend for HttpRequestBackend<R, T> {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        (request.capability == CapabilityId::HttpRequestV1
            && request.operation == HTTP_STREAM_OPERATION
            && matches!(request.authorized_scope, CapabilityScope::HttpRequest(_)))
        .then_some(ResourceLimits {
            reserved_buffered_bytes: HTTP_STREAM_RESERVED_BYTES,
            maximum_events_per_second: u16::MAX,
        })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        self.authorize_operation(request, HTTP_STREAM_OPERATION, now_monotonic_micros)
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        let (wire, scope) = decode_request(request, HTTP_STREAM_OPERATION)?;
        let wire = wire.clone();
        let scope = scope.clone();
        let resolver = Arc::clone(&self.resolver);
        let transport = Arc::clone(&self.transport);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new(Arc::clone(&cancelled), HTTP_STREAM_TIMEOUT)?;
        let worker = thread::Builder::new()
            .name(format!("touchbar-http-stream-{resource_id}"))
            .spawn(move || {
                match execute_stream(
                    resolver.as_ref(),
                    transport.as_ref(),
                    wire,
                    &scope,
                    &cancellation,
                    &events,
                ) {
                    Ok(total_bytes) => {
                        let payload = HttpStreamEvent::Complete { total_bytes }.encode();
                        match payload {
                            Ok(payload) => {
                                let _ = events.complete(payload);
                            }
                            Err(_) => events.finish(BrokerErrorCode::Internal),
                        }
                    }
                    Err(error) => events.finish(error),
                }
            })
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        let response_payload = HttpStreamOpened { resource_id }
            .encode()
            .map_err(|_| BrokerErrorCode::Internal)?;
        Ok(OpenedResource {
            handle: Box::new(HttpStreamHandle {
                cancelled,
                worker: Some(worker),
            }),
            response_payload,
        })
    }
}

fn execute_stream<R: HttpResolver, T: HttpTransport>(
    resolver: &R,
    transport: &T,
    mut wire: HttpRequest,
    scope: &HttpRequestScope,
    cancellation: &CancellationToken,
    events: &ResourceEventSink,
) -> Result<u64, BrokerErrorCode> {
    let mut url = authorize_wire_request(&wire, scope)?;
    let maximum_response_bytes = scope.maximum_response_bytes;
    let mut total_bytes = 0_u64;
    let mut transmitted_request_bytes = 0_u64;
    for redirect_count in 0..=MAX_REDIRECTS {
        if let Some(reason) = cancellation.reason() {
            return Err(reason);
        }
        authorize_url(scope, wire.method, &url)?;
        account_request_body(
            &mut transmitted_request_bytes,
            wire.body.len(),
            scope.maximum_request_bytes,
        )?;
        let addresses = resolve_and_validate(resolver, &url, scope.private_network, cancellation)?;
        let current_url = url.to_string();
        let mut emit_metadata = |response: &HttpTransportResponse| {
            if !is_redirect(response.status) || response.location.is_none() {
                let payload = HttpStreamEvent::Metadata {
                    status: response.status,
                    final_url: current_url.clone(),
                    content_type: response.content_type.clone(),
                    etag: response.etag.clone(),
                }
                .encode()
                .map_err(schema_error)?;
                events.emit_buffered(payload)?;
            }
            Ok(())
        };
        let mut emit_chunk = |chunk: &[u8]| {
            total_bytes = total_bytes
                .checked_add(chunk.len() as u64)
                .ok_or(BrokerErrorCode::QuotaExceeded)?;
            if total_bytes > maximum_response_bytes {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
            let payload = HttpStreamEvent::Chunk(chunk.to_vec())
                .encode()
                .map_err(schema_error)?;
            events.emit_buffered(payload)
        };
        let mut callbacks = HttpStreamCallbacks {
            metadata: &mut emit_metadata,
            chunk: &mut emit_chunk,
        };
        let response = transport.stream(
            &wire,
            &url,
            &addresses,
            maximum_response_bytes,
            cancellation,
            &mut callbacks,
        )?;
        validate_transport_response(&response, maximum_response_bytes)?;
        let Some(location) = response
            .location
            .as_deref()
            .filter(|_| is_redirect(response.status))
        else {
            return Ok(total_bytes);
        };
        if redirect_count == MAX_REDIRECTS {
            return Err(BrokerErrorCode::QuotaExceeded);
        }
        let next = url
            .join(location)
            .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        if response.status == 303 && wire.method != HttpRequestMethod::Head {
            wire.method = HttpRequestMethod::Get;
            wire.body.clear();
            wire.content_type = None;
        }
        authorize_wire_request_for_url(&wire, scope, &next)?;
        url = next;
    }
    Err(BrokerErrorCode::Internal)
}

impl<R: HttpResolver, T: HttpTransport> HttpRequestBackend<R, T> {
    fn execute_inner(
        &self,
        request: &BackendRequest,
        cancellation: &CancellationToken,
    ) -> Result<HttpResponse, BrokerErrorCode> {
        let (mut wire, scope) = decode_request(request, HTTP_REQUEST_OPERATION)?;
        let mut url = authorize_wire_request(&wire, scope)?;
        let maximum_response_bytes = scope.maximum_response_bytes.min(MAX_HTTP_INLINE_BODY_BYTES);
        let mut transmitted_request_bytes = 0_u64;
        for redirect_count in 0..=MAX_REDIRECTS {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            authorize_url(scope, wire.method, &url)?;
            account_request_body(
                &mut transmitted_request_bytes,
                wire.body.len(),
                scope.maximum_request_bytes,
            )?;
            let addresses = resolve_and_validate(
                self.resolver.as_ref(),
                &url,
                scope.private_network,
                cancellation,
            )?;
            let response = self.transport.send(
                &wire,
                &url,
                &addresses,
                maximum_response_bytes,
                cancellation,
            )?;
            validate_transport_response(&response, maximum_response_bytes)?;
            let Some(location) = response
                .location
                .as_deref()
                .filter(|_| is_redirect(response.status))
            else {
                return Ok(HttpResponse {
                    status: response.status,
                    final_url: url.to_string(),
                    content_type: response.content_type,
                    etag: response.etag,
                    body: response.body,
                });
            };
            if redirect_count == MAX_REDIRECTS {
                return Err(BrokerErrorCode::QuotaExceeded);
            }
            let next = url
                .join(location)
                .map_err(|_| BrokerErrorCode::InvalidRequest)?;
            if response.status == 303 && wire.method != HttpRequestMethod::Head {
                wire.method = HttpRequestMethod::Get;
                wire.body.clear();
                wire.content_type = None;
            }
            authorize_wire_request_for_url(&wire, scope, &next)?;
            url = next;
        }
        Err(BrokerErrorCode::Internal)
    }
}

fn account_request_body(
    transmitted: &mut u64,
    next_body_bytes: usize,
    maximum_request_bytes: u64,
) -> Result<(), BrokerErrorCode> {
    *transmitted = transmitted
        .checked_add(next_body_bytes as u64)
        .ok_or(BrokerErrorCode::QuotaExceeded)?;
    if *transmitted > maximum_request_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    Ok(())
}

fn decode_request<'a>(
    request: &'a BackendRequest,
    operation: &str,
) -> Result<(HttpRequest, &'a HttpRequestScope), BrokerErrorCode> {
    if request.capability != CapabilityId::HttpRequestV1 || request.operation != operation {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let CapabilityScope::HttpRequest(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    let wire = HttpRequest::decode(&request.payload).map_err(schema_error)?;
    Ok((wire, scope))
}

fn authorize_wire_request(
    request: &HttpRequest,
    scope: &HttpRequestScope,
) -> Result<Url, BrokerErrorCode> {
    let url = Url::parse(&request.url).map_err(|_| BrokerErrorCode::InvalidRequest)?;
    authorize_wire_request_for_url(request, scope, &url)?;
    Ok(url)
}

fn authorize_wire_request_for_url(
    request: &HttpRequest,
    scope: &HttpRequestScope,
    url: &Url,
) -> Result<(), BrokerErrorCode> {
    if request.body.len() as u64 > scope.maximum_request_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    if matches!(
        request.method,
        HttpRequestMethod::Get | HttpRequestMethod::Head
    ) && (!request.body.is_empty() || request.content_type.is_some())
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    for header in [request.accept.as_deref(), request.content_type.as_deref()]
        .into_iter()
        .flatten()
    {
        validate_header_value(header)?;
    }
    authorize_url(scope, request.method, url)
}

fn authorize_url(
    scope: &HttpRequestScope,
    method: HttpRequestMethod,
    url: &Url,
) -> Result<(), BrokerErrorCode> {
    if !scope.methods.contains(&policy_method(method)) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.cannot_be_a_base()
        || url.host().is_none()
        || url.as_str().contains('\\')
        || dangerous_encoded_path(url.path())
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(BrokerErrorCode::OutOfScope);
    }
    let host = canonical_url_host(url).ok_or(BrokerErrorCode::InvalidRequest)?;
    let port = url
        .port_or_known_default()
        .ok_or(BrokerErrorCode::InvalidRequest)?;
    let allowed = scope.origins.iter().any(|origin| {
        origin.scheme == scheme
            && origin.host == host
            && origin.port == port
            && path_allowed(origin, url.path())
    });
    allowed.then_some(()).ok_or(BrokerErrorCode::OutOfScope)
}

fn path_allowed(origin: &HttpOriginRule, path: &str) -> bool {
    origin.path_prefixes.is_empty()
        || origin
            .path_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn resolve_and_validate(
    resolver: &impl HttpResolver,
    url: &Url,
    private_network: bool,
    cancellation: &CancellationToken,
) -> Result<Vec<SocketAddr>, BrokerErrorCode> {
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    let port = url
        .port_or_known_default()
        .ok_or(BrokerErrorCode::InvalidRequest)?;
    let mut addresses = match url.host().ok_or(BrokerErrorCode::InvalidRequest)? {
        Host::Ipv4(address) => vec![SocketAddr::new(IpAddr::V4(address), port)],
        Host::Ipv6(address) => vec![SocketAddr::new(IpAddr::V6(address), port)],
        Host::Domain(host) => resolver.resolve(host, port)?,
    };
    if let Some(reason) = cancellation.reason() {
        return Err(reason);
    }
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty()
        || addresses.iter().any(|address| {
            address.port() != port || !address_allowed(address.ip(), private_network)
        })
    {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(addresses)
}

fn address_allowed(address: IpAddr, private_network: bool) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_allowed(address, private_network),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return ipv4_allowed(mapped, private_network);
            }
            ipv6_allowed(address, private_network)
        }
    }
}

fn ipv4_allowed(address: Ipv4Addr, private_network: bool) -> bool {
    let octets = address.octets();
    let always_forbidden = address.is_unspecified()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
        || octets[0] == 0
        || octets[0] >= 240;
    if always_forbidden {
        return false;
    }
    let non_public = address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113);
    !non_public || private_network
}

fn ipv6_allowed(address: Ipv6Addr, private_network: bool) -> bool {
    if address.is_unspecified() || address.is_multicast() {
        return false;
    }
    if address.is_loopback() {
        return private_network;
    }
    let segments = address.segments();
    if segments[..6].iter().all(|segment| *segment == 0) {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return ipv4_allowed(embedded, private_network);
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6].iter().all(|s| *s == 0) {
        let embedded = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return ipv4_allowed(embedded, private_network);
    }
    if segments[0] == 0x2002 {
        let embedded = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return ipv4_allowed(embedded, private_network);
    }
    let non_public = address.is_loopback()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x0100 && segments[1..].iter().all(|segment| *segment == 0));
    !non_public || private_network
}

fn canonical_url_host(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(host) => Some(host.to_owned()),
        Host::Ipv4(host) => Some(host.to_string()),
        Host::Ipv6(host) => Some(host.to_string()),
    }
}

fn dangerous_encoded_path(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    lowercase.contains("%2f")
        || lowercase.contains("%5c")
        || lowercase.contains("%2e")
        || lowercase.contains("%00")
}

fn validate_header_value(value: &str) -> Result<(), BrokerErrorCode> {
    if value.is_empty()
        || value.len() > MAX_SAFE_HEADER_BYTES
        || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        Err(BrokerErrorCode::InvalidRequest)
    } else {
        Ok(())
    }
}

fn validate_transport_response(
    response: &HttpTransportResponse,
    maximum_response_bytes: u64,
) -> Result<(), BrokerErrorCode> {
    if !(100..=599).contains(&response.status) {
        return Err(BrokerErrorCode::BackendFailed);
    }
    if response.body.len() as u64 > maximum_response_bytes {
        return Err(BrokerErrorCode::QuotaExceeded);
    }
    if response
        .content_encoding
        .as_deref()
        .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
    {
        return Err(BrokerErrorCode::Unsupported);
    }
    for value in [
        response.location.as_deref(),
        response.content_type.as_deref(),
        response.etag.as_deref(),
        response.content_encoding.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_header_value(value).map_err(|_| BrokerErrorCode::BackendFailed)?;
    }
    Ok(())
}

fn safe_header(
    response: &reqwest::blocking::Response,
    name: reqwest::header::HeaderName,
) -> Result<Option<String>, BrokerErrorCode> {
    let Some(value) = response.headers().get(name) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| BrokerErrorCode::BackendFailed)?;
    validate_header_value(value).map_err(|_| BrokerErrorCode::BackendFailed)?;
    Ok(Some(value.to_owned()))
}

fn reqwest_method(method: HttpRequestMethod) -> Method {
    match method {
        HttpRequestMethod::Get => Method::GET,
        HttpRequestMethod::Head => Method::HEAD,
        HttpRequestMethod::Post => Method::POST,
        HttpRequestMethod::Put => Method::PUT,
        HttpRequestMethod::Patch => Method::PATCH,
        HttpRequestMethod::Delete => Method::DELETE,
    }
}

fn policy_method(method: HttpRequestMethod) -> HttpMethod {
    match method {
        HttpRequestMethod::Get => HttpMethod::Get,
        HttpRequestMethod::Head => HttpMethod::Head,
        HttpRequestMethod::Post => HttpMethod::Post,
        HttpRequestMethod::Put => HttpMethod::Put,
        HttpRequestMethod::Patch => HttpMethod::Patch,
        HttpRequestMethod::Delete => HttpMethod::Delete,
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

fn reqwest_error(error: reqwest::Error) -> BrokerErrorCode {
    if error.is_timeout() {
        BrokerErrorCode::Timeout
    } else if error.is_connect() {
        BrokerErrorCode::Unavailable
    } else {
        BrokerErrorCode::BackendFailed
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, VecDeque},
        io::{Read, Write},
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
        sync::{Arc, Condvar, Mutex, atomic::AtomicBool, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use semver::Version;
    use touchbar_broker_schema::{
        HttpRequest, HttpRequestMethod, HttpResponse, HttpStreamEvent, HttpStreamOpened,
        MAX_HTTP_STREAM_CHUNK_BYTES,
    };
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        CapabilityId, CapabilityScope, HttpMethod, HttpOriginRule, HttpRequestScope,
        PackageInstance, Provenance, RuntimeKind,
    };
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

    use crate::{
        ActivationLedger, Backend, BackendRequest, CancellationToken, ConnectionIdentity,
        HostEvent, HostEventQueue, ResourceBackend, ResourceManager,
    };

    use super::{
        HTTP_REQUEST_OPERATION, HTTP_STREAM_OPERATION, HttpRequestBackend, HttpResolver,
        HttpStreamCallbacks, HttpTransport, HttpTransportResponse, ReqwestHttpTransport,
        SystemHttpResolver, address_allowed,
    };

    #[derive(Default)]
    struct FakeResolver {
        answers: Mutex<VecDeque<Result<Vec<SocketAddr>, BrokerErrorCode>>>,
    }

    impl FakeResolver {
        fn new<I, A>(answers: I) -> Self
        where
            I: IntoIterator<Item = A>,
            A: IntoIterator<Item = SocketAddr>,
        {
            Self {
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|answer| Ok(answer.into_iter().collect()))
                        .collect(),
                ),
            }
        }
    }

    impl HttpResolver for FakeResolver {
        fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<SocketAddr>, BrokerErrorCode> {
            self.answers
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?
                .pop_front()
                .unwrap_or(Err(BrokerErrorCode::Unavailable))
        }
    }

    #[test]
    fn system_dns_pool_bounds_hangs_threads_and_queued_work() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (entered_sender, entered) = mpsc::channel();
        let resolver = Arc::new(SystemHttpResolver::start(
            Arc::new(move |_host, _port| {
                let _ = entered_sender.send(());
                let (lock, wake) = &*worker_gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
                Ok(vec![SocketAddr::from(([93, 184, 216, 34], 443))])
            }),
            1,
            1,
            Duration::from_millis(25),
        ));
        let first_resolver = Arc::clone(&resolver);
        let first = thread::spawn(move || first_resolver.resolve("example.com", 443));
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert_eq!(first.join().unwrap(), Err(BrokerErrorCode::Timeout));
        assert_eq!(
            resolver.resolve("queued.example", 443),
            Err(BrokerErrorCode::Timeout)
        );
        assert_eq!(
            resolver.resolve("overflow.example", 443),
            Err(BrokerErrorCode::RateLimited)
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    #[derive(Default)]
    struct FakeTransport {
        responses: Mutex<VecDeque<Result<HttpTransportResponse, BrokerErrorCode>>>,
        calls: Mutex<Vec<(String, Vec<SocketAddr>)>>,
    }

    impl FakeTransport {
        fn new(responses: impl IntoIterator<Item = HttpTransportResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl HttpTransport for Arc<FakeTransport> {
        fn send(
            &self,
            _request: &HttpRequest,
            url: &url::Url,
            addresses: &[SocketAddr],
            _maximum_response_bytes: u64,
            _cancellation: &CancellationToken,
        ) -> Result<HttpTransportResponse, BrokerErrorCode> {
            self.calls
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?
                .push((url.to_string(), addresses.to_vec()));
            self.responses
                .lock()
                .map_err(|_| BrokerErrorCode::Internal)?
                .pop_front()
                .unwrap_or(Err(BrokerErrorCode::BackendFailed))
        }
    }

    fn public() -> SocketAddr {
        SocketAddr::from(([93, 184, 216, 34], 443))
    }

    fn response(status: u16, location: Option<&str>, body: &[u8]) -> HttpTransportResponse {
        HttpTransportResponse {
            status,
            location: location.map(str::to_owned),
            content_type: Some("application/json".into()),
            etag: None,
            content_encoding: None,
            body: body.to_vec(),
        }
    }

    fn scope(private_network: bool, maximum_requests_per_minute: u16) -> HttpRequestScope {
        HttpRequestScope {
            origins: BTreeSet::from([HttpOriginRule {
                scheme: "https".into(),
                host: "api.example.com".into(),
                port: 443,
                path_prefixes: BTreeSet::from(["/v1/".into()]),
            }]),
            methods: BTreeSet::from([HttpMethod::Get, HttpMethod::Post]),
            private_network,
            maximum_request_bytes: 1024,
            maximum_response_bytes: 1024,
            maximum_requests_per_minute,
        }
    }

    fn wire(method: HttpRequestMethod, url: &str) -> HttpRequest {
        HttpRequest {
            method,
            url: url.into(),
            accept: Some("application/json".into()),
            content_type: None,
            body: Vec::new(),
        }
    }

    fn request(scope: HttpRequestScope, wire: &HttpRequest) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 9,
                package: PackageInstance {
                    source: GithubSource::new("alice", "weather").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::HttpRequestV1,
            authorized_scope: CapabilityScope::HttpRequest(scope),
            bindings: touchbar_policy::GrantBindings::default(),
            activation: None,
            operation: HTTP_REQUEST_OPERATION.into(),
            payload: wire.encode().unwrap(),
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn exact_origin_method_and_path_reach_only_pinned_addresses() {
        let transport = Arc::new(FakeTransport::new([response(200, None, b"{}")]));
        let backend =
            HttpRequestBackend::with_parts(FakeResolver::new([[public()]]), Arc::clone(&transport));
        let result = backend.execute(
            &request(
                scope(false, 10),
                &wire(HttpRequestMethod::Get, "https://api.example.com/v1/state"),
            ),
            &token(),
        );
        let BrokerResult::Success { payload } = result else {
            panic!("request should succeed")
        };
        let response = HttpResponse::decode(&payload).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}");
        assert_eq!(transport.calls.lock().unwrap()[0].1, [public()]);
    }

    #[test]
    fn streamed_response_is_metadata_bounded_chunks_and_explicit_completion() {
        let body = vec![0x5a; MAX_HTTP_STREAM_CHUNK_BYTES + 37];
        let transport = Arc::new(FakeTransport::new([response(200, None, &body)]));
        let backend = Arc::new(HttpRequestBackend::with_parts(
            FakeResolver::new([[public()]]),
            Arc::clone(&transport),
        ));
        let mut wire_request = request(
            HttpRequestScope {
                maximum_response_bytes: 64 * 1024,
                ..scope(false, 10)
            },
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/data"),
        );
        wire_request.operation = HTTP_STREAM_OPERATION.into();
        // One queued event forces the worker to exercise bounded backpressure
        // between metadata and both body chunks.
        let mut manager = ResourceManager::new(1).unwrap();
        manager.register(CapabilityId::HttpRequestV1, backend.clone());
        let response_payload = manager
            .open(
                41,
                &wire_request,
                backend.limits(&wire_request).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        assert_eq!(
            HttpStreamOpened::decode(&response_payload)
                .unwrap()
                .resource_id,
            41
        );

        let mut host_events = HostEventQueue::new(4, 16);
        let mut received = Vec::new();
        for _ in 0..10_000 {
            let finished = manager.pump(&mut host_events, 3);
            while let Some(event) = host_events.pop() {
                received.push(event);
            }
            if !finished.is_empty() {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(manager.resource_count(), 0);
        assert_eq!(received.len(), 4);
        let decoded = received
            .into_iter()
            .enumerate()
            .map(|(index, event)| match event {
                HostEvent::ResourceEvent {
                    resource_id: 41,
                    sequence,
                    result: BrokerResult::Success { payload },
                } => {
                    assert_eq!(sequence, index as u64 + 1);
                    HttpStreamEvent::decode(&payload).unwrap()
                }
                other => panic!("unexpected stream event: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            decoded[0],
            HttpStreamEvent::Metadata { status: 200, .. }
        ));
        assert!(
            matches!(&decoded[1], HttpStreamEvent::Chunk(chunk) if chunk.len() == MAX_HTTP_STREAM_CHUNK_BYTES)
        );
        assert!(matches!(&decoded[2], HttpStreamEvent::Chunk(chunk) if chunk.len() == 37));
        assert_eq!(
            decoded[3],
            HttpStreamEvent::Complete {
                total_bytes: body.len() as u64
            }
        );
    }

    #[test]
    fn streaming_quota_failure_emits_no_partial_body_or_metadata() {
        let transport = Arc::new(FakeTransport::new([response(200, None, &vec![7; 1025])]));
        let backend = Arc::new(HttpRequestBackend::with_parts(
            FakeResolver::new([[public()]]),
            transport,
        ));
        let mut wire_request = request(
            scope(false, 10),
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/data"),
        );
        wire_request.operation = HTTP_STREAM_OPERATION.into();
        let mut manager = ResourceManager::new(8).unwrap();
        manager.register(CapabilityId::HttpRequestV1, backend.clone());
        manager
            .open(
                42,
                &wire_request,
                backend.limits(&wire_request).unwrap(),
                &mut ActivationLedger::new(1),
                0,
            )
            .unwrap();
        let mut host_events = HostEventQueue::new(4, 8);
        for _ in 0..10_000 {
            if !manager.pump(&mut host_events, 1).is_empty() {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(manager.resource_count(), 0);
        assert!(matches!(
            host_events.pop(),
            Some(HostEvent::ResourceEvent {
                sequence: 1,
                result: BrokerResult::Error(BrokerErrorCode::QuotaExceeded),
                ..
            })
        ));
        assert!(host_events.pop().is_none());
    }

    #[test]
    fn permission_confusion_inputs_fail_before_transport() {
        let transport = Arc::new(FakeTransport::new([]));
        let backend = HttpRequestBackend::with_parts(
            FakeResolver::new(std::iter::repeat_n([public()].to_vec(), 10)),
            Arc::clone(&transport),
        );
        let cases = [
            wire(
                HttpRequestMethod::Delete,
                "https://api.example.com/v1/state",
            ),
            wire(HttpRequestMethod::Get, "https://evil.example/v1/state"),
            wire(HttpRequestMethod::Get, "https://api.example.com/v2/state"),
            wire(
                HttpRequestMethod::Get,
                "https://user:secret@api.example.com/v1/state",
            ),
            wire(
                HttpRequestMethod::Get,
                "https://api.example.com/v1/%2e%2e/private",
            ),
            wire(HttpRequestMethod::Get, "https://api.example.com/v1/a%2fb"),
        ];
        for wire in cases {
            assert!(matches!(
                backend.execute(&request(scope(false, 10), &wire), &token()),
                BrokerResult::Error(BrokerErrorCode::OutOfScope | BrokerErrorCode::InvalidRequest)
            ));
        }
        let mut injected = wire(HttpRequestMethod::Get, "https://api.example.com/v1/state");
        injected.accept = Some("text/plain\r\nAuthorization: secret".into());
        assert_eq!(
            backend.execute(&request(scope(false, 10), &injected), &token()),
            BrokerResult::Error(BrokerErrorCode::InvalidRequest)
        );
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn mixed_dns_answers_and_rebinding_on_redirect_fail_closed() {
        let private = SocketAddr::from(([127, 0, 0, 1], 443));
        let transport = Arc::new(FakeTransport::new([]));
        let mixed = HttpRequestBackend::with_parts(
            FakeResolver::new([[public(), private]]),
            Arc::clone(&transport),
        );
        let request = request(
            scope(false, 10),
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/start"),
        );
        assert_eq!(
            mixed.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(transport.calls(), 0);

        let transport = Arc::new(FakeTransport::new([response(
            302,
            Some("/v1/next"),
            b"ignored",
        )]));
        let rebound = HttpRequestBackend::with_parts(
            FakeResolver::new([[public()], [private]]),
            Arc::clone(&transport),
        );
        assert_eq!(
            rebound.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(transport.calls(), 1);
    }

    #[test]
    fn redirect_origin_method_encoding_and_count_are_rechecked() {
        let transport = Arc::new(FakeTransport::new([response(
            302,
            Some("https://evil.example/v1/steal"),
            &[],
        )]));
        let backend =
            HttpRequestBackend::with_parts(FakeResolver::new([[public()]]), Arc::clone(&transport));
        let request = request(
            scope(false, 10),
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/start"),
        );
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(transport.calls(), 1);

        let transport = Arc::new(FakeTransport::new(
            (0..=5).map(|_| response(307, Some("/v1/again"), &[])),
        ));
        let backend = HttpRequestBackend::with_parts(
            FakeResolver::new(std::iter::repeat_n([public()].to_vec(), 6)),
            Arc::clone(&transport),
        );
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(transport.calls(), 6);
    }

    #[test]
    fn redirects_cannot_multiply_the_approved_upload_budget() {
        let transport = Arc::new(FakeTransport::new([
            response(307, Some("/v1/again"), &[]),
            response(200, None, b"should-not-arrive"),
        ]));
        let backend = HttpRequestBackend::with_parts(
            FakeResolver::new([[public()], [public()]]),
            Arc::clone(&transport),
        );
        let mut post = wire(HttpRequestMethod::Post, "https://api.example.com/v1/start");
        post.content_type = Some("application/octet-stream".into());
        post.body = vec![1, 2];
        let request = request(
            HttpRequestScope {
                maximum_request_bytes: 3,
                ..scope(false, 10)
            },
            &post,
        );
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
        assert_eq!(transport.calls(), 1);
    }

    #[test]
    fn private_and_non_routable_address_classes_cannot_bypass_policy() {
        for address in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fd00::1".parse().unwrap()),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()),
            IpAddr::V6("64:ff9b::7f00:1".parse().unwrap()),
            IpAddr::V6("2002:7f00:1::1".parse().unwrap()),
        ] {
            assert!(!address_allowed(address, false), "{address}");
            assert!(address_allowed(address, true), "{address}");
        }
        for address in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6("ff02::1".parse().unwrap()),
        ] {
            assert!(!address_allowed(address, false), "{address}");
            assert!(!address_allowed(address, true), "{address}");
        }
        assert!(address_allowed(
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            false
        ));
        assert!(address_allowed(
            "2606:4700:4700::1111".parse().unwrap(),
            false
        ));
    }

    #[test]
    fn compressed_and_oversized_responses_are_never_forwarded() {
        let mut compressed = response(200, None, b"compressed");
        compressed.content_encoding = Some("gzip".into());
        let transport = Arc::new(FakeTransport::new([compressed]));
        let backend =
            HttpRequestBackend::with_parts(FakeResolver::new([[public()]]), Arc::clone(&transport));
        let request = request(
            scope(false, 10),
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/data"),
        );
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::Unsupported)
        );

        let transport = Arc::new(FakeTransport::new([response(200, None, &vec![0; 1025])]));
        let backend =
            HttpRequestBackend::with_parts(FakeResolver::new([[public()]]), Arc::clone(&transport));
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Error(BrokerErrorCode::QuotaExceeded)
        );
    }

    #[test]
    fn request_rate_is_consumed_during_preflight() {
        let backend = HttpRequestBackend::with_parts(
            FakeResolver::default(),
            Arc::new(FakeTransport::default()),
        );
        let request = request(
            scope(false, 2),
            &wire(HttpRequestMethod::Get, "https://api.example.com/v1/data"),
        );
        let mut activations = ActivationLedger::new(4);
        assert_eq!(
            Backend::authorize(&backend, &request, &mut activations, 1),
            Ok(())
        );
        assert_eq!(
            Backend::authorize(&backend, &request, &mut activations, 2),
            Ok(())
        );
        assert_eq!(
            Backend::authorize(&backend, &request, &mut activations, 3),
            Err(BrokerErrorCode::RateLimited)
        );
        assert_eq!(
            Backend::authorize(&backend, &request, &mut activations, 60_000_001),
            Ok(())
        );
    }

    #[test]
    fn production_transport_uses_private_opt_in_and_sends_no_ambient_credentials() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]).to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("proxy-authorization:"));
            assert!(!request.contains("cookie:"));
            assert!(!request.contains("referer:"));
            assert!(request.contains("accept-encoding: identity"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .unwrap();
        });
        let local_scope = HttpRequestScope {
            origins: BTreeSet::from([HttpOriginRule {
                scheme: "http".into(),
                host: "127.0.0.1".into(),
                port: address.port(),
                path_prefixes: BTreeSet::from(["/demo/".into()]),
            }]),
            methods: BTreeSet::from([HttpMethod::Get]),
            private_network: true,
            maximum_request_bytes: 16,
            maximum_response_bytes: 16,
            maximum_requests_per_minute: 2,
        };
        let request = request(
            local_scope,
            &wire(
                HttpRequestMethod::Get,
                &format!("http://127.0.0.1:{}/demo/status", address.port()),
            ),
        );
        let backend = HttpRequestBackend::new();
        assert_eq!(
            backend.execute(&request, &token()),
            BrokerResult::Success {
                payload: HttpResponse {
                    status: 200,
                    final_url: format!("http://127.0.0.1:{}/demo/status", address.port()),
                    content_type: Some("text/plain".into()),
                    etag: None,
                    body: b"ok".to_vec(),
                }
                .encode()
                .unwrap()
            }
        );
        server.join().unwrap();
    }

    #[test]
    fn production_stream_transport_chunks_without_decompression_or_buffering() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let body = vec![b'x'; MAX_HTTP_STREAM_CHUNK_BYTES * 2 + 19];
        let expected_body = body.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
            assert!(request.contains("accept-encoding: identity"));
            assert!(!request.contains("cookie:"));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let request = wire(
            HttpRequestMethod::Get,
            &format!("http://127.0.0.1:{}/demo/data", address.port()),
        );
        let url = url::Url::parse(&request.url).unwrap();
        let mut statuses = Vec::new();
        let mut chunks = Vec::new();
        let mut metadata = |metadata: &HttpTransportResponse| {
            statuses.push(metadata.status);
            Ok(())
        };
        let mut chunk = |chunk: &[u8]| {
            assert!(chunk.len() <= MAX_HTTP_STREAM_CHUNK_BYTES);
            chunks.extend_from_slice(chunk);
            Ok(())
        };
        let mut callbacks = HttpStreamCallbacks {
            metadata: &mut metadata,
            chunk: &mut chunk,
        };
        let response = ReqwestHttpTransport
            .stream(
                &request,
                &url,
                &[address],
                expected_body.len() as u64,
                &token(),
                &mut callbacks,
            )
            .unwrap();
        assert_eq!(response.body, Vec::<u8>::new());
        assert_eq!(statuses, vec![200]);
        assert_eq!(chunks, expected_body);
        server.join().unwrap();
    }
}
